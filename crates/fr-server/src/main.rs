//! FrankenRedis standalone server binary.
//!
//! Implements a single-threaded TCP server using `mio` for non-blocking I/O.
//! Each client gets its own `ClientSession` (per-connection auth, transactions,
//! etc.) while sharing a single `ServerState` (store, config) via the `Runtime`.

#![forbid(unsafe_code)]

#[cfg(all(feature = "jemalloc", feature = "mimalloc"))]
compile_error!("features \"jemalloc\" and \"mimalloc\" are mutually exclusive");

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
use fr_runtime::{ClientSession, ClientUnblockMode, Runtime};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};

/// Default port matching Redis convention.
const DEFAULT_PORT: u16 = 6379;

/// Token for the TCP listener socket.
const LISTENER: Token = Token(0); // ubs:ignore

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

impl Drop for ReplicaPrimaryConnection {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
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

impl Drop for ClientConnection {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl ClientConnection {
    fn new(stream: TcpStream, mut session: ClientSession, now_ms: u64) -> Self {
        session.connected_at_ms = now_ms;
        session.last_interaction_ms = now_ms;
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
        let mut total_written = 0;
        let mut result = Ok(true);
        while total_written < self.write_buf.len() {
            match self.stream.write(&self.write_buf[total_written..]) {
                Ok(0) => {
                    result = Err(io::Error::new(ErrorKind::WriteZero, "write zero"));
                    break;
                }
                Ok(n) => {
                    total_written += n;
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    result = Ok(false);
                    break;
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if total_written > 0 {
            self.write_buf.drain(..total_written);
        }
        result
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn server_help_text() -> String {
    format!(
        "frankenredis — FrankenRedis server\n\n\
USAGE: frankenredis [OPTIONS]\n\n\
OPTIONS:\n\
  --bind <ADDR>              Listen address (default: 127.0.0.1)\n\
  --port <PORT>              Listen port (default: {DEFAULT_PORT})\n\
  --mode <MODE>              Runtime mode: strict or hardened (default: hardened)\n\
  --aof <PATH>               AOF persistence file path (enables persistence)\n\
  --rdb <PATH>               RDB snapshot file path (enables SAVE/BGSAVE snapshots)\n\
  --replicaof <HOST> <PORT>  Configure this server as a replica of the given primary\n\
  --masteruser <USERNAME>    Authenticate to the configured primary as this ACL user\n\
  --masterauth <PASSWORD>    Authenticate to the configured primary with this password\n\
  --help                     Show this help\n"
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    let mut port = DEFAULT_PORT;
    let mut mode_str = "hardened";
    let mut bind_addr = "127.0.0.1".to_string();
    let mut aof_path: Option<String> = None;
    let mut rdb_path: Option<String> = None;
    let mut config_path: Option<String> = None;
    let mut replicaof: Option<(String, u16)> = None;
    let mut masteruser: Option<String> = None;
    let mut masterauth: Option<String> = None;
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
            "--config" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --config requires a file path");
                    return ExitCode::from(1);
                }
                config_path = Some(args[i].clone());
            }
            "--replicaof" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --replicaof requires a host and port");
                    return ExitCode::from(1);
                }
                let host = args[i].clone();
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --replicaof requires a host and port");
                    return ExitCode::from(1);
                }
                let replica_port = match args[i].parse() {
                    Ok(port) => port,
                    Err(_) => {
                        eprintln!("error: invalid replicaof port number: {}", args[i]);
                        return ExitCode::from(1);
                    }
                };
                replicaof = Some((host, replica_port));
            }
            "--masteruser" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --masteruser requires a username");
                    return ExitCode::from(1);
                }
                masteruser = Some(args[i].clone());
            }
            "--masterauth" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --masterauth requires a password");
                    return ExitCode::from(1);
                }
                masterauth = Some(args[i].clone());
            }
            "--help" | "-h" => {
                print!("{}", server_help_text());
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
    runtime.set_config_file_path(config_path.map(std::path::PathBuf::from));
    runtime.set_masteruser(masteruser.map(String::into_bytes));
    runtime.set_masterauth(masterauth.map(String::into_bytes));
    if let Some((host, primary_port)) = replicaof {
        let response = runtime.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"REPLICAOF".to_vec())),
                RespFrame::BulkString(Some(host.clone().into_bytes())),
                RespFrame::BulkString(Some(primary_port.to_string().into_bytes())),
            ])),
            now_ms(),
        );
        match response {
            RespFrame::SimpleString(ref line) if line.starts_with("OK") => {
                eprintln!("Replication: configured primary {host}:{primary_port}");
            }
            RespFrame::Error(err) => {
                eprintln!("error: failed to configure replica mode: {err}");
                return ExitCode::from(1);
            }
            other => {
                eprintln!("error: unexpected REPLICAOF response during startup: {other:?}");
                return ExitCode::from(1);
            }
        }
    }

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

    // Configure and load RDB snapshot persistence if requested.
    // When both AOF and RDB are configured, AOF takes precedence for data loading
    // (matches Redis behavior). RDB path is still set so SAVE/BGSAVE can write snapshots.
    if let Some(path) = &rdb_path {
        runtime.set_rdb_path(std::path::PathBuf::from(path));
        if aof_path.is_none() {
            match runtime.load_rdb(now_ms()) {
                Ok(0) => eprintln!("RDB: no existing file or empty (will create on SAVE/BGSAVE)"),
                Ok(n) => eprintln!("RDB: loaded {n} entries from {path}"),
                Err(e) => {
                    eprintln!("RDB: load warning: {e:?} (starting with empty store)");
                }
            }
        } else {
            eprintln!("RDB: snapshot path configured (AOF takes precedence for data loading)");
        }
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
    let mut next_handle: usize = 1;
    let tick_budget = TickBudget::default();
    let mut last_ops_sample_ms: u64 = now_ms();

    loop {
        // Use fr-eventloop's tick planner to determine poll timeout.
        let has_blocked = !blocked_tokens.is_empty();
        let pending_writes = write_tokens.len();
        let tick_plan = plan_tick(0, pending_writes, tick_budget, EventLoopMode::Normal);
        let poll_timeout = if tick_plan.poll_timeout_ms == 0 {
            Some(std::time::Duration::from_millis(0))
        } else if has_blocked {
            // When clients are blocked, use a short poll timeout so we
            // can check for available data and timeout expiry frequently.
            Some(std::time::Duration::from_millis(100))
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
                        &mut next_handle,
                        &mut runtime,
                    );
                }
                conn_handle => {
                    if event.is_readable() {
                        handle_readable(
                            conn_handle,
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
                            conn_handle,
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

        // Sample instantaneous ops/sec and throughput once per tick.
        let elapsed = ts.saturating_sub(last_ops_sample_ms);
        if elapsed >= 100 {
            runtime.record_ops_sec_sample(elapsed);
            last_ops_sample_ms = ts;
        }

        // Drive the primary link from the main loop so replicas can sustain
        // online deltas, ACK traffic, and reconnect after link loss.
        drive_replica_sync(&mut runtime, &mut replica_sync, ts);

        apply_pending_client_unblocks(PendingClientUnblocksContext {
            clients: &mut clients,
            client_id_to_token: &client_id_to_token,
            blocked_tokens: &mut blocked_tokens,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            ts,
        });

        // Check blocked clients (BLPOP/BRPOP/BLMOVE) for available data
        // or timeout expiry.
        check_blocked_clients(CheckBlockedClientsContext {
            clients: &mut clients,
            blocked_tokens: &mut blocked_tokens,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            ts,
        });

        // Re-process clients whose commands were deferred by CLIENT PAUSE.
        // When the pause expires, we must re-trigger processing since mio won't
        // generate a readable event for data already in the read buffer.
        if !paused_tokens.is_empty() && !runtime.is_client_paused(ts) {
            let tokens = std::mem::take(&mut paused_tokens);
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
        propagate_writes_to_replicas(&mut clients, &mut runtime, &mut poll, &mut write_tokens);

        // Deliver pending Pub/Sub messages to subscribed clients.
        deliver_pubsub_messages(
            &mut clients,
            &client_id_to_token,
            &mut runtime,
            &mut poll,
            &mut write_tokens,
            &mut closing_tokens,
        );

        // Deliver MONITOR output to monitor clients.
        deliver_monitor_output(
            &mut clients,
            &client_id_to_token,
            &mut runtime,
            &mut poll,
            &mut write_tokens,
            &mut closing_tokens,
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
                runtime.mark_client_unblocked(conn.session.client_id);
                client_id_to_token.remove(&conn.session.client_id);
                // Clean up Pub/Sub subscriptions and stats for this client.
                runtime.pubsub_cleanup_client(conn.session.client_id);
                runtime.remove_client_session(conn.session.client_id);
                runtime.cleanup_disconnected_client(conn.session.client_id);
                runtime.track_connection_closed();
                let _ = poll.registry().deregister(&mut conn.stream);
                let _ = conn.stream.shutdown(std::net::Shutdown::Both);
            }
        }

        // Disconnect clients that have exceeded the configured idle timeout.
        let client_timeout_sec = runtime.server.client_timeout_sec;
        if client_timeout_sec > 0 {
            let timeout_ms = client_timeout_sec * 1000;
            for (&token, conn) in clients.iter_mut() {
                if conn.closing || conn.blocked.is_some() || conn.replication_sent_offset.is_some()
                {
                    continue; // Skip closing, blocked, and replica clients.
                }
                if runtime.is_pubsub_client(conn.session.client_id) {
                    continue;
                }
                let idle_ms = ts.saturating_sub(conn.session.last_interaction_ms);
                if idle_ms > timeout_ms {
                    conn.closing = true;
                    closing_tokens.insert(token);
                }
            }
        }

        // Process any CLIENT KILL requests from the runtime.
        let kills: Vec<u64> = std::mem::take(&mut runtime.server.pending_client_kills);
        for target_id in kills {
            if let Some(&token) = client_id_to_token.get(&target_id)
                && let Some(conn) = clients.get_mut(&token)
            {
                conn.closing = true;
                closing_tokens.insert(token);
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
    next_handle: &mut usize,
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
                runtime.track_rejected_connection();
                drop(stream);
            }
            break;
        }

        match listener.accept() {
            Ok((mut stream, peer_addr)) => {
                let conn_handle = Token(*next_handle);
                *next_handle = next_handle.wrapping_add(1);
                // Avoid colliding with LISTENER token (0).
                if *next_handle == 0 {
                    *next_handle = 1;
                }

                if let Err(e) = stream.set_nodelay(true) {
                    eprintln!("warn: failed to set TCP_NODELAY: {e}");
                }

                if let Err(e) = poll.registry().register(
                    &mut stream,
                    conn_handle,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    eprintln!("warn: failed to register client: {e}");
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    continue;
                }

                let mut session = runtime.new_session();
                session.peer_addr = Some(peer_addr);
                let client_id = session.client_id;
                let conn = ClientConnection::new(stream, session, now_ms());
                runtime.record_client_session(&conn.session);
                clients.insert(conn_handle, conn);
                client_id_to_token.insert(client_id, conn_handle);
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
                        runtime.track_net_input_bytes(n as u64);
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

    let write_buf_before = conn.write_buf.len();
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
    // Track output bytes generated by command processing.
    let output_delta = conn.write_buf.len().saturating_sub(write_buf_before);
    runtime.track_net_output_bytes(output_delta as u64);

    // Swap session back.
    let updated_session = runtime.swap_session(prev);
    conn.session = updated_session;
    runtime.record_client_session(&conn.session);

    // If there's data to write, ensure we're registered for WRITABLE so the
    // writable handler can flush once per poll cycle.
    if !conn.write_buf.is_empty() {
        write_tokens.insert(token);
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
        let parse_result = if !unparsed.is_empty() && should_try_inline_parsing(unparsed[0]) {
            match try_parse_inline(unparsed) {
                Ok(InlineParseResult::EmptyLine(consumed)) => {
                    // Silently consume empty lines (Redis behavior).
                    consumed_total += consumed;
                    continue;
                }
                Ok(InlineParseResult::ProtocolError(err_frame, consumed)) => {
                    // Send the error directly to the client without executing.
                    err_frame.encode_into(&mut conn.write_buf);
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
                    reject.encode_into(&mut conn.write_buf);
                    consumed_total += consumed;
                    continue;
                }
                runtime.set_blocked_clients_count_for_info(blocked_tokens.len());
                // CLIENT PAUSE gate: delay command processing while paused.
                if let Ok(argv) = fr_command::frame_to_argv(&frame)
                    && runtime.is_command_paused(&argv, ts)
                    && !is_client_pause_exempt(&argv)
                {
                    // Don't process the command — leave it in the read buffer.
                    // Track paused token so we can re-process when pause expires.
                    paused_tokens.insert(token);
                    break;
                }
                let response = runtime.execute_frame(frame.clone(), ts);
                let parsed_frame = frame;

                // Check for QUIT command.
                if is_quit_frame(&parsed_frame) {
                    response.encode_into(&mut conn.write_buf);
                    write_tokens.insert(token);
                    conn.closing = true;
                    closing_tokens.insert(token);
                    consumed_total += consumed;
                    break;
                }

                // Check for blocking commands that returned nil — block the
                // client instead of sending the nil response immediately.
                if (response == RespFrame::Array(None) || response == RespFrame::BulkString(None))
                    && let Some(blocked) = try_build_blocked_state(&parsed_frame, ts).and_then(
                        |BlockedState { op, deadline_ms }| {
                            Some(BlockedState {
                                op: resolve_blocked_op(op, runtime, ts)?,
                                deadline_ms,
                            })
                        },
                    )
                {
                    // Redis behavior: if the keys already have data, we shouldn't block.
                    // try_build_blocked_state only returns Some if it's a blocking command.
                    if let Some(immediate_response) = try_fulfill_blocked(&blocked.op, runtime, ts)
                    {
                        immediate_response.encode_into(&mut conn.write_buf);
                    } else {
                        conn.blocked = Some(blocked);
                        blocked_tokens.insert(token);
                        runtime.mark_client_blocked(runtime.client_id());
                        consumed_total += consumed;
                        break; // Stop processing — client is now blocked.
                    }
                } else if suppress_client_network_reply(&parsed_frame, &response) {
                    // Redis treats REPLCONF ACK/GETACK as internal control
                    // frames and does not send a direct reply on client links.
                } else {
                    response.encode_into(&mut conn.write_buf);
                }
                if let Some(follow_up) =
                    replication_follow_up_bytes(runtime, &parsed_frame, &response, ts)
                {
                    conn.write_buf.extend_from_slice(&follow_up);
                    if runtime.is_replica(runtime.client_id()) {
                        conn.replication_sent_offset = Some(runtime.replication_primary_offset());
                    }
                }

                // Drain and deliver any pending pub/sub messages (including
                // shard pub/sub SMessage) generated by the command.
                for msg in runtime.drain_pending_pubsub() {
                    let frame = pubsub_message_to_frame(msg);
                    frame.encode_into(&mut conn.write_buf);
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
                err_reply.encode_into(&mut conn.write_buf);
                conn.closing = true;
                closing_tokens.insert(token);
                break;
            }
        }
    }

    if consumed_total > 0 {
        conn.read_buf.drain(..consumed_total);
    }

    if !conn.write_buf.is_empty() {
        // Defer flushing to the writable handler to coalesce per poll cycle.
        write_tokens.insert(token);
    }
}

/// Try to parse an inline command (non-RESP). Inline commands are
/// space-separated tokens terminated by \r\n or \n.
/// Returns (frame, consumed_bytes) on success.
/// Result of inline parsing: either a command frame or an empty line that
/// should be silently consumed.
enum InlineParseResult {
    Command(RespFrame, usize),
    EmptyLine(usize),
    /// Protocol error (e.g., unbalanced quotes) — send error reply directly.
    ProtocolError(RespFrame, usize),
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
    let argv = match split_inline_args(line) {
        Ok(v) => v,
        Err(msg) => {
            // Unbalanced quotes — return protocol error directly (matching Redis behavior).
            let err_frame = RespFrame::Error(msg.to_string());
            return Ok(InlineParseResult::ProtocolError(err_frame, consumed));
        }
    };
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
/// Returns Err if quotes are unbalanced (matching Redis behavior).
fn split_inline_args(line: &[u8]) -> Result<Vec<Vec<u8>>, &'static str> {
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
                    if line[i] == b'x'
                        && i + 2 < line.len()
                        && let Some(byte) = parse_hex_escape(line[i + 1], line[i + 2])
                    {
                        arg.push(byte);
                        i += 3;
                        continue;
                    }
                    match line[i] {
                        b'n' => arg.push(b'\n'),
                        b'r' => arg.push(b'\r'),
                        b't' => arg.push(b'\t'),
                        b'b' => arg.push(b'\x08'),
                        b'a' => arg.push(b'\x07'),
                        b'"' => arg.push(b'"'),
                        b'\\' => arg.push(b'\\'),
                        other => {
                            arg.push(other);
                        }
                    }
                } else {
                    arg.push(line[i]);
                }
                i += 1;
            }
            if i >= line.len() {
                return Err("ERR Protocol error: unbalanced quotes in request");
            }
            i += 1; // Skip closing quote.
            args.push(arg);
        } else if line[i] == b'\'' {
            // Single-quoted argument. Supports escaping only single quotes.
            i += 1;
            let mut arg = Vec::new();
            while i < line.len() && line[i] != b'\'' {
                if line[i] == b'\\' && i + 1 < line.len() && line[i + 1] == b'\'' {
                    i += 1;
                    arg.push(b'\'');
                } else {
                    arg.push(line[i]);
                }
                i += 1;
            }
            if i >= line.len() {
                return Err("ERR Protocol error: unbalanced quotes in request");
            }
            args.push(arg);
            i += 1; // Skip closing quote.
        } else {
            // Unquoted argument.
            let start = i;
            while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
                i += 1;
            }
            args.push(line[start..i].to_vec());
        }
    }
    Ok(args)
}

fn parse_hex_escape(h1: u8, h2: u8) -> Option<u8> {
    let parse_hex = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let b1 = parse_hex(h1)?;
    let b2 = parse_hex(h2)?;
    Some((b1 << 4) | b2)
}

fn replica_handshake_frame(args: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        args.iter()
            .map(|arg| RespFrame::BulkString(Some(arg.to_vec())))
            .collect(),
    ))
}

fn encode_replication_snapshot(snapshot: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(snapshot.len().saturating_add(34));
    out.extend_from_slice(b"$");
    out.extend_from_slice(snapshot.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(snapshot);
    out.extend_from_slice(b"\r\n");
    out
}

#[cfg(test)]
fn encode_eof_marked_replication_snapshot(snapshot: &[u8], eof_mark: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        snapshot
            .len()
            .saturating_add(eof_mark.len().saturating_mul(2))
            .saturating_add(8),
    );
    out.extend_from_slice(b"$EOF:");
    out.extend_from_slice(eof_mark);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(snapshot);
    out.extend_from_slice(eof_mark);
    out
}

fn find_crlf(input: &[u8]) -> Option<usize> {
    input.windows(2).position(|window| window == b"\r\n")
}

fn drain_leading_replication_keepalive_bytes(read_buf: &mut Vec<u8>) {
    loop {
        if read_buf.starts_with(b"\r\n") {
            read_buf.drain(..2);
        } else if read_buf.starts_with(b"\n") {
            read_buf.drain(..1);
        } else {
            break;
        }
    }
}

fn read_more_replication_bytes(
    stream: &mut StdTcpStream,
    read_buf: &mut Vec<u8>,
    query_buffer_limit: usize,
) -> io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "primary closed replication stream",
                ));
            }
            Ok(n) => {
                return match validate_read_path(read_buf.len(), n, query_buffer_limit, false) {
                    Ok(_) => {
                        read_buf.extend_from_slice(&chunk[..n]);
                        Ok(())
                    }
                    Err(err) => Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "replication read exceeded query buffer limit ({})",
                            err.reason_code()
                        ),
                    )),
                };
            }
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

fn read_frame_from_stream(
    stream: &mut StdTcpStream,
    read_buf: &mut Vec<u8>,
    parser_config: &ParserConfig,
    query_buffer_limit: usize,
) -> io::Result<RespFrame> {
    loop {
        drain_leading_replication_keepalive_bytes(read_buf);
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
        read_more_replication_bytes(stream, read_buf, query_buffer_limit)?;
    }
}

fn read_replication_snapshot_from_stream(
    stream: &mut StdTcpStream,
    read_buf: &mut Vec<u8>,
    query_buffer_limit: usize,
) -> io::Result<Vec<u8>> {
    loop {
        drain_leading_replication_keepalive_bytes(read_buf);
        if let Some(preamble_end) = find_crlf(read_buf) {
            let preamble = read_buf[..preamble_end].to_vec();
            if !preamble.starts_with(b"$") {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "primary did not send replication snapshot preamble",
                ));
            }

            read_buf.drain(..preamble_end + 2);

            if let Some(eof_mark) = preamble.strip_prefix(b"$EOF:") {
                if eof_mark.len() < 40 {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "primary sent invalid EOF snapshot marker",
                    ));
                }
                let eof_mark = eof_mark[..40].to_vec();
                loop {
                    if let Some(marker_index) = read_buf
                        .windows(eof_mark.len())
                        .position(|window| window == eof_mark.as_slice())
                    {
                        let snapshot = read_buf[..marker_index].to_vec();
                        read_buf.drain(..marker_index + eof_mark.len());
                        if read_buf.starts_with(b"\r\n") {
                            read_buf.drain(..2);
                        }
                        return Ok(snapshot);
                    }
                    read_more_replication_bytes(stream, read_buf, query_buffer_limit)?;
                }
            }

            let data_len = std::str::from_utf8(&preamble[1..])
                .ok()
                .and_then(|text| text.parse::<usize>().ok())
                .ok_or_else(|| {
                    io::Error::new(
                        ErrorKind::InvalidData,
                        "primary sent invalid replication snapshot length",
                    )
                })?;

            while read_buf.len() < data_len {
                read_more_replication_bytes(stream, read_buf, query_buffer_limit)?;
            }
            let snapshot = read_buf[..data_len].to_vec();
            read_buf.drain(..data_len);
            if read_buf.starts_with(b"\r\n") {
                read_buf.drain(..2);
            }
            return Ok(snapshot);
        }
        read_more_replication_bytes(stream, read_buf, query_buffer_limit)?;
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

fn consume_complete_replication_prefix(
    read_buf: &mut Vec<u8>,
    parser_config: &ParserConfig,
) -> io::Result<Vec<u8>> {
    let mut consumed_total = 0usize;
    loop {
        if consumed_total >= read_buf.len() {
            break;
        }
        let unparsed = &read_buf[consumed_total..];
        match fr_protocol::parse_frame_with_config(unparsed, parser_config) {
            Ok(parsed) => {
                consumed_total = consumed_total.saturating_add(parsed.consumed);
            }
            Err(RespParseError::Incomplete) => break,
            Err(err) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("invalid replication backlog from primary: {err}"),
                ));
            }
        }
    }
    if consumed_total == 0 {
        return Ok(Vec::new());
    }
    let payload = read_buf[..consumed_total].to_vec();
    read_buf.drain(..consumed_total);
    Ok(payload)
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

    if let Some((masteruser, masterauth)) = runtime.replica_primary_auth() {
        let mut auth_argv = vec![b"AUTH".as_slice()];
        if let Some(masteruser) = masteruser.as_ref() {
            auth_argv.push(masteruser.as_slice());
        }
        auth_argv.push(masterauth.as_slice());
        stream.write_all(&replica_handshake_frame(&auth_argv).to_bytes())?;
        expect_simple_string(
            read_frame_from_stream(
                &mut stream,
                &mut read_buf,
                &parser_config,
                runtime.server.query_buffer_limit,
            )?,
            "OK",
        )?;
    }

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
        read_replication_snapshot_from_stream(
            &mut stream,
            &mut read_buf,
            runtime.server.query_buffer_limit,
        )?
    } else {
        let disconnected = read_available_stream_bytes(
            &mut stream,
            &mut read_buf,
            runtime.server.query_buffer_limit,
        )?;
        // First try to process any buffered data before erroring out on disconnect.
        // The primary might have sent data before closing the connection.
        let payload = consume_complete_replication_prefix(&mut read_buf, &parser_config)?;
        if disconnected && payload.is_empty() {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "primary closed replication stream during sync",
            ));
        }
        payload
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
    let mut total_written = 0;
    let mut result = Ok(());
    while total_written < connection.write_buf.len() {
        match connection
            .stream
            .write(&connection.write_buf[total_written..])
        {
            Ok(0) => {
                result = Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "replica write zero on primary stream",
                ));
                break;
            }
            Ok(written) => {
                total_written += written;
            }
            Err(ref err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                break;
            }
            Err(ref err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => {
                result = Err(err);
                break;
            }
        }
    }
    if total_written > 0 {
        connection.write_buf.drain(..total_written);
    }
    result
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
    frame.encode_into(&mut connection.write_buf);
    connection.next_ack_ms = now_ms.saturating_add(REPLICA_ACK_INTERVAL_MS);
}

fn validate_replica_write_buffer_limit(
    connection: &ReplicaPrimaryConnection,
    output_buffer_limit: usize,
) -> io::Result<()> {
    if connection.write_buf.len() > output_buffer_limit {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "replica write buffer exceeded output buffer limit",
        ));
    }
    Ok(())
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
    let mut consumed_total = 0;
    loop {
        if consumed_total >= connection.read_buf.len() {
            break;
        }

        let unparsed = &connection.read_buf[consumed_total..];
        match fr_protocol::parse_frame_with_config(unparsed, &runtime.parser_config()) {
            Ok(parsed) => {
                let frame = parsed.frame;
                consumed_total += parsed.consumed;
                let response =
                    runtime.execute_frame(frame.clone(), now_ms.saturating_add(frame_index));
                if let RespFrame::Error(message) = &response {
                    eprintln!("warn: replica replay command failed for frame {frame:?}: {message}");
                }
                if let Some(follow_up) = replication_stream_follow_up_bytes(&frame, &response) {
                    connection.write_buf.extend_from_slice(&follow_up);
                    validate_replica_write_buffer_limit(
                        connection,
                        runtime.server.output_buffer_limit,
                    )?;
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

    if consumed_total > 0 {
        connection.read_buf.drain(..consumed_total);
    }

    Ok(disconnected)
}

fn drive_replica_sync(runtime: &mut Runtime, replica_sync: &mut ReplicaSyncState, now_ms: u64) {
    if runtime.take_replica_reconfigure_request() {
        replica_sync.connection = None;
        replica_sync.retry_after_ms = 0;
    }
    if let Some(connection) = replica_sync.connection.as_mut() {
        queue_replica_periodic_ack(runtime, connection, now_ms);
        if let Err(err) =
            validate_replica_write_buffer_limit(connection, runtime.server.output_buffer_limit)
        {
            replica_sync.connection = None;
            replica_sync.schedule_retry(now_ms);
            runtime.set_replica_connection_state("reconnect");
            eprintln!("warn: replica stream write failed: {err}");
            return;
        }
        if let Err(err) = flush_replica_primary_writes(connection) {
            replica_sync.connection = None;
            replica_sync.schedule_retry(now_ms);
            runtime.set_replica_connection_state("reconnect");
            eprintln!("warn: replica stream write failed: {err}");
            return;
        }
        match drain_replica_stream(runtime, connection, now_ms) {
            Ok(false) => {
                if let Err(err) = flush_replica_primary_writes(connection) {
                    replica_sync.connection = None;
                    replica_sync.schedule_retry(now_ms);
                    runtime.set_replica_connection_state("reconnect");
                    eprintln!("warn: replica stream write failed: {err}");
                    return;
                }
            }
            Ok(true) => {
                replica_sync.connection = None;
                replica_sync.schedule_retry(now_ms);
                runtime.set_replica_connection_state("reconnect");
            }
            Err(err) => {
                replica_sync.connection = None;
                replica_sync.schedule_retry(now_ms);
                runtime.set_replica_connection_state("reconnect");
                eprintln!("warn: replica stream read failed: {err}");
            }
        }
    }

    if replica_sync.connection.is_some() {
        return;
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
    if !is_replication_sync_frame(frame) {
        return None;
    }
    let RespFrame::SimpleString(line) = response else {
        return None;
    };
    if line.starts_with("FULLRESYNC ") {
        let snapshot = runtime.encoded_rdb_snapshot(now_ms);
        return Some(encode_replication_snapshot(snapshot.as_slice()));
    }
    if line == "CONTINUE" {
        let offset = psync_requested_offset(frame)?;
        return Some(runtime.encoded_aof_stream_from_offset(offset));
    }
    None
}

pub(crate) fn is_replication_sync_frame(frame: &RespFrame) -> bool {
    if let RespFrame::Array(Some(items)) = frame
        && let Some(RespFrame::BulkString(Some(cmd))) = items.first()
    {
        return cmd.eq_ignore_ascii_case(b"PSYNC") || cmd.eq_ignore_ascii_case(b"SYNC");
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
    if timeout_secs == 0.0 {
        return Some(u64::MAX);
    }

    let timeout_ms = timeout_secs * 1000.0;
    if !timeout_ms.is_finite() {
        return None;
    }

    let delta_ms = timeout_ms.ceil();
    if delta_ms > u64::MAX as f64 {
        return None;
    }

    now_ms.checked_add(delta_ms as u64)
}

/// Extract the BLOCK timeout from XREAD/XREADGROUP argv and compute a deadline.
fn parse_xread_block_deadline_argv(argv: &[Vec<u8>], now_ms: u64) -> Option<u64> {
    let streams_pos = argv
        .iter()
        .position(|arg| arg.eq_ignore_ascii_case(b"STREAMS"));
    let search_end = streams_pos.unwrap_or(argv.len());
    for i in 0..search_end {
        let arg = &argv[i];
        if !arg.eq_ignore_ascii_case(b"BLOCK") {
            continue;
        }
        let timeout_bytes = argv.get(i + 1)?;
        let ms: i64 = std::str::from_utf8(timeout_bytes).ok()?.parse().ok()?;
        if ms < 0 {
            return None;
        }
        if ms == 0 {
            return Some(u64::MAX);
        }
        return now_ms.checked_add(ms as u64);
    }
    None // BLOCK keyword not found
}

fn resolve_xread_block_argv(
    argv: &[Vec<u8>],
    runtime: &mut Runtime,
    now_ms: u64,
) -> Option<Vec<Vec<u8>>> {
    let streams_idx = argv
        .iter()
        .position(|arg| arg.eq_ignore_ascii_case(b"STREAMS"))?;
    let ids_start = streams_idx + 1;
    let remaining = argv.len().checked_sub(ids_start)?;
    if remaining < 2 || !remaining.is_multiple_of(2) {
        return None;
    }
    let stream_count = remaining / 2;
    let mut resolved = argv.to_vec();
    for offset in 0..stream_count {
        let id_idx = ids_start + stream_count + offset;
        if resolved.get(id_idx)?.as_slice() != b"$" {
            continue;
        }
        let key = resolved.get(ids_start + offset)?.clone();
        let resume_id = runtime
            .xread_block_resume_id(&key, now_ms)
            .unwrap_or((0, 0));
        resolved[id_idx] = format!("{}-{}", resume_id.0, resume_id.1).into_bytes();
    }
    Some(resolved)
}

fn resolve_blocked_op(op: BlockingOp, runtime: &mut Runtime, now_ms: u64) -> Option<BlockingOp> {
    match op {
        BlockingOp::BXread { argv } => Some(BlockingOp::BXread {
            argv: resolve_xread_block_argv(&argv, runtime, now_ms)?,
        }),
        other => Some(other),
    }
}

/// Parse a blocking command frame and build `BlockedState` if the command
/// is a blocking operation with a non-zero timeout.
fn try_build_blocked_state(frame: &RespFrame, now_ms: u64) -> Option<BlockedState> {
    let argv = fr_command::frame_to_argv(frame).ok()?;
    let cmd = argv.first()?;

    if cmd.eq_ignore_ascii_case(b"BLPOP")
        || cmd.eq_ignore_ascii_case(b"BRPOP")
        || cmd.eq_ignore_ascii_case(b"BZPOPMAX")
        || cmd.eq_ignore_ascii_case(b"BZPOPMIN")
    {
        if argv.len() < 3 {
            return None;
        }
        // Last element is the timeout.
        let timeout_bytes = argv.last()?;
        let deadline_ms = parse_blocking_deadline(timeout_bytes, now_ms)?;
        let keys: Vec<Vec<u8>> = argv[1..argv.len() - 1].to_vec();
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
        if argv.len() != 6 {
            return None;
        }
        let timeout_bytes = &argv[5];
        let deadline_ms = parse_blocking_deadline(timeout_bytes, now_ms)?;
        let source = argv[1].clone();
        let destination = argv[2].clone();
        let wherefrom = argv[3].clone();
        let whereto = argv[4].clone();
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
        if argv.len() != 4 {
            return None;
        }
        let timeout_bytes = &argv[3];
        let deadline_ms = parse_blocking_deadline(timeout_bytes, now_ms)?;
        let source = argv[1].clone();
        let destination = argv[2].clone();
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
        if argv.len() < 5 {
            return None;
        }
        let timeout_bytes = &argv[1];
        let deadline_ms = parse_blocking_deadline(timeout_bytes, now_ms)?;
        let op = if cmd.eq_ignore_ascii_case(b"BLMPOP") {
            BlockingOp::BLmpop { argv }
        } else {
            BlockingOp::BZmpop { argv }
        };
        Some(BlockedState { op, deadline_ms })
    } else if cmd.eq_ignore_ascii_case(b"XREAD") || cmd.eq_ignore_ascii_case(b"XREADGROUP") {
        let deadline_ms = parse_xread_block_deadline_argv(&argv, now_ms)?;
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

struct CheckBlockedClientsContext<'a> {
    clients: &'a mut HashMap<Token, ClientConnection>,
    blocked_tokens: &'a mut HashSet<Token>,
    closing_tokens: &'a mut HashSet<Token>,
    paused_tokens: &'a mut HashSet<Token>,
    runtime: &'a mut Runtime,
    poll: &'a mut Poll,
    write_tokens: &'a mut HashSet<Token>,
    ts: u64,
}

/// Check all blocked clients. Unblock them if their keys have data or
/// their timeout has expired.
fn check_blocked_clients(ctx: CheckBlockedClientsContext<'_>) {
    let CheckBlockedClientsContext {
        clients,
        blocked_tokens,
        closing_tokens,
        paused_tokens,
        runtime,
        poll,
        write_tokens,
        ts,
    } = ctx;
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
                BlockingOp::BLmove { .. } => RespFrame::BulkString(None),
                _ => RespFrame::Array(None),
            };
            nil_response.encode_into(&mut conn.write_buf);
            conn.blocked = None;
            blocked_tokens.remove(&token);
            runtime.mark_client_unblocked(conn.session.client_id);

            // Process any commands the client pipelined while blocked.
            if !conn.read_buf.is_empty() {
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
                let updated_session = runtime.swap_session(prev);
                conn.session = updated_session;
            }

            let _ = arm_write_interest(token, conn, poll, write_tokens);
            continue;
        }

        // Try to fulfill the blocking operation.
        let session = std::mem::take(&mut conn.session);
        let prev = runtime.swap_session(session);

        let result = try_fulfill_blocked(&blocked.op, runtime, ts);

        if let Some(response) = result {
            response.encode_into(&mut conn.write_buf);
            conn.blocked = None;
            blocked_tokens.remove(&token);
            runtime.mark_client_unblocked(runtime.client_id());

            // Process any commands the client pipelined while blocked.
            if !conn.read_buf.is_empty() {
                // The session is already swapped in here.
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
            }
        }

        let updated_session = runtime.swap_session(prev);
        conn.session = updated_session;

        // Always arm write interest if there's pending output, even if the
        // client re-blocked during pipelined command processing — the response
        // from the first unblock still needs to reach the client.
        let _ = arm_write_interest(token, conn, poll, write_tokens);
    }
}

struct PendingClientUnblocksContext<'a> {
    clients: &'a mut HashMap<Token, ClientConnection>,
    client_id_to_token: &'a HashMap<u64, Token>,
    blocked_tokens: &'a mut HashSet<Token>,
    closing_tokens: &'a mut HashSet<Token>,
    paused_tokens: &'a mut HashSet<Token>,
    runtime: &'a mut Runtime,
    poll: &'a mut Poll,
    write_tokens: &'a mut HashSet<Token>,
    ts: u64,
}

fn apply_pending_client_unblocks(ctx: PendingClientUnblocksContext<'_>) {
    let PendingClientUnblocksContext {
        clients,
        client_id_to_token,
        blocked_tokens,
        closing_tokens,
        paused_tokens,
        runtime,
        poll,
        write_tokens,
        ts,
    } = ctx;
    let requests = runtime.drain_pending_client_unblocks();
    for (client_id, mode) in requests {
        let Some(&token) = client_id_to_token.get(&client_id) else {
            runtime.mark_client_unblocked(client_id);
            continue;
        };
        let Some(conn) = clients.get_mut(&token) else {
            runtime.mark_client_unblocked(client_id);
            blocked_tokens.remove(&token);
            continue;
        };
        let Some(blocked) = &conn.blocked else {
            runtime.mark_client_unblocked(client_id);
            blocked_tokens.remove(&token);
            continue;
        };

        let response = match mode {
            ClientUnblockMode::Timeout => match blocked.op {
                BlockingOp::BLmove { .. } => RespFrame::BulkString(None),
                _ => RespFrame::Array(None),
            },
            ClientUnblockMode::Error => {
                RespFrame::Error("UNBLOCKED client unblocked via CLIENT UNBLOCK".to_string())
            }
        };

        response.encode_into(&mut conn.write_buf);
        conn.blocked = None;
        blocked_tokens.remove(&token);
        runtime.mark_client_unblocked(client_id);

        if !conn.read_buf.is_empty() {
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
            let updated_session = runtime.swap_session(prev);
            conn.session = updated_session;
        }

        let _ = arm_write_interest(token, conn, poll, write_tokens);
    }
}

fn arm_write_interest(
    token: Token,
    conn: &mut ClientConnection,
    poll: &mut Poll,
    write_tokens: &mut HashSet<Token>,
) -> io::Result<()> {
    if conn.write_buf.is_empty() {
        write_tokens.remove(&token);
        poll.registry()
            .reregister(&mut conn.stream, token, Interest::READABLE)?;
        return Ok(());
    }

    write_tokens.insert(token);
    poll.registry().reregister(
        &mut conn.stream,
        token,
        Interest::READABLE | Interest::WRITABLE,
    )?;

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
                if matches!(response, RespFrame::Error(_)) {
                    return Some(response);
                }
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
                if matches!(response, RespFrame::Error(_)) {
                    return Some(response);
                }
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
                if matches!(response, RespFrame::Error(_)) {
                    return Some(response);
                }
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
                if matches!(response, RespFrame::Error(_)) {
                    return Some(response);
                }
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
            if response == RespFrame::Array(None) {
                None
            } else {
                Some(response)
            }
        }
    }
}

fn propagate_writes_to_replicas(
    clients: &mut HashMap<Token, ClientConnection>,
    runtime: &mut Runtime,
    poll: &mut Poll,
    write_tokens: &mut HashSet<Token>,
) {
    let primary_offset = runtime.replication_primary_offset();
    let aof_base = runtime.aof_base_offset();
    let mut encoded_stream: Option<Vec<u8>> = None;
    for (&token, conn) in clients.iter_mut() {
        if let Some(sent_offset) = conn.replication_sent_offset
            && sent_offset < primary_offset
        {
            let stream = encoded_stream.get_or_insert_with(|| runtime.encoded_aof_stream());
            // Convert absolute replication offset to local stream index by subtracting
            // the AOF base offset. The encoded stream starts at index 0, which
            // corresponds to absolute offset aof_base.
            let relative_offset = sent_offset.0.saturating_sub(aof_base);
            let start = usize::try_from(relative_offset).unwrap_or(usize::MAX);
            let bytes = stream.get(start..).unwrap_or(&[]);
            if !bytes.is_empty() {
                conn.write_buf.extend_from_slice(bytes);
                write_tokens.insert(token);
                let _ = poll.registry().reregister(
                    &mut conn.stream,
                    token,
                    Interest::READABLE | Interest::WRITABLE,
                );
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
    closing_tokens: &mut HashSet<Token>,
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
        if conn.write_buf.len() > runtime.server.output_buffer_limit {
            eprintln!(
                "warn: client write buffer exceeded limit during monitor delivery, disconnecting"
            );
            conn.closing = true;
            closing_tokens.insert(token);
            continue;
        }
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
    closing_tokens: &mut HashSet<Token>,
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
            frame.encode_into(&mut conn.write_buf);
        }

        if conn.write_buf.len() > runtime.server.output_buffer_limit {
            eprintln!(
                "warn: client write buffer exceeded limit during pubsub delivery, disconnecting"
            );
            conn.closing = true;
            closing_tokens.insert(token);
            continue;
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
    let argv = fr_command::frame_to_argv(frame).ok()?;
    let cmd = argv.first()?;
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
    fr_command::frame_to_argv(frame)
        .ok()
        .and_then(|argv| argv.first().cloned())
        .is_some_and(|cmd| cmd.eq_ignore_ascii_case(b"QUIT"))
}

fn suppress_client_network_reply(frame: &RespFrame, response: &RespFrame) -> bool {
    if matches!(response, RespFrame::Error(_)) {
        return false;
    }
    let Ok(argv) = fr_command::frame_to_argv(frame) else {
        return false;
    };
    match argv.as_slice() {
        [command, subcommand, _]
            if command.eq_ignore_ascii_case(b"REPLCONF")
                && subcommand.eq_ignore_ascii_case(b"ACK") =>
        {
            true
        }
        [command] if command.eq_ignore_ascii_case(b"SYNC") => true,
        [command, subcommand, argument]
            if command.eq_ignore_ascii_case(b"REPLCONF")
                && subcommand.eq_ignore_ascii_case(b"GETACK")
                && argument.as_slice() == b"*" =>
        {
            true
        }
        _ => false,
    }
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
        BlockingOp, CheckBlockedClientsContext, InlineParseResult, PendingClientUnblocksContext,
        REPLICA_ACK_INTERVAL_MS, REPLICA_RECONNECT_BACKOFF_MS, ReplicaPrimaryConnection,
        ReplicaSyncState, apply_pending_client_unblocks, check_blocked_clients,
        consume_complete_replication_prefix, drain_replica_stream, drive_replica_sync,
        encode_eof_marked_replication_snapshot, encode_replication_snapshot, find_crlf,
        parse_blocking_deadline, parse_xread_block_deadline_argv, read_frame_from_stream,
        read_replication_snapshot_from_stream, replica_handshake_frame,
        replica_handshake_read_timeout, replication_follow_up_bytes, resolve_xread_block_argv,
        server_help_text, should_try_inline_parsing, sync_replica_with_primary,
        try_build_blocked_state, try_fulfill_blocked,
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
    fn help_text_documents_all_supported_cli_flags() {
        let help = server_help_text();

        assert!(help.contains("USAGE: frankenredis [OPTIONS]"));
        assert!(help.contains("--bind <ADDR>"));
        assert!(help.contains("--port <PORT>"));
        assert!(help.contains("--mode <MODE>"));
        assert!(help.contains("--aof <PATH>"));
        assert!(help.contains("--rdb <PATH>"));
        assert!(help.contains("--replicaof <HOST> <PORT>"));
        assert!(help.contains("--masteruser <USERNAME>"));
        assert!(help.contains("--masterauth <PASSWORD>"));
        assert!(help.contains("--help"));
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
        let preamble_end = find_crlf(&follow_up).expect("snapshot preamble terminator");
        let preamble = std::str::from_utf8(&follow_up[..preamble_end]).expect("utf8 preamble");
        let snapshot_len = preamble
            .strip_prefix('$')
            .expect("snapshot preamble")
            .parse::<usize>()
            .expect("snapshot length");
        let snapshot_start = preamble_end + 2;
        let snapshot_end = snapshot_start.saturating_add(snapshot_len);
        assert!(
            follow_up.len() >= snapshot_end,
            "snapshot payload shorter than preamble length"
        );
        let snapshot = &follow_up[snapshot_start..snapshot_end];
        let trailing = &follow_up[snapshot_end..];
        assert_eq!(trailing, b"\r\n");
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
            assert!(
                matches!(replconf_port, RespFrame::Array(Some(_))),
                "unexpected replconf frame: {replconf_port:?}"
            );
            let RespFrame::Array(Some(items)) = replconf_port else {
                return;
            };
            assert_eq!(items[0], RespFrame::BulkString(Some(b"REPLCONF".to_vec())));
            assert_eq!(
                items[1],
                RespFrame::BulkString(Some(b"listening-port".to_vec()))
            );
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
                .write_all(&encode_replication_snapshot(snapshot.as_slice()))
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
        // Clear reconfigure flag so sync proceeds instead of resetting.
        let _ = replica.take_replica_reconfigure_request();
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
    fn replica_sync_helper_authenticates_with_masteruser_and_masterauth() {
        let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind primary socket");
        let addr = listener.local_addr().expect("local addr");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept replica");
            let parser = ParserConfig::default();
            let mut read_buf = Vec::new();

            let auth = read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                .expect("read auth");
            assert_eq!(
                auth,
                replica_handshake_frame(&[b"AUTH", b"replica", b"secret"])
            );
            stream
                .write_all(&RespFrame::SimpleString("OK".to_string()).to_bytes())
                .expect("write auth ok");

            let ping = read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                .expect("read ping");
            assert_eq!(ping, replica_handshake_frame(&[b"PING"]));
            stream
                .write_all(&RespFrame::SimpleString("PONG".to_string()).to_bytes())
                .expect("write pong");

            let replconf_port =
                read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                    .expect("replconf");
            let RespFrame::Array(Some(items)) = replconf_port else {
                panic!("unexpected replconf frame");
            };
            assert_eq!(items[0], RespFrame::BulkString(Some(b"REPLCONF".to_vec())));
            assert_eq!(
                items[1],
                RespFrame::BulkString(Some(b"listening-port".to_vec()))
            );
            stream
                .write_all(&RespFrame::SimpleString("OK".to_string()).to_bytes())
                .expect("write replconf ok");

            let replconf_capa =
                read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                    .expect("capa");
            assert_eq!(
                replconf_capa,
                replica_handshake_frame(&[b"REPLCONF", b"capa", b"psync2"])
            );
            stream
                .write_all(&RespFrame::SimpleString("OK".to_string()).to_bytes())
                .expect("write capa ok");

            let psync = read_frame_from_stream(&mut stream, &mut read_buf, &parser, usize::MAX)
                .expect("psync");
            assert_eq!(psync, replica_handshake_frame(&[b"PSYNC", b"?", b"-1"]));
            stream
                .write_all(&RespFrame::SimpleString("CONTINUE".to_string()).to_bytes())
                .expect("write continue");
            stream
                .write_all(&replica_handshake_frame(&[b"PING"]).to_bytes())
                .expect("write trailing ping");
        });

        let mut replica = Runtime::default_strict();
        replica.set_server_port(6381);
        replica.set_masteruser(Some(b"replica".to_vec()));
        replica.set_masterauth(Some(b"secret".to_vec()));
        let host = addr.ip().to_string();
        let connection = sync_replica_with_primary(&mut replica, &host, addr.port(), "?", -1, 1)
            .expect("sync with auth");
        drop(connection);

        server.join().expect("primary thread");
    }

    #[test]
    fn replica_sync_helper_accepts_eof_marked_snapshot_from_primary_socket() {
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
        let eof_mark = b"0123456789abcdef0123456789abcdef01234567";

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
            assert!(
                matches!(replconf_port, RespFrame::Array(Some(_))),
                "unexpected replconf frame: {replconf_port:?}"
            );
            let RespFrame::Array(Some(items)) = replconf_port else {
                return;
            };
            assert_eq!(items[0], RespFrame::BulkString(Some(b"REPLCONF".to_vec())));
            assert_eq!(
                items[1],
                RespFrame::BulkString(Some(b"listening-port".to_vec()))
            );
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
                .write_all(&encode_eof_marked_replication_snapshot(
                    snapshot.as_slice(),
                    eof_mark,
                ))
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
        // Clear reconfigure flag so sync proceeds instead of resetting.
        let _ = replica.take_replica_reconfigure_request();
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

        server.join().expect("primary thread");
    }

    #[test]
    fn replication_prefix_parser_stops_on_incomplete_frame() {
        let frame1 = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"PING".to_vec()))]));
        let frame2 = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"SET".to_vec())),
            RespFrame::BulkString(Some(b"k".to_vec())),
            RespFrame::BulkString(Some(b"v".to_vec())),
        ]));
        let frame1_bytes = frame1.to_bytes();
        let frame2_bytes = frame2.to_bytes();
        let split_at = frame2_bytes.len().saturating_sub(2);
        let mut read_buf = Vec::new();
        read_buf.extend_from_slice(&frame1_bytes);
        read_buf.extend_from_slice(&frame2_bytes[..split_at]);

        let payload = consume_complete_replication_prefix(&mut read_buf, &ParserConfig::default())
            .expect("prefix parse");
        assert_eq!(payload, frame1_bytes);
        // After consuming the complete frame, the incomplete frame remains
        assert_eq!(read_buf, &frame2_bytes[..split_at]);
    }

    #[test]
    fn replication_snapshot_reader_consumes_trailing_crlf() {
        let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let _writer = StdTcpStream::connect(addr).expect("connect writer");
        let (mut stream, _) = listener.accept().expect("accept reader");

        let mut read_buf = Vec::new();
        read_buf.extend_from_slice(b"$3\r\nabc\r\n*1\r\n$4\r\nPING\r\n");

        let snapshot =
            read_replication_snapshot_from_stream(&mut stream, &mut read_buf, usize::MAX)
                .expect("snapshot");
        assert_eq!(snapshot, b"abc");
        assert_eq!(read_buf, b"*1\r\n$4\r\nPING\r\n");
    }

    #[test]
    fn replica_sync_helper_rejects_continue_when_primary_disconnects() {
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
            assert!(
                matches!(replconf_port, RespFrame::Array(Some(_))),
                "unexpected replconf frame: {replconf_port:?}"
            );
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
                .write_all(&RespFrame::SimpleString("CONTINUE".to_string()).to_bytes())
                .unwrap();
        });

        let mut replica = Runtime::default_strict();
        replica.set_server_port(6381);
        let host = addr.ip().to_string();
        let result = sync_replica_with_primary(&mut replica, &host, addr.port(), "?", -1, 1);
        assert!(
            result.is_err(),
            "expected sync to fail after primary disconnects"
        );

        server.join().expect("primary thread");
    }

    #[test]
    fn replica_stream_applies_select_prefixed_delta_commands() {
        let mut primary = Runtime::default_strict();
        primary.set_server_port(6380);
        let snapshot = primary.encoded_rdb_snapshot(1);

        let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind primary socket");
        let addr = listener.local_addr().expect("local addr");
        let server = thread::spawn(move || {
            let parser = ParserConfig::default();
            let (mut stream, _) = listener.accept().expect("accept replica");
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
                    &RespFrame::SimpleString(
                        "FULLRESYNC 0000000000000000000000000000000000000000 0".to_string(),
                    )
                    .to_bytes(),
                )
                .unwrap();
            stream
                .write_all(&encode_replication_snapshot(snapshot.as_slice()))
                .unwrap();
            stream
                .write_all(&replica_handshake_frame(&[b"SELECT", b"0"]).to_bytes())
                .unwrap();
            stream
                .write_all(&replica_handshake_frame(&[b"SET", b"alpha", b"1"]).to_bytes())
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
        // Clear reconfigure flag so sync proceeds instead of resetting.
        let _ = replica.take_replica_reconfigure_request();

        drive_replica_sync(&mut replica, &mut replica_sync, 1);
        drive_replica_sync(&mut replica, &mut replica_sync, 2);

        assert_eq!(
            replica.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"GET".to_vec())),
                    RespFrame::BulkString(Some(b"alpha".to_vec())),
                ])),
                3,
            ),
            RespFrame::BulkString(Some(b"1".to_vec()))
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
                    .write_all(&encode_replication_snapshot(snapshot.as_slice()))
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
        // Clear reconfigure flag so sync proceeds instead of resetting.
        let _ = replica.take_replica_reconfigure_request();

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
                    .write_all(&encode_replication_snapshot(snapshot.as_slice()))
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
        // Clear reconfigure flag so sync proceeds instead of resetting.
        let _ = replica.take_replica_reconfigure_request();

        drive_replica_sync(&mut replica, &mut replica_sync, 1);
        drive_replica_sync(
            &mut replica,
            &mut replica_sync,
            1 + REPLICA_ACK_INTERVAL_MS + 1,
        );

        server.join().expect("primary thread");
    }

    #[test]
    fn replica_sync_clears_failed_connection_and_schedules_retry() {
        let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind primary socket");
        let addr = listener.local_addr().expect("local addr");

        let mut runtime = Runtime::default_strict();
        runtime.set_server_port(6381);
        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"REPLICAOF".to_vec())),
                    RespFrame::BulkString(Some(addr.ip().to_string().into_bytes())),
                    RespFrame::BulkString(Some(addr.port().to_string().into_bytes())),
                ])),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        // Clear the reconfigure flag so we test connection-failure retry behavior,
        // not the reconfigure-reset behavior.
        let _ = runtime.take_replica_reconfigure_request();

        let stream = StdTcpStream::connect(addr).expect("connect primary");
        let (server_stream, _) = listener.accept().expect("accept replica");
        drop(server_stream);
        stream
            .set_nonblocking(true)
            .expect("set replica stream nonblocking");

        let mut replica_sync = ReplicaSyncState::new();
        replica_sync.connection = Some(ReplicaPrimaryConnection {
            stream,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
            next_ack_ms: 0,
        });

        drive_replica_sync(&mut runtime, &mut replica_sync, 10);

        assert!(replica_sync.connection.is_none());
        assert!(replica_sync.retry_after_ms >= 10 + REPLICA_RECONNECT_BACKOFF_MS);
        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"ROLE".to_vec()))])),
                11,
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"slave".to_vec())),
                RespFrame::BulkString(Some(addr.ip().to_string().into_bytes())),
                RespFrame::Integer(i64::from(addr.port())),
                RespFrame::BulkString(Some(b"reconnect".to_vec())),
                RespFrame::Integer(0),
            ]))
        );
    }

    #[test]
    fn replicaof_reconfigure_resets_replica_sync_state() {
        let mut runtime = Runtime::default_strict();
        let mut replica_sync = ReplicaSyncState::new();
        // Simulate an existing replica connection.
        let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let stream = StdTcpStream::connect(addr).expect("connect");
        let (server_stream, _) = listener.accept().expect("accept");
        drop(server_stream);
        replica_sync.connection = Some(ReplicaPrimaryConnection {
            stream,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
            next_ack_ms: 0,
        });

        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"REPLICAOF".to_vec())),
                    RespFrame::BulkString(Some(b"127.0.0.1".to_vec())),
                    RespFrame::BulkString(Some(b"6380".to_vec())),
                ])),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        drive_replica_sync(&mut runtime, &mut replica_sync, 1);
        assert!(replica_sync.connection.is_none());
        assert!(replica_sync.retry_after_ms > REPLICA_RECONNECT_BACKOFF_MS);
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
    fn replica_stream_write_path_enforces_output_buffer_limit() {
        use std::net::TcpListener as StdTcpListener;

        let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind primary socket");
        let addr = listener.local_addr().expect("local addr");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept replica");
            stream
                .write_all(&replica_handshake_frame(&[b"REPLCONF", b"GETACK", b"*"]).to_bytes())
                .expect("write getack");
        });

        let mut runtime = Runtime::default_strict();
        runtime.server.output_buffer_limit = 0;
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
                .contains("replica write buffer exceeded output buffer limit"),
            "{err}"
        );
        assert!(!connection.write_buf.is_empty());

        server.join().expect("primary thread");
    }

    #[test]
    fn inline_command_parsing() {
        let parsed = crate::try_parse_inline(b"SET key value\r\n").expect("parse inline");
        assert!(
            matches!(parsed, InlineParseResult::Command(_, _)),
            "expected inline command"
        );
        let InlineParseResult::Command(frame, consumed) = parsed else {
            return;
        };
        assert_eq!(consumed, 15);
        assert!(
            matches!(frame, RespFrame::Array(Some(_))),
            "expected array, got {frame:?}"
        );
        let RespFrame::Array(Some(items)) = frame else {
            return;
        };
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], RespFrame::BulkString(Some(b"SET".to_vec())));
        assert_eq!(items[1], RespFrame::BulkString(Some(b"key".to_vec())));
        assert_eq!(items[2], RespFrame::BulkString(Some(b"value".to_vec())));
    }

    #[test]
    fn inline_quoted_strings() {
        let args = crate::split_inline_args(b"SET key \"hello world\"").expect("parse quoted");
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], b"SET");
        assert_eq!(args[1], b"key");
        assert_eq!(args[2], b"hello world");
    }

    #[test]
    fn inline_unbalanced_double_quotes_rejected() {
        let result = crate::split_inline_args(b"SET key \"unclosed");
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "ERR Protocol error: unbalanced quotes in request"
        );
    }

    #[test]
    fn inline_unbalanced_single_quotes_rejected() {
        let result = crate::split_inline_args(b"SET key 'unclosed");
        assert!(result.is_err());
    }

    #[test]
    fn inline_unbalanced_quotes_via_try_parse() {
        let result = crate::try_parse_inline(b"SET key \"unclosed\r\n").expect("should parse ok");
        assert!(
            matches!(result, InlineParseResult::ProtocolError(_, _)),
            "expected ProtocolError"
        );
        let InlineParseResult::ProtocolError(frame, consumed) = result else {
            return;
        };
        assert_eq!(consumed, 19);
        assert!(matches!(frame, RespFrame::Error(ref e) if e.contains("unbalanced")));
    }

    #[test]
    fn inline_incomplete_returns_error() {
        let result = crate::try_parse_inline(b"SET key value");
        assert!(result.is_err());
    }

    #[test]
    fn blank_inline_line_is_consumed_without_command() {
        let result = crate::try_parse_inline(b"\r\n").expect("blank line should parse");
        assert!(
            matches!(result, InlineParseResult::EmptyLine(_)),
            "blank line should not become a command or error"
        );
        let InlineParseResult::EmptyLine(consumed) = result else {
            return;
        };
        assert_eq!(consumed, 2);
    }

    #[test]
    fn drain_leading_replication_keepalive_bytes_strips_newline_prefixes_only() {
        let mut read_buf = b"\n\r\n+PONG\r\n".to_vec();
        crate::drain_leading_replication_keepalive_bytes(&mut read_buf);
        assert_eq!(read_buf, b"+PONG\r\n");

        let mut unchanged = b"+OK\r\n\n".to_vec();
        crate::drain_leading_replication_keepalive_bytes(&mut unchanged);
        assert_eq!(unchanged, b"+OK\r\n\n");
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
    fn parse_blocking_deadline_rounds_positive_fractional_values_up() {
        assert_eq!(parse_blocking_deadline(b"0.0001", 123), Some(124));
        assert_eq!(parse_blocking_deadline(b"1.0001", 123), Some(1124));
    }

    #[test]
    fn parse_blocking_deadline_rejects_out_of_range_values() {
        assert_eq!(parse_blocking_deadline(b"1e100", 123), None);
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
    fn blocking_state_builder_rounds_fractional_blpop_timeout_up() {
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"BLPOP".to_vec())),
            RespFrame::BulkString(Some(b"queue".to_vec())),
            RespFrame::BulkString(Some(b"0.0001".to_vec())),
        ]));
        let blocked = try_build_blocked_state(&frame, 1_000).expect("must block");
        assert_eq!(blocked.deadline_ms, 1_001);
    }

    #[test]
    fn bzpopmax_propagates_wrongtype_error() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        let now_ms = 1_000;
        let _ = runtime.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SET".to_vec())),
                RespFrame::BulkString(Some(b"myzset".to_vec())),
                RespFrame::BulkString(Some(b"value".to_vec())),
            ])),
            now_ms,
        );

        let op = BlockingOp::BZpopMax {
            keys: vec![b"myzset".to_vec()],
        };
        let response = try_fulfill_blocked(&op, &mut runtime, now_ms + 1);
        assert!(
            matches!(response, Some(RespFrame::Error(_))),
            "expected wrongtype error, got {response:?}"
        );
    }

    #[test]
    fn xread_wrongtype_unblocks_with_error() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        let now_ms = 1_000;
        let _ = runtime.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SET".to_vec())),
                RespFrame::BulkString(Some(b"stream".to_vec())),
                RespFrame::BulkString(Some(b"value".to_vec())),
            ])),
            now_ms,
        );

        let op = BlockingOp::BXread {
            argv: vec![
                b"XREAD".to_vec(),
                b"BLOCK".to_vec(),
                b"0".to_vec(),
                b"STREAMS".to_vec(),
                b"stream".to_vec(),
                b"0-0".to_vec(),
            ],
        };
        let response = try_fulfill_blocked(&op, &mut runtime, now_ms + 1);
        assert!(
            matches!(response, Some(RespFrame::Error(_))),
            "expected wrongtype error, got {response:?}"
        );
    }

    #[test]
    fn resolve_xread_block_argv_freezes_dollar_at_block_time() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        let now_ms = 1_000;
        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"XADD".to_vec())),
                    RespFrame::BulkString(Some(b"s".to_vec())),
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::BulkString(Some(b"field".to_vec())),
                    RespFrame::BulkString(Some(b"seed".to_vec())),
                ])),
                now_ms,
            ),
            RespFrame::BulkString(Some(b"1000-0".to_vec()))
        );

        let resolved = resolve_xread_block_argv(
            &[
                b"XREAD".to_vec(),
                b"BLOCK".to_vec(),
                b"0".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                b"$".to_vec(),
            ],
            &mut runtime,
            now_ms,
        )
        .expect("resolve xread argv");
        assert_eq!(resolved.last(), Some(&b"1000-0".to_vec()));

        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"XADD".to_vec())),
                    RespFrame::BulkString(Some(b"s".to_vec())),
                    RespFrame::BulkString(Some(b"1001-0".to_vec())),
                    RespFrame::BulkString(Some(b"field".to_vec())),
                    RespFrame::BulkString(Some(b"value".to_vec())),
                ])),
                now_ms + 1,
            ),
            RespFrame::BulkString(Some(b"1001-0".to_vec()))
        );

        let response = try_fulfill_blocked(
            &BlockingOp::BXread { argv: resolved },
            &mut runtime,
            now_ms + 2,
        );
        assert_eq!(
            response,
            Some(RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1001-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field".to_vec())),
                        RespFrame::BulkString(Some(b"value".to_vec())),
                    ])),
                ]))])),
            ]))])))
        );
    }

    #[test]
    fn parse_xread_block_deadline_rejects_fractional_and_out_of_range_values() {
        let fractional = vec![b"XREAD".to_vec(), b"BLOCK".to_vec(), b"1.5".to_vec()];
        assert_eq!(parse_xread_block_deadline_argv(&fractional, 123), None);

        let overflow = vec![b"XREAD".to_vec(), b"BLOCK".to_vec(), b"1".to_vec()];
        assert_eq!(parse_xread_block_deadline_argv(&overflow, u64::MAX), None);
    }

    #[test]
    fn parse_xread_block_deadline_ignores_block_after_streams() {
        let argv = vec![
            b"XREAD".to_vec(),
            b"STREAMS".to_vec(),
            b"BLOCK".to_vec(),
            b"0-0".to_vec(),
        ];
        assert_eq!(parse_xread_block_deadline_argv(&argv, 123), None);
    }

    #[test]
    fn blocking_state_builder_rejects_fractional_xread_block_timeout() {
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"XREAD".to_vec())),
            RespFrame::BulkString(Some(b"BLOCK".to_vec())),
            RespFrame::BulkString(Some(b"1.5".to_vec())),
            RespFrame::BulkString(Some(b"STREAMS".to_vec())),
            RespFrame::BulkString(Some(b"stream".to_vec())),
            RespFrame::BulkString(Some(b"0-0".to_vec())),
        ]));
        assert!(try_build_blocked_state(&frame, 1_000).is_none());
    }

    #[test]
    fn blocking_state_builder_accepts_resp3_integer_timeout() {
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"BLPOP".to_vec())),
            RespFrame::BulkString(Some(b"list".to_vec())),
            RespFrame::Integer(0),
        ]));
        let blocked = try_build_blocked_state(&frame, 1_000).expect("must block");
        assert!(matches!(blocked.op, BlockingOp::BLpop { .. }));
        let BlockingOp::BLpop { keys } = blocked.op else {
            return;
        };
        assert_eq!(keys, vec![b"list".to_vec()]);
        assert_eq!(blocked.deadline_ms, u64::MAX);
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
            ClientConnection::new(mio::net::TcpStream::from_std(stream), replica_session, 0);

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
        let token = Token(1); // ubs:ignore
        clients.insert(token, replica_conn);
        let mut write_tokens = HashSet::new();
        let mut poll = mio::Poll::new().unwrap();

        propagate_writes_to_replicas(&mut clients, &mut runtime, &mut poll, &mut write_tokens);

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
    fn replica_of_replica_chains_propagate_writes() {
        use crate::{ClientConnection, propagate_writes_to_replicas, replication_follow_up_bytes};
        use fr_persist::{AofRecord, encode_aof_stream};
        use fr_protocol::RespFrame;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::{HashMap, HashSet};
        use std::net::{TcpListener, TcpStream};

        let mut primary = Runtime::default_strict();
        let ts = 1000;

        // 1. Setup a "replica" client connection to primary.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (_server_stream, _server_addr) = listener.accept().unwrap();

        let replica_session = primary.new_session();
        let replica_id = replica_session.client_id;
        let mut replica_conn =
            ClientConnection::new(mio::net::TcpStream::from_std(stream), replica_session, 0);

        // 2. Perform PSYNC to mark as replica on the primary.
        let psync_frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PSYNC".to_vec())),
            RespFrame::BulkString(Some(b"?".to_vec())),
            RespFrame::BulkString(Some(b"-1".to_vec())),
        ]));

        let prev = primary.swap_session(std::mem::take(&mut replica_conn.session));
        let response = primary.execute_frame(psync_frame.clone(), ts);

        if let Some(follow_up) =
            replication_follow_up_bytes(&mut primary, &psync_frame, &response, ts)
        {
            replica_conn.write_buf.extend_from_slice(&follow_up);
            if primary.is_replica(replica_id) {
                replica_conn.replication_sent_offset = Some(primary.replication_primary_offset());
            }
        }
        replica_conn.session = primary.swap_session(prev);

        assert!(replica_conn.replication_sent_offset.is_some());

        // Simulate reading the FULLRESYNC reply and payload on the replica side
        let mut replica_rt = Runtime::default_strict();

        let reply_str = match &response {
            RespFrame::SimpleString(s) => s.clone(),
            _ => panic!("Expected simple string response"),
        };

        let payload_rdb = primary.encoded_rdb_snapshot(ts);

        replica_rt
            .apply_replication_sync_payload(&reply_str, &payload_rdb, ts)
            .unwrap();

        // 3. Now setup a "sub-replica" client connection to replica_rt.
        let listener_sub = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr_sub = listener_sub.local_addr().unwrap();
        let stream_sub = TcpStream::connect(addr_sub).unwrap();
        let (_server_stream_sub, _server_addr_sub) = listener_sub.accept().unwrap();

        let sub_replica_session = replica_rt.new_session();
        let sub_replica_id = sub_replica_session.client_id;
        let mut sub_replica_conn = ClientConnection::new(
            mio::net::TcpStream::from_std(stream_sub),
            sub_replica_session,
            0,
        );

        let prev = replica_rt.swap_session(std::mem::take(&mut sub_replica_conn.session));
        let response_sub = replica_rt.execute_frame(psync_frame.clone(), ts);

        if let Some(follow_up) =
            replication_follow_up_bytes(&mut replica_rt, &psync_frame, &response_sub, ts)
        {
            sub_replica_conn.write_buf.extend_from_slice(&follow_up);
            if replica_rt.is_replica(sub_replica_id) {
                sub_replica_conn.replication_sent_offset =
                    Some(replica_rt.replication_primary_offset());
            }
        }
        sub_replica_conn.session = replica_rt.swap_session(prev);

        assert!(sub_replica_conn.replication_sent_offset.is_some());

        // 4. Primary gets a write command.
        let other_session = primary.new_session();
        let prev = primary.swap_session(other_session);
        let _ = primary.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SET".to_vec())),
                RespFrame::BulkString(Some(b"foo".to_vec())),
                RespFrame::BulkString(Some(b"bar".to_vec())),
            ])),
            ts + 1,
        );
        let _ = primary.swap_session(prev);

        // 5. Propagate write to replica
        replica_conn.write_buf.clear();
        let mut clients = HashMap::new();
        let token = Token(1);
        clients.insert(token, replica_conn);
        let mut write_tokens = HashSet::new();
        let mut poll = mio::Poll::new().unwrap();

        propagate_writes_to_replicas(&mut clients, &mut primary, &mut poll, &mut write_tokens);

        let conn = clients.get(&token).unwrap();
        let replica_received_bytes = conn.write_buf.clone();

        // 6. Replica applies the replication stream
        replica_rt
            .apply_replication_sync_payload("CONTINUE", &replica_received_bytes, ts + 2)
            .unwrap();

        // 7. Replica propagates write to sub-replica
        sub_replica_conn.write_buf.clear();
        let mut sub_clients = HashMap::new();
        let sub_token = Token(2);
        sub_clients.insert(sub_token, sub_replica_conn);
        let mut sub_write_tokens = HashSet::new();

        propagate_writes_to_replicas(
            &mut sub_clients,
            &mut replica_rt,
            &mut poll,
            &mut sub_write_tokens,
        );

        let sub_conn = sub_clients.get(&sub_token).unwrap();
        let expected_bytes = encode_aof_stream(&[AofRecord {
            argv: vec![b"SET".to_vec(), b"foo".to_vec(), b"bar".to_vec()],
        }]);

        assert_eq!(sub_conn.write_buf, expected_bytes);
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
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);

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
        assert!(conn.try_flush().unwrap());

        let mut response = [0_u8; 5];
        std::io::Read::read_exact(&mut server_stream, &mut response).unwrap();
        assert_eq!(response, *b"+OK\r\n");
        assert!(conn.read_buf.is_empty());
        assert!(!runtime.is_client_paused(ts + 1));
    }

    #[test]
    fn client_pause_blocks_simple_string_command_frames() {
        use crate::ClientConnection;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::HashSet;
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();
        let ts = 5;
        let session = runtime.new_session();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (_server_stream, _server_addr) = listener.accept().unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);

        let pause = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"CLIENT".to_vec())),
            RespFrame::BulkString(Some(b"PAUSE".to_vec())),
            RespFrame::BulkString(Some(b"1000".to_vec())),
            RespFrame::BulkString(Some(b"ALL".to_vec())),
        ]));
        assert_eq!(
            runtime.execute_frame(pause, ts),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(runtime.is_client_paused(ts + 1));

        let ping = RespFrame::Array(Some(vec![RespFrame::SimpleString("PING".to_string())]));
        conn.read_buf.extend_from_slice(&ping.to_bytes());

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

        assert!(paused_tokens.contains(&Token(1)));
        assert!(conn.write_buf.is_empty());
        assert!(!conn.read_buf.is_empty());
    }

    #[test]
    fn subscription_gate_rejects_simple_string_commands() {
        use crate::ClientConnection;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::HashSet;
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();
        let ts = 10;
        let session = runtime.new_session();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (_server_stream, _server_addr) = listener.accept().unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);

        let subscribe = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"SUBSCRIBE".to_vec())),
            RespFrame::BulkString(Some(b"chan".to_vec())),
        ]));
        let prev = runtime.swap_session(std::mem::take(&mut conn.session));
        let _subscribe_reply = runtime.execute_frame(subscribe, ts);
        conn.session = runtime.swap_session(prev);

        let set_frame = RespFrame::Array(Some(vec![
            RespFrame::SimpleString("SET".to_string()),
            RespFrame::SimpleString("k".to_string()),
            RespFrame::SimpleString("v".to_string()),
        ]));
        conn.read_buf.extend_from_slice(&set_frame.to_bytes());

        let mut blocked_tokens = HashSet::new();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let prev = runtime.swap_session(std::mem::take(&mut conn.session));
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
        conn.session = runtime.swap_session(prev);

        let parsed = fr_protocol::parse_frame(&conn.write_buf).expect("parse reply");
        if let RespFrame::Error(msg) = parsed.frame {
            assert!(msg.contains("SET"), "unexpected error: {msg}");
        } else {
            assert!(
                matches!(parsed.frame, RespFrame::Error(_)),
                "expected error reply"
            );
        }
    }

    #[test]
    fn simple_string_quit_closes_connection() {
        use crate::ClientConnection;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::HashSet;
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();
        let ts = 0;
        let session = runtime.new_session();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (_server_stream, _server_addr) = listener.accept().unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);

        let quit = RespFrame::Array(Some(vec![RespFrame::SimpleString("QUIT".to_string())]));
        conn.read_buf.extend_from_slice(&quit.to_bytes());

        let mut blocked_tokens = HashSet::new();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let prev = runtime.swap_session(std::mem::take(&mut conn.session));
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts,
        );
        conn.session = runtime.swap_session(prev);

        assert!(conn.closing);
        assert!(closing_tokens.contains(&Token(1)));
    }

    #[test]
    fn output_buffer_limit_is_enforced_after_appending_current_response() {
        use crate::ClientConnection;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::HashSet;
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();
        let ts = 0;
        runtime.server.output_buffer_limit = 0;
        let session = runtime.new_session();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (_server_stream, _server_addr) = listener.accept().unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);
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
            ts,
        );

        assert!(conn.closing);
        assert!(closing_tokens.contains(&Token(1)));
    }

    #[test]
    fn client_unblock_error_mode_unblocks_blocked_connection() {
        use crate::{BlockedState, BlockingOp, ClientConnection};
        use mio::{Poll, Token};
        use std::collections::{HashMap, HashSet};
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();

        let blocked_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let blocked_addr = blocked_listener.local_addr().unwrap();
        let blocked_stream = TcpStream::connect(blocked_addr).unwrap();
        let (mut blocked_peer, _) = blocked_listener.accept().unwrap();
        blocked_peer
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();

        let requester_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let requester_addr = requester_listener.local_addr().unwrap();
        let requester_stream = TcpStream::connect(requester_addr).unwrap();
        let (mut requester_peer, _) = requester_listener.accept().unwrap();
        requester_peer
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();

        let blocked_token = Token(1); // ubs:ignore
        let blocked_session = runtime.new_session();
        let blocked_client_id = blocked_session.client_id;
        let mut blocked_conn = ClientConnection::new(
            mio::net::TcpStream::from_std(blocked_stream),
            blocked_session,
            0,
        );
        blocked_conn.blocked = Some(BlockedState {
            op: BlockingOp::BLpop {
                keys: vec![b"queue".to_vec()],
            },
            deadline_ms: u64::MAX,
        });

        runtime.mark_client_blocked(blocked_client_id);

        let requester_session = runtime.new_session();
        let mut requester_conn = ClientConnection::new(
            mio::net::TcpStream::from_std(requester_stream),
            requester_session,
            0,
        );
        requester_conn.read_buf.extend_from_slice(
            &RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"CLIENT".to_vec())),
                RespFrame::BulkString(Some(b"UNBLOCK".to_vec())),
                RespFrame::BulkString(Some(blocked_client_id.to_string().into_bytes())),
                RespFrame::BulkString(Some(b"ERROR".to_vec())),
            ]))
            .to_bytes(),
        );

        let mut blocked_tokens = HashSet::from([blocked_token]);
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();

        crate::process_buffered_frames(
            Token(2),
            &mut requester_conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            1,
        );
        assert!(requester_conn.try_flush().unwrap());
        let mut requester_reply = [0_u8; 4];
        std::io::Read::read_exact(&mut requester_peer, &mut requester_reply).unwrap();
        assert_eq!(requester_reply, *b":1\r\n");

        let mut clients = HashMap::from([(blocked_token, blocked_conn)]);
        let client_id_to_token = HashMap::from([(blocked_client_id, blocked_token)]); // ubs:ignore
        let mut poll = Poll::new().unwrap();

        apply_pending_client_unblocks(PendingClientUnblocksContext {
            clients: &mut clients,
            client_id_to_token: &client_id_to_token,
            blocked_tokens: &mut blocked_tokens,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            ts: 2,
        });
        let blocked_conn = clients.get_mut(&blocked_token).unwrap();
        assert!(blocked_conn.try_flush().unwrap());

        let blocked_conn = clients.get(&blocked_token).unwrap();
        assert!(blocked_conn.blocked.is_none());
        assert!(!blocked_tokens.contains(&blocked_token));
        assert!(
            !runtime
                .server
                .blocked_client_ids
                .contains(&blocked_client_id)
        );

        let mut blocked_reply =
            vec![
                0_u8;
                RespFrame::Error("UNBLOCKED client unblocked via CLIENT UNBLOCK".to_string())
                    .to_bytes()
                    .len()
            ];
        std::io::Read::read_exact(&mut blocked_peer, &mut blocked_reply).unwrap();
        assert_eq!(
            blocked_reply,
            RespFrame::Error("UNBLOCKED client unblocked via CLIENT UNBLOCK".to_string())
                .to_bytes()
        );
    }

    #[test]
    fn client_unblock_tracks_paused_tokens_for_pipelined_commands() {
        use crate::{BlockedState, BlockingOp, ClientConnection};
        use mio::{Poll, Token};
        use std::collections::{HashMap, HashSet};
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();

        let blocked_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let blocked_addr = blocked_listener.local_addr().unwrap();
        let blocked_stream = TcpStream::connect(blocked_addr).unwrap();
        let (mut blocked_peer, _) = blocked_listener.accept().unwrap();
        blocked_peer
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();

        let blocked_token = Token(1); // ubs:ignore
        let blocked_session = runtime.new_session();
        let blocked_client_id = blocked_session.client_id;
        let mut blocked_conn = ClientConnection::new(
            mio::net::TcpStream::from_std(blocked_stream),
            blocked_session,
            0,
        );
        blocked_conn.blocked = Some(BlockedState {
            op: BlockingOp::BLpop {
                keys: vec![b"queue".to_vec()],
            },
            deadline_ms: u64::MAX,
        });
        blocked_conn.read_buf.extend_from_slice(
            &RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"GET".to_vec())),
                RespFrame::BulkString(Some(b"after".to_vec())),
            ]))
            .to_bytes(),
        );

        runtime.mark_client_blocked(blocked_client_id);
        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"CLIENT".to_vec())),
                    RespFrame::BulkString(Some(b"PAUSE".to_vec())),
                    RespFrame::BulkString(Some(b"1000".to_vec())),
                    RespFrame::BulkString(Some(b"ALL".to_vec())),
                ])),
                10,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(runtime.is_client_paused(11));

        let requester = runtime.new_session();
        let previous = runtime.swap_session(requester);
        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"CLIENT".to_vec())),
                    RespFrame::BulkString(Some(b"UNBLOCK".to_vec())),
                    RespFrame::BulkString(Some(blocked_client_id.to_string().into_bytes())),
                    RespFrame::BulkString(Some(b"ERROR".to_vec())),
                ])),
                11,
            ),
            RespFrame::Integer(1)
        );
        let _ = runtime.swap_session(previous);

        let mut clients = HashMap::from([(blocked_token, blocked_conn)]);
        let client_id_to_token = HashMap::from([(blocked_client_id, blocked_token)]); // ubs:ignore
        let mut blocked_tokens = HashSet::from([blocked_token]);
        let mut closing_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut poll = Poll::new().unwrap();

        apply_pending_client_unblocks(PendingClientUnblocksContext {
            clients: &mut clients,
            client_id_to_token: &client_id_to_token,
            blocked_tokens: &mut blocked_tokens,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            ts: 11,
        });
        let blocked_conn = clients.get_mut(&blocked_token).unwrap();
        assert!(blocked_conn.try_flush().unwrap());

        let blocked_conn = clients.get(&blocked_token).unwrap();
        assert!(blocked_conn.blocked.is_none());
        assert!(!blocked_tokens.contains(&blocked_token));
        assert!(paused_tokens.contains(&blocked_token));
        assert!(!blocked_conn.read_buf.is_empty());

        let mut blocked_reply =
            vec![
                0_u8;
                RespFrame::Error("UNBLOCKED client unblocked via CLIENT UNBLOCK".to_string())
                    .to_bytes()
                    .len()
            ];
        std::io::Read::read_exact(&mut blocked_peer, &mut blocked_reply).unwrap();
        assert_eq!(
            blocked_reply,
            RespFrame::Error("UNBLOCKED client unblocked via CLIENT UNBLOCK".to_string())
                .to_bytes()
        );
    }

    #[test]
    fn blocked_client_timeout_tracks_paused_tokens_for_pipelined_commands() {
        use crate::{BlockedState, BlockingOp, ClientConnection};
        use mio::{Poll, Token};
        use std::collections::{HashMap, HashSet};
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();

        let blocked_token = Token(1); // ubs:ignore
        let blocked_session = runtime.new_session();
        let blocked_client_id = blocked_session.client_id;
        let mut blocked_conn =
            ClientConnection::new(mio::net::TcpStream::from_std(stream), blocked_session, 0);
        blocked_conn.blocked = Some(BlockedState {
            op: BlockingOp::BLpop {
                keys: vec![b"queue".to_vec()],
            },
            deadline_ms: 20,
        });
        blocked_conn.read_buf.extend_from_slice(
            &RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"GET".to_vec())),
                RespFrame::BulkString(Some(b"after".to_vec())),
            ]))
            .to_bytes(),
        );

        runtime.mark_client_blocked(blocked_client_id);
        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"CLIENT".to_vec())),
                    RespFrame::BulkString(Some(b"PAUSE".to_vec())),
                    RespFrame::BulkString(Some(b"1000".to_vec())),
                    RespFrame::BulkString(Some(b"ALL".to_vec())),
                ])),
                10,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(runtime.is_client_paused(21));

        let mut clients = HashMap::from([(blocked_token, blocked_conn)]);
        let mut blocked_tokens = HashSet::from([blocked_token]);
        let mut closing_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut poll = Poll::new().unwrap();

        check_blocked_clients(CheckBlockedClientsContext {
            clients: &mut clients,
            blocked_tokens: &mut blocked_tokens,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            ts: 21,
        });
        let blocked_conn = clients.get_mut(&blocked_token).unwrap();
        assert!(blocked_conn.try_flush().unwrap());

        let blocked_conn = clients.get(&blocked_token).unwrap();
        assert!(blocked_conn.blocked.is_none());
        assert!(!blocked_tokens.contains(&blocked_token));
        assert!(paused_tokens.contains(&blocked_token));
        assert!(!blocked_conn.read_buf.is_empty());
        assert!(
            !runtime
                .server
                .blocked_client_ids
                .contains(&blocked_client_id)
        );

        let mut blocked_reply = vec![0_u8; RespFrame::Array(None).to_bytes().len()];
        std::io::Read::read_exact(&mut peer, &mut blocked_reply).unwrap();
        assert_eq!(blocked_reply, RespFrame::Array(None).to_bytes());
    }
}
