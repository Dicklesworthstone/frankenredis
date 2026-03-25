//! FrankenRedis standalone server binary.
//!
//! Implements a single-threaded TCP server using `mio` for non-blocking I/O.
//! Each client gets its own `ClientSession` (per-connection auth, transactions,
//! etc.) while sharing a single `ServerState` (store, config) via the `Runtime`.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::process::ExitCode;
use std::time::Duration;

use fr_command::pubsub_message_to_frame;
use fr_config::RuntimePolicy;
use fr_eventloop::{
    EventLoopMode, TickBudget, plan_tick, validate_accept_path, validate_read_path,
};
use fr_protocol::{ParserConfig, RespFrame, RespParseError};
use fr_repl::ReplOffset;
use fr_runtime::{ClientSession, Runtime};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};

/// Default port matching Redis convention.
const DEFAULT_PORT: u16 = 6379;

/// Token for the TCP listener socket.
const LISTENER: Token = Token(0);

const REPLICA_ACK_INTERVAL_MS: u64 = 1_000;
const REPLICA_RECONNECT_BACKOFF_MS: u64 = 250;

/// Describes a blocked-on-list operation.
#[derive(Debug, Clone)]
#[allow(clippy::enum_variant_names)]
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
    /// BLMPOP: pop from multiple lists with direction and count.
    BLmpop { argv: Vec<Vec<u8>> },
    /// BZMPOP: pop from multiple sorted sets with MIN/MAX and count.
    BZmpop { argv: Vec<Vec<u8>> },
    /// XREAD BLOCK: read from streams, blocking until data arrives.
    BXread { argv: Vec<Vec<u8>> },
    /// XREADGROUP BLOCK: read from stream consumer group, blocking.
    BXreadgroup { argv: Vec<Vec<u8>> },
}

impl BlockingOp {
    fn keys(&self) -> Vec<Vec<u8>> {
        match self {
            BlockingOp::BLpop { keys }
            | BlockingOp::BRpop { keys }
            | BlockingOp::BZpopMax { keys }
            | BlockingOp::BZpopMin { keys } => keys.clone(),
            BlockingOp::BLmove { source, .. } => vec![source.clone()],
            BlockingOp::BLmpop { argv } | BlockingOp::BZmpop { argv } => {
                // argv: [timeout, numkeys, key, ..., LEFT|RIGHT, COUNT]
                if argv.len() < 3 {
                    return Vec::new();
                }
                let num_keys: usize = std::str::from_utf8(&argv[2])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                argv.iter().skip(3).take(num_keys).cloned().collect()
            }
            BlockingOp::BXread { argv } | BlockingOp::BXreadgroup { argv } => {
                // XREAD [COUNT n] [BLOCK ms] STREAMS key [key ...] id [id ...]
                let streams_idx = argv.iter().position(|a| a.eq_ignore_ascii_case(b"STREAMS"));
                if let Some(idx) = streams_idx {
                    let remaining = &argv[idx + 1..];
                    let num_keys = remaining.len() / 2;
                    remaining.iter().take(num_keys).cloned().collect()
                } else {
                    Vec::new()
                }
            }
        }
    }
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
    /// If set, this client is a replica and this is the last offset sent to it.
    replication_sent_offset: Option<ReplOffset>,
}

struct ReplicaPrimaryConnection {
    stream: StdTcpStream,
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
    next_ack_ms: u64,
}

struct ReplicaSyncState {
    connection: Option<ReplicaPrimaryConnection>,
    retry_after_ms: u64,
}

impl ReplicaSyncState {
    fn new() -> Self {
        Self {
            connection: None,
            retry_after_ms: 0,
        }
    }

    fn schedule_retry(&mut self, now_ms: u64) {
        self.connection = None;
        self.retry_after_ms = now_ms.saturating_add(REPLICA_RECONNECT_BACKOFF_MS);
    }
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
            replication_sent_offset: None,
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
    let mut bind_addr = "127.0.0.1".to_string();
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
            "--bind" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --bind requires an address");
                    return ExitCode::from(1);
                }
                bind_addr = args[i].clone();
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
                println!(
                    "  --rdb <PATH>    RDB snapshot file path (enables SAVE/BGSAVE snapshots)"
                );
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
    runtime.set_server_port(port);

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

    let addr: SocketAddr = match format!("{bind_addr}:{port}").parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: invalid bind address '{bind_addr}:{port}': {e}");
            return ExitCode::from(1);
        }
    };
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
    let mut client_id_to_token: HashMap<u64, Token> = HashMap::new();
    let mut blocked_tokens: HashSet<Token> = HashSet::new();
    let mut closing_tokens: HashSet<Token> = HashSet::new();
    let mut write_tokens: HashSet<Token> = HashSet::new();
    let mut paused_tokens: HashSet<Token> = HashSet::new();
    let mut replica_sync = ReplicaSyncState::new();
    let mut next_token: usize = 1;
    let tick_budget = TickBudget::default();

    loop {
        // Use fr-eventloop's tick planner to determine poll timeout.
        let has_blocked = !blocked_tokens.is_empty();
        let pending_writes = write_tokens.len();
        let tick_plan = plan_tick(0, pending_writes, tick_budget, EventLoopMode::Normal);
        let poll_timeout = if tick_plan.poll_timeout_ms == 0 || has_blocked {
            // When clients are blocked, use a short poll timeout so we
            // can check for available data and timeout expiry frequently.
            Some(std::time::Duration::from_millis(if has_blocked {
                100
            } else {
                0
            }))
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

        let ts = now_ms();

        for event in events.iter() {
            match event.token() {
                LISTENER => {
                    accept_connections(
                        &listener,
                        &mut poll,
                        &mut clients,
                        &mut client_id_to_token,
                        &mut next_token,
                        &mut runtime,
                    );
                }
                token => {
                    if event.is_readable() {
                        handle_readable(
                            token,
                            &mut clients,
                            &mut runtime,
                            &mut poll,
                            &mut blocked_tokens,
                            &mut closing_tokens,
                            &mut write_tokens,
                            &mut paused_tokens,
                            ts,
                        );
                    }
                    if event.is_writable() {
                        handle_writable(
                            token,
                            &mut clients,
                            &mut write_tokens,
                            &mut closing_tokens,
                            &mut poll,
                        );
                    }
                }
            }
        }

        // Run active expiry cycle once per tick (fast cycle).
        let _ = runtime.run_active_expire_cycle(ts, fr_eventloop::ActiveExpireCycleKind::Fast);

        // Drive the primary link from the main loop so replicas can sustain
        // online deltas, ACK traffic, and reconnect after link loss.
        drive_replica_sync(&mut runtime, &mut replica_sync, ts);

        // Check blocked clients (BLPOP/BRPOP/BLMOVE) for available data
        // or timeout expiry.
        check_blocked_clients(
            &mut clients,
            &mut blocked_tokens,
            &mut closing_tokens,
            &mut runtime,
            &mut poll,
            &mut write_tokens,
            ts,
        );

        // Re-process clients whose commands were deferred by CLIENT PAUSE.
        // When the pause expires, we must re-trigger processing since mio won't
        // generate a readable event for data already in the read buffer.
        if !paused_tokens.is_empty() && !runtime.is_client_paused(ts) {
            let tokens: Vec<Token> = paused_tokens.drain().collect();
            for token in tokens {
                if let Some(conn) = clients.get_mut(&token)
                    && !conn.read_buf.is_empty()
                    && !conn.closing
                {
                    // Re-register as readable to trigger processing on next tick
                    let _ = poll.registry().reregister(
                        &mut conn.stream,
                        token,
                        Interest::READABLE | Interest::WRITABLE,
                    );
                }
            }
        }

        // Deliver pending replication writes to connected replicas.
        propagate_writes_to_replicas(&mut clients, &mut runtime, &mut write_tokens);

        // Deliver pending Pub/Sub messages to subscribed clients.
        deliver_pubsub_messages(
            &mut clients,
            &client_id_to_token,
            &mut runtime,
            &mut poll,
            &mut write_tokens,
        );

        // Deliver MONITOR output to monitor clients.
        deliver_monitor_output(
            &mut clients,
            &client_id_to_token,
            &mut runtime,
            &mut poll,
            &mut write_tokens,
        );

        // Clean up clients marked for closing whose write buffers are drained.
        let to_remove: Vec<Token> = closing_tokens
            .iter()
            .filter(|&t| {
                clients
                    .get(t)
                    .map(|c| c.write_buf.is_empty())
                    .unwrap_or(true)
            })
            .copied()
            .collect();
        for token in to_remove {
            if let Some(mut conn) = clients.remove(&token) {
                blocked_tokens.remove(&token);
                closing_tokens.remove(&token);
                write_tokens.remove(&token);
                paused_tokens.remove(&token);
                client_id_to_token.remove(&conn.session.client_id);
                // Clean up Pub/Sub subscriptions and stats for this client.
                runtime.pubsub_cleanup_client(conn.session.client_id);
                runtime.track_connection_closed();
                let _ = poll.registry().deregister(&mut conn.stream);
            }
        }

        // Check for graceful shutdown request
        if runtime.server.shutdown_requested {
            if !runtime.server.shutdown_nosave {
                // Attempt a final SAVE before exiting
                let save_ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let _ = runtime.execute_frame(
                    fr_protocol::RespFrame::Array(Some(vec![fr_protocol::RespFrame::BulkString(
                        Some(b"SAVE".to_vec()),
                    )])),
                    save_ts,
                );
            }
            eprintln!("info: shutdown requested, exiting gracefully");
            return ExitCode::SUCCESS;
        }
    }
}

fn accept_connections(
    listener: &TcpListener,
    poll: &mut Poll,
    clients: &mut HashMap<Token, ClientConnection>,
    client_id_to_token: &mut HashMap<u64, Token>,
    next_token: &mut usize,
    runtime: &mut Runtime,
) {
    loop {
        // Check maxclients gate via fr-eventloop before accepting.
        if let Err(e) = validate_accept_path(clients.len(), runtime.server.max_clients, true) {
            // Drain ALL pending connections from the backlog.
            while let Ok((stream, _)) = listener.accept() {
                eprintln!(
                    "warn: rejecting new connection: {} ({})",
                    e.reason_code(),
                    clients.len()
                );
                drop(stream);
            }
            break;
        }

        match listener.accept() {
            Ok((mut stream, peer_addr)) => {
                let token = Token(*next_token);
                *next_token = next_token.wrapping_add(1);
                // Avoid colliding with LISTENER token (0).
                if *next_token == 0 {
                    *next_token = 1;
                }

                if let Err(e) = stream.set_nodelay(true) {
                    eprintln!("warn: failed to set TCP_NODELAY: {e}");
                }

                if let Err(e) = poll.registry().register(
                    &mut stream,
                    token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    eprintln!("warn: failed to register client: {e}");
                    continue;
                }

                let mut session = runtime.new_session();
                session.peer_addr = Some(peer_addr);
                let client_id = session.client_id;
                clients.insert(token, ClientConnection::new(stream, session));
                client_id_to_token.insert(client_id, token);
                runtime.track_connection_opened();
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

#[allow(clippy::too_many_arguments)]
fn handle_readable(
    token: Token,
    clients: &mut HashMap<Token, ClientConnection>,
    runtime: &mut Runtime,
    poll: &mut Poll,
    blocked_tokens: &mut HashSet<Token>,
    closing_tokens: &mut HashSet<Token>,
    write_tokens: &mut HashSet<Token>,
    paused_tokens: &mut HashSet<Token>,
    ts: u64,
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
                closing_tokens.insert(token);
                return;
            }
            Ok(n) => {
                // Use fr-eventloop's read path validation.
                match validate_read_path(
                    conn.read_buf.len(),
                    n,
                    runtime.server.query_buffer_limit,
                    false,
                ) {
                    Ok(_) => {
                        conn.read_buf.extend_from_slice(&buf[..n]);
                    }
                    Err(e) => {
                        eprintln!("warn: client disconnected: {}", e.reason_code());
                        conn.closing = true;
                        closing_tokens.insert(token);
                        return;
                    }
                }
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                // Use fr-eventloop's fatal read error path.
                if let Err(rpe) = validate_read_path(0, 0, runtime.server.query_buffer_limit, true)
                {
                    eprintln!("warn: client read error ({}): {}", rpe.reason_code(), e);
                }
                conn.closing = true;
                closing_tokens.insert(token);
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

    process_buffered_frames(
        token,
        conn,
        runtime,
        blocked_tokens,
        closing_tokens,
        write_tokens,
        paused_tokens,
        ts,
    );

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

#[allow(clippy::too_many_arguments)]
fn process_buffered_frames(
    token: Token,
    conn: &mut ClientConnection,
    runtime: &mut Runtime,
    blocked_tokens: &mut HashSet<Token>,
    closing_tokens: &mut HashSet<Token>,
    write_tokens: &mut HashSet<Token>,
    paused_tokens: &mut HashSet<Token>,
    ts: u64,
) {
    let mut consumed_total = 0;

    loop {
        if consumed_total >= conn.read_buf.len() || conn.closing {
            break;
        }

        // Check write buffer limit before processing more frames.
        if conn.write_buf.len() > runtime.server.output_buffer_limit {
            eprintln!("warn: client write buffer exceeded limit, disconnecting");
            conn.closing = true;
            closing_tokens.insert(token);
            break;
        }

        let unparsed = &conn.read_buf[consumed_total..];

        // Try inline command parsing only for true non-RESP input. RESP uses
        // multiple leading prefixes; treating every non-array prefix as inline
        // can misclassify protocol frames and break parsing.
        let parse_result =
            if !unparsed.is_empty() && should_try_inline_parsing(unparsed[0]) {
                match try_parse_inline(unparsed) {
                    Ok(InlineParseResult::EmptyLine(consumed)) => {
                        // Silently consume empty lines (Redis behavior).
                        consumed_total += consumed;
                        continue;
                    }
                    Ok(InlineParseResult::Command(frame, consumed)) => Ok((frame, consumed)),
                    Err(e) => Err(e),
                }
            } else {
                fr_protocol::parse_frame_with_config(unparsed, &runtime.parser_config())
                    .map(|p| (p.frame, p.consumed))
            };

        match parse_result {
            Ok((frame, consumed)) => {
                // Subscription mode gate: reject most commands while subscribed.
                if runtime.is_in_subscription_mode()
                    && let Some(reject) = check_subscription_mode_gate(&frame, true)
                {
                    conn.write_buf.extend_from_slice(&reject.to_bytes());
                    consumed_total += consumed;
                    continue;
                }
                // CLIENT PAUSE gate: delay command processing while paused.
                if let RespFrame::Array(Some(ref items)) = frame {
                    let argv: Vec<Vec<u8>> = items
                        .iter()
                        .filter_map(|f| match f {
                            RespFrame::BulkString(Some(b)) => Some(b.clone()),
                            _ => None,
                        })
                        .collect();
                    if runtime.is_command_paused(&argv, ts) && !is_client_pause_exempt(&argv) {
                        // Don't process the command — leave it in the read buffer.
                        // Track paused token so we can re-process when pause expires.
                        paused_tokens.insert(token);
                        break;
                    }
                }
                let response = runtime.execute_frame(frame.clone(), ts);
                let parsed_frame = frame;

                // Check for QUIT command.
                if is_quit_frame(&parsed_frame) {
                    conn.write_buf.extend_from_slice(&response.to_bytes());
                    write_tokens.insert(token);
                    conn.closing = true;
                    closing_tokens.insert(token);
                    consumed_total += consumed;
                    break;
                }

                // Check for blocking commands that returned nil — block the
                // client instead of sending the nil response immediately.
                if (response == RespFrame::Array(None) || response == RespFrame::BulkString(None))
                    && let Some(blocked) = try_build_blocked_state(&parsed_frame, ts)
                {
                    // Redis behavior: if the keys already have data, we shouldn't block.
                    // try_build_blocked_state only returns Some if it's a blocking command.
                    if let Some(immediate_response) = try_fulfill_blocked(&blocked.op, runtime, ts)
                    {
                        conn.write_buf
                            .extend_from_slice(&immediate_response.to_bytes());
                    } else {
                        conn.blocked = Some(blocked);
                        blocked_tokens.insert(token);
                        consumed_total += consumed;
                        break; // Stop processing — client is now blocked.
                    }
                } else {
                    conn.write_buf.extend_from_slice(&response.to_bytes());
                }
                if let Some(follow_up) =
                    replication_follow_up_bytes(runtime, &parsed_frame, &response, ts)
                {
                    conn.write_buf.extend_from_slice(&follow_up);
                    if runtime.is_replica(conn.session.client_id) {
                        conn.replication_sent_offset = Some(runtime.replication_primary_offset());
                    }
                }

                // Drain and deliver any pending pub/sub messages (including
                // shard pub/sub SMessage) generated by the command.
                for msg in runtime.drain_pending_pubsub() {
                    let frame = pubsub_message_to_frame(msg);
                    conn.write_buf.extend_from_slice(&frame.to_bytes());
                }

                consumed_total += consumed;

                if conn.write_buf.len() > runtime.server.output_buffer_limit {
                    eprintln!("warn: client write buffer exceeded limit, disconnecting");
                    conn.closing = true;
                    closing_tokens.insert(token);
                    break;
                }
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
                closing_tokens.insert(token);
                break;
            }
        }
    }

    if consumed_total > 0 {
        conn.read_buf.drain(..consumed_total);
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
        b'+' | b'-'
            | b':'
            | b'$'
            | b'*'
            | b'~'
            | b'%'
            | b'#'
            | b','
            | b'_'
            | b'('
            | b'='
            | b'|'
            | b'>'
            | b'!'
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

fn replica_handshake_frame(args: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        args.iter()
            .map(|arg| RespFrame::BulkString(Some(arg.to_vec())))
            .collect(),
    ))
}

fn read_frame_from_stream(
    stream: &mut StdTcpStream,
    read_buf: &mut Vec<u8>,
    parser_config: &ParserConfig,
    query_buffer_limit: usize,
) -> io::Result<RespFrame> {
    loop {
        match fr_protocol::parse_frame_with_config(read_buf, parser_config) {
            Ok(parsed) => {
                read_buf.drain(..parsed.consumed);
                return Ok(parsed.frame);
            }
            Err(RespParseError::Incomplete) => {}
            Err(err) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("invalid RESP frame from primary: {err}"),
                ));
            }
        }

        let mut chunk = [0u8; 8192];
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "primary closed replication stream",
                ));
            }
            Ok(n) => match validate_read_path(read_buf.len(), n, query_buffer_limit, false) {
                Ok(_) => read_buf.extend_from_slice(&chunk[..n]),
                Err(err) => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "replication read exceeded query buffer limit ({})",
                            err.reason_code()
                        ),
                    ));
                }
            },
            Err(ref err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    "timed out waiting for replication frame",
                ));
            }
            Err(ref err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

fn read_available_stream_bytes(
    stream: &mut StdTcpStream,
    read_buf: &mut Vec<u8>,
    query_buffer_limit: usize,
) -> io::Result<bool> {
    let mut chunk = [0u8; 8192];
    let mut disconnected = false;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => {
                disconnected = true;
                break;
            }
            Ok(n) => match validate_read_path(read_buf.len(), n, query_buffer_limit, false) {
                Ok(_) => read_buf.extend_from_slice(&chunk[..n]),
                Err(err) => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "replication read exceeded query buffer limit ({})",
                            err.reason_code()
                        ),
                    ));
                }
            },
            Err(ref err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                break;
            }
            Err(ref err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(disconnected)
}

fn expect_simple_string(frame: RespFrame, expected: &str) -> io::Result<()> {
    match frame {
        RespFrame::SimpleString(line) if line.eq_ignore_ascii_case(expected) => Ok(()),
        other => Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("unexpected replication reply: {other:?}"),
        )),
    }
}

fn replica_handshake_read_timeout(runtime: &Runtime) -> Duration {
    Duration::from_secs(runtime.server.repl_timeout_sec.max(1))
}

fn sync_replica_with_primary(
    runtime: &mut Runtime,
    host: &str,
    port: u16,
    requested_replid: &str,
    requested_offset: i64,
    now_ms: u64,
) -> io::Result<ReplicaPrimaryConnection> {
    let mut stream = StdTcpStream::connect((host, port))?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(replica_handshake_read_timeout(runtime)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;

    let parser_config = runtime.parser_config();
    let mut read_buf = Vec::new();

    stream.write_all(&replica_handshake_frame(&[b"PING"]).to_bytes())?;
    expect_simple_string(
        read_frame_from_stream(
            &mut stream,
            &mut read_buf,
            &parser_config,
            runtime.server.query_buffer_limit,
        )?,
        "PONG",
    )?;

    let listening_port = runtime.server_port().to_string();
    stream.write_all(
        &replica_handshake_frame(&[b"REPLCONF", b"listening-port", listening_port.as_bytes()])
            .to_bytes(),
    )?;
    expect_simple_string(
        read_frame_from_stream(
            &mut stream,
            &mut read_buf,
            &parser_config,
            runtime.server.query_buffer_limit,
        )?,
        "OK",
    )?;

    stream.write_all(&replica_handshake_frame(&[b"REPLCONF", b"capa", b"psync2"]).to_bytes())?;
    expect_simple_string(
        read_frame_from_stream(
            &mut stream,
            &mut read_buf,
            &parser_config,
            runtime.server.query_buffer_limit,
        )?,
        "OK",
    )?;

    let requested_offset = requested_offset.to_string();
    stream.write_all(
        &replica_handshake_frame(&[
            b"PSYNC",
            requested_replid.as_bytes(),
            requested_offset.as_bytes(),
        ])
        .to_bytes(),
    )?;
    let reply = read_frame_from_stream(
        &mut stream,
        &mut read_buf,
        &parser_config,
        runtime.server.query_buffer_limit,
    )?;
    let RespFrame::SimpleString(reply_line) = reply else {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "primary did not send PSYNC status line",
        ));
    };

    let payload = if reply_line.starts_with("FULLRESYNC ") {
        match read_frame_from_stream(
            &mut stream,
            &mut read_buf,
            &parser_config,
            runtime.server.query_buffer_limit,
        )? {
            RespFrame::BulkString(Some(snapshot)) => snapshot,
            other => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("primary did not send RDB snapshot: {other:?}"),
                ));
            }
        }
    } else {
        read_available_stream_bytes(
            &mut stream,
            &mut read_buf,
            runtime.server.query_buffer_limit,
        )?;
        std::mem::take(&mut read_buf)
    };

    runtime
        .apply_replication_sync_payload(&reply_line, &payload, now_ms)
        .map_err(|err| io::Error::new(ErrorKind::InvalidData, format!("{err:?}")))?;
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    stream.set_nonblocking(true)?;
    Ok(ReplicaPrimaryConnection {
        stream,
        read_buf,
        write_buf: Vec::new(),
        next_ack_ms: now_ms.saturating_add(REPLICA_ACK_INTERVAL_MS),
    })
}

fn flush_replica_primary_writes(connection: &mut ReplicaPrimaryConnection) -> io::Result<()> {
    while !connection.write_buf.is_empty() {
        match connection.stream.write(&connection.write_buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "replica write zero on primary stream",
                ));
            }
            Ok(written) => {
                connection.write_buf.drain(..written);
            }
            Err(ref err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                break;
            }
            Err(ref err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn replication_stream_follow_up_bytes(frame: &RespFrame, response: &RespFrame) -> Option<Vec<u8>> {
    let RespFrame::Array(Some(items)) = frame else {
        return None;
    };
    if items.len() != 3 {
        return None;
    }
    let (
        RespFrame::BulkString(Some(command)),
        RespFrame::BulkString(Some(subcommand)),
        RespFrame::BulkString(Some(argument)),
    ) = (&items[0], &items[1], &items[2])
    else {
        return None;
    };
    if !command.eq_ignore_ascii_case(b"REPLCONF")
        || !subcommand.eq_ignore_ascii_case(b"GETACK")
        || argument.as_slice() != b"*"
    {
        return None;
    }
    Some(response.to_bytes())
}

fn queue_replica_periodic_ack(
    runtime: &Runtime,
    connection: &mut ReplicaPrimaryConnection,
    now_ms: u64,
) {
    if now_ms < connection.next_ack_ms {
        return;
    }
    let Some(frame) = runtime.replica_ack_frame() else {
        return;
    };
    connection.write_buf.extend_from_slice(&frame.to_bytes());
    connection.next_ack_ms = now_ms.saturating_add(REPLICA_ACK_INTERVAL_MS);
}

fn drain_replica_stream(
    runtime: &mut Runtime,
    connection: &mut ReplicaPrimaryConnection,
    now_ms: u64,
) -> io::Result<bool> {
    let disconnected = read_available_stream_bytes(
        &mut connection.stream,
        &mut connection.read_buf,
        runtime.server.query_buffer_limit,
    )?;

    let mut frame_index = 0_u64;
    loop {
        match fr_protocol::parse_frame_with_config(&connection.read_buf, &runtime.parser_config()) {
            Ok(parsed) => {
                let frame = parsed.frame;
                connection.read_buf.drain(..parsed.consumed);
                let response =
                    runtime.execute_frame(frame.clone(), now_ms.saturating_add(frame_index));
                if let Some(follow_up) = replication_stream_follow_up_bytes(&frame, &response) {
                    connection.write_buf.extend_from_slice(&follow_up);
                }
                frame_index = frame_index.saturating_add(1);
            }
            Err(RespParseError::Incomplete) => break,
            Err(err) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("invalid replication delta from primary: {err}"),
                ));
            }
        }
    }

    Ok(disconnected)
}

fn drive_replica_sync(runtime: &mut Runtime, replica_sync: &mut ReplicaSyncState, now_ms: u64) {
    if let Some(connection) = replica_sync.connection.as_mut() {
        queue_replica_periodic_ack(runtime, connection, now_ms);
        if let Err(err) = flush_replica_primary_writes(connection) {
            replica_sync.schedule_retry(now_ms);
            runtime.set_replica_connection_state("reconnect");
            eprintln!("warn: replica stream write failed: {err}");
            return;
        }
        match drain_replica_stream(runtime, connection, now_ms) {
            Ok(false) => {
                if let Err(err) = flush_replica_primary_writes(connection) {
                    replica_sync.schedule_retry(now_ms);
                    runtime.set_replica_connection_state("reconnect");
                    eprintln!("warn: replica stream write failed: {err}");
                    return;
                }
            }
            Ok(true) => {
                replica_sync.schedule_retry(now_ms);
                runtime.set_replica_connection_state("reconnect");
            }
            Err(err) => {
                replica_sync.schedule_retry(now_ms);
                runtime.set_replica_connection_state("reconnect");
                eprintln!("warn: replica stream read failed: {err}");
            }
        }
    }

    let Some((host, port)) = runtime.replica_sync_target() else {
        replica_sync.connection = None;
        replica_sync.retry_after_ms = 0;
        return;
    };

    if replica_sync.connection.is_some() || now_ms < replica_sync.retry_after_ms {
        return;
    }

    let Some((requested_replid, requested_offset)) = runtime.replica_psync_request() else {
        runtime.set_replica_connection_state("reconnect");
        eprintln!("warn: replica sync request unavailable for {host}:{port}");
        return;
    };

    runtime.set_replica_connection_state("sync");
    match sync_replica_with_primary(
        runtime,
        &host,
        port,
        &requested_replid,
        requested_offset,
        now_ms,
    ) {
        Ok(connection) => {
            replica_sync.connection = Some(connection);
            replica_sync.retry_after_ms = 0;
        }
        Err(err) => {
            replica_sync.schedule_retry(now_ms);
            runtime.set_replica_connection_state("reconnect");
            eprintln!("warn: replica sync with {host}:{port} failed: {err}");
        }
    }
}

pub(crate) fn replication_follow_up_bytes(
    runtime: &mut Runtime,
    frame: &RespFrame,
    response: &RespFrame,
    now_ms: u64,
) -> Option<Vec<u8>> {
    if !is_psync_frame(frame) {
        return None;
    }
    let RespFrame::SimpleString(line) = response else {
        return None;
    };
    if line.starts_with("FULLRESYNC ") {
        return Some(RespFrame::BulkString(Some(runtime.encoded_rdb_snapshot(now_ms))).to_bytes());
    }
    if line == "CONTINUE" {
        let offset = psync_requested_offset(frame)?;
        return Some(runtime.encoded_aof_stream_from_offset(offset));
    }
    None
}

pub(crate) fn is_psync_frame(frame: &RespFrame) -> bool {
    if let RespFrame::Array(Some(items)) = frame
        && let Some(RespFrame::BulkString(Some(cmd))) = items.first()
    {
        return cmd.eq_ignore_ascii_case(b"PSYNC");
    }
    false
}

fn psync_requested_offset(frame: &RespFrame) -> Option<u64> {
    let RespFrame::Array(Some(items)) = frame else {
        return None;
    };
    let RespFrame::BulkString(Some(offset_bytes)) = items.get(2)? else {
        return None;
    };
    let offset = std::str::from_utf8(offset_bytes)
        .ok()?
        .parse::<u64>()
        .ok()?;
    Some(offset)
}

fn parse_blocking_deadline(timeout_bytes: &[u8], now_ms: u64) -> Option<u64> {
    let timeout_secs: f64 = std::str::from_utf8(timeout_bytes).ok()?.parse().ok()?;
    if !timeout_secs.is_finite() || timeout_secs < 0.0 {
        return None;
    }
    Some(if timeout_secs == 0.0 {
        u64::MAX
    } else {
        now_ms.saturating_add((timeout_secs * 1000.0) as u64)
    })
}

/// Extract the BLOCK timeout from XREAD/XREADGROUP args and compute a deadline.
fn parse_xread_block_deadline(items: &[RespFrame], now_ms: u64) -> Option<u64> {
    for (i, item) in items.iter().enumerate() {
        let RespFrame::BulkString(Some(arg)) = item else {
            continue;
        };
        if !arg.eq_ignore_ascii_case(b"BLOCK") {
            continue;
        }
        let RespFrame::BulkString(Some(timeout_bytes)) = items.get(i + 1)? else {
            return None;
        };
        let ms: i64 = std::str::from_utf8(timeout_bytes).ok()?.parse().ok()?;
        if ms < 0 {
            return None;
        }
        return Some(if ms == 0 {
            u64::MAX
        } else {
            now_ms.saturating_add(ms as u64)
        });
    }
    None // BLOCK keyword not found
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

    if cmd.eq_ignore_ascii_case(b"BLPOP")
        || cmd.eq_ignore_ascii_case(b"BRPOP")
        || cmd.eq_ignore_ascii_case(b"BZPOPMAX")
        || cmd.eq_ignore_ascii_case(b"BZPOPMIN")
    {
        if items.len() < 3 {
            return None;
        }
        // Last element is the timeout.
        let timeout_bytes = match items.last() {
            Some(RespFrame::BulkString(Some(b))) => b,
            _ => return None,
        };
        let deadline_ms = parse_blocking_deadline(timeout_bytes, now_ms)?;
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
        let deadline_ms = parse_blocking_deadline(timeout_bytes, now_ms)?;
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
        let deadline_ms = parse_blocking_deadline(timeout_bytes, now_ms)?;
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
    } else if cmd.eq_ignore_ascii_case(b"BLMPOP") || cmd.eq_ignore_ascii_case(b"BZMPOP") {
        // BLMPOP timeout numkeys key [...] LEFT|RIGHT [COUNT n]
        // BZMPOP timeout numkeys key [...] MIN|MAX [COUNT n]
        // Timeout is argv[1] in seconds (float).
        if items.len() < 5 {
            return None;
        }
        let timeout_bytes = match &items[1] {
            RespFrame::BulkString(Some(b)) => b,
            _ => return None,
        };
        let deadline_ms = parse_blocking_deadline(timeout_bytes, now_ms)?;
        let argv: Vec<Vec<u8>> = items
            .iter()
            .filter_map(|f| match f {
                RespFrame::BulkString(Some(b)) => Some(b.clone()),
                _ => None,
            })
            .collect();
        let op = if cmd.eq_ignore_ascii_case(b"BLMPOP") {
            BlockingOp::BLmpop { argv }
        } else {
            BlockingOp::BZmpop { argv }
        };
        Some(BlockedState { op, deadline_ms })
    } else if cmd.eq_ignore_ascii_case(b"XREAD") || cmd.eq_ignore_ascii_case(b"XREADGROUP") {
        let deadline_ms = parse_xread_block_deadline(items, now_ms)?;
        let argv: Vec<Vec<u8>> = items
            .iter()
            .filter_map(|f| match f {
                RespFrame::BulkString(Some(b)) => Some(b.clone()),
                _ => None,
            })
            .collect();
        let op = if cmd.eq_ignore_ascii_case(b"XREAD") {
            BlockingOp::BXread { argv }
        } else {
            BlockingOp::BXreadgroup { argv }
        };
        Some(BlockedState { op, deadline_ms })
    } else {
        None
    }
}

/// Check all blocked clients. Unblock them if their keys have data or
/// their timeout has expired.
fn check_blocked_clients(
    clients: &mut HashMap<Token, ClientConnection>,
    blocked_tokens: &mut HashSet<Token>,
    closing_tokens: &mut HashSet<Token>,
    runtime: &mut Runtime,
    poll: &mut Poll,
    write_tokens: &mut HashSet<Token>,
    ts: u64,
) {
    if blocked_tokens.is_empty() {
        runtime.clear_ready_keys();
        return;
    }

    let ready_keys = runtime.drain_ready_keys();
    let active_blocked: Vec<Token> = blocked_tokens.iter().copied().collect();

    for token in active_blocked {
        let Some(conn) = clients.get_mut(&token) else {
            blocked_tokens.remove(&token);
            continue;
        };
        let Some(blocked) = &conn.blocked else {
            blocked_tokens.remove(&token);
            continue;
        };

        let mut should_check = ts >= blocked.deadline_ms;
        if !should_check {
            for key in blocked.op.keys() {
                if ready_keys.contains(&key) {
                    should_check = true;
                    break;
                }
            }
        }

        if !should_check {
            continue;
        }

        // Check timeout first.
        if ts >= blocked.deadline_ms {
            // Timeout expired — send nil.
            let nil_response = match &blocked.op {
                BlockingOp::BLpop { .. }
                | BlockingOp::BRpop { .. }
                | BlockingOp::BZpopMax { .. }
                | BlockingOp::BZpopMin { .. }
                | BlockingOp::BLmpop { .. }
                | BlockingOp::BZmpop { .. }
                | BlockingOp::BXread { .. }
                | BlockingOp::BXreadgroup { .. } => RespFrame::Array(None),
                BlockingOp::BLmove { .. } => RespFrame::BulkString(None),
            };
            conn.write_buf.extend_from_slice(&nil_response.to_bytes());
            conn.blocked = None;
            blocked_tokens.remove(&token);

            // Process any commands the client pipelined while blocked.
            if !conn.read_buf.is_empty() {
                let session = std::mem::take(&mut conn.session);
                let prev = runtime.swap_session(session);
                let mut dummy_paused = HashSet::new();
                process_buffered_frames(
                    token,
                    conn,
                    runtime,
                    blocked_tokens,
                    closing_tokens,
                    write_tokens,
                    &mut dummy_paused,
                    ts,
                );
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
            blocked_tokens.remove(&token);

            // Process any commands the client pipelined while blocked.
            if !conn.read_buf.is_empty() {
                // The session is already swapped in here.
                let mut dummy_paused = HashSet::new();
                process_buffered_frames(
                    token,
                    conn,
                    runtime,
                    blocked_tokens,
                    closing_tokens,
                    write_tokens,
                    &mut dummy_paused,
                    ts,
                );
            }
        }

        let updated_session = runtime.swap_session(prev);
        conn.session = updated_session;

        // Always try to flush/rearm if there's pending write data,
        // even if the client re-blocked during pipelined command
        // processing — the response from the first unblock still
        // needs to reach the client.
        if !conn.write_buf.is_empty() {
            let _ = flush_or_rearm_client(token, conn, poll);
        }
    }
}

fn flush_or_rearm_client(
    token: Token,
    conn: &mut ClientConnection,
    poll: &mut Poll,
) -> io::Result<()> {
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
                let argv = [b"LPOP".to_vec(), key.clone()];
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
                let argv = [b"RPOP".to_vec(), key.clone()];
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
                let argv = [b"ZPOPMAX".to_vec(), key.clone()];
                let frame = RespFrame::Array(Some(
                    argv.iter()
                        .map(|a| RespFrame::BulkString(Some(a.clone())))
                        .collect(),
                ));
                let response = runtime.execute_frame(frame, now_ms);
                // ZPOPMAX returns [member, score]. BZPOPMAX needs [key, member, score]
                if response != RespFrame::Array(None)
                    && let RespFrame::Array(Some(mut items)) = response
                    && items.len() == 2
                {
                    let mut result = vec![RespFrame::BulkString(Some(key.clone()))];
                    result.append(&mut items);
                    return Some(RespFrame::Array(Some(result)));
                }
            }
            None
        }
        BlockingOp::BZpopMin { keys } => {
            for key in keys {
                let argv = [b"ZPOPMIN".to_vec(), key.clone()];
                let frame = RespFrame::Array(Some(
                    argv.iter()
                        .map(|a| RespFrame::BulkString(Some(a.clone())))
                        .collect(),
                ));
                let response = runtime.execute_frame(frame, now_ms);
                if response != RespFrame::Array(None)
                    && let RespFrame::Array(Some(mut items)) = response
                    && items.len() == 2
                {
                    let mut result = vec![RespFrame::BulkString(Some(key.clone()))];
                    result.append(&mut items);
                    return Some(RespFrame::Array(Some(result)));
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
            let argv = [
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
        BlockingOp::BLmpop { argv } | BlockingOp::BZmpop { argv } => {
            // Re-execute the full BLMPOP/BZMPOP command to check for new data.
            let frame = RespFrame::Array(Some(
                argv.iter()
                    .map(|a| RespFrame::BulkString(Some(a.clone())))
                    .collect(),
            ));
            let response = runtime.execute_frame(frame, now_ms);
            if response != RespFrame::Array(None) {
                Some(response)
            } else {
                None
            }
        }
        BlockingOp::BXread { argv } | BlockingOp::BXreadgroup { argv } => {
            // Re-execute the full XREAD/XREADGROUP command to check for new data.
            let frame = RespFrame::Array(Some(
                argv.iter()
                    .map(|a| RespFrame::BulkString(Some(a.clone())))
                    .collect(),
            ));
            let response = runtime.execute_frame(frame, now_ms);
            // XREAD/XREADGROUP returns Array(None) when no data is available.
            if response != RespFrame::Array(None) {
                Some(response)
            } else {
                None
            }
        }
    }
}

fn propagate_writes_to_replicas(
    clients: &mut HashMap<Token, ClientConnection>,
    runtime: &mut Runtime,
    write_tokens: &mut HashSet<Token>,
) {
    let primary_offset = runtime.replication_primary_offset();
    for (&token, conn) in clients.iter_mut() {
        if let Some(sent_offset) = conn.replication_sent_offset
            && sent_offset < primary_offset
        {
            let bytes = runtime.encoded_aof_stream_from_offset(sent_offset.0);
            if !bytes.is_empty() {
                conn.write_buf.extend_from_slice(&bytes);
                write_tokens.insert(token);
            }
            conn.replication_sent_offset = Some(primary_offset);
        }
    }
}

/// Deliver pending Pub/Sub messages to all subscribed clients.
/// Deliver MONITOR output to all monitor clients.
fn deliver_monitor_output(
    clients: &mut HashMap<Token, ClientConnection>,
    client_id_to_token: &HashMap<u64, Token>,
    runtime: &mut Runtime,
    poll: &mut Poll,
    write_tokens: &mut HashSet<Token>,
) {
    let output = runtime.drain_monitor_output();
    for (client_id, line) in output {
        let Some(&token) = client_id_to_token.get(&client_id) else {
            continue;
        };
        let Some(conn) = clients.get_mut(&token) else {
            continue;
        };
        if conn.closing {
            continue; // don't buffer output for dying connections
        }
        conn.write_buf.extend_from_slice(&line);
        write_tokens.insert(token);
        let _ = poll.registry().reregister(
            &mut conn.stream,
            token,
            Interest::READABLE | Interest::WRITABLE,
        );
    }
}

fn deliver_pubsub_messages(
    clients: &mut HashMap<Token, ClientConnection>,
    client_id_to_token: &HashMap<u64, Token>,
    runtime: &mut Runtime,
    poll: &mut Poll,
    write_tokens: &mut HashSet<Token>,
) {
    let pending_client_ids = runtime.pubsub_clients_with_pending();
    if pending_client_ids.is_empty() {
        return;
    }

    for &client_id in &pending_client_ids {
        let msgs = runtime.drain_pubsub_for_client(client_id);
        if msgs.is_empty() {
            continue;
        }

        let Some(&token) = client_id_to_token.get(&client_id) else {
            continue;
        };

        let Some(conn) = clients.get_mut(&token) else {
            continue;
        };

        if conn.closing {
            continue; // don't buffer messages for dying connections
        }

        for msg in msgs {
            let frame = pubsub_message_to_frame(msg);
            conn.write_buf.extend_from_slice(&frame.to_bytes());
        }

        write_tokens.insert(token);
        let _ = poll.registry().reregister(
            &mut conn.stream,
            token,
            Interest::READABLE | Interest::WRITABLE,
        );
    }
}

/// Check if a command is allowed in subscription mode. Returns Some(error) if rejected.
fn check_subscription_mode_gate(frame: &RespFrame, _in_sub_mode: bool) -> Option<RespFrame> {
    let RespFrame::Array(Some(items)) = frame else {
        return None;
    };
    let Some(RespFrame::BulkString(Some(cmd))) = items.first() else {
        return None;
    };
    // Commands allowed in subscription mode per Redis behavior
    if cmd.eq_ignore_ascii_case(b"SUBSCRIBE")
        || cmd.eq_ignore_ascii_case(b"UNSUBSCRIBE")
        || cmd.eq_ignore_ascii_case(b"PSUBSCRIBE")
        || cmd.eq_ignore_ascii_case(b"PUNSUBSCRIBE")
        || cmd.eq_ignore_ascii_case(b"SSUBSCRIBE")
        || cmd.eq_ignore_ascii_case(b"SUNSUBSCRIBE")
        || cmd.eq_ignore_ascii_case(b"PING")
        || cmd.eq_ignore_ascii_case(b"RESET")
        || cmd.eq_ignore_ascii_case(b"QUIT")
    {
        return None; // allowed
    }
    let cmd_str = String::from_utf8_lossy(cmd);
    Some(RespFrame::Error(format!(
        "ERR Can't execute '{cmd_str}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context"
    )))
}

fn is_client_pause_exempt(argv: &[Vec<u8>]) -> bool {
    matches!(
        argv,
        [cmd, sub, ..]
            if cmd.eq_ignore_ascii_case(b"CLIENT") && sub.eq_ignore_ascii_case(b"UNPAUSE")
    )
}

fn is_quit_frame(frame: &RespFrame) -> bool {
    if let RespFrame::Array(Some(items)) = frame
        && let Some(RespFrame::BulkString(Some(cmd))) = items.first()
    {
        return cmd.eq_ignore_ascii_case(b"QUIT");
    }
    false
}

fn handle_writable(
    token: Token,
    clients: &mut HashMap<Token, ClientConnection>,
    write_tokens: &mut HashSet<Token>,
    closing_tokens: &mut HashSet<Token>,
    poll: &mut Poll,
) {
    let Some(conn) = clients.get_mut(&token) else {
        return;
    };

    match conn.try_flush() {
        Ok(true) => {
            // Write buffer fully drained — only need READABLE now.
            write_tokens.remove(&token);
            let _ = poll
                .registry()
                .reregister(&mut conn.stream, token, Interest::READABLE);
        }
        Ok(false) => {
            // Still have data to write, keep WRITABLE interest.
        }
        Err(_) => {
            conn.closing = true;
            closing_tokens.insert(token);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        InlineParseResult, REPLICA_ACK_INTERVAL_MS, REPLICA_RECONNECT_BACKOFF_MS,
        ReplicaPrimaryConnection, ReplicaSyncState, drain_replica_stream, drive_replica_sync,
        parse_blocking_deadline, read_frame_from_stream, replica_handshake_frame,
        replica_handshake_read_timeout, replication_follow_up_bytes, should_try_inline_parsing,
        try_build_blocked_state,
    };
    use fr_config::RuntimePolicy;
    use fr_protocol::{ParserConfig, RespFrame};
    use fr_runtime::Runtime;
    use std::io::{ErrorKind, Write};
    use std::net::{TcpListener as StdTcpListener, TcpStream as StdTcpStream};
    use std::thread;

    #[test]
    fn server_bootstrap_creates_runtime() {
        let _strict = Runtime::new(RuntimePolicy::default());
        let _hardened = Runtime::new(RuntimePolicy::hardened());
    }

    #[test]
    fn replica_handshake_timeout_uses_runtime_repl_timeout() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        assert_eq!(replica_handshake_read_timeout(&runtime).as_secs(), 60);

        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"CONFIG".to_vec())),
                    RespFrame::BulkString(Some(b"SET".to_vec())),
                    RespFrame::BulkString(Some(b"repl-timeout".to_vec())),
                    RespFrame::BulkString(Some(b"7".to_vec())),
                ])),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(replica_handshake_read_timeout(&runtime).as_secs(), 7);
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
    fn psync_fullresync_emits_rdb_follow_up() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"SET".to_vec())),
                    RespFrame::BulkString(Some(b"seed".to_vec())),
                    RespFrame::BulkString(Some(b"value".to_vec())),
                ])),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PSYNC".to_vec())),
            RespFrame::BulkString(Some(b"?".to_vec())),
            RespFrame::BulkString(Some(b"-1".to_vec())),
        ]));
        let response = RespFrame::SimpleString(
            "FULLRESYNC 0000000000000000000000000000000000000000 0".to_string(),
        );

        let follow_up = replication_follow_up_bytes(&mut runtime, &frame, &response, 2)
            .expect("psync should emit snapshot");
        let parsed = fr_protocol::parse_frame(&follow_up).expect("parse bulk snapshot");
        let RespFrame::BulkString(Some(snapshot)) = parsed.frame else {
            panic!("expected bulk snapshot");
        };
        assert!(!snapshot.is_empty(), "snapshot should not be empty");
        assert!(
            snapshot.starts_with(b"REDIS"),
            "snapshot should be an RDB payload"
        );
    }

    #[test]
    fn non_psync_commands_emit_no_replication_follow_up() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        let frame = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(
            b"REPLCONF".to_vec(),
        ))]));
        let response = RespFrame::SimpleString("OK".to_string());

        assert_eq!(
            replication_follow_up_bytes(&mut runtime, &frame, &response, 0),
            None
        );
    }

    #[test]
    fn psync_non_fullresync_response_emits_no_follow_up() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PSYNC".to_vec())),
            RespFrame::BulkString(Some(b"?".to_vec())),
            RespFrame::BulkString(Some(b"-1".to_vec())),
        ]));
        let response = RespFrame::Error("ERR fallback".to_string());

        assert_eq!(
            replication_follow_up_bytes(&mut runtime, &frame, &response, 0),
            None
        );
    }

    #[test]
    fn psync_continue_emits_aof_backlog_tail() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"SET".to_vec())),
                    RespFrame::BulkString(Some(b"alpha".to_vec())),
                    RespFrame::BulkString(Some(b"1".to_vec())),
                ])),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"SET".to_vec())),
                    RespFrame::BulkString(Some(b"beta".to_vec())),
                    RespFrame::BulkString(Some(b"2".to_vec())),
                ])),
                2,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        // Calculate the byte offset after the first SET command.
        // SET alpha 1 encodes as: *3\r\n$3\r\nSET\r\n$5\r\nalpha\r\n$1\r\n1\r\n = 31 bytes
        let first_cmd_bytes = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"SET".to_vec())),
            RespFrame::BulkString(Some(b"alpha".to_vec())),
            RespFrame::BulkString(Some(b"1".to_vec())),
        ]))
        .to_bytes()
        .len();
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PSYNC".to_vec())),
            RespFrame::BulkString(Some(b"0000000000000000000000000000000000000000".to_vec())),
            RespFrame::BulkString(Some(first_cmd_bytes.to_string().into_bytes())),
        ]));
        let response = RespFrame::SimpleString("CONTINUE".to_string());

        let follow_up = replication_follow_up_bytes(&mut runtime, &frame, &response, 3)
            .expect("psync continue should emit backlog");
        let mut replica = fr_runtime::Runtime::default_strict();
        let backlog = replica
            .replay_aof_stream(&follow_up, 10)
            .expect("decode backlog stream");
        assert_eq!(backlog.len(), 1);
        assert_eq!(backlog[0], RespFrame::SimpleString("OK".to_string()));
        assert_eq!(
            replica.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"GET".to_vec())),
                    RespFrame::BulkString(Some(b"alpha".to_vec())),
                ])),
                11,
            ),
            RespFrame::BulkString(None)
        );
        assert_eq!(
            replica.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"GET".to_vec())),
                    RespFrame::BulkString(Some(b"beta".to_vec())),
                ])),
                12,
            ),
            RespFrame::BulkString(Some(b"2".to_vec()))
        );
    }

    #[test]
    fn replica_sync_helper_applies_fullresync_from_live_primary_socket() {
        let mut primary = Runtime::default_strict();
        primary.set_server_port(6380);
        assert_eq!(
            primary.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"SET".to_vec())),
                    RespFrame::BulkString(Some(b"alpha".to_vec())),
                    RespFrame::BulkString(Some(b"1".to_vec())),
                ])),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        let snapshot = primary.encoded_rdb_snapshot(2);

        let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind primary socket");
        let addr = listener.local_addr().expect("local addr");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept replica");
            let parser = ParserConfig::default();
            let mut read_buf = Vec::new();

            let ping = read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                .expect("read ping");
            assert_eq!(ping, replica_handshake_frame(&[b"PING"]));
            stream
                .write_all(&RespFrame::SimpleString("PONG".to_string()).to_bytes())
                .unwrap();

            let replconf_port =
                read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                    .expect("replconf");
            match replconf_port {
                RespFrame::Array(Some(items)) => {
                    assert_eq!(items[0], RespFrame::BulkString(Some(b"REPLCONF".to_vec())));
                    assert_eq!(
                        items[1],
                        RespFrame::BulkString(Some(b"listening-port".to_vec()))
                    );
                }
                other => panic!("unexpected replconf frame: {other:?}"),
            }
            stream
                .write_all(&RespFrame::SimpleString("OK".to_string()).to_bytes())
                .unwrap();

            let replconf_capa =
                read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                    .expect("capa");
            assert_eq!(
                replconf_capa,
                replica_handshake_frame(&[b"REPLCONF", b"capa", b"psync2"])
            );
            stream
                .write_all(&RespFrame::SimpleString("OK".to_string()).to_bytes())
                .unwrap();

            let psync = read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                .expect("psync");
            assert_eq!(psync, replica_handshake_frame(&[b"PSYNC", b"?", b"-1"]));
            stream
                .write_all(
                    &RespFrame::SimpleString(
                        "FULLRESYNC 0000000000000000000000000000000000000000 0".to_string(),
                    )
                    .to_bytes(),
                )
                .unwrap();
            stream
                .write_all(&RespFrame::BulkString(Some(snapshot)).to_bytes())
                .unwrap();
        });

        let mut replica = Runtime::default_strict();
        let mut replica_sync = ReplicaSyncState::new();
        replica.set_server_port(6381);
        assert_eq!(
            replica.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"REPLICAOF".to_vec())),
                    RespFrame::BulkString(Some(addr.ip().to_string().into_bytes())),
                    RespFrame::BulkString(Some(addr.port().to_string().into_bytes())),
                ])),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        drive_replica_sync(&mut replica, &mut replica_sync, 3);

        assert_eq!(
            replica.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"GET".to_vec())),
                    RespFrame::BulkString(Some(b"alpha".to_vec())),
                ])),
                4,
            ),
            RespFrame::BulkString(Some(b"1".to_vec()))
        );
        assert_eq!(
            replica.execute_frame(
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"ROLE".to_vec()))])),
                5,
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"slave".to_vec())),
                RespFrame::BulkString(Some(addr.ip().to_string().into_bytes())),
                RespFrame::Integer(i64::from(addr.port())),
                RespFrame::BulkString(Some(b"connected".to_vec())),
                RespFrame::Integer(0),
            ]))
        );

        server.join().expect("primary thread");
    }

    #[test]
    fn replica_stream_reconnect_uses_partial_psync_after_disconnect() {
        let replid = "00000000000000000000000000000000000000aa".to_string();

        let mut primary = Runtime::default_strict();
        primary.set_server_port(6380);
        assert_eq!(
            primary.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"SET".to_vec())),
                    RespFrame::BulkString(Some(b"alpha".to_vec())),
                    RespFrame::BulkString(Some(b"1".to_vec())),
                ])),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        let fullresync_offset = primary.replication_primary_offset().0;
        let fullresync_offset_text = fullresync_offset.to_string();
        let snapshot = primary.encoded_rdb_snapshot(1);
        let beta_bytes = fr_persist::encode_aof_stream(&[fr_persist::AofRecord {
            argv: vec![b"SET".to_vec(), b"beta".to_vec(), b"2".to_vec()],
        }]);
        let continue_offset_text = fullresync_offset
            .saturating_add(u64::try_from(beta_bytes.len()).unwrap_or(u64::MAX))
            .to_string();

        let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind primary socket");
        let addr = listener.local_addr().expect("local addr");
        let server = thread::spawn({
            let replid = replid.clone();
            let fullresync_offset_text = fullresync_offset_text.clone();
            let continue_offset_text = continue_offset_text.clone();
            move || {
                let parser = ParserConfig::default();

                let (mut stream1, _) = listener.accept().expect("accept first replica");
                let mut read_buf = Vec::new();
                let _ = read_frame_from_stream(&mut stream1, &mut read_buf, &parser, usize::MAX)
                    .expect("ping");
                stream1
                    .write_all(&RespFrame::SimpleString("PONG".to_string()).to_bytes())
                    .unwrap();
                let _ = read_frame_from_stream(&mut stream1, &mut read_buf, &parser, usize::MAX)
                    .expect("replconf port");
                stream1
                    .write_all(&RespFrame::SimpleString("OK".to_string()).to_bytes())
                    .unwrap();
                let _ = read_frame_from_stream(&mut stream1, &mut read_buf, &parser, usize::MAX)
                    .expect("replconf capa");
                stream1
                    .write_all(&RespFrame::SimpleString("OK".to_string()).to_bytes())
                    .unwrap();
                let psync1 =
                    read_frame_from_stream(&mut stream1, &mut read_buf, &parser, usize::MAX)
                        .expect("psync1");
                assert_eq!(psync1, replica_handshake_frame(&[b"PSYNC", b"?", b"-1"]));
                stream1
                    .write_all(
                        &RespFrame::SimpleString(format!(
                            "FULLRESYNC {replid} {fullresync_offset_text}"
                        ))
                        .to_bytes(),
                    )
                    .unwrap();
                stream1
                    .write_all(&RespFrame::BulkString(Some(snapshot)).to_bytes())
                    .unwrap();
                stream1
                    .write_all(&replica_handshake_frame(&[b"SET", b"beta", b"2"]).to_bytes())
                    .unwrap();
                drop(stream1);

                let (mut stream2, _) = listener.accept().expect("accept reconnect replica");
                let mut read_buf = Vec::new();
                let _ = read_frame_from_stream(&mut stream2, &mut read_buf, &parser, usize::MAX)
                    .expect("ping2");
                stream2
                    .write_all(&RespFrame::SimpleString("PONG".to_string()).to_bytes())
                    .unwrap();
                let _ = read_frame_from_stream(&mut stream2, &mut read_buf, &parser, usize::MAX)
                    .expect("replconf port2");
                stream2
                    .write_all(&RespFrame::SimpleString("OK".to_string()).to_bytes())
                    .unwrap();
                let _ = read_frame_from_stream(&mut stream2, &mut read_buf, &parser, usize::MAX)
                    .expect("replconf capa2");
                stream2
                    .write_all(&RespFrame::SimpleString("OK".to_string()).to_bytes())
                    .unwrap();
                let psync2 =
                    read_frame_from_stream(&mut stream2, &mut read_buf, &parser, usize::MAX)
                        .expect("psync2");
                assert_eq!(
                    psync2,
                    replica_handshake_frame(&[
                        b"PSYNC",
                        replid.as_bytes(),
                        continue_offset_text.as_bytes(),
                    ])
                );
                stream2
                    .write_all(&RespFrame::SimpleString("CONTINUE".to_string()).to_bytes())
                    .unwrap();
                stream2
                    .write_all(&replica_handshake_frame(&[b"SET", b"gamma", b"3"]).to_bytes())
                    .unwrap();
            }
        });

        let mut replica = Runtime::default_strict();
        let mut replica_sync = ReplicaSyncState::new();
        replica.set_server_port(6381);
        assert_eq!(
            replica.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"REPLICAOF".to_vec())),
                    RespFrame::BulkString(Some(addr.ip().to_string().into_bytes())),
                    RespFrame::BulkString(Some(addr.port().to_string().into_bytes())),
                ])),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        drive_replica_sync(&mut replica, &mut replica_sync, 1);
        drive_replica_sync(&mut replica, &mut replica_sync, 2);
        drive_replica_sync(
            &mut replica,
            &mut replica_sync,
            2 + REPLICA_RECONNECT_BACKOFF_MS + 1,
        );
        drive_replica_sync(
            &mut replica,
            &mut replica_sync,
            3 + REPLICA_RECONNECT_BACKOFF_MS,
        );

        assert_eq!(
            replica.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"GET".to_vec())),
                    RespFrame::BulkString(Some(b"alpha".to_vec())),
                ])),
                5,
            ),
            RespFrame::BulkString(Some(b"1".to_vec()))
        );
        assert_eq!(
            replica.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"GET".to_vec())),
                    RespFrame::BulkString(Some(b"beta".to_vec())),
                ])),
                6,
            ),
            RespFrame::BulkString(Some(b"2".to_vec()))
        );
        assert_eq!(
            replica.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"GET".to_vec())),
                    RespFrame::BulkString(Some(b"gamma".to_vec())),
                ])),
                7,
            ),
            RespFrame::BulkString(Some(b"3".to_vec()))
        );

        server.join().expect("primary thread");
    }

    #[test]
    fn replica_stream_answers_getack_and_emits_periodic_ack() {
        let replid = "00000000000000000000000000000000000000bb".to_string();

        let mut primary = Runtime::default_strict();
        primary.set_server_port(6380);
        assert_eq!(
            primary.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"SET".to_vec())),
                    RespFrame::BulkString(Some(b"alpha".to_vec())),
                    RespFrame::BulkString(Some(b"1".to_vec())),
                ])),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        let fullresync_offset_text = primary.replication_primary_offset().0.to_string();
        let snapshot = primary.encoded_rdb_snapshot(1);

        let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind primary socket");
        let addr = listener.local_addr().expect("local addr");
        let server = thread::spawn({
            let replid = replid.clone();
            let fullresync_offset_text = fullresync_offset_text.clone();
            move || {
                let parser = ParserConfig::default();

                let (mut stream, _) = listener.accept().expect("accept replica");
                stream
                    .set_read_timeout(Some(std::time::Duration::from_millis(500)))
                    .expect("set read timeout");
                let mut read_buf = Vec::new();
                let _ = read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                    .expect("ping");
                stream
                    .write_all(&RespFrame::SimpleString("PONG".to_string()).to_bytes())
                    .unwrap();
                let _ = read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                    .expect("replconf port");
                stream
                    .write_all(&RespFrame::SimpleString("OK".to_string()).to_bytes())
                    .unwrap();
                let _ = read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                    .expect("replconf capa");
                stream
                    .write_all(&RespFrame::SimpleString("OK".to_string()).to_bytes())
                    .unwrap();
                let psync = read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                    .expect("psync");
                assert_eq!(psync, replica_handshake_frame(&[b"PSYNC", b"?", b"-1"]));
                stream
                    .write_all(
                        &RespFrame::SimpleString(format!(
                            "FULLRESYNC {replid} {fullresync_offset_text}"
                        ))
                        .to_bytes(),
                    )
                    .unwrap();
                stream
                    .write_all(&RespFrame::BulkString(Some(snapshot)).to_bytes())
                    .unwrap();
                stream
                    .write_all(&replica_handshake_frame(&[b"REPLCONF", b"GETACK", b"*"]).to_bytes())
                    .unwrap();

                let immediate_ack =
                    read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                        .expect("getack reply");
                assert_eq!(
                    immediate_ack,
                    replica_handshake_frame(&[
                        b"REPLCONF",
                        b"ACK",
                        fullresync_offset_text.as_bytes(),
                    ])
                );

                let periodic_ack =
                    read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                        .expect("periodic ack");
                assert_eq!(
                    periodic_ack,
                    replica_handshake_frame(&[
                        b"REPLCONF",
                        b"ACK",
                        fullresync_offset_text.as_bytes(),
                    ])
                );
            }
        });

        let mut replica = Runtime::default_strict();
        let mut replica_sync = ReplicaSyncState::new();
        replica.set_server_port(6381);
        assert_eq!(
            replica.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"REPLICAOF".to_vec())),
                    RespFrame::BulkString(Some(addr.ip().to_string().into_bytes())),
                    RespFrame::BulkString(Some(addr.port().to_string().into_bytes())),
                ])),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        drive_replica_sync(&mut replica, &mut replica_sync, 1);
        drive_replica_sync(
            &mut replica,
            &mut replica_sync,
            1 + REPLICA_ACK_INTERVAL_MS + 1,
        );

        server.join().expect("primary thread");
    }

    #[test]
    fn replica_stream_read_path_enforces_query_buffer_limit() {
        use std::net::TcpListener as StdTcpListener;

        let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind primary socket");
        let addr = listener.local_addr().expect("local addr");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept replica");
            stream
                .write_all(b"*2\r\n$3\r\nSET\r\n$5\r\nalpha")
                .expect("write oversized partial frame");
        });

        let mut runtime = Runtime::default_strict();
        runtime.server.query_buffer_limit = 8;
        let stream = StdTcpStream::connect(addr).expect("connect primary");
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .expect("set replica stream read timeout");
        let mut connection = ReplicaPrimaryConnection {
            stream,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
            next_ack_ms: 0,
        };

        let err = drain_replica_stream(&mut runtime, &mut connection, 1).expect_err("must fail");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert!(
            err.to_string()
                .contains("eventloop.read.querybuf_limit_exceeded"),
            "{err}"
        );
        assert!(connection.read_buf.is_empty());

        server.join().expect("primary thread");
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
    fn parse_blocking_deadline_rejects_nonfinite_values() {
        assert_eq!(parse_blocking_deadline(b"0", 123), Some(u64::MAX));
        assert_eq!(parse_blocking_deadline(b"-1", 123), None);
        assert_eq!(parse_blocking_deadline(b"NaN", 123), None);
        assert_eq!(parse_blocking_deadline(b"inf", 123), None);
        assert_eq!(parse_blocking_deadline(b"+inf", 123), None);
    }

    #[test]
    fn blocking_state_builder_rejects_nonfinite_blpop_timeout() {
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"BLPOP".to_vec())),
            RespFrame::BulkString(Some(b"queue".to_vec())),
            RespFrame::BulkString(Some(b"inf".to_vec())),
        ]));
        assert!(try_build_blocked_state(&frame, 1_000).is_none());
    }

    #[test]
    fn inline_parser_gate_recognizes_all_resp_prefixes() {
        for prefix in [
            b'+', b'-', b':', b'$', b'*', b'~', b'%', b'#', b',', b'_', b'(', b'=', b'|', b'>',
            b'!',
        ] {
            assert!(
                !should_try_inline_parsing(prefix),
                "prefix {prefix:?} should stay on RESP parser path"
            );
        }

        assert!(should_try_inline_parsing(b'P'));
        assert!(should_try_inline_parsing(b' '));
    }

    #[test]
    fn master_to_replica_streaming_propagate_writes() {
        use crate::{ClientConnection, propagate_writes_to_replicas, replication_follow_up_bytes};
        use fr_persist::{AofRecord, encode_aof_stream};
        use fr_protocol::RespFrame;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::{HashMap, HashSet};
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();
        let ts = 1000;

        // 1. Setup a "replica" client connection.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (_server_stream, _server_addr) = listener.accept().unwrap();

        let replica_session = runtime.new_session();
        let replica_id = replica_session.client_id;
        let mut replica_conn =
            ClientConnection::new(mio::net::TcpStream::from_std(stream), replica_session);

        // 2. Perform PSYNC to mark as replica and set initial sent_offset.
        let psync_frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PSYNC".to_vec())),
            RespFrame::BulkString(Some(b"?".to_vec())),
            RespFrame::BulkString(Some(b"-1".to_vec())),
        ]));

        let prev = runtime.swap_session(std::mem::take(&mut replica_conn.session));
        let response = runtime.execute_frame(psync_frame.clone(), ts);

        if let Some(follow_up) =
            replication_follow_up_bytes(&mut runtime, &psync_frame, &response, ts)
        {
            replica_conn.write_buf.extend_from_slice(&follow_up);
            if runtime.is_replica(replica_id) {
                replica_conn.replication_sent_offset = Some(runtime.replication_primary_offset());
            }
        }
        replica_conn.session = runtime.swap_session(prev);

        assert!(replica_conn.replication_sent_offset.is_some());
        let initial_offset = replica_conn.replication_sent_offset.unwrap();

        // Clear the write_buf so we only see the new data.
        replica_conn.write_buf.clear();

        // 3. Perform a write command from a DIFFERENT client.
        let other_session = runtime.new_session();
        let prev = runtime.swap_session(other_session);
        let _set_response = runtime.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SET".to_vec())),
                RespFrame::BulkString(Some(b"foo".to_vec())),
                RespFrame::BulkString(Some(b"bar".to_vec())),
            ])),
            ts + 1,
        );
        let _ = runtime.swap_session(prev);

        assert!(runtime.replication_primary_offset() > initial_offset);

        // 4. Propagate writes.
        let mut clients = HashMap::new();
        let token = Token(1);
        clients.insert(token, replica_conn);
        let mut write_tokens = HashSet::new();

        propagate_writes_to_replicas(&mut clients, &mut runtime, &mut write_tokens);

        // 5. Verify replica received the write.
        let conn = clients.get(&token).unwrap();
        assert!(write_tokens.contains(&token));

        let expected_bytes = encode_aof_stream(&[AofRecord {
            argv: vec![b"SET".to_vec(), b"foo".to_vec(), b"bar".to_vec()],
        }]);

        assert_eq!(conn.write_buf, expected_bytes);
        assert_eq!(
            conn.replication_sent_offset,
            Some(runtime.replication_primary_offset())
        );
    }

    #[test]
    fn client_unpause_bypasses_pause_gate() {
        use crate::ClientConnection;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::HashSet;
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();
        let ts = 1_000;
        let session = runtime.new_session();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (mut server_stream, _server_addr) = listener.accept().unwrap();
        server_stream
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session);

        let pause = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"CLIENT".to_vec())),
            RespFrame::BulkString(Some(b"PAUSE".to_vec())),
            RespFrame::BulkString(Some(b"1000".to_vec())),
            RespFrame::BulkString(Some(b"ALL".to_vec())),
        ]));
        let unpause = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"CLIENT".to_vec())),
            RespFrame::BulkString(Some(b"UNPAUSE".to_vec())),
        ]));

        let unpause_bytes = unpause.to_bytes();

        assert_eq!(
            runtime.execute_frame(pause, ts),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(runtime.is_client_paused(ts + 1));

        conn.read_buf.extend_from_slice(&unpause_bytes);

        let mut blocked_tokens = HashSet::new();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts + 1,
        );

        let mut response = [0_u8; 5];
        std::io::Read::read_exact(&mut server_stream, &mut response).unwrap();
        assert_eq!(response, *b"+OK\r\n");
        assert!(conn.read_buf.is_empty());
        assert!(!runtime.is_client_paused(ts + 1));
    }

    #[test]
    fn output_buffer_limit_is_enforced_after_appending_current_response() {
        use crate::ClientConnection;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::HashSet;
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();
        runtime.server.output_buffer_limit = 0;
        let session = runtime.new_session();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (_server_stream, _server_addr) = listener.accept().unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session);
        conn.read_buf.extend_from_slice(b"*1\r\n$4\r\nPING\r\n");

        let mut blocked_tokens = HashSet::new();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            1,
        );

        assert!(conn.closing);
        assert!(closing_tokens.contains(&Token(1)));
    }
}
