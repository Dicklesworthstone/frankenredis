//! FrankenRedis standalone server binary.
//!
//! Implements a single-threaded TCP server using `mio` for non-blocking I/O.
//! Each client gets its own `ClientSession` (per-connection auth, transactions,
//! etc.) while sharing a single `ServerState` (store, config) via the `Runtime`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::SocketAddr;
use std::process::ExitCode;

use fr_command::pubsub_message_to_frame;
use fr_config::RuntimePolicy;
use fr_eventloop::{EventLoopMode, TickBudget, plan_tick, validate_accept_path, validate_read_path};
use fr_protocol::{RespFrame, parse_frame};
use fr_runtime::{ClientSession, Runtime};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};

/// Default port matching Redis convention.
const DEFAULT_PORT: u16 = 6379;

/// Token for the TCP listener socket.
const LISTENER: Token = Token(0);

/// Maximum connected clients. Matches Redis default.
const MAX_CLIENTS: usize = 10_000;

/// Per-client query buffer limit (1 MiB). Matches Redis proto-max-bulk-len default.
const QUERY_BUFFER_LIMIT: usize = 1024 * 1024;

/// Maximum write buffer size per client (1 MiB).
const MAX_WRITE_BUFFER: usize = 1024 * 1024;

/// Describes a blocked-on-list operation.
#[derive(Debug, Clone)]
enum BlockingOp {
    /// BLPOP: pop from left of first available key.
    BLpop { keys: Vec<Vec<u8>> },
    /// BRPOP: pop from right of first available key.
    BRpop { keys: Vec<Vec<u8>> },
    /// BLMOVE: move between lists.
    BLmove {
        source: Vec<u8>,
        destination: Vec<u8>,
        wherefrom: Vec<u8>,
        whereto: Vec<u8>,
    },
    /// BZPOPMAX: pop max score from first available key.
    BZpopMax { keys: Vec<Vec<u8>> },
    /// BZPOPMIN: pop min score from first available key.
    BZpopMin { keys: Vec<Vec<u8>> },
}

/// A client that is blocked waiting for data on one or more keys.
struct BlockedState {
    op: BlockingOp,
    /// Absolute timestamp (ms) when the block expires. `u64::MAX` = no timeout.
    deadline_ms: u64,
}

/// Per-client connection state.
struct ClientConnection {
    stream: TcpStream,
    session: ClientSession,
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
    /// True if the client sent QUIT or must be disconnected.
    closing: bool,
    /// If set, the client is blocked waiting for data.
    blocked: Option<BlockedState>,
}

impl ClientConnection {
    fn new(stream: TcpStream, session: ClientSession) -> Self {
        Self {
            stream,
            session,
            read_buf: Vec::with_capacity(4096),
            write_buf: Vec::new(),
            closing: false,
            blocked: None,
        }
    }

    /// Try to flush the write buffer. Returns true if the buffer is fully
    /// drained (or was already empty).
    fn try_flush(&mut self) -> io::Result<bool> {
        while !self.write_buf.is_empty() {
            match self.stream.write(&self.write_buf) {
                Ok(0) => return Err(io::Error::new(ErrorKind::WriteZero, "write zero")),
                Ok(n) => {
                    self.write_buf.drain(..n);
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    let mut port = DEFAULT_PORT;
    let mut mode_str = "hardened";
    let mut aof_path: Option<String> = None;
    let mut rdb_path: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --port requires a value");
                    return ExitCode::from(1);
                }
                port = match args[i].parse() {
                    Ok(p) => p,
                    Err(_) => {
                        eprintln!("error: invalid port number: {}", args[i]);
                        return ExitCode::from(1);
                    }
                };
            }
            "--mode" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --mode requires a value (strict or hardened)");
                    return ExitCode::from(1);
                }
                mode_str = match args[i].as_str() {
                    "strict" | "hardened" => &args[i],
                    other => {
                        eprintln!("error: unknown mode '{other}' (expected: strict, hardened)");
                        return ExitCode::from(1);
                    }
                };
            }
            "--aof" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --aof requires a file path");
                    return ExitCode::from(1);
                }
                aof_path = Some(args[i].clone());
            }
            "--rdb" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --rdb requires a file path");
                    return ExitCode::from(1);
                }
                rdb_path = Some(args[i].clone());
            }
            "--help" | "-h" => {
                println!("frankenredis — FrankenRedis server");
                println!();
                println!("USAGE: frankenredis [OPTIONS]");
                println!();
                println!("OPTIONS:");
                println!("  --port <PORT>   Listen port (default: {DEFAULT_PORT})");
                println!("  --mode <MODE>   Runtime mode: strict or hardened (default: hardened)");
                println!("  --aof <PATH>    AOF persistence file path (enables persistence)");
                println!("  --rdb <PATH>    RDB snapshot file path (enables SAVE/BGSAVE snapshots)");
                println!("  --help          Show this help");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unknown argument: {other}");
                eprintln!("Try 'frankenredis --help' for usage.");
                return ExitCode::from(1);
            }
        }
        i += 1;
    }

    let policy = match mode_str {
        "strict" => RuntimePolicy::default(),
        _ => RuntimePolicy::hardened(),
    };
    let mut runtime = Runtime::new(policy);

    // Configure and load AOF persistence if requested.
    if let Some(path) = &aof_path {
        let aof = std::path::PathBuf::from(path);
        runtime.set_aof_path(aof);
        match runtime.load_aof(now_ms()) {
            Ok(0) => eprintln!("AOF: no existing file or empty (will create on first write)"),
            Ok(n) => eprintln!("AOF: replayed {n} records from {path}"),
            Err(e) => {
                // Non-fatal: AOF file might not exist yet.
                eprintln!("AOF: load warning: {e:?} (starting with empty store)");
            }
        }
    }

    // Configure RDB snapshot persistence if requested.
    if let Some(path) = &rdb_path {
        runtime.set_rdb_path(std::path::PathBuf::from(path));
        eprintln!("RDB: snapshot path configured at {path} (SAVE/BGSAVE will write here)");
    }

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let mut listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: failed to bind to {addr}: {e}");
            return ExitCode::from(1);
        }
    };

    let mut poll = match Poll::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: failed to create poll instance: {e}");
            return ExitCode::from(1);
        }
    };

    if let Err(e) = poll
        .registry()
        .register(&mut listener, LISTENER, Interest::READABLE)
    {
        eprintln!("error: failed to register listener: {e}");
        return ExitCode::from(1);
    }

    eprintln!(
        "FrankenRedis v{} ready (mode={mode_str}, port={port})",
        env!("CARGO_PKG_VERSION"),
    );

    let mut events = Events::with_capacity(1024);
    let mut clients: HashMap<Token, ClientConnection> = HashMap::new();
    let mut next_token: usize = 1;
    let tick_budget = TickBudget::default();

    loop {
        // Use fr-eventloop's tick planner to determine poll timeout.
        let has_blocked = clients.values().any(|c| c.blocked.is_some());
        let pending_writes = clients.values().filter(|c| !c.write_buf.is_empty()).count();
        let tick_plan = plan_tick(0, pending_writes, tick_budget, EventLoopMode::Normal);
        let poll_timeout = if tick_plan.poll_timeout_ms == 0 || has_blocked {
            // When clients are blocked, use a short poll timeout so we
            // can check for available data and timeout expiry frequently.
            Some(std::time::Duration::from_millis(if has_blocked { 100 } else { 0 }))
        } else {
            Some(std::time::Duration::from_millis(tick_plan.poll_timeout_ms))
        };

        if let Err(e) = poll.poll(&mut events, poll_timeout) {
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            eprintln!("error: poll failed: {e}");
            return ExitCode::from(1);
        }

        for event in events.iter() {
            match event.token() {
                LISTENER => {
                    accept_connections(
                        &listener,
                        &mut poll,
                        &mut clients,
                        &mut next_token,
                        &runtime,
                    );
                }
                token => {
                    if event.is_readable() {
                        handle_readable(token, &mut clients, &mut runtime, &mut poll);
                    }
                    if event.is_writable() {
                        handle_writable(token, &mut clients, &mut poll);
                    }
                }
            }
        }

        // Run active expiry cycle once per tick (fast cycle).
        let _ = runtime.run_active_expire_cycle(
            now_ms(),
            fr_eventloop::ActiveExpireCycleKind::Fast,
        );

        // Check blocked clients (BLPOP/BRPOP/BLMOVE) for available data
        // or timeout expiry.
        check_blocked_clients(&mut clients, &mut runtime, &mut poll);

        // Clean up clients marked for closing whose write buffers are drained.
        let to_remove: Vec<Token> = clients
            .iter()
            .filter(|(_, c)| c.closing && c.write_buf.is_empty())
            .map(|(&t, _)| t)
            .collect();
        for token in to_remove {
            if let Some(mut conn) = clients.remove(&token) {
                let _ = poll.registry().deregister(&mut conn.stream);
            }
        }
    }
}

fn accept_connections(
    listener: &TcpListener,
    poll: &mut Poll,
    clients: &mut HashMap<Token, ClientConnection>,
    next_token: &mut usize,
    runtime: &Runtime,
) {
    loop {
        // Check maxclients gate via fr-eventloop before accepting.
        if let Err(e) = validate_accept_path(clients.len(), MAX_CLIENTS, true) {
            eprintln!(
                "warn: rejecting new connection: {} ({})",
                e.reason_code(),
                clients.len()
            );
            // Drain and drop the pending connection.
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
            break;
        }

        match listener.accept() {
            Ok((mut stream, _addr)) => {
                let token = Token(*next_token);
                *next_token = next_token.wrapping_add(1);
                // Avoid colliding with LISTENER token (0).
                if *next_token == 0 {
                    *next_token = 1;
                }

                if let Err(e) = poll.registry().register(
                    &mut stream,
                    token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    eprintln!("warn: failed to register client: {e}");
                    continue;
                }

                let session = runtime.new_session();
                clients.insert(token, ClientConnection::new(stream, session));
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("warn: accept error: {e}");
                break;
            }
        }
    }
}

fn handle_readable(
    token: Token,
    clients: &mut HashMap<Token, ClientConnection>,
    runtime: &mut Runtime,
    poll: &mut Poll,
) {
    let Some(conn) = clients.get_mut(&token) else {
        return;
    };

    // Read available data into the client's buffer.
    let mut buf = [0u8; 8192];
    loop {
        match conn.stream.read(&mut buf) {
            Ok(0) => {
                // Client disconnected.
                conn.closing = true;
                return;
            }
            Ok(n) => {
                // Use fr-eventloop's read path validation.
                match validate_read_path(conn.read_buf.len(), n, QUERY_BUFFER_LIMIT, false) {
                    Ok(_) => {
                        conn.read_buf.extend_from_slice(&buf[..n]);
                    }
                    Err(e) => {
                        eprintln!(
                            "warn: client disconnected: {}",
                            e.reason_code()
                        );
                        conn.closing = true;
                        return;
                    }
                }
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                // Use fr-eventloop's fatal read error path.
                if let Err(rpe) = validate_read_path(0, 0, QUERY_BUFFER_LIMIT, true) {
                    eprintln!(
                        "warn: client read error ({}): {}",
                        rpe.reason_code(),
                        e
                    );
                }
                conn.closing = true;
                return;
            }
        }
    }

    // If the client is blocked (BLPOP/BRPOP/etc.), don't process new
    // commands. We still read data above (to detect disconnection and
    // prevent kernel buffer overflow), but commands are held in read_buf
    // until the blocking operation completes or times out.
    if conn.blocked.is_some() {
        return;
    }

    // Swap in this client's session, process frames, swap back.
    let session = std::mem::take(&mut conn.session);
    let prev = runtime.swap_session(session);

    process_buffered_frames(conn, runtime);

    // Swap session back.
    let updated_session = runtime.swap_session(prev);
    conn.session = updated_session;

    // If there's data to write, ensure we're registered for WRITABLE.
    if !conn.write_buf.is_empty() {
        let _ = poll.registry().reregister(
            &mut conn.stream,
            token,
            Interest::READABLE | Interest::WRITABLE,
        );
    }
}

fn process_buffered_frames(conn: &mut ClientConnection, runtime: &mut Runtime) {
    let ts = now_ms();

    loop {
        if conn.read_buf.is_empty() || conn.closing {
            break;
        }

        // Check write buffer limit before processing more frames.
        if conn.write_buf.len() > MAX_WRITE_BUFFER {
            eprintln!("warn: client write buffer exceeded limit, disconnecting");
            conn.closing = true;
            break;
        }

        // Try inline command parsing only for true non-RESP input. RESP uses
        // multiple leading prefixes; treating every non-array prefix as inline
        // can misclassify protocol frames and break parsing.
        let parse_result = if !conn.read_buf.is_empty()
            && should_try_inline_parsing(conn.read_buf[0])
        {
            match try_parse_inline(&conn.read_buf) {
                Ok(InlineParseResult::EmptyLine(consumed)) => {
                    // Silently consume empty lines (Redis behavior).
                    conn.read_buf.drain(..consumed);
                    continue;
                }
                Ok(InlineParseResult::Command(frame, consumed)) => Ok((frame, consumed)),
                Err(e) => Err(e),
            }
        } else {
            fr_protocol::parse_frame_with_config(&conn.read_buf, &runtime.parser_config()).map(|p| (p.frame, p.consumed))
        };

        match parse_result {
            Ok((frame, consumed)) => {
                let response = runtime.execute_frame(frame.clone(), ts);
                let parsed_frame = frame;

                // Check for QUIT command.
                if is_quit_frame(&parsed_frame) {
                    conn.write_buf.extend_from_slice(&response.to_bytes());
                    conn.closing = true;
                    conn.read_buf.drain(..consumed);
                    break;
                }

                // Check for blocking commands that returned nil — block the
                // client instead of sending the nil response immediately.
                if response == RespFrame::Array(None) || response == RespFrame::BulkString(None) {
                    if let Some(blocked) = try_build_blocked_state(&parsed_frame, ts) {
                        conn.blocked = Some(blocked);
                        conn.read_buf.drain(..consumed);
                        break; // Stop processing — client is now blocked.
                    }
                }

                conn.write_buf.extend_from_slice(&response.to_bytes());
                if let Some(follow_up) = replication_follow_up_bytes(&parsed_frame, &response) {
                    conn.write_buf.extend_from_slice(&follow_up);
                }

                // Drain and deliver any pending pub/sub messages (including
                // shard pub/sub SMessage) generated by the command.
                for msg in runtime.drain_pending_pubsub() {
                    let frame = pubsub_message_to_frame(msg);
                    conn.write_buf.extend_from_slice(&frame.to_bytes());
                }

                conn.read_buf.drain(..consumed);
            }
            Err(fr_protocol::RespParseError::Incomplete) => {
                // Need more data.
                break;
            }
            Err(_) => {
                // Protocol error — send error and disconnect.
                let err_reply = RespFrame::Error("ERR Protocol error: invalid frame".to_string());
                conn.write_buf.extend_from_slice(&err_reply.to_bytes());
                conn.closing = true;
                break;
            }
        }
    }

    // Eagerly try to flush.
    let _ = conn.try_flush();
}

/// Try to parse an inline command (non-RESP). Inline commands are
/// space-separated tokens terminated by \r\n or \n.
/// Returns (frame, consumed_bytes) on success.
/// Result of inline parsing: either a command frame or an empty line that
/// should be silently consumed.
enum InlineParseResult {
    Command(RespFrame, usize),
    EmptyLine(usize),
}

fn should_try_inline_parsing(first_byte: u8) -> bool {
    !matches!(
        first_byte,
        b'+' | b'-' | b':' | b'$' | b'*' | b'~' | b'%' | b'#' | b',' | b'_' | b'(' | b'='
            | b'|' | b'>' | b'!'
    )
}

fn try_parse_inline(buf: &[u8]) -> Result<InlineParseResult, fr_protocol::RespParseError> {
    // Find the line terminator.
    let newline_pos = buf.iter().position(|&b| b == b'\n');
    let Some(nl) = newline_pos else {
        return Err(fr_protocol::RespParseError::Incomplete);
    };
    let consumed = nl + 1;
    let line_end = if nl > 0 && buf[nl - 1] == b'\r' {
        nl - 1
    } else {
        nl
    };
    let line = &buf[..line_end];

    // Split on whitespace, respecting double-quoted strings.
    let argv = split_inline_args(line);
    if argv.is_empty() {
        // Empty line (bare \r\n) — consume it silently (Redis behavior).
        return Ok(InlineParseResult::EmptyLine(consumed));
    }

    let frame = RespFrame::Array(Some(
        argv.into_iter()
            .map(|a| RespFrame::BulkString(Some(a)))
            .collect(),
    ));
    Ok(InlineParseResult::Command(frame, consumed))
}

/// Split inline command arguments, supporting double-quoted strings.
fn split_inline_args(line: &[u8]) -> Vec<Vec<u8>> {
    let mut args = Vec::new();
    let mut i = 0;
    while i < line.len() {
        // Skip whitespace.
        if line[i] == b' ' || line[i] == b'\t' {
            i += 1;
            continue;
        }

        if line[i] == b'"' {
            // Quoted argument.
            i += 1;
            let mut arg = Vec::new();
            while i < line.len() && line[i] != b'"' {
                if line[i] == b'\\' && i + 1 < line.len() {
                    // Handle escape sequences.
                    i += 1;
                    match line[i] {
                        b'n' => arg.push(b'\n'),
                        b'r' => arg.push(b'\r'),
                        b't' => arg.push(b'\t'),
                        b'"' => arg.push(b'"'),
                        b'\\' => arg.push(b'\\'),
                        other => {
                            arg.push(b'\\');
                            arg.push(other);
                        }
                    }
                } else {
                    arg.push(line[i]);
                }
                i += 1;
            }
            if i < line.len() {
                i += 1; // Skip closing quote.
            }
            args.push(arg);
        } else if line[i] == b'\'' {
            // Single-quoted argument (no escape processing).
            i += 1;
            let start = i;
            while i < line.len() && line[i] != b'\'' {
                i += 1;
            }
            args.push(line[start..i].to_vec());
            if i < line.len() {
                i += 1;
            }
        } else {
            // Unquoted argument.
            let start = i;
            while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
                i += 1;
            }
            args.push(line[start..i].to_vec());
        }
    }
    args
}

pub(crate) fn replication_follow_up_bytes(frame: &RespFrame, response: &RespFrame) -> Option<Vec<u8>> {
    if !is_psync_frame(frame) {
        return None;
    }
    let RespFrame::SimpleString(line) = response else {
        return None;
    };
    if !line.starts_with("FULLRESYNC ") {
        return None;
    }

    // Full resync on the wire is followed by an RDB bulk payload. Until the
    // snapshot pipeline is integrated here, emit an empty snapshot so replica
    // handshake clients can complete the TCP-level negotiation.
    Some(RespFrame::BulkString(Some(Vec::new())).to_bytes())
}

pub(crate) fn is_psync_frame(frame: &RespFrame) -> bool {
    if let RespFrame::Array(Some(items)) = frame
        && let Some(RespFrame::BulkString(Some(cmd))) = items.first()
    {
        return cmd.eq_ignore_ascii_case(b"PSYNC");
    }
    false
}

/// Parse a blocking command frame and build `BlockedState` if the command
/// is a blocking operation with a non-zero timeout.
fn try_build_blocked_state(frame: &RespFrame, now_ms: u64) -> Option<BlockedState> {
    let RespFrame::Array(Some(items)) = frame else {
        return None;
    };
    let Some(RespFrame::BulkString(Some(cmd))) = items.first() else {
        return None;
    };

    if cmd.eq_ignore_ascii_case(b"BLPOP") || cmd.eq_ignore_ascii_case(b"BRPOP") || cmd.eq_ignore_ascii_case(b"BZPOPMAX") || cmd.eq_ignore_ascii_case(b"BZPOPMIN") {
        if items.len() < 3 {
            return None;
        }
        // Last element is the timeout.
        let timeout_bytes = match items.last() {
            Some(RespFrame::BulkString(Some(b))) => b,
            _ => return None,
        };
        let timeout_secs: f64 = std::str::from_utf8(timeout_bytes)
            .ok()?
            .parse()
            .ok()?;
        if timeout_secs < 0.0 {
            return None;
        }
        // timeout 0 means block indefinitely (until data arrives).
        let deadline_ms = if timeout_secs == 0.0 {
            u64::MAX
        } else {
            now_ms.saturating_add((timeout_secs * 1000.0) as u64)
        };
        let keys: Vec<Vec<u8>> = items[1..items.len() - 1]
            .iter()
            .filter_map(|f| match f {
                RespFrame::BulkString(Some(k)) => Some(k.clone()),
                _ => None,
            })
            .collect();
        if keys.is_empty() {
            return None;
        }
        let op = if cmd.eq_ignore_ascii_case(b"BLPOP") {
            BlockingOp::BLpop { keys }
        } else if cmd.eq_ignore_ascii_case(b"BRPOP") {
            BlockingOp::BRpop { keys }
        } else if cmd.eq_ignore_ascii_case(b"BZPOPMAX") {
            BlockingOp::BZpopMax { keys }
        } else {
            BlockingOp::BZpopMin { keys }
        };
        Some(BlockedState { op, deadline_ms })
    } else if cmd.eq_ignore_ascii_case(b"BLMOVE") {
        if items.len() != 6 {
            return None;
        }
        let timeout_bytes = match &items[5] {
            RespFrame::BulkString(Some(b)) => b,
            _ => return None,
        };
        let timeout_secs: f64 = std::str::from_utf8(timeout_bytes)
            .ok()?
            .parse()
            .ok()?;
        if timeout_secs < 0.0 {
            return None;
        }
        let deadline_ms = if timeout_secs == 0.0 {
            u64::MAX
        } else {
            now_ms.saturating_add((timeout_secs * 1000.0) as u64)
        };
        let source = match &items[1] {
            RespFrame::BulkString(Some(b)) => b.clone(),
            _ => return None,
        };
        let destination = match &items[2] {
            RespFrame::BulkString(Some(b)) => b.clone(),
            _ => return None,
        };
        let wherefrom = match &items[3] {
            RespFrame::BulkString(Some(b)) => b.clone(),
            _ => return None,
        };
        let whereto = match &items[4] {
            RespFrame::BulkString(Some(b)) => b.clone(),
            _ => return None,
        };
        Some(BlockedState {
            op: BlockingOp::BLmove {
                source,
                destination,
                wherefrom,
                whereto,
            },
            deadline_ms,
        })
    } else if cmd.eq_ignore_ascii_case(b"BRPOPLPUSH") {
        if items.len() != 4 {
            return None;
        }
        let timeout_bytes = match &items[3] {
            RespFrame::BulkString(Some(b)) => b,
            _ => return None,
        };
        let timeout_secs: f64 = std::str::from_utf8(timeout_bytes)
            .ok()?
            .parse()
            .ok()?;
        if timeout_secs < 0.0 {
            return None;
        }
        let deadline_ms = if timeout_secs == 0.0 {
            u64::MAX
        } else {
            now_ms.saturating_add((timeout_secs * 1000.0) as u64)
        };
        let source = match &items[1] {
            RespFrame::BulkString(Some(b)) => b.clone(),
            _ => return None,
        };
        let destination = match &items[2] {
            RespFrame::BulkString(Some(b)) => b.clone(),
            _ => return None,
        };
        Some(BlockedState {
            op: BlockingOp::BLmove {
                source,
                destination,
                wherefrom: b"RIGHT".to_vec(),
                whereto: b"LEFT".to_vec(),
            },
            deadline_ms,
        })
    } else {
        None
    }
}

/// Check all blocked clients. Unblock them if their keys have data or
/// their timeout has expired.
fn check_blocked_clients(
    clients: &mut HashMap<Token, ClientConnection>,
    runtime: &mut Runtime,
    poll: &mut Poll,
) {
    let ts = now_ms();
    let blocked_tokens: Vec<Token> = clients
        .iter()
        .filter(|(_, c)| c.blocked.is_some())
        .map(|(&t, _)| t)
        .collect();

    for token in blocked_tokens {
        let Some(conn) = clients.get_mut(&token) else {
            continue;
        };
        let Some(blocked) = &conn.blocked else {
            continue;
        };

        // Check timeout first.
        if ts >= blocked.deadline_ms {
            // Timeout expired — send nil.
            let nil_response = match &blocked.op {
                BlockingOp::BLpop { .. } | BlockingOp::BRpop { .. } | BlockingOp::BZpopMax { .. } | BlockingOp::BZpopMin { .. } => {
                    RespFrame::Array(None)
                }
                BlockingOp::BLmove { .. } => RespFrame::BulkString(None),
            };
            conn.write_buf.extend_from_slice(&nil_response.to_bytes());
            conn.blocked = None;

            // Process any commands the client pipelined while blocked.
            if !conn.read_buf.is_empty() {
                let session = std::mem::take(&mut conn.session);
                let prev = runtime.swap_session(session);
                process_buffered_frames(conn, runtime);
                let updated_session = runtime.swap_session(prev);
                conn.session = updated_session;
            }

            let _ = flush_or_rearm_client(token, conn, poll);
            continue;
        }

        // Try to fulfill the blocking operation.
        let session = std::mem::take(&mut conn.session);
        let prev = runtime.swap_session(session);

        let result = try_fulfill_blocked(&blocked.op, runtime, ts);

        if let Some(response) = result {
            conn.write_buf.extend_from_slice(&response.to_bytes());
            conn.blocked = None;

            // Process any commands the client pipelined while blocked.
            if !conn.read_buf.is_empty() {
                process_buffered_frames(conn, runtime);
            }
        }

        let updated_session = runtime.swap_session(prev);
        conn.session = updated_session;

        if conn.blocked.is_none() {
            let _ = flush_or_rearm_client(token, conn, poll);
        }
    }
}

fn flush_or_rearm_client(token: Token, conn: &mut ClientConnection, poll: &mut Poll) -> io::Result<()> {
    if conn.write_buf.is_empty() {
        return Ok(());
    }

    match conn.try_flush()? {
        true => {
            poll.registry()
                .reregister(&mut conn.stream, token, Interest::READABLE)?;
        }
        false => {
            poll.registry().reregister(
                &mut conn.stream,
                token,
                Interest::READABLE | Interest::WRITABLE,
            )?;
        }
    }

    Ok(())
}

/// Try to fulfill a blocked operation by checking if the watched keys have
/// data. Returns Some(response) if fulfilled, None if still blocked.
fn try_fulfill_blocked(op: &BlockingOp, runtime: &mut Runtime, now_ms: u64) -> Option<RespFrame> {
    match op {
        BlockingOp::BLpop { keys } => {
            for key in keys {
                // Build LPOP command and execute.
                let argv = vec![b"LPOP".to_vec(), key.clone()];
                let frame = RespFrame::Array(Some(
                    argv.iter()
                        .map(|a| RespFrame::BulkString(Some(a.clone())))
                        .collect(),
                ));
                let response = runtime.execute_frame(frame, now_ms);
                if response != RespFrame::BulkString(None) {
                    // Got data — return [key, value] array.
                    return Some(RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(key.clone())),
                        response,
                    ])));
                }
            }
            None
        }
        BlockingOp::BRpop { keys } => {
            for key in keys {
                let argv = vec![b"RPOP".to_vec(), key.clone()];
                let frame = RespFrame::Array(Some(
                    argv.iter()
                        .map(|a| RespFrame::BulkString(Some(a.clone())))
                        .collect(),
                ));
                let response = runtime.execute_frame(frame, now_ms);
                if response != RespFrame::BulkString(None) {
                    return Some(RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(key.clone())),
                        response,
                    ])));
                }
            }
            None
        }
        BlockingOp::BZpopMax { keys } => {
            for key in keys {
                let argv = vec![b"ZPOPMAX".to_vec(), key.clone()];
                let frame = RespFrame::Array(Some(
                    argv.iter()
                        .map(|a| RespFrame::BulkString(Some(a.clone())))
                        .collect(),
                ));
                let response = runtime.execute_frame(frame, now_ms);
                if response != RespFrame::Array(None) {
                    // ZPOPMAX returns [member, score]. BZPOPMAX needs [key, member, score]
                    if let RespFrame::Array(Some(mut items)) = response {
                        if items.len() == 2 {
                            let mut result = vec![RespFrame::BulkString(Some(key.clone()))];
                            result.append(&mut items);
                            return Some(RespFrame::Array(Some(result)));
                        }
                    }
                }
            }
            None
        }
        BlockingOp::BZpopMin { keys } => {
            for key in keys {
                let argv = vec![b"ZPOPMIN".to_vec(), key.clone()];
                let frame = RespFrame::Array(Some(
                    argv.iter()
                        .map(|a| RespFrame::BulkString(Some(a.clone())))
                        .collect(),
                ));
                let response = runtime.execute_frame(frame, now_ms);
                if response != RespFrame::Array(None) {
                    if let RespFrame::Array(Some(mut items)) = response {
                        if items.len() == 2 {
                            let mut result = vec![RespFrame::BulkString(Some(key.clone()))];
                            result.append(&mut items);
                            return Some(RespFrame::Array(Some(result)));
                        }
                    }
                }
            }
            None
        }
        BlockingOp::BLmove {
            source,
            destination,
            wherefrom,
            whereto,
        } => {
            let argv = vec![
                b"LMOVE".to_vec(),
                source.clone(),
                destination.clone(),
                wherefrom.clone(),
                whereto.clone(),
            ];
            let frame = RespFrame::Array(Some(
                argv.iter()
                    .map(|a| RespFrame::BulkString(Some(a.clone())))
                    .collect(),
            ));
            let response = runtime.execute_frame(frame, now_ms);
            if response != RespFrame::BulkString(None) {
                Some(response)
            } else {
                None
            }
        }
    }
}

fn is_quit_frame(frame: &RespFrame) -> bool {
    if let RespFrame::Array(Some(items)) = frame
        && let Some(RespFrame::BulkString(Some(cmd))) = items.first()
    {
        return cmd.eq_ignore_ascii_case(b"QUIT");
    }
    false
}

fn handle_writable(token: Token, clients: &mut HashMap<Token, ClientConnection>, poll: &mut Poll) {
    let Some(conn) = clients.get_mut(&token) else {
        return;
    };

    match conn.try_flush() {
        Ok(true) => {
            // Write buffer fully drained — only need READABLE now.
            let _ = poll
                .registry()
                .reregister(&mut conn.stream, token, Interest::READABLE);
        }
        Ok(false) => {
            // Still have data to write, keep WRITABLE interest.
        }
        Err(_) => {
            conn.closing = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::replication_follow_up_bytes;
    use crate::{InlineParseResult, should_try_inline_parsing};
    use fr_config::RuntimePolicy;
    use fr_protocol::RespFrame;
    use fr_runtime::Runtime;

    #[test]
    fn server_bootstrap_creates_runtime() {
        let _strict = Runtime::new(RuntimePolicy::default());
        let _hardened = Runtime::new(RuntimePolicy::hardened());
    }

    #[test]
    fn server_bootstrap_processes_ping() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        let now_ms = 1_000_000u64;
        let response = runtime.execute_bytes(b"*1\r\n$4\r\nPING\r\n", now_ms);
        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("PONG"),
            "PING should return PONG, got: {response_str}"
        );
    }

    #[test]
    fn session_swap_preserves_isolation() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        let session_a = runtime.new_session();
        let session_b = runtime.new_session();

        // Swap in session A, execute SET.
        let prev = runtime.swap_session(session_a);
        runtime.execute_bytes(b"*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n", 1_000);
        let session_a = runtime.swap_session(prev);

        // Swap in session B, execute GET — should see the value because the
        // store is shared.
        let prev = runtime.swap_session(session_b);
        let resp = runtime.execute_bytes(b"*2\r\n$3\r\nGET\r\n$1\r\na\r\n", 1_000);
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains('1'),
            "sessions share store, got: {resp_str}"
        );
        let _session_b = runtime.swap_session(prev);

        // Verify session A is still intact.
        drop(session_a);
    }

    #[test]
    fn psync_fullresync_emits_empty_rdb_follow_up() {
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PSYNC".to_vec())),
            RespFrame::BulkString(Some(b"?".to_vec())),
            RespFrame::BulkString(Some(b"-1".to_vec())),
        ]));
        let response = RespFrame::SimpleString(
            "FULLRESYNC 0000000000000000000000000000000000000000 0".to_string(),
        );

        let follow_up =
            replication_follow_up_bytes(&frame, &response).expect("psync should emit snapshot");

        assert_eq!(follow_up, b"$0\r\n\r\n");
    }

    #[test]
    fn non_psync_commands_emit_no_replication_follow_up() {
        let frame = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(
            b"REPLCONF".to_vec(),
        ))]));
        let response = RespFrame::SimpleString("OK".to_string());

        assert_eq!(replication_follow_up_bytes(&frame, &response), None);
    }

    #[test]
    fn psync_non_fullresync_response_emits_no_follow_up() {
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PSYNC".to_vec())),
            RespFrame::BulkString(Some(b"?".to_vec())),
            RespFrame::BulkString(Some(b"-1".to_vec())),
        ]));
        let response = RespFrame::Error("ERR fallback".to_string());

        assert_eq!(replication_follow_up_bytes(&frame, &response), None);
    }

    #[test]
    fn inline_command_parsing() {
        let parsed = crate::try_parse_inline(b"SET key value\r\n").expect("parse inline");
        let InlineParseResult::Command(frame, consumed) = parsed else {
            panic!("expected inline command");
        };
        assert_eq!(consumed, 15);
        let RespFrame::Array(Some(items)) = frame else {
            panic!("expected array");
        };
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], RespFrame::BulkString(Some(b"SET".to_vec())));
        assert_eq!(items[1], RespFrame::BulkString(Some(b"key".to_vec())));
        assert_eq!(items[2], RespFrame::BulkString(Some(b"value".to_vec())));
    }

    #[test]
    fn inline_quoted_strings() {
        let args = crate::split_inline_args(b"SET key \"hello world\"");
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], b"SET");
        assert_eq!(args[1], b"key");
        assert_eq!(args[2], b"hello world");
    }

    #[test]
    fn inline_incomplete_returns_error() {
        let result = crate::try_parse_inline(b"SET key value");
        assert!(result.is_err());
    }

    #[test]
    fn blank_inline_line_is_consumed_without_command() {
        let result = crate::try_parse_inline(b"\r\n").expect("blank line should parse");
        match result {
            InlineParseResult::EmptyLine(consumed) => assert_eq!(consumed, 2),
            InlineParseResult::Command(_, _) => panic!("blank line should not become a command"),
        }
    }

    #[test]
    fn inline_parser_gate_recognizes_all_resp_prefixes() {
        for prefix in [
            b'+', b'-', b':', b'$', b'*', b'~', b'%', b'#', b',', b'_', b'(', b'=', b'|',
            b'>', b'!',
        ] {
            assert!(
                !should_try_inline_parsing(prefix),
                "prefix {prefix:?} should stay on RESP parser path"
            );
        }

        assert!(should_try_inline_parsing(b'P'));
        assert!(should_try_inline_parsing(b' '));
    }
}
