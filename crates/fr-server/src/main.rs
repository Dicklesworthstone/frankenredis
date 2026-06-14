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

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream as StdTcpStream};
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use fr_command::pubsub_message_to_frame_for_protocol;
use fr_config::{RuntimePolicy, parse_redis_config_bytes};
use fr_eventloop::{
    EventLoopMode, TickBudget, plan_tick, validate_accept_path, validate_read_path,
};
use fr_protocol::{BorrowedCommandArgsKind, ParserConfig, RespFrame, RespParseError};
use fr_repl::ReplOffset;
use fr_runtime::{
    ClientSession, ClientUnblockMode, PlainCardinalityCmd, PlainKeyMetaCmd, PlainKeyedPopCmd,
    PlainKeyedValuesCmd, PlainRankCmd, Runtime,
};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};

/// Default port matching Redis convention.
const DEFAULT_PORT: u16 = 6379;
const DEFAULT_MODE: &str = "strict";

/// (frankenredis-jd75g) Tokens `0..MAX_LISTENERS` are reserved for listening
/// sockets (one per bind address, mirroring redis CONFIG_BINDADDR_MAX); client
/// connection handles start at `MAX_LISTENERS`. Lets CONFIG SET bind rebind a
/// multi-address listener set without colliding with client tokens.
const MAX_LISTENERS: usize = 16;
const WRITER_WAKE_TOKEN: Token = Token(usize::MAX);
const WRITER_POOL_WORKERS: usize = 2;
const WRITER_QUEUE_BOUND: usize = 1024;

const REPLICA_ACK_INTERVAL_MS: u64 = 1_000;
const REPLICA_RECONNECT_BACKOFF_MS: u64 = 250;
const MAX_FRAMES_PER_CLIENT_TICK: usize = 4096;

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
    /// WAITAOF: wait for local and/or replica fsync thresholds.
    Waitaof { argv: Vec<Vec<u8>> },
    /// WAIT: wait for replica ACK count to reach required threshold.
    Wait { argv: Vec<Vec<u8>> },
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
            BlockingOp::Waitaof { .. } | BlockingOp::Wait { .. } => Vec::new(),
        }
    }

    /// True if any key this op is waiting on is present in `ready`, computed
    /// WITHOUT allocating. This is the per-tick hot path in
    /// `check_blocked_clients`: previously it called `keys()`, which deep-
    /// clones every waited-on key (`Vec<Vec<u8>>`) for every blocked client
    /// on every event-loop tick, just to test membership. Iterating the
    /// borrowed key slices and probing the `ready` set directly is byte-for-
    /// byte equivalent (same keys, same membership) with zero allocation.
    fn any_key_ready(&self, ready: &HashSet<Vec<u8>>) -> bool {
        match self {
            BlockingOp::BLpop { keys }
            | BlockingOp::BRpop { keys }
            | BlockingOp::BZpopMax { keys }
            | BlockingOp::BZpopMin { keys } => keys.iter().any(|k| ready.contains(k)),
            BlockingOp::BLmove { source, .. } => ready.contains(source),
            BlockingOp::BLmpop { argv } | BlockingOp::BZmpop { argv } => {
                if argv.len() < 3 {
                    return false;
                }
                let num_keys: usize = std::str::from_utf8(&argv[2])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                argv.iter()
                    .skip(3)
                    .take(num_keys)
                    .any(|k| ready.contains(k))
            }
            BlockingOp::BXread { argv } | BlockingOp::BXreadgroup { argv } => {
                let streams_idx = argv.iter().position(|a| a.eq_ignore_ascii_case(b"STREAMS"));
                if let Some(idx) = streams_idx {
                    let remaining = &argv[idx + 1..];
                    let num_keys = remaining.len() / 2;
                    remaining.iter().take(num_keys).any(|k| ready.contains(k))
                } else {
                    false
                }
            }
            BlockingOp::Waitaof { .. } | BlockingOp::Wait { .. } => false,
        }
    }
}

/// A client that is blocked waiting for data on one or more keys.
struct BlockedState {
    op: BlockingOp,
    /// Absolute timestamp (ms) when the block expires. `u64::MAX` = no timeout.
    deadline_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockedWakeRef {
    seq: u64,
    token: Token,
}

#[derive(Debug)]
struct BlockedWakeRegistration {
    seq: u64,
    deadline_ms: u64,
    is_wait: bool,
}

/// Advisory wake index for blocked clients.
///
/// `blocked_tokens` and `conn.blocked` remain authoritative. This sidecar only
/// narrows which tokens `check_blocked_clients` visits on a tick; every
/// candidate is revalidated against the current connection state before a
/// timeout or key-ready wake is applied.
#[derive(Debug, Default)]
struct BlockedWakeIndex {
    next_seq: u64,
    by_key: HashMap<Vec<u8>, VecDeque<BlockedWakeRef>>,
    timeouts: BinaryHeap<Reverse<(u64, u64, usize)>>,
    waiters: VecDeque<BlockedWakeRef>,
    live: HashMap<Token, BlockedWakeRegistration>,
}

impl BlockedWakeIndex {
    fn insert(&mut self, token: Token, blocked: &BlockedState) {
        self.remove(token);
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        let seq = self.next_seq;
        let wake_ref = BlockedWakeRef { seq, token };
        for key in blocked.op.keys() {
            self.by_key.entry(key).or_default().push_back(wake_ref);
        }
        if blocked.deadline_ms != u64::MAX {
            // ubs:ignore
            self.timeouts
                .push(Reverse((blocked.deadline_ms, seq, token.0)));
        }
        let is_wait = matches!(
            blocked.op,
            BlockingOp::Waitaof { .. } | BlockingOp::Wait { .. }
        );
        if is_wait {
            self.waiters.push_back(wake_ref);
        }
        self.live.insert(
            token,
            BlockedWakeRegistration {
                seq,
                deadline_ms: blocked.deadline_ms,
                is_wait,
            },
        );
    }

    fn remove(&mut self, token: Token) {
        self.live.remove(&token);
    }

    fn clear(&mut self) {
        self.by_key.clear();
        self.timeouts.clear();
        self.waiters.clear();
        self.live.clear();
    }

    fn is_live_ref(&self, wake_ref: BlockedWakeRef) -> bool {
        self.live
            .get(&wake_ref.token)
            .is_some_and(|registration| registration.seq == wake_ref.seq) // ubs:ignore
    }

    fn candidates(&mut self, ready_keys: &HashSet<Vec<u8>>, now_ms: u64) -> Vec<Token> {
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();

        let live = &self.live;
        for key in ready_keys {
            if let Some(queue) = self.by_key.get_mut(key) {
                while queue.front().is_some_and(|wake_ref| {
                    live.get(&wake_ref.token)
                        .is_none_or(|registration| registration.seq != wake_ref.seq) // ubs:ignore
                }) {
                    queue.pop_front();
                }
                for wake_ref in queue.iter().copied() {
                    if live
                        .get(&wake_ref.token)
                        .is_some_and(|registration| registration.seq == wake_ref.seq) // ubs:ignore
                        && seen.insert(wake_ref.token)
                    {
                        candidates.push(wake_ref.token);
                    }
                }
            }
        }

        while let Some(Reverse((deadline_ms, seq, token_raw))) = self.timeouts.peek().copied() {
            if deadline_ms > now_ms {
                break;
            }
            self.timeouts.pop();
            let token = Token(token_raw);
            if self.live.get(&token).is_some_and(|registration| {
                registration.seq == seq && registration.deadline_ms == deadline_ms // ubs:ignore
            }) && seen.insert(token)
            {
                candidates.push(token);
            }
        }

        while self
            .waiters
            .front()
            .is_some_and(|wake_ref| !self.is_live_ref(*wake_ref))
        {
            self.waiters.pop_front();
        }
        for wake_ref in self.waiters.iter().copied() {
            if self.live.get(&wake_ref.token).is_some_and(|registration| {
                registration.seq == wake_ref.seq && registration.is_wait // ubs:ignore
            }) && seen.insert(wake_ref.token)
            {
                candidates.push(wake_ref.token);
            }
        }

        candidates
    }
}

/// Per-client connection state.
struct ClientConnection {
    stream: TcpStream,
    writer_stream: Option<StdTcpStream>,
    writer_in_flight_bytes: usize,
    write_failed: bool,
    session: ClientSession,
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
    main_writable_armed: bool,
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
    #[cfg(test)]
    fn new(stream: TcpStream, session: ClientSession, now_ms: u64) -> Self {
        Self::new_with_writer(stream, None, session, now_ms)
    }

    fn new_with_writer(
        stream: TcpStream,
        writer_stream: Option<StdTcpStream>,
        mut session: ClientSession,
        now_ms: u64,
    ) -> Self {
        session.connected_at_ms = now_ms;
        session.last_interaction_ms = now_ms;
        Self {
            stream,
            writer_stream,
            writer_in_flight_bytes: 0,
            write_failed: false,
            session,
            read_buf: Vec::with_capacity(4096),
            write_buf: Vec::new(),
            main_writable_armed: false,
            closing: false,
            blocked: None,
            replication_sent_offset: None,
        }
    }

    fn writer_in_flight(&self) -> bool {
        self.writer_in_flight_bytes > 0
    }

    fn pending_output_bytes(&self) -> usize {
        self.write_buf
            .len()
            .saturating_add(self.writer_in_flight_bytes)
    }

    fn has_pending_output(&self) -> bool {
        self.pending_output_bytes() > 0
    }

    fn output_drained_or_failed(&self) -> bool {
        self.write_failed || !self.has_pending_output()
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

struct WriterJob {
    token: Token,
    stream: StdTcpStream,
    bytes: Vec<u8>,
}

struct WriterCompletion {
    token: Token,
    stream: StdTcpStream,
    bytes: Vec<u8>,
    status: WriterCompletionStatus,
}

enum WriterCompletionStatus {
    Drained,
    WouldBlock,
    Failed(io::Error),
}

struct WriterPool {
    jobs: mpsc::SyncSender<WriterJob>,
    completions: mpsc::Receiver<WriterCompletion>,
}

impl WriterPool {
    fn new(poll: &Poll) -> io::Result<Self> {
        let (job_tx, job_rx) = mpsc::sync_channel(WRITER_QUEUE_BOUND);
        let (completion_tx, completion_rx) = mpsc::channel();
        let shared_rx = Arc::new(Mutex::new(job_rx));
        let waker = Arc::new(Waker::new(poll.registry(), WRITER_WAKE_TOKEN)?);

        for worker_idx in 0..WRITER_POOL_WORKERS {
            let rx = Arc::clone(&shared_rx);
            let tx = completion_tx.clone();
            let wake = Arc::clone(&waker);
            thread::Builder::new()
                .name(format!("fr-writer-{worker_idx}"))
                .spawn(move || {
                    loop {
                        let recv_result = {
                            let Ok(receiver) = rx.lock() else {
                                return;
                            };
                            receiver.recv()
                        };
                        let Ok(job) = recv_result else {
                            return;
                        };
                        let completion = flush_writer_job(job);
                        if tx.send(completion).is_err() {
                            return;
                        }
                        let _ = wake.wake();
                    }
                })?;
        }

        Ok(Self {
            jobs: job_tx,
            completions: completion_rx,
        })
    }

    fn try_enqueue(
        &self,
        token: Token,
        stream: StdTcpStream,
        bytes: Vec<u8>,
    ) -> Result<(), mpsc::TrySendError<WriterJob>> {
        self.jobs.try_send(WriterJob {
            token,
            stream,
            bytes,
        })
    }

    fn try_recv(&self) -> Result<WriterCompletion, mpsc::TryRecvError> {
        self.completions.try_recv()
    }
}

fn flush_writer_job(job: WriterJob) -> WriterCompletion {
    let WriterJob {
        token,
        mut stream,
        mut bytes,
    } = job;
    let mut total_written = 0usize;
    let mut status = WriterCompletionStatus::Drained;

    while total_written < bytes.len() {
        match stream.write(&bytes[total_written..]) {
            Ok(0) => {
                status = WriterCompletionStatus::Failed(io::Error::new(
                    ErrorKind::WriteZero,
                    "write zero",
                ));
                break;
            }
            Ok(n) => {
                total_written += n;
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                status = WriterCompletionStatus::WouldBlock;
                break;
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                status = WriterCompletionStatus::Failed(e);
                break;
            }
        }
    }

    if total_written > 0 {
        bytes.drain(..total_written);
    }
    if bytes.is_empty() && !matches!(status, WriterCompletionStatus::Failed(_)) {
        status = WriterCompletionStatus::Drained;
    }

    WriterCompletion {
        token,
        stream,
        bytes,
        status,
    }
}

#[cfg(unix)]
fn clone_writer_stream(stream: &TcpStream) -> io::Result<StdTcpStream> {
    let owned_fd = stream.as_fd().try_clone_to_owned()?;
    let writer_stream = StdTcpStream::from(owned_fd);
    writer_stream.set_nonblocking(true)?;
    writer_stream.set_nodelay(true)?;
    Ok(writer_stream)
}

#[cfg(not(unix))]
fn clone_writer_stream(_stream: &TcpStream) -> io::Result<StdTcpStream> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "writer handoff requires safe fd cloning",
    ))
}

#[derive(Clone, Copy)]
struct UnixTime {
    ms: u64,
    us: u64,
}

fn now_unix_time() -> UnixTime {
    let us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    UnixTime { ms: us / 1000, us }
}

fn now_ms() -> u64 {
    now_unix_time().ms
}

fn server_help_text() -> String {
    format!(
        "frankenredis — FrankenRedis server\n\n\
USAGE: frankenredis [OPTIONS]\n\n\
OPTIONS:\n\
  --bind <ADDR>              Listen address (default: 127.0.0.1)\n\
  --port <PORT>              Listen port (default: {DEFAULT_PORT})\n\
  --mode <MODE>              Runtime mode: strict or hardened (default: {DEFAULT_MODE})\n\
  --sentinel                 Run in Sentinel mode (enables SENTINEL command dispatch)\n\
  --config <PATH>            Load redis.conf startup directives and use path for CONFIG REWRITE\n\
  --aof <PATH>               AOF persistence file path (enables persistence)\n\
  --rdb <PATH>               RDB snapshot file path (enables SAVE/BGSAVE snapshots)\n\
  --replicaof <HOST> <PORT>  Configure this server as a replica of the given primary\n\
  --masteruser <USERNAME>    Authenticate to the configured primary as this ACL user\n\
  --masterauth <PASSWORD>    Authenticate to the configured primary with this password\n\
  --enable-debug-command <VALUE>  Allow DEBUG commands: no | local | yes (default: no, matches upstream Redis 7.2)\n\
  --help                     Show this help\n"
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct StartupConfig {
    bind_addr: Option<String>,
    port: Option<u16>,
    requirepass: Option<Option<Vec<u8>>>,
    masteruser: Option<Option<String>>,
    masterauth: Option<Option<String>>,
    replicaof: Option<Option<(String, u16)>>,
    dir: Option<String>,
    dbfilename: Option<String>,
    appendonly: Option<bool>,
    appenddirname: Option<String>,
    appendfilename: Option<String>,
    aclfile: Option<String>,
    enable_debug_command: Option<String>,
}

impl StartupConfig {
    fn configured_rdb_path(&self) -> Option<String> {
        if self.dir.is_none() && self.dbfilename.is_none() {
            return None;
        }
        let dir = self.dir.as_deref().unwrap_or(".");
        let filename = self.dbfilename.as_deref().unwrap_or("dump.rdb");
        Some(
            std::path::PathBuf::from(dir)
                .join(filename)
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn configured_aof_path(&self) -> Option<String> {
        if self.appendonly != Some(true) {
            return None;
        }
        let dir = self.dir.as_deref().unwrap_or(".");
        let dirname = self.appenddirname.as_deref().unwrap_or("appendonlydir");
        let filename = self.appendfilename.as_deref().unwrap_or("appendonly.aof");
        Some(
            std::path::PathBuf::from(dir)
                .join(dirname)
                .join(filename)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

fn load_startup_config_file(path: &str) -> Result<StartupConfig, String> {
    let input =
        std::fs::read(path).map_err(|err| format!("failed to read config file '{path}': {err}"))?;
    let parsed = parse_redis_config_bytes(&input)
        .map_err(|err| format!("failed to parse config file '{path}': {err}"))?;
    startup_config_from_directives(&parsed.directives)
}

fn startup_config_from_directives(
    directives: &[fr_config::ParsedConfigDirective],
) -> Result<StartupConfig, String> {
    let mut config = StartupConfig::default();

    for directive in directives {
        match directive.name.as_slice() {
            b"bind" => {
                if directive.args.is_empty() {
                    return Err(config_directive_error(
                        directive,
                        "bind requires at least one address",
                    ));
                }
                config.bind_addr = Some(config_arg_string(directive, 0)?);
            }
            b"port" => {
                expect_config_arg_count(directive, 1)?;
                config.port = Some(config_arg_port(directive, 0)?);
            }
            b"requirepass" => {
                expect_config_arg_count(directive, 1)?;
                config.requirepass = Some(if directive.args[0].is_empty() {
                    None
                } else {
                    Some(directive.args[0].clone())
                });
            }
            b"masteruser" => {
                expect_config_arg_count(directive, 1)?;
                config.masteruser = Some(non_empty_config_arg_string(directive, 0)?);
            }
            b"masterauth" => {
                expect_config_arg_count(directive, 1)?;
                config.masterauth = Some(non_empty_config_arg_string(directive, 0)?);
            }
            b"replicaof" | b"slaveof" => {
                expect_config_arg_count(directive, 2)?;
                let host = config_arg_string(directive, 0)?;
                let port_text = config_arg_string(directive, 1)?;
                if host.eq_ignore_ascii_case("no") && port_text.eq_ignore_ascii_case("one") {
                    config.replicaof = Some(None);
                } else {
                    let port = config_arg_port(directive, 1)?;
                    config.replicaof = Some(Some((host, port)));
                }
            }
            b"dir" => {
                expect_config_arg_count(directive, 1)?;
                config.dir = Some(config_arg_string(directive, 0)?);
            }
            b"dbfilename" => {
                expect_config_arg_count(directive, 1)?;
                config.dbfilename = Some(config_arg_string(directive, 0)?);
            }
            b"appendonly" => {
                expect_config_arg_count(directive, 1)?;
                config.appendonly = Some(config_arg_bool(directive, 0)?);
            }
            b"appenddirname" => {
                expect_config_arg_count(directive, 1)?;
                config.appenddirname = Some(config_arg_string(directive, 0)?);
            }
            b"appendfilename" => {
                expect_config_arg_count(directive, 1)?;
                config.appendfilename = Some(config_arg_string(directive, 0)?);
            }
            b"aclfile" => {
                expect_config_arg_count(directive, 1)?;
                if let Some(path) = non_empty_config_arg_string(directive, 0)? {
                    config.aclfile = Some(path);
                }
            }
            b"enable-debug-command" => {
                expect_config_arg_count(directive, 1)?;
                config.enable_debug_command = Some(config_arg_string(directive, 0)?);
            }
            _ => {}
        }
    }

    Ok(config)
}

fn expect_config_arg_count(
    directive: &fr_config::ParsedConfigDirective,
    expected: usize,
) -> Result<(), String> {
    if directive.args.len() == expected {
        return Ok(());
    }
    Err(config_directive_error(
        directive,
        &format!(
            "expected {expected} argument(s), got {}",
            directive.args.len()
        ),
    ))
}

fn config_arg_string(
    directive: &fr_config::ParsedConfigDirective,
    index: usize,
) -> Result<String, String> {
    String::from_utf8(directive.args[index].clone()).map_err(|_| {
        config_directive_error(
            directive,
            &format!("argument {} must be valid UTF-8", index + 1),
        )
    })
}

fn non_empty_config_arg_string(
    directive: &fr_config::ParsedConfigDirective,
    index: usize,
) -> Result<Option<String>, String> {
    if directive.args[index].is_empty() {
        Ok(None)
    } else {
        Ok(Some(config_arg_string(directive, index)?))
    }
}

fn config_arg_port(
    directive: &fr_config::ParsedConfigDirective,
    index: usize,
) -> Result<u16, String> {
    let value = config_arg_string(directive, index)?;
    value.parse::<u16>().map_err(|_| {
        config_directive_error(
            directive,
            &format!("argument {} must be a TCP port", index + 1),
        )
    })
}

fn config_arg_bool(
    directive: &fr_config::ParsedConfigDirective,
    index: usize,
) -> Result<bool, String> {
    let value = config_arg_string(directive, index)?;
    if value.eq_ignore_ascii_case("yes") {
        Ok(true)
    } else if value.eq_ignore_ascii_case("no") {
        Ok(false)
    } else {
        Err(config_directive_error(
            directive,
            &format!("argument {} must be yes or no", index + 1),
        ))
    }
}

fn config_directive_error(directive: &fr_config::ParsedConfigDirective, message: &str) -> String {
    format!(
        "invalid config directive '{}' on line {}: {message}",
        String::from_utf8_lossy(&directive.name),
        directive.line_number
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    let mut port = DEFAULT_PORT;
    let mut mode_str = DEFAULT_MODE;
    let mut bind_addr = "127.0.0.1".to_string();
    let mut aof_path: Option<String> = None;
    let mut rdb_path: Option<String> = None;
    let mut config_path: Option<String> = None;
    let mut replicaof: Option<(String, u16)> = None;
    let mut masteruser: Option<String> = None;
    let mut masterauth: Option<String> = None;
    let mut cli_port = false;
    let mut cli_bind_addr = false;
    let mut cli_replicaof = false;
    let mut cli_masteruser = false;
    let mut cli_masterauth = false;
    let mut cli_aof = false;
    let mut cli_rdb = false;
    let mut cli_enable_debug_command: Option<String> = None;
    let mut sentinel_mode = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                cli_port = true;
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
                cli_bind_addr = true;
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
            "--sentinel" => {
                sentinel_mode = true;
            }
            "--aof" => {
                cli_aof = true;
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --aof requires a file path");
                    return ExitCode::from(1);
                }
                aof_path = Some(args[i].clone());
            }
            "--rdb" => {
                cli_rdb = true;
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
                cli_replicaof = true;
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
                cli_masteruser = true;
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --masteruser requires a username");
                    return ExitCode::from(1);
                }
                masteruser = Some(args[i].clone());
            }
            "--masterauth" => {
                cli_masterauth = true;
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --masterauth requires a password");
                    return ExitCode::from(1);
                }
                masterauth = Some(args[i].clone());
            }
            "--enable-debug-command" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --enable-debug-command requires a value (no, local, yes)");
                    return ExitCode::from(1);
                }
                cli_enable_debug_command = Some(args[i].clone());
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

    let mut requirepass = None;
    let mut aclfile_path = None;
    let mut config_enable_debug_command: Option<String> = None;
    if let Some(path) = &config_path {
        let startup_config = match load_startup_config_file(path) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::from(1);
            }
        };
        config_enable_debug_command = startup_config.enable_debug_command.clone();
        let config_rdb_path = startup_config.configured_rdb_path();
        let config_aof_path = startup_config.configured_aof_path();
        if !cli_bind_addr && let Some(config_bind_addr) = startup_config.bind_addr {
            bind_addr = config_bind_addr;
        }
        if !cli_port && let Some(config_port) = startup_config.port {
            port = config_port;
        }
        if !cli_replicaof && let Some(config_replicaof) = startup_config.replicaof {
            replicaof = config_replicaof;
        }
        if !cli_masteruser && let Some(config_masteruser) = startup_config.masteruser {
            masteruser = config_masteruser;
        }
        if !cli_masterauth && let Some(config_masterauth) = startup_config.masterauth {
            masterauth = config_masterauth;
        }
        if !cli_rdb && let Some(config_path) = config_rdb_path {
            rdb_path = Some(config_path);
        }
        if !cli_aof && let Some(config_path) = config_aof_path {
            aof_path = Some(config_path);
        }
        aclfile_path = startup_config.aclfile;
        requirepass = startup_config.requirepass;
    }

    let policy = match mode_str {
        "strict" => RuntimePolicy::default(),
        _ => RuntimePolicy::hardened(),
    };
    let mut runtime = Runtime::new(policy);
    runtime.set_server_port(port);
    // (frankenredis-zyx9q) Let the runtime's CONFIG SET port handler test-bind
    // the new port and signal a live listener rebind.
    runtime.set_bind_addr(bind_addr.clone());
    runtime.set_sentinel_mode(sentinel_mode);
    if sentinel_mode {
        // (frankenredis-pkdgs) Announce our listening port in hello messages so
        // peer sentinels discover us at the right address.
        runtime.set_sentinel_announce_port(port);
    }
    runtime.set_config_file_path(config_path.map(std::path::PathBuf::from));
    // CLI flag wins over config-file directive; both override the
    // runtime's "no" default which mirrors upstream Redis 7.2's
    // safe-by-default `enable-debug-command` behavior.
    // (br-frankenredis-j29y)
    if let Some(value) = cli_enable_debug_command
        .as_deref()
        .or(config_enable_debug_command.as_deref())
    {
        runtime.set_enable_debug_command(value);
    }
    if let Some(config_requirepass) = requirepass {
        runtime.set_requirepass(config_requirepass);
    }
    runtime.set_masteruser(masteruser.map(String::into_bytes));
    runtime.set_masterauth(masterauth.map(String::into_bytes));
    if let Some(path) = aclfile_path {
        runtime.set_acl_file_path(std::path::PathBuf::from(&path));
        let response = runtime.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"ACL".to_vec())),
                RespFrame::BulkString(Some(b"LOAD".to_vec())),
            ])),
            now_ms(),
        );
        match response {
            RespFrame::SimpleString(ref line) if line == "OK" => {
                eprintln!("ACL: loaded rules from {path}");
            }
            RespFrame::Error(err) => {
                eprintln!("error: failed to load aclfile '{path}': {err}");
                return ExitCode::from(1);
            }
            other => {
                eprintln!("error: unexpected ACL LOAD response during startup: {other:?}");
                return ExitCode::from(1);
            }
        }
    }
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

    let mut poll = match Poll::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: failed to create poll instance: {e}");
            return ExitCode::from(1);
        }
    };
    let writer_pool = match WriterPool::new(&poll) {
        Ok(pool) => Some(pool),
        Err(e) => {
            eprintln!("warn: writer handoff disabled: {e}");
            None
        }
    };

    // (frankenredis-jd75g) Bind one listener per configured address. Startup
    // binds the single configured bind address; CONFIG SET bind can later grow
    // this to a set of up to MAX_LISTENERS. cur_binds / cur_listen_port track
    // the live set so a CONFIG SET port or bind change can recompute it.
    let mut cur_binds: Vec<String> = vec![bind_addr.clone()];
    let mut cur_listen_port: u16 = port;
    let mut listeners: Vec<TcpListener> =
        match bind_and_register(&poll, &cur_binds, cur_listen_port) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(1);
            }
        };

    eprintln!(
        "FrankenRedis v{} ready (mode={mode_str}, port={port})",
        env!("CARGO_PKG_VERSION"),
    );

    let mut events = Events::with_capacity(1024);
    let mut clients: HashMap<Token, ClientConnection> = HashMap::new();
    let mut client_id_to_token: HashMap<u64, Token> = HashMap::new();
    let mut blocked_tokens: HashSet<Token> = HashSet::new();
    let mut blocked_wake_index = BlockedWakeIndex::default();
    let mut closing_tokens: HashSet<Token> = HashSet::new();
    let mut write_tokens: HashSet<Token> = HashSet::new();
    let mut paused_tokens: HashSet<Token> = HashSet::new();
    let mut deferred_tokens: HashSet<Token> = HashSet::new();
    let mut replica_sync = ReplicaSyncState::new();
    // (frankenredis-jd75g) Client handles start above the reserved listener
    // token range (0..MAX_LISTENERS).
    let mut next_handle: usize = MAX_LISTENERS;
    let tick_budget = TickBudget::default();
    let mut last_ops_sample_ms: u64 = now_ms();
    // (frankenredis-pkdgs) Last wall-clock ms a sentinel-mode INFO/PING probe of
    // the monitored masters ran. 0 = never, so the first tick probes immediately.
    let mut last_sentinel_probe_ms: u64 = 0;
    // (frankenredis-pkdgs) Per-master persistent __sentinel__:hello subscriptions
    // (sentinel mode only), drained each iteration to discover peer sentinels.
    let mut sentinel_hello_subs: HashMap<String, SentinelHelloSub> = HashMap::new();

    loop {
        // Use fr-eventloop's tick planner to determine poll timeout.
        let has_blocked = !blocked_tokens.is_empty();
        // (frankenredis) Clients deferred by CLIENT PAUSE must be released when
        // the pause deadline passes. No socket I/O is involved, so mio will not
        // wake us on its own — if we sleep for the full idle tick the paused
        // command hangs until unrelated traffic arrives. Bound the sleep like the
        // blocked case so the pause-expiry re-check below runs promptly.
        let has_paused = !paused_tokens.is_empty();
        let has_deferred = !deferred_tokens.is_empty() && !runtime.is_client_paused(now_ms());
        let pending_writes = write_tokens.len();
        let tick_plan = plan_tick(0, pending_writes, tick_budget, EventLoopMode::Normal);
        let poll_timeout = if tick_plan.poll_timeout_ms == 0 || has_deferred {
            Some(std::time::Duration::from_millis(0))
        } else if has_blocked || has_paused {
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

        let eventloop_start = std::time::Instant::now();
        let timestamp = now_unix_time();
        let ts = timestamp.ms;
        let ts_us = timestamp.us;

        drain_writer_completions(
            writer_pool.as_ref(),
            &mut clients,
            &mut runtime,
            &mut poll,
            &mut write_tokens,
            &mut closing_tokens,
        );

        for event in events.iter() {
            match event.token() {
                // (frankenredis-jd75g) Tokens 0..listeners.len() are listening
                // sockets; accept from the one that signalled readiness.
                listener_tok if listener_tok.0 < listeners.len() => {
                    accept_connections(
                        &listeners[listener_tok.0],
                        &mut poll,
                        &mut clients,
                        &mut client_id_to_token,
                        &mut next_handle,
                        &mut runtime,
                        writer_pool.is_some(),
                    );
                }
                token if token == WRITER_WAKE_TOKEN => {
                    drain_writer_completions(
                        writer_pool.as_ref(),
                        &mut clients,
                        &mut runtime,
                        &mut poll,
                        &mut write_tokens,
                        &mut closing_tokens,
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
                            &mut blocked_wake_index,
                            &mut closing_tokens,
                            &mut write_tokens,
                            &mut paused_tokens,
                            &mut deferred_tokens,
                            ts,
                            ts_us,
                            writer_pool.as_ref(),
                        );
                    }
                    if event.is_writable() {
                        handle_writable(
                            conn_handle,
                            &mut clients,
                            &mut runtime,
                            &mut write_tokens,
                            &mut closing_tokens,
                            &mut poll,
                            writer_pool.as_ref(),
                        );
                    }
                }
            }
        }
        drain_writer_completions(
            writer_pool.as_ref(),
            &mut clients,
            &mut runtime,
            &mut poll,
            &mut write_tokens,
            &mut closing_tokens,
        );

        // Run active expiry cycle once per tick (fast cycle).
        let _ = runtime.run_active_expire_cycle(ts, fr_eventloop::ActiveExpireCycleKind::Fast);

        // Sample instantaneous ops/sec and throughput once per tick.
        let elapsed = ts.saturating_sub(last_ops_sample_ms);
        if elapsed >= 100 {
            runtime.record_ops_sec_sample(elapsed);
            last_ops_sample_ms = ts;
        }

        // Check for completed background child processes
        runtime.check_child_processes(ts);

        // Drive the primary link from the main loop so replicas can sustain
        // online deltas, ACK traffic, and reconnect after link loss.
        drive_replica_sync(&mut runtime, &mut replica_sync, ts);

        apply_pending_client_unblocks(PendingClientUnblocksContext {
            clients: &mut clients,
            client_id_to_token: &client_id_to_token,
            blocked_tokens: &mut blocked_tokens,
            blocked_wake_index: &mut blocked_wake_index,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            deferred_tokens: &mut deferred_tokens,
            ts,
            writer_pool: writer_pool.as_ref(),
        });

        // Check blocked clients (BLPOP/BRPOP/BLMOVE) for available data
        // or timeout expiry.
        check_blocked_clients(CheckBlockedClientsContext {
            clients: &mut clients,
            blocked_tokens: &mut blocked_tokens,
            blocked_wake_index: &mut blocked_wake_index,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            deferred_tokens: &mut deferred_tokens,
            ts,
            writer_pool: writer_pool.as_ref(),
        });

        process_deferred_buffered_clients(DeferredBufferedClientsContext {
            clients: &mut clients,
            blocked_tokens: &mut blocked_tokens,
            blocked_wake_index: &mut blocked_wake_index,
            closing_tokens: &mut closing_tokens,
            write_tokens: &mut write_tokens,
            paused_tokens: &mut paused_tokens,
            deferred_tokens: &mut deferred_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            ts,
            ts_us,
            writer_pool: writer_pool.as_ref(),
        });

        // Re-process clients whose commands were deferred by CLIENT PAUSE once the
        // pause window expires (see release_expired_client_pause for why a direct
        // re-drive is required rather than a mio re-register).
        release_expired_client_pause(
            &mut clients,
            &mut runtime,
            &mut poll,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            &mut deferred_tokens,
            ts,
            ts_us,
            writer_pool.as_ref(),
        );

        // (frankenredis-pkdgs) In Sentinel mode, actively PING + INFO the
        // monitored masters so their runid / flags / ping & info-refresh times
        // fill in (otherwise SENTINEL MASTER reports an empty,
        // "master,disconnected" instance forever).
        run_sentinel_monitoring_tick(
            &mut runtime,
            ts,
            &mut last_sentinel_probe_ms,
            &mut sentinel_hello_subs,
        );

        // Deliver pending replication writes to connected replicas.
        propagate_writes_to_replicas(
            &mut clients,
            &mut runtime,
            &mut poll,
            &mut write_tokens,
            &mut closing_tokens,
            writer_pool.as_ref(),
        );

        // (frankenredis-ol9tz) Incrementally append this tick's captured AOF
        // records to the on-disk AOF file and fsync per appendfsync policy.
        // Without this, writes between full rewrites (SAVE/BGREWRITEAOF) were
        // never persisted and were lost on restart.
        runtime.flush_aof_to_disk(ts);

        // Deliver pending Pub/Sub messages to subscribed clients.
        deliver_pubsub_messages(
            &mut clients,
            &client_id_to_token,
            &mut runtime,
            &mut poll,
            &mut write_tokens,
            &mut closing_tokens,
            writer_pool.as_ref(),
        );

        // Deliver MONITOR output to monitor clients.
        deliver_monitor_output(
            &mut clients,
            &client_id_to_token,
            &mut runtime,
            &mut poll,
            &mut write_tokens,
            &mut closing_tokens,
            writer_pool.as_ref(),
        );

        drain_writer_completions(
            writer_pool.as_ref(),
            &mut clients,
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
                    .map(ClientConnection::output_drained_or_failed)
                    .unwrap_or(true)
            })
            .copied()
            .collect();
        for token in to_remove {
            if let Some(mut conn) = clients.remove(&token) {
                blocked_tokens.remove(&token);
                blocked_wake_index.remove(token);
                closing_tokens.remove(&token);
                write_tokens.remove(&token);
                paused_tokens.remove(&token);
                deferred_tokens.remove(&token);
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

        // (frankenredis-zyx9q / jd75g) Apply a pending CONFIG SET port or bind
        // change by rebinding the whole listener set. The runtime has already
        // test-bound the new addresses (so this should succeed); rebind_listeners
        // binds + registers the NEW set first and only swaps it in on success,
        // leaving the old set reachable on any rare TOCTOU failure. Existing
        // client connections (tokens >= MAX_LISTENERS) are untouched.
        if let Some(new_port) = runtime.take_pending_port_change()
            && rebind_listeners(
                &mut poll,
                &mut listeners,
                &cur_binds,
                cur_listen_port,
                &cur_binds.clone(),
                new_port,
            )
        {
            cur_listen_port = new_port;
        }
        if let Some(new_binds) = runtime.take_pending_bind_change()
            && rebind_listeners(
                &mut poll,
                &mut listeners,
                &cur_binds,
                cur_listen_port,
                &new_binds,
                cur_listen_port,
            )
        {
            cur_binds = new_binds;
        }

        let eventloop_duration_us =
            u64::try_from(eventloop_start.elapsed().as_micros()).unwrap_or(u64::MAX);
        runtime.record_eventloop_cycle(eventloop_duration_us);

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

/// (frankenredis-jd75g) Bind one TCP listener per address in `addrs` at `port`
/// and register each with the poll under its listener token (`Token(0..N)`),
/// mirroring redis's multi-address bind. Binds all first, then registers all,
/// so a mid-way failure cleans up fully and never disturbs the caller's
/// existing listeners. An empty `addrs` yields zero listeners (server listens
/// on nothing — matching redis `bind ""`).
fn bind_and_register(poll: &Poll, addrs: &[String], port: u16) -> Result<Vec<TcpListener>, String> {
    if addrs.len() > MAX_LISTENERS {
        return Err(format!(
            "too many bind addresses ({} > {MAX_LISTENERS})",
            addrs.len()
        ));
    }
    let mut listeners: Vec<TcpListener> = Vec::with_capacity(addrs.len());
    for a in addrs {
        let sa: SocketAddr = format!("{a}:{port}")
            .parse()
            .map_err(|e| format!("invalid bind address '{a}:{port}': {e}"))?;
        let listener = TcpListener::bind(sa).map_err(|e| format!("failed to bind to {sa}: {e}"))?;
        listeners.push(listener);
    }
    for (i, listener) in listeners.iter_mut().enumerate() {
        if let Err(e) = poll
            .registry()
            .register(listener, Token(i), Interest::READABLE)
        {
            for prev in listeners.iter_mut().take(i) {
                let _ = poll.registry().deregister(prev);
            }
            return Err(format!("failed to register listener {i}: {e}"));
        }
    }
    Ok(listeners)
}

/// (frankenredis-jd75g) Apply a CONFIG SET port/bind change by rebinding the
/// whole listener set to `new_binds` x `new_port`, mirroring redis
/// changeListener: deregister + close the OLD listeners first (so a retained
/// address:port can be re-bound — the server can't hold two sockets on its own
/// address), then bind + register the NEW set. On failure the OLD set is
/// rebound (rollback) so the server stays reachable. The runtime test-binds any
/// genuinely-new addresses beforehand, so this normally succeeds; the no-listener
/// window is a single event-loop iteration (sub-millisecond, no awaits). Existing
/// client connections (tokens >= MAX_LISTENERS) are untouched. Returns true iff
/// the new set is now live.
fn rebind_listeners(
    poll: &mut Poll,
    listeners: &mut Vec<TcpListener>,
    old_binds: &[String],
    old_port: u16,
    new_binds: &[String],
    new_port: u16,
) -> bool {
    for old in listeners.iter_mut() {
        let _ = poll.registry().deregister(old);
    }
    listeners.clear(); // drop closes the old sockets, freeing their addresses
    match bind_and_register(poll, new_binds, new_port) {
        Ok(new_listeners) => {
            *listeners = new_listeners;
            true
        }
        Err(e) => {
            eprintln!(
                "warn: CONFIG SET port/bind: rebind failed ({e}); restoring previous listeners"
            );
            match bind_and_register(poll, old_binds, old_port) {
                Ok(restored) => *listeners = restored,
                Err(e2) => eprintln!("error: failed to restore previous listeners: {e2}"),
            }
            false
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
    writer_handoff_enabled: bool,
) {
    loop {
        // Check maxclients gate via fr-eventloop before accepting.
        if let Err(e) = validate_accept_path(clients.len(), runtime.server.max_clients, true) {
            // Drain ALL pending connections from the backlog.
            while let Ok((mut stream, _)) = listener.accept() {
                eprintln!(
                    "warn: rejecting new connection: {} ({})",
                    e.reason_code(),
                    clients.len()
                );
                // (frankenredis) Match upstream networking.c::acceptCommonHandler:
                // reply "-ERR max number of clients reached" before closing so the
                // client learns why, instead of seeing a bare TCP reset. The socket
                // is mio-nonblocking; drain any request bytes the client already
                // sent so the subsequent close is a clean FIN that delivers the
                // reply rather than an RST that can discard it. Best-effort
                // throughout, exactly like upstream's connWrite-and-free.
                let mut scratch = [0u8; 256];
                while let Ok(n) = stream.read(&mut scratch) {
                    if n == 0 {
                        break;
                    }
                }
                let _ = stream.write_all(b"-ERR max number of clients reached\r\n");
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Write);
                runtime.track_rejected_connection();
                drop(stream);
            }
            break;
        }

        match listener.accept() {
            Ok((mut stream, peer_addr)) => {
                if *next_handle < MAX_LISTENERS || Token(*next_handle) == WRITER_WAKE_TOKEN {
                    *next_handle = MAX_LISTENERS;
                }
                let conn_handle = Token(*next_handle);
                *next_handle = next_handle.wrapping_add(1);
                // Avoid colliding with the reserved listener token range
                // (0..MAX_LISTENERS). (frankenredis-jd75g)
                if *next_handle < MAX_LISTENERS || Token(*next_handle) == WRITER_WAKE_TOKEN {
                    *next_handle = MAX_LISTENERS;
                }

                if let Err(e) = stream.set_nodelay(true) {
                    eprintln!("warn: failed to set TCP_NODELAY: {e}");
                }
                let writer_stream = if writer_handoff_enabled {
                    match clone_writer_stream(&stream) {
                        Ok(writer_stream) => Some(writer_stream),
                        Err(e) => {
                            eprintln!("warn: writer handoff unavailable for client: {e}");
                            None
                        }
                    }
                } else {
                    None
                };

                if let Err(e) =
                    poll.registry()
                        .register(&mut stream, conn_handle, Interest::READABLE)
                {
                    eprintln!("warn: failed to register client: {e}");
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    continue;
                }

                let mut session = runtime.new_session();
                session.peer_addr = Some(peer_addr);
                // (frankenredis-lxccd) Record the accepted socket's
                // real file descriptor so CLIENT INFO / CLIENT LIST
                // emit fd=<N> matching vendored Redis 7.2.4 instead
                // of the previous hardcoded 0.
                #[cfg(unix)]
                {
                    session.socket_fd = Some(stream.as_raw_fd());
                }
                let client_id = session.client_id;
                let conn =
                    ClientConnection::new_with_writer(stream, writer_stream, session, now_ms());
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

/// Re-drive command processing for clients whose commands were deferred by
/// CLIENT PAUSE, once the pause window has expired.
///
/// The deferred command bytes already sit in each client's `read_buf`, the
/// socket has no NEW data, and mio registers epoll EDGE-triggered — so merely
/// re-registering READABLE never re-fires and the command would hang until
/// unrelated traffic happened to arrive on that connection. We therefore re-run
/// the full read/dispatch path: `handle_readable` drains the socket (an
/// immediate WouldBlock), then `process_buffered_frames` executes the now
/// un-paused command and flushes its reply. The event loop bounds its poll
/// timeout while any client is paused so this runs promptly at expiry.
/// (frankenredis)
#[allow(clippy::too_many_arguments)]
fn release_expired_client_pause(
    clients: &mut HashMap<Token, ClientConnection>,
    runtime: &mut Runtime,
    poll: &mut Poll,
    blocked_tokens: &mut HashSet<Token>,
    blocked_wake_index: &mut BlockedWakeIndex,
    closing_tokens: &mut HashSet<Token>,
    write_tokens: &mut HashSet<Token>,
    paused_tokens: &mut HashSet<Token>,
    deferred_tokens: &mut HashSet<Token>,
    ts: u64,
    ts_us: u64,
    writer_pool: Option<&WriterPool>,
) {
    if paused_tokens.is_empty() || runtime.is_client_paused(ts) {
        return;
    }
    let tokens = std::mem::take(paused_tokens);
    for token in tokens {
        let still_pending = clients
            .get(&token)
            .is_some_and(|c| !c.read_buf.is_empty() && !c.closing && c.blocked.is_none());
        if still_pending {
            handle_readable(
                token,
                clients,
                runtime,
                poll,
                blocked_tokens,
                blocked_wake_index,
                closing_tokens,
                write_tokens,
                paused_tokens,
                deferred_tokens,
                ts,
                ts_us,
                writer_pool,
            );
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
    blocked_wake_index: &mut BlockedWakeIndex,
    closing_tokens: &mut HashSet<Token>,
    write_tokens: &mut HashSet<Token>,
    paused_tokens: &mut HashSet<Token>,
    deferred_tokens: &mut HashSet<Token>,
    ts: u64,
    ts_us: u64,
    writer_pool: Option<&WriterPool>,
) {
    let Some(conn) = clients.get_mut(&token) else {
        return;
    };

    // Read available data into the client's buffer, draining to WouldBlock.
    // mio registers epoll EDGE-triggered, so a single readiness event must be
    // fully drained: the kernel only re-notifies on NEW data, never for bytes
    // left unread. The earlier "process after one read and rely on
    // level-triggered readiness" optimization (ql59p) was unsound under
    // edge-triggered readiness — any command/pipeline larger than what one
    // event delivered (≈ >16KB in practice) stranded its tail bytes and hung
    // the connection. (frankenredis-apg7r, reverts the read-side no-drain)
    let mut buf = [0u8; 8192];
    let mut read_any = false;
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
                        read_any = true;
                        // (frankenredis-recvdrain) A non-blocking read that
                        // returned FEWER bytes than the buffer means the socket is
                        // drained right now — the kernel had exactly `n` bytes
                        // queued. Under edge-triggered epoll any later data raises a
                        // fresh EPOLLIN, so we stop here and skip the extra recv()
                        // that would only return EAGAIN — one syscall saved per
                        // readable event in the common (sub-8KB pipeline) case. We
                        // keep looping ONLY when the read filled the buffer
                        // (`n == buf.len()`), where more data may be queued — that
                        // is the >8KB-pipeline case apg7r must not strand.
                        if n < buf.len() {
                            break;
                        }
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

    // (frankenredis-k96mc) One readable handler invocation that drained data is
    // a single READ event, mirroring upstream readQueryFromClient — counted
    // before dispatch (and before the blocked-hold below) so a command reading
    // this stat (INFO) sees its own read, and a pipelined batch counts once.
    if read_any {
        runtime.note_read_event();
    }

    // If the client is blocked (BLPOP/BRPOP/etc.), don't process new
    // commands. We still read data above (to detect disconnection and
    // prevent kernel buffer overflow), but commands are held in read_buf
    // until the blocking operation completes or times out.
    if conn.blocked.is_some() {
        return;
    }

    // (frankenredis-tepuj) Refresh the per-client read/write buffer
    // metrics on the session before swapping it into the runtime so
    // CLIENT INFO / CLIENT LIST emit live qbuf/qbuf-free/obl/tot-mem
    // values that diff cleanly against vendored 7.2.
    conn.session.qbuf_bytes = conn.read_buf.len();
    conn.session.qbuf_free_bytes = conn.read_buf.capacity().saturating_sub(conn.read_buf.len());
    conn.session.output_buffer_bytes = conn.pending_output_bytes();
    // (frankenredis-jrqgd) Sample the *pre-dispatch* read/write buffer
    // sizes into the server-wide recent-max accumulators. We must do
    // this BEFORE the dispatch drains read_buf — by the time we get
    // to record_client_session below, qbuf_bytes is back to 0 because
    // the parser consumed every byte.
    runtime.observe_client_buffer_sizes(conn.read_buf.len(), conn.pending_output_bytes());

    // Swap in this client's session, process frames, swap back.
    let session = std::mem::take(&mut conn.session);
    let prev = runtime.swap_session(session);

    let write_buf_before = conn.write_buf.len();
    let budget_exhausted = process_buffered_frames(
        token,
        conn,
        runtime,
        blocked_tokens,
        blocked_wake_index,
        closing_tokens,
        write_tokens,
        paused_tokens,
        ts,
        ts_us,
    );
    record_deferred_buffered_token(token, conn, deferred_tokens, budget_exhausted);
    // Track output bytes generated by command processing.
    let output_delta = conn.write_buf.len().saturating_sub(write_buf_before);
    runtime.track_net_output_bytes(output_delta as u64);

    // Swap session back.
    let updated_session = runtime.swap_session(prev);
    conn.session = updated_session;
    // (frankenredis-tepuj) Re-sample buffer metrics post-dispatch so
    // record_client_session snapshots a coherent view: the dispatch may
    // have consumed bytes off read_buf and appended replies to write_buf.
    conn.session.qbuf_bytes = conn.read_buf.len();
    conn.session.qbuf_free_bytes = conn.read_buf.capacity().saturating_sub(conn.read_buf.len());
    conn.session.output_buffer_bytes = conn.pending_output_bytes();
    // (frankenredis-jrqgd) Observe the *post-dispatch* write buffer
    // here as well: handle_writable may drain it before the next
    // handle_readable runs, so this is the only moment we see the
    // reply's pre-flush size. We rely on observe_client_buffer_sizes
    // taking the running max -- passing 0 for qbuf is fine.
    runtime.observe_client_buffer_sizes(0, conn.pending_output_bytes());
    runtime.record_client_session(&conn.session);

    // `process_buffered_frames` has already coalesced all currently
    // readable pipeline replies for this client. Try the nonblocking
    // flush now and only arm WRITABLE for the rare partial/WouldBlock
    // case; this avoids an epoll_ctl add/remove pair on the hot path.
    if conn.has_pending_output() {
        drive_client_output(
            token,
            conn,
            OutputDriveContext {
                runtime,
                poll,
                write_tokens,
                closing_tokens,
                writer_pool,
            },
            !budget_exhausted,
        );
    }
}

fn record_deferred_buffered_token(
    token: Token,
    conn: &ClientConnection,
    deferred_tokens: &mut HashSet<Token>,
    budget_exhausted: bool,
) {
    if budget_exhausted && !conn.read_buf.is_empty() && !conn.closing && conn.blocked.is_none() {
        deferred_tokens.insert(token);
    } else {
        deferred_tokens.remove(&token);
    }
}

#[allow(clippy::too_many_arguments)]
fn process_buffered_frames(
    token: Token,
    conn: &mut ClientConnection,
    runtime: &mut Runtime,
    blocked_tokens: &mut HashSet<Token>,
    blocked_wake_index: &mut BlockedWakeIndex,
    closing_tokens: &mut HashSet<Token>,
    write_tokens: &mut HashSet<Token>,
    paused_tokens: &mut HashSet<Token>,
    ts: u64,
    ts_us: u64,
) -> bool {
    let mut consumed_total = 0;
    let mut processed_frames = 0usize;
    let mut budget_exhausted = false;
    let mut argv_scratch: Vec<Vec<u8>> = Vec::new();
    let mut plain_get_read_gate_cache: Option<bool> = None;

    loop {
        if consumed_total >= conn.read_buf.len() || conn.closing {
            break;
        }

        if processed_frames >= MAX_FRAMES_PER_CLIENT_TICK {
            budget_exhausted = true;
            break;
        }

        // Check write buffer limit before processing more frames.
        if conn.pending_output_bytes() > runtime.effective_output_hard_limit(conn.session.client_id)
        {
            eprintln!("warn: client write buffer exceeded limit, disconnecting");
            conn.closing = true;
            closing_tokens.insert(token);
            break;
        }

        let Some(&first_byte) = conn.read_buf.get(consumed_total) else {
            break;
        };

        // Try inline command parsing only for true non-RESP input. RESP uses
        // multiple leading prefixes; treating every non-array prefix as inline
        // can misclassify protocol frames and break parsing.
        if should_try_inline_parsing(first_byte) {
            let inline_parse_result = {
                let unparsed = &conn.read_buf[consumed_total..];
                try_parse_inline(unparsed)
            };
            match inline_parse_result {
                Ok(InlineParseResult::EmptyLine(consumed)) => {
                    // Silently consume empty lines (Redis behavior).
                    processed_frames = processed_frames.saturating_add(1);
                    consumed_total += consumed;
                    continue;
                }
                Ok(InlineParseResult::Command(frame, consumed)) => {
                    processed_frames = processed_frames.saturating_add(1);
                    if !command_frame_can_move_to_argv(&frame) {
                        plain_get_read_gate_cache = None;
                        let response = runtime.execute_frame_with_unix_time_us(&frame, ts, ts_us);
                        let client_resp3 = runtime.client_session().resp_protocol_version() == 3;
                        encode_client_reply(&response, client_resp3, &mut conn.write_buf);
                        consumed_total += consumed;
                        continue;
                    }
                    let argv = fr_command::argv_from_frame(frame)
                        .expect("command frame prevalidated for argv move");
                    plain_get_read_gate_cache = None;
                    match process_argv_frame(
                        token,
                        &argv,
                        conn,
                        runtime,
                        blocked_tokens,
                        blocked_wake_index,
                        closing_tokens,
                        write_tokens,
                        paused_tokens,
                        ts,
                        ts_us,
                    ) {
                        ProcessArgvAction::Continue => {
                            consumed_total += consumed;
                            if disconnect_if_output_limit_exceeded(
                                conn,
                                runtime.effective_output_hard_limit(conn.session.client_id),
                                closing_tokens,
                                token,
                            ) {
                                break;
                            }
                            continue;
                        }
                        ProcessArgvAction::BreakAfterConsume => {
                            consumed_total += consumed;
                            break;
                        }
                        ProcessArgvAction::BreakWithoutConsume => break,
                    }
                }
                Err(err) => {
                    if handle_parse_error(err, conn, closing_tokens, token) {
                        break;
                    }
                    continue;
                }
            }
        }

        // (frankenredis-08d0x) The hot client command path is strict multibulk.
        // Parse argv as borrowed slices, then copy into caller-reused Vec
        // storage for dispatch. This keeps Redis-visible ownership and reply
        // ordering unchanged while replacing per-command argv allocation churn
        // with one scratch arena per buffered processing pass.
        if matches!(first_byte, b'*') {
            let borrowed_parse_result = {
                let unparsed = &conn.read_buf[consumed_total..];
                let parser_config = runtime.parser_config();
                if let Some(packet) = parse_borrowed_plain_get_packet(unparsed, &parser_config) {
                    let client_resp3 = runtime.client_session().resp_protocol_version() == 3;
                    let default_read_allowed = *plain_get_read_gate_cache
                        .get_or_insert_with(|| runtime.plain_borrowed_default_key_read_gate(ts));
                    if runtime
                        .execute_plain_get_borrowed_into_with_default_read_gate(
                            packet.key,
                            ts,
                            client_resp3,
                            &mut conn.write_buf,
                            default_read_allowed,
                        )
                        .is_some()
                    {
                        Ok(BorrowedMultibulkAction::FastEncodedReply {
                            consumed: packet.consumed,
                        })
                    } else {
                        parse_borrowed_multibulk_action(
                            unparsed,
                            parser_config,
                            runtime,
                            ts,
                            &mut conn.write_buf,
                            &mut argv_scratch,
                        )
                    }
                } else if let Some(packet) =
                    parse_borrowed_plain_set_packet(unparsed, &parser_config)
                    && let Some(response) =
                        runtime.execute_plain_set_borrowed(packet.key, packet.value, ts)
                {
                    Ok(BorrowedMultibulkAction::FastReply {
                        consumed: packet.consumed,
                        response,
                    })
                } else if let Some(packet) =
                    parse_borrowed_plain_hset_packet(unparsed, &parser_config)
                {
                    let pairs = [packet.field, packet.value];
                    if let Some(response) =
                        runtime.execute_plain_hset_borrowed(packet.key, &pairs, ts)
                    {
                        Ok(BorrowedMultibulkAction::FastReply {
                            consumed: packet.consumed,
                            response,
                        })
                    } else {
                        parse_borrowed_multibulk_action(
                            unparsed,
                            parser_config,
                            runtime,
                            ts,
                            &mut conn.write_buf,
                            &mut argv_scratch,
                        )
                    }
                } else {
                    parse_borrowed_multibulk_action(
                        unparsed,
                        parser_config,
                        runtime,
                        ts,
                        &mut conn.write_buf,
                        &mut argv_scratch,
                    )
                }
            };
            match borrowed_parse_result {
                Ok(BorrowedMultibulkAction::FastEncodedReply { consumed }) => {
                    processed_frames = processed_frames.saturating_add(1);
                    drain_pending_pubsub_to_connection(runtime, conn);
                    consumed_total += consumed;
                    if disconnect_if_output_limit_exceeded(
                        conn,
                        runtime.effective_output_hard_limit(conn.session.client_id),
                        closing_tokens,
                        token,
                    ) {
                        break;
                    }
                    continue;
                }
                Ok(BorrowedMultibulkAction::FastReply { consumed, response }) => {
                    processed_frames = processed_frames.saturating_add(1);
                    plain_get_read_gate_cache = None;
                    let client_resp3 = runtime.client_session().resp_protocol_version() == 3;
                    if !runtime.suppress_current_network_reply() {
                        encode_client_reply(&response, client_resp3, &mut conn.write_buf);
                    }
                    drain_pending_pubsub_to_connection(runtime, conn);
                    consumed_total += consumed;
                    if disconnect_if_output_limit_exceeded(
                        conn,
                        runtime.effective_output_hard_limit(conn.session.client_id),
                        closing_tokens,
                        token,
                    ) {
                        break;
                    }
                    continue;
                }
                Ok(BorrowedMultibulkAction::Parsed {
                    kind,
                    consumed,
                    argv_len,
                }) => {
                    processed_frames = processed_frames.saturating_add(1);
                    if matches!(kind, BorrowedCommandArgsKind::NullArray) || argv_len == 0 {
                        consumed_total += consumed;
                        continue;
                    }
                    let argv = &argv_scratch[..argv_len];
                    plain_get_read_gate_cache = None;
                    match process_argv_frame(
                        token,
                        argv,
                        conn,
                        runtime,
                        blocked_tokens,
                        blocked_wake_index,
                        closing_tokens,
                        write_tokens,
                        paused_tokens,
                        ts,
                        ts_us,
                    ) {
                        ProcessArgvAction::Continue => {
                            consumed_total += consumed;
                            if disconnect_if_output_limit_exceeded(
                                conn,
                                runtime.effective_output_hard_limit(conn.session.client_id),
                                closing_tokens,
                                token,
                            ) {
                                break;
                            }
                            continue;
                        }
                        ProcessArgvAction::BreakAfterConsume => {
                            consumed_total += consumed;
                            break;
                        }
                        ProcessArgvAction::BreakWithoutConsume => break,
                    }
                }
                Err(err) => {
                    if handle_parse_error(err, conn, closing_tokens, token) {
                        break;
                    }
                    continue;
                }
            }
        }

        // Non-multibulk RESP reaches the generic parser exactly as before.
        let parse_result = {
            let unparsed = &conn.read_buf[consumed_total..];
            fr_protocol::parse_command_frame(unparsed, &runtime.parser_config())
                .map(|p| (p.frame, p.consumed))
        };
        match parse_result {
            Ok((frame, consumed)) => {
                processed_frames = processed_frames.saturating_add(1);
                // (frankenredis-w7xy8) An empty or null multibulk (`*0\r\n` /
                // `*-1\r\n`) is not a command — upstream networking.c resets and
                // moves on with no reply. fr previously dispatched it and
                // answered "ERR Protocol error: invalid command frame".
                if matches!(&frame, RespFrame::Array(None))
                    || matches!(&frame, RespFrame::Array(Some(items)) if items.is_empty())
                {
                    consumed_total += consumed;
                    continue;
                }
                if !command_frame_can_move_to_argv(&frame) {
                    plain_get_read_gate_cache = None;
                    let response = runtime.execute_frame_with_unix_time_us(&frame, ts, ts_us);
                    let client_resp3 = runtime.client_session().resp_protocol_version() == 3;
                    encode_client_reply(&response, client_resp3, &mut conn.write_buf);
                    consumed_total += consumed;
                    continue;
                }
                let argv = fr_command::argv_from_frame(frame)
                    .expect("command frame prevalidated for argv move");
                plain_get_read_gate_cache = None;
                match process_argv_frame(
                    token,
                    &argv,
                    conn,
                    runtime,
                    blocked_tokens,
                    blocked_wake_index,
                    closing_tokens,
                    write_tokens,
                    paused_tokens,
                    ts,
                    ts_us,
                ) {
                    ProcessArgvAction::Continue => {
                        consumed_total += consumed;
                    }
                    ProcessArgvAction::BreakAfterConsume => {
                        consumed_total += consumed;
                        break;
                    }
                    ProcessArgvAction::BreakWithoutConsume => break,
                }

                if conn.pending_output_bytes()
                    > runtime.effective_output_hard_limit(conn.session.client_id)
                {
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
            Err(err) => {
                let _ = handle_parse_error(err, conn, closing_tokens, token);
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

    budget_exhausted
}

fn parse_borrowed_multibulk_action(
    unparsed: &[u8],
    parser_config: ParserConfig,
    runtime: &mut Runtime,
    ts: u64,
    out: &mut Vec<u8>,
    argv_scratch: &mut Vec<Vec<u8>>,
) -> Result<BorrowedMultibulkAction, RespParseError> {
    let mut borrowed_args = Vec::new();
    match fr_protocol::parse_command_args_borrowed_into(
        unparsed,
        &parser_config,
        &mut borrowed_args,
    ) {
        Ok(parsed) => {
            let argv_len = borrowed_args.len();
            if matches!(parsed.kind, BorrowedCommandArgsKind::Arguments) && argv_len > 0 {
                if let Some(msg) = borrowed_plain_ping_args(&borrowed_args) {
                    let client_resp3 = runtime.client_session().resp_protocol_version() == 3;
                    if runtime
                        .execute_plain_ping_borrowed_into(msg, ts, client_resp3, out)
                        .is_some()
                    {
                        return Ok(BorrowedMultibulkAction::FastEncodedReply {
                            consumed: parsed.consumed,
                        });
                    }
                    copy_borrowed_argv_into_scratch(&borrowed_args, argv_scratch);
                    return Ok(BorrowedMultibulkAction::Parsed {
                        kind: parsed.kind,
                        consumed: parsed.consumed,
                        argv_len,
                    });
                }
                if let Some(msg) = borrowed_plain_echo_args(&borrowed_args) {
                    let client_resp3 = runtime.client_session().resp_protocol_version() == 3;
                    if runtime
                        .execute_plain_echo_borrowed_into(msg, ts, client_resp3, out)
                        .is_some()
                    {
                        return Ok(BorrowedMultibulkAction::FastEncodedReply {
                            consumed: parsed.consumed,
                        });
                    }
                    copy_borrowed_argv_into_scratch(&borrowed_args, argv_scratch);
                    return Ok(BorrowedMultibulkAction::Parsed {
                        kind: parsed.kind,
                        consumed: parsed.consumed,
                        argv_len,
                    });
                }
                if let Some(key) = borrowed_plain_get_args(&borrowed_args) {
                    let client_resp3 = runtime.client_session().resp_protocol_version() == 3;
                    if runtime
                        .execute_plain_get_borrowed_into(key, ts, client_resp3, out)
                        .is_some()
                    {
                        return Ok(BorrowedMultibulkAction::FastEncodedReply {
                            consumed: parsed.consumed,
                        });
                    }
                    copy_borrowed_argv_into_scratch(&borrowed_args, argv_scratch);
                    return Ok(BorrowedMultibulkAction::Parsed {
                        kind: parsed.kind,
                        consumed: parsed.consumed,
                        argv_len,
                    });
                }
                if let Some(key) = borrowed_plain_smembers_args(&borrowed_args) {
                    let client_resp3 = runtime.client_session().resp_protocol_version() == 3;
                    if runtime
                        .execute_plain_smembers_borrowed_into(key, ts, client_resp3, out)
                        .is_some()
                    {
                        return Ok(BorrowedMultibulkAction::FastEncodedReply {
                            consumed: parsed.consumed,
                        });
                    }
                    copy_borrowed_argv_into_scratch(&borrowed_args, argv_scratch);
                    return Ok(BorrowedMultibulkAction::Parsed {
                        kind: parsed.kind,
                        consumed: parsed.consumed,
                        argv_len,
                    });
                }
                if let Some((key, start, stop)) = borrowed_plain_lrange_args(&borrowed_args) {
                    if runtime
                        .execute_plain_lrange_borrowed_into(key, start, stop, ts, out)
                        .is_some()
                    {
                        return Ok(BorrowedMultibulkAction::FastEncodedReply {
                            consumed: parsed.consumed,
                        });
                    }
                    copy_borrowed_argv_into_scratch(&borrowed_args, argv_scratch);
                    return Ok(BorrowedMultibulkAction::Parsed {
                        kind: parsed.kind,
                        consumed: parsed.consumed,
                        argv_len,
                    });
                }
                if let Some(key) = borrowed_plain_hgetall_args(&borrowed_args) {
                    let client_resp3 = runtime.client_session().resp_protocol_version() == 3;
                    if runtime
                        .execute_plain_hgetall_borrowed_into(key, ts, client_resp3, out)
                        .is_some()
                    {
                        return Ok(BorrowedMultibulkAction::FastEncodedReply {
                            consumed: parsed.consumed,
                        });
                    }
                    copy_borrowed_argv_into_scratch(&borrowed_args, argv_scratch);
                    return Ok(BorrowedMultibulkAction::Parsed {
                        kind: parsed.kind,
                        consumed: parsed.consumed,
                        argv_len,
                    });
                }
                if let Some((key, values)) = borrowed_plain_hcoll_args(&borrowed_args) {
                    if runtime
                        .execute_plain_hcoll_borrowed_into(key, ts, values, out)
                        .is_some()
                    {
                        return Ok(BorrowedMultibulkAction::FastEncodedReply {
                            consumed: parsed.consumed,
                        });
                    }
                    copy_borrowed_argv_into_scratch(&borrowed_args, argv_scratch);
                    return Ok(BorrowedMultibulkAction::Parsed {
                        kind: parsed.kind,
                        consumed: parsed.consumed,
                        argv_len,
                    });
                }
                if let Some((key, value)) = borrowed_plain_set_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_set_borrowed(key, value, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some(key) = borrowed_plain_incr_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_incr_borrowed(key, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, delta)) = borrowed_plain_incrby_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_incrby_borrowed(key, delta, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some(key) = borrowed_plain_decr_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_decr_borrowed(key, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, delta)) = borrowed_plain_decrby_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_decrby_borrowed(key, delta, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, value)) = borrowed_plain_append_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_append_borrowed(key, value, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((cmd, key, values)) = borrowed_plain_keyed_values_args(&borrowed_args)
                    && let Some(response) =
                        runtime.execute_plain_keyed_values_write_borrowed(cmd, key, values, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, pairs)) = borrowed_plain_hset_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_hset_borrowed(key, pairs, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some(pairs) = borrowed_plain_mset_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_mset_borrowed(&pairs, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, pairs)) = borrowed_plain_zadd_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_zadd_borrowed(key, pairs, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, delta, member)) = borrowed_plain_zincrby_args(&borrowed_args)
                    && let Some(response) =
                        runtime.execute_plain_zincrby_borrowed(key, delta, member, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((cmd, key)) = borrowed_plain_keyed_pop_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_keyed_pop_borrowed(cmd, key, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, field)) = borrowed_plain_hget_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_hget_borrowed(key, field, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some(keys) = borrowed_plain_mget_args(&borrowed_args) {
                    // (frankenredis-5gisf) MGET encodes its `*N` reply directly
                    // into `out` (zero value clones) — FastEncodedReply, not the
                    // owned-RespFrame FastReply path.
                    let client_resp3 = runtime.client_session().resp_protocol_version() == 3;
                    if runtime
                        .execute_plain_mget_borrowed_into(keys, ts, client_resp3, out)
                        .is_some()
                    {
                        return Ok(BorrowedMultibulkAction::FastEncodedReply {
                            consumed: parsed.consumed,
                        });
                    }
                    copy_borrowed_argv_into_scratch(&borrowed_args, argv_scratch);
                    return Ok(BorrowedMultibulkAction::Parsed {
                        kind: parsed.kind,
                        consumed: parsed.consumed,
                        argv_len,
                    });
                }
                if let Some(keys) = borrowed_plain_exists_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_exists_borrowed(keys, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some(key) = borrowed_plain_strlen_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_strlen_borrowed(key, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some(key) = borrowed_plain_llen_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_llen_borrowed(key, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some(key) = borrowed_plain_scard_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_scard_borrowed(key, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((cmd, key)) = borrowed_plain_keymeta_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_keymeta_borrowed(cmd, key, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((cmd, key)) = borrowed_plain_cardinality_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_cardinality_borrowed(cmd, key, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((cmd, key, member)) = borrowed_plain_rank_args(&borrowed_args)
                    && let Some(response) =
                        runtime.execute_plain_rank_borrowed(cmd, key, member, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, index)) = borrowed_plain_lindex_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_lindex_borrowed(key, index, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, member)) = borrowed_plain_zscore_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_zscore_borrowed(key, member, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, start, end)) = borrowed_plain_getrange_args(&borrowed_args) {
                    let client_resp3 = runtime.client_session().resp_protocol_version() == 3;
                    if runtime
                        .execute_plain_getrange_borrowed_into(key, start, end, ts, client_resp3, out)
                        .is_some()
                    {
                        return Ok(BorrowedMultibulkAction::FastEncodedReply {
                            consumed: parsed.consumed,
                        });
                    }
                    copy_borrowed_argv_into_scratch(&borrowed_args, argv_scratch);
                    return Ok(BorrowedMultibulkAction::Parsed {
                        kind: parsed.kind,
                        consumed: parsed.consumed,
                        argv_len,
                    });
                }
                if let Some((key, fields)) = borrowed_plain_hmget_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_hmget_borrowed(key, fields, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, members)) = borrowed_plain_smismember_args(&borrowed_args)
                    && let Some(response) =
                        runtime.execute_plain_smismember_borrowed(key, members, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, members)) = borrowed_plain_zmscore_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_zmscore_borrowed(key, members, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some(tail) = borrowed_plain_sintercard_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_sintercard_borrowed(tail, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, member)) = borrowed_plain_sismember_args(&borrowed_args)
                    && let Some(response) =
                        runtime.execute_plain_sismember_borrowed(key, member, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, offset_arg)) = borrowed_plain_getbit_args(&borrowed_args)
                    && let Some(response) =
                        runtime.execute_plain_getbit_borrowed(key, offset_arg, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, element)) = borrowed_plain_lpos_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_lpos_borrowed(key, element, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some(key) = borrowed_plain_object_encoding_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_object_encoding_borrowed(key, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some(key) = borrowed_plain_memory_usage_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_memory_usage_borrowed(key, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some(key) = borrowed_plain_bitcount_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_bitcount_borrowed(key, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, bit_arg)) = borrowed_plain_bitpos_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_bitpos_borrowed(key, bit_arg, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if borrowed_plain_command_count_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_command_count_borrowed(ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if borrowed_plain_dbsize_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_dbsize_borrowed(ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, seconds)) = borrowed_plain_expire_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_expire_borrowed(key, seconds, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, field)) = borrowed_plain_hstrlen_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_hstrlen_borrowed(key, field, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }
                if let Some((key, field)) = borrowed_plain_hexists_args(&borrowed_args)
                    && let Some(response) = runtime.execute_plain_hexists_borrowed(key, field, ts)
                {
                    return Ok(BorrowedMultibulkAction::FastReply {
                        consumed: parsed.consumed,
                        response,
                    });
                }

                copy_borrowed_argv_into_scratch(&borrowed_args, argv_scratch);
            }

            Ok(BorrowedMultibulkAction::Parsed {
                kind: parsed.kind,
                consumed: parsed.consumed,
                argv_len,
            })
        }
        Err(err) => Err(err),
    }
}

enum BorrowedMultibulkAction {
    Parsed {
        kind: BorrowedCommandArgsKind,
        consumed: usize,
        argv_len: usize,
    },
    FastReply {
        consumed: usize,
        response: RespFrame,
    },
    FastEncodedReply {
        consumed: usize,
    },
}

struct BorrowedPlainGetPacket<'a> {
    consumed: usize,
    key: &'a [u8],
}

fn parse_borrowed_plain_get_packet<'a>(
    input: &'a [u8],
    config: &ParserConfig,
) -> Option<BorrowedPlainGetPacket<'a>> {
    if config.max_array_len < 2 || config.max_bulk_len < b"GET".len() {
        return None;
    }
    let mut cursor = input.strip_prefix(b"*2\r\n$3\r\n").and_then(|rest| {
        rest.get(..3)
            .filter(|command| command.eq_ignore_ascii_case(b"GET"))
            .map(|_| input.len() - rest.len() + 3)
    })?;
    if input.get(cursor..cursor + 2)? != b"\r\n" {
        return None;
    }
    cursor += 2;
    let (key, consumed) = parse_borrowed_plain_set_bulk(input, cursor, config.max_bulk_len)?;
    Some(BorrowedPlainGetPacket { consumed, key })
}

struct BorrowedPlainSetPacket<'a> {
    consumed: usize,
    key: &'a [u8],
    value: &'a [u8],
}

fn parse_borrowed_plain_set_packet<'a>(
    input: &'a [u8],
    config: &ParserConfig,
) -> Option<BorrowedPlainSetPacket<'a>> {
    if config.max_array_len < 3 || config.max_bulk_len < b"SET".len() {
        return None;
    }
    let mut cursor = input.strip_prefix(b"*3\r\n$3\r\n").and_then(|rest| {
        rest.get(..3)
            .filter(|command| command.eq_ignore_ascii_case(b"SET"))
            .map(|_| input.len() - rest.len() + 3)
    })?;
    if input.get(cursor..cursor + 2)? != b"\r\n" {
        return None;
    }
    cursor += 2;
    let (key, next) = parse_borrowed_plain_set_bulk(input, cursor, config.max_bulk_len)?;
    let (value, consumed) = parse_borrowed_plain_set_bulk(input, next, config.max_bulk_len)?;
    Some(BorrowedPlainSetPacket {
        consumed,
        key,
        value,
    })
}

struct BorrowedPlainHsetPacket<'a> {
    consumed: usize,
    key: &'a [u8],
    field: &'a [u8],
    value: &'a [u8],
}

fn parse_borrowed_plain_hset_packet<'a>(
    input: &'a [u8],
    config: &ParserConfig,
) -> Option<BorrowedPlainHsetPacket<'a>> {
    if config.max_array_len < 4 || config.max_bulk_len < b"HSET".len() {
        return None;
    }
    let mut cursor = input.strip_prefix(b"*4\r\n$4\r\n").and_then(|rest| {
        rest.get(..4)
            .filter(|command| command.eq_ignore_ascii_case(b"HSET"))
            .map(|_| input.len() - rest.len() + 4)
    })?;
    if input.get(cursor..cursor + 2)? != b"\r\n" {
        return None;
    }
    cursor += 2;
    let (key, next) = parse_borrowed_plain_set_bulk(input, cursor, config.max_bulk_len)?;
    let (field, next) = parse_borrowed_plain_set_bulk(input, next, config.max_bulk_len)?;
    let (value, consumed) = parse_borrowed_plain_set_bulk(input, next, config.max_bulk_len)?;
    Some(BorrowedPlainHsetPacket {
        consumed,
        key,
        field,
        value,
    })
}

fn parse_borrowed_plain_set_bulk(
    input: &[u8],
    cursor: usize,
    max_bulk_len: usize,
) -> Option<(&[u8], usize)> {
    if *input.get(cursor)? != b'$' {
        return None;
    }
    let mut idx = cursor + 1;
    let first = *input.get(idx)?;
    let bulk_len = if first == b'0' {
        idx += 1;
        if !matches!(input.get(idx), Some(b'\r')) {
            return None;
        }
        0usize
    } else if first.is_ascii_digit() && first != b'0' {
        let mut value = usize::from(first - b'0');
        idx += 1;
        while let Some(&byte) = input.get(idx) {
            if byte == b'\r' {
                break;
            }
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value.checked_mul(10)?;
            value = value.checked_add(usize::from(byte - b'0'))?;
            idx += 1;
        }
        value
    } else {
        return None;
    };
    if bulk_len > max_bulk_len || input.get(idx..idx + 2)? != b"\r\n" {
        return None;
    }
    idx += 2;
    let end = idx.checked_add(bulk_len)?;
    if input.get(end..end + 2)? != b"\r\n" {
        return None;
    }
    let arg = input.get(idx..end)?;
    Some((arg, end + 2))
}

fn borrowed_plain_get_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, key] if command.eq_ignore_ascii_case(b"GET") => Some(*key),
        _ => None,
    }
}

/// Recognize a fast-path PING: `PING` -> `Some(None)`, `PING <msg>` ->
/// `Some(Some(msg))`. Returns the outer `None` for anything else (incl. >1 arg,
/// which the generic path rejects with the arity error). The inner option is the
/// optional echo message. (frankenredis-ping-fastpath)
/// `ECHO message` (argc 2 only); wrong-arity falls to generic.
fn borrowed_plain_echo_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, msg] if command.eq_ignore_ascii_case(b"ECHO") => Some(*msg),
        _ => None,
    }
}

fn borrowed_plain_ping_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<Option<&'a [u8]>> {
    match borrowed_args {
        [command] if command.eq_ignore_ascii_case(b"PING") => Some(None),
        [command, msg] if command.eq_ignore_ascii_case(b"PING") => Some(Some(*msg)),
        _ => None,
    }
}

fn borrowed_plain_set_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, value] if command.eq_ignore_ascii_case(b"SET") => Some((*key, *value)),
        _ => None,
    }
}

fn borrowed_plain_smembers_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, key] if command.eq_ignore_ascii_case(b"SMEMBERS") => Some(*key),
        _ => None,
    }
}

fn borrowed_plain_lrange_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(&'a [u8], &'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, start, stop] if command.eq_ignore_ascii_case(b"LRANGE") => {
            Some((*key, *start, *stop))
        }
        _ => None,
    }
}

fn borrowed_plain_hgetall_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, key] if command.eq_ignore_ascii_case(b"HGETALL") => Some(*key),
        _ => None,
    }
}

/// `HKEYS key` / `HVALS key` -> (key, values?) where `values=true` for HVALS.
fn borrowed_plain_hcoll_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], bool)> {
    match borrowed_args {
        [command, key] if command.eq_ignore_ascii_case(b"HKEYS") => Some((*key, false)),
        [command, key] if command.eq_ignore_ascii_case(b"HVALS") => Some((*key, true)),
        _ => None,
    }
}

fn borrowed_plain_incr_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, key] if command.eq_ignore_ascii_case(b"INCR") => Some(*key),
        _ => None,
    }
}

fn borrowed_plain_incrby_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, delta] if command.eq_ignore_ascii_case(b"INCRBY") => Some((*key, *delta)),
        _ => None,
    }
}

fn borrowed_plain_decr_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, key] if command.eq_ignore_ascii_case(b"DECR") => Some(*key),
        _ => None,
    }
}

fn borrowed_plain_append_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, value] if command.eq_ignore_ascii_case(b"APPEND") => Some((*key, *value)),
        _ => None,
    }
}

/// `SADD | LPUSH | RPUSH key value [value ...]` borrowed-arg matcher for the
/// shared keyed-values write fast path. (frankenredis-ev067)
type BorrowedKeyedValuesArgs<'a> = (PlainKeyedValuesCmd, &'a [u8], &'a [&'a [u8]]);

/// (frankenredis-5gisf) `MSET k v [k v ...]` borrowed-arg matcher: a non-empty
/// even-length key/value tail. Returns borrowed `(key, value)` pairs so the MSET
/// fast path avoids the general parse path's 2N per-arg `.to_vec()` allocations.
/// Odd/empty falls back to the generic WrongArity path.
fn borrowed_plain_mset_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<Vec<(&'a [u8], &'a [u8])>> {
    let [command, rest @ ..] = borrowed_args else {
        return None;
    };
    if !command.eq_ignore_ascii_case(b"MSET") || rest.is_empty() || rest.len() % 2 != 0 {
        return None;
    }
    Some(rest.chunks_exact(2).map(|c| (c[0], c[1])).collect())
}

fn borrowed_plain_keyed_values_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<BorrowedKeyedValuesArgs<'a>> {
    let [command, key, values @ ..] = borrowed_args else {
        return None;
    };
    if values.is_empty() {
        return None;
    }
    let cmd = if command.eq_ignore_ascii_case(b"SADD") {
        PlainKeyedValuesCmd::Sadd
    } else if command.eq_ignore_ascii_case(b"LPUSH") {
        PlainKeyedValuesCmd::Lpush
    } else if command.eq_ignore_ascii_case(b"RPUSH") {
        PlainKeyedValuesCmd::Rpush
    } else {
        return None;
    };
    Some((cmd, *key, values))
}

/// `HSET key field value [field value ...]` borrowed-arg matcher: requires a
/// non-empty even-length field/value tail (odd/empty falls back to the generic
/// WrongArity path). (frankenredis-ev067)
fn borrowed_plain_hset_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(&'a [u8], &'a [&'a [u8]])> {
    let [command, key, pairs @ ..] = borrowed_args else {
        return None;
    };
    if !command.eq_ignore_ascii_case(b"HSET") || pairs.is_empty() || pairs.len() % 2 != 0 {
        return None;
    }
    Some((*key, pairs))
}

/// `ZADD key score member [score member ...]` borrowed-arg matcher for the PLAIN
/// flagless form only: the first tail token must not be an NX/XX/GT/LT/CH/INCR
/// flag (upstream stops flag parsing at the first non-flag token, so a non-flag
/// at that position means no leading flags), and the tail must be a non-empty
/// even-length score/member sequence. Anything else (flags, odd/empty tail)
/// falls back to the generic handler, which owns flag + arity semantics. Score
/// validity is checked in the runtime fast path. (frankenredis-ev067)
fn borrowed_plain_zadd_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(&'a [u8], &'a [&'a [u8]])> {
    let [command, key, pairs @ ..] = borrowed_args else {
        return None;
    };
    if !command.eq_ignore_ascii_case(b"ZADD") || pairs.is_empty() || pairs.len() % 2 != 0 {
        return None;
    }
    let first = pairs[0];
    let is_flag = first.eq_ignore_ascii_case(b"NX")
        || first.eq_ignore_ascii_case(b"XX")
        || first.eq_ignore_ascii_case(b"GT")
        || first.eq_ignore_ascii_case(b"LT")
        || first.eq_ignore_ascii_case(b"CH")
        || first.eq_ignore_ascii_case(b"INCR");
    if is_flag {
        return None;
    }
    Some((*key, pairs))
}

fn borrowed_plain_zincrby_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(&'a [u8], &'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, delta, member] if command.eq_ignore_ascii_case(b"ZINCRBY") => {
            Some((*key, *delta, *member))
        }
        _ => None,
    }
}

/// `LPOP | RPOP | SPOP key` borrowed-arg matcher for the no-count pop fast path.
/// The COUNT form (`CMD key count`) falls back to the generic handler, which
/// owns the array-reply + count-validation semantics. (frankenredis-ev067)
fn borrowed_plain_keyed_pop_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(PlainKeyedPopCmd, &'a [u8])> {
    let [command, key] = borrowed_args else {
        return None;
    };
    let cmd = if command.eq_ignore_ascii_case(b"LPOP") {
        PlainKeyedPopCmd::Lpop
    } else if command.eq_ignore_ascii_case(b"RPOP") {
        PlainKeyedPopCmd::Rpop
    } else if command.eq_ignore_ascii_case(b"SPOP") {
        PlainKeyedPopCmd::Spop
    } else if command.eq_ignore_ascii_case(b"ZPOPMIN") {
        // (frankenredis-zpopfast) 2-arg (no-count) ZPOPMIN/ZPOPMAX share the
        // borrowed pop fast path; the count form (3 args) won't match this
        // [command, key] shape and falls through to the generic handler.
        PlainKeyedPopCmd::Zpopmin
    } else if command.eq_ignore_ascii_case(b"ZPOPMAX") {
        PlainKeyedPopCmd::Zpopmax
    } else {
        return None;
    };
    Some((cmd, *key))
}

fn borrowed_plain_decrby_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, delta] if command.eq_ignore_ascii_case(b"DECRBY") => Some((*key, *delta)),
        _ => None,
    }
}

fn borrowed_plain_hget_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, field] if command.eq_ignore_ascii_case(b"HGET") => Some((*key, *field)),
        _ => None,
    }
}

fn borrowed_plain_mget_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [&'a [u8]]> {
    match borrowed_args {
        [command, keys @ ..] if !keys.is_empty() && command.eq_ignore_ascii_case(b"MGET") => {
            Some(keys)
        }
        _ => None,
    }
}

fn borrowed_plain_exists_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [&'a [u8]]> {
    match borrowed_args {
        [command, keys @ ..] if !keys.is_empty() && command.eq_ignore_ascii_case(b"EXISTS") => {
            Some(keys)
        }
        _ => None,
    }
}

#[allow(clippy::type_complexity)]
fn borrowed_plain_hmget_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(&'a [u8], &'a [&'a [u8]])> {
    match borrowed_args {
        [command, key, fields @ ..]
            if !fields.is_empty() && command.eq_ignore_ascii_case(b"HMGET") =>
        {
            Some((*key, fields))
        }
        _ => None,
    }
}

#[allow(clippy::type_complexity)]
fn borrowed_plain_smismember_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(&'a [u8], &'a [&'a [u8]])> {
    match borrowed_args {
        [command, key, members @ ..]
            if !members.is_empty() && command.eq_ignore_ascii_case(b"SMISMEMBER") =>
        {
            Some((*key, members))
        }
        _ => None,
    }
}

#[allow(clippy::type_complexity)]
fn borrowed_plain_zmscore_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(&'a [u8], &'a [&'a [u8]])> {
    match borrowed_args {
        [command, key, members @ ..]
            if !members.is_empty() && command.eq_ignore_ascii_case(b"ZMSCORE") =>
        {
            Some((*key, members))
        }
        _ => None,
    }
}

// `tail` = [numkeys, key...]; the runtime validates numkeys + the no-LIMIT shape.
fn borrowed_plain_sintercard_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [&'a [u8]]> {
    match borrowed_args {
        [command, tail @ ..]
            if tail.len() >= 2 && command.eq_ignore_ascii_case(b"SINTERCARD") =>
        {
            Some(tail)
        }
        _ => None,
    }
}

fn borrowed_plain_strlen_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, key] if command.eq_ignore_ascii_case(b"STRLEN") => Some(*key),
        _ => None,
    }
}

fn borrowed_plain_llen_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, key] if command.eq_ignore_ascii_case(b"LLEN") => Some(*key),
        _ => None,
    }
}

fn borrowed_plain_scard_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, key] if command.eq_ignore_ascii_case(b"SCARD") => Some(*key),
        _ => None,
    }
}

fn borrowed_plain_keymeta_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(PlainKeyMetaCmd, &'a [u8])> {
    let [command, key] = borrowed_args else {
        return None;
    };
    let cmd = if command.eq_ignore_ascii_case(b"TTL") {
        PlainKeyMetaCmd::Ttl
    } else if command.eq_ignore_ascii_case(b"PTTL") {
        PlainKeyMetaCmd::Pttl
    } else if command.eq_ignore_ascii_case(b"TYPE") {
        PlainKeyMetaCmd::Type
    } else if command.eq_ignore_ascii_case(b"EXPIRETIME") {
        PlainKeyMetaCmd::Expiretime
    } else if command.eq_ignore_ascii_case(b"PEXPIRETIME") {
        PlainKeyMetaCmd::Pexpiretime
    } else {
        return None;
    };
    Some((cmd, *key))
}

fn borrowed_plain_cardinality_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(PlainCardinalityCmd, &'a [u8])> {
    let [command, key] = borrowed_args else {
        return None;
    };
    let cmd = if command.eq_ignore_ascii_case(b"ZCARD") {
        PlainCardinalityCmd::Zcard
    } else if command.eq_ignore_ascii_case(b"HLEN") {
        PlainCardinalityCmd::Hlen
    } else if command.eq_ignore_ascii_case(b"XLEN") {
        PlainCardinalityCmd::Xlen
    } else {
        return None;
    };
    Some((cmd, *key))
}

fn borrowed_plain_rank_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(PlainRankCmd, &'a [u8], &'a [u8])> {
    let [command, key, member] = borrowed_args else {
        return None;
    };
    let cmd = if command.eq_ignore_ascii_case(b"ZRANK") {
        PlainRankCmd::Zrank
    } else if command.eq_ignore_ascii_case(b"ZREVRANK") {
        PlainRankCmd::Zrevrank
    } else {
        return None;
    };
    Some((cmd, *key, *member))
}

fn borrowed_plain_lindex_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, index] if command.eq_ignore_ascii_case(b"LINDEX") => Some((*key, *index)),
        _ => None,
    }
}

fn borrowed_plain_zscore_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, member] if command.eq_ignore_ascii_case(b"ZSCORE") => Some((*key, *member)),
        _ => None,
    }
}

fn borrowed_plain_getbit_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, offset] if command.eq_ignore_ascii_case(b"GETBIT") => Some((*key, *offset)),
        _ => None,
    }
}

/// Only the no-option `LPOS key element` form (argc 3); RANK/COUNT/MAXLEN forms
/// fall to generic dispatch.
fn borrowed_plain_lpos_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, element] if command.eq_ignore_ascii_case(b"LPOS") => Some((*key, *element)),
        _ => None,
    }
}

/// Only `OBJECT ENCODING key` (argc 3); other OBJECT subcommands fall to generic.
fn borrowed_plain_object_encoding_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, sub, key]
            if command.eq_ignore_ascii_case(b"OBJECT") && sub.eq_ignore_ascii_case(b"ENCODING") =>
        {
            Some(*key)
        }
        _ => None,
    }
}

/// Only `MEMORY USAGE key` (argc 3, no SAMPLES); SAMPLES/other forms fall to generic.
fn borrowed_plain_memory_usage_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, sub, key]
            if command.eq_ignore_ascii_case(b"MEMORY") && sub.eq_ignore_ascii_case(b"USAGE") =>
        {
            Some(*key)
        }
        _ => None,
    }
}

/// Only the no-range `BITCOUNT key` form (argc 2); ranged forms fall to generic.
/// Only the no-range `BITPOS key bit` form (argc 3); ranged forms fall to generic.
fn borrowed_plain_bitpos_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, bit] if command.eq_ignore_ascii_case(b"BITPOS") => Some((*key, *bit)),
        _ => None,
    }
}

fn borrowed_plain_bitcount_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    match borrowed_args {
        [command, key] if command.eq_ignore_ascii_case(b"BITCOUNT") => Some(*key),
        _ => None,
    }
}

/// Keyless `DBSIZE` (argc 1); any args fall to generic (arity error).
/// Keyless `COMMAND COUNT` (argc 2); other COMMAND subcommands / arity fall to generic.
fn borrowed_plain_command_count_args(borrowed_args: &[&[u8]]) -> bool {
    matches!(borrowed_args, [c, sub]
        if c.eq_ignore_ascii_case(b"COMMAND") && sub.eq_ignore_ascii_case(b"COUNT"))
}

fn borrowed_plain_dbsize_args(borrowed_args: &[&[u8]]) -> bool {
    matches!(borrowed_args, [command] if command.eq_ignore_ascii_case(b"DBSIZE"))
}

/// Only the no-flag `EXPIRE key seconds` form (argc 3); flagged forms
/// (NX/XX/GT/LT) fall to generic dispatch.
fn borrowed_plain_expire_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, seconds] if command.eq_ignore_ascii_case(b"EXPIRE") => {
            Some((*key, *seconds))
        }
        _ => None,
    }
}

fn borrowed_plain_sismember_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, member] if command.eq_ignore_ascii_case(b"SISMEMBER") => {
            Some((*key, *member))
        }
        _ => None,
    }
}

fn borrowed_plain_hexists_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, field] if command.eq_ignore_ascii_case(b"HEXISTS") => Some((*key, *field)),
        _ => None,
    }
}

fn borrowed_plain_hstrlen_args<'a>(borrowed_args: &'a [&'a [u8]]) -> Option<(&'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, field] if command.eq_ignore_ascii_case(b"HSTRLEN") => Some((*key, *field)),
        _ => None,
    }
}

#[allow(clippy::type_complexity)]
fn borrowed_plain_getrange_args<'a>(
    borrowed_args: &'a [&'a [u8]],
) -> Option<(&'a [u8], &'a [u8], &'a [u8])> {
    match borrowed_args {
        [command, key, start, end] if command.eq_ignore_ascii_case(b"GETRANGE") => {
            Some((*key, *start, *end))
        }
        _ => None,
    }
}

fn copy_borrowed_argv_into_scratch(borrowed_args: &[&[u8]], argv_scratch: &mut Vec<Vec<u8>>) {
    while argv_scratch.len() < borrowed_args.len() {
        argv_scratch.push(Vec::new());
    }
    for (idx, arg) in borrowed_args.iter().enumerate() {
        argv_scratch[idx].clear();
        argv_scratch[idx].extend_from_slice(arg);
    }
}

enum ProcessArgvAction {
    Continue,
    BreakAfterConsume,
    BreakWithoutConsume,
}

#[allow(clippy::too_many_arguments)]
fn process_argv_frame(
    token: Token,
    argv: &[Vec<u8>],
    conn: &mut ClientConnection,
    runtime: &mut Runtime,
    blocked_tokens: &mut HashSet<Token>,
    blocked_wake_index: &mut BlockedWakeIndex,
    closing_tokens: &mut HashSet<Token>,
    write_tokens: &mut HashSet<Token>,
    paused_tokens: &mut HashSet<Token>,
    ts: u64,
    ts_us: u64,
) -> ProcessArgvAction {
    // Subscription mode gate: reject most commands while subscribed.
    // (frankenredis-j7nwu) Only RESP2 subscribers are restricted —
    // upstream server.c::processCommand gates the allow-list on
    // `c->resp == 2`. RESP3 clients may freely interleave any
    // command with push frames, so the gate (and its runtime mirror
    // at lib.rs ~5426) must be skipped for them.
    //
    // (frankenredis-nnbig) Upstream processCommand performs command
    // LOOKUP (unknown -> "unknown command") and the generic ARITY check
    // (server.c:3787) BEFORE the pub/sub-context gate (server.c:4072), so a
    // wrong-arity or unknown command issued while subscribed surfaces its own
    // error rather than the "...allowed in this context" wording. The runtime
    // mirror gate (lib.rs ~8861) already conditions on `command_arity_ok`; this
    // fast-path gate must too — when the command is unknown or its argc fails
    // the generic arity, skip the gate so the command reaches dispatch, which
    // emits the matching unknown/arity error (no side effect: arity fails
    // before execution).
    //
    // (frankenredis-7tpx0) Two refinements so this fast gate stays a strict
    // subset of the runtime gate:
    //   * Full arity (`check_full_command_arity`) so a known container
    //     subcommand with the wrong argc (e.g. `CONFIG GET`, `OBJECT ENCODING`)
    //     skips the gate and reaches dispatch for its own arity error.
    //   * DEBUG is deferred to the runtime: it is CMD_PROTECTED (server.c:3878),
    //     so a subscribed DEBUG must get the protected/arity error from
    //     handle_debug_command_gate (which runs before the runtime context
    //     gate), not the context wording this fast gate would emit.
    if runtime.is_in_subscription_mode()
        && runtime.client_session().resp_protocol_version() != 3
        && !argv
            .first()
            .is_some_and(|command| command.eq_ignore_ascii_case(b"DEBUG"))
        && fr_command::check_full_command_arity(argv).is_ok()
        && let Some(reject) = check_subscription_mode_gate(argv, true)
    {
        reject.encode_into(&mut conn.write_buf);
        return ProcessArgvAction::Continue;
    }
    runtime.set_blocked_clients_count_for_info(blocked_tokens.len());
    // CLIENT PAUSE gate: delay command processing while paused.
    // Fast path: `is_client_paused` is an O(1) deadline check that is
    // false on the overwhelmingly common no-pause path. Guarding the
    // gate with it avoids materializing a full `frame_to_argv` heap
    // copy (one Vec + one Vec per argument) on every command — the
    // single largest per-request allocation in the dispatch hot path
    // (gdb profiling: ~58% of on-CPU samples in the allocator). When a
    // pause IS active, behavior is identical: `is_command_paused`
    // already re-checks `is_client_paused` internally.
    // (frankenredis) Upstream server.c::processCommand defers EVERY non-replica
    // command while PAUSE_ACTION_CLIENT_ALL is active — there is no exemption for
    // CLIENT UNPAUSE, so a PAUSE ALL can only be lifted by its own deadline, not
    // by an UNPAUSE that arrives during the window (verified live: redis ignores
    // an in-pause UNPAUSE and releases at the original deadline). Under PAUSE
    // WRITE, non-write commands like UNPAUSE are not paused by is_command_paused
    // in the first place, so they still pass through. Mirror that exactly.
    if runtime.is_client_paused(ts) && runtime.is_command_paused(argv, ts) {
        // Don't process the command — leave it in the read buffer.
        // Track paused token so we can re-process when pause expires.
        paused_tokens.insert(token);
        return ProcessArgvAction::BreakWithoutConsume;
    }
    // Dispatch on argv storage that lives through all post-dispatch checks.
    // The owned fallback still moves argv out of the parsed frame; the hot
    // multibulk path reuses a per-pass scratch arena. (frankenredis-8yfmt,
    // frankenredis-08d0x)
    let response = runtime.execute_argv_with_unix_time_us(argv, ts, ts_us);
    // (frankenredis-pgplm) Choose the RESP3 null encoding (`_`)
    // when the client negotiated HELLO 3. Captured before the
    // block-detection check below, which still compares the
    // unmutated `response`.
    let client_resp3 = runtime.client_session().resp_protocol_version() == 3;

    // Check for QUIT command.
    if is_quit_frame(argv) {
        encode_client_reply(&response, client_resp3, &mut conn.write_buf);
        write_tokens.insert(token);
        conn.closing = true;
        closing_tokens.insert(token);
        return ProcessArgvAction::BreakAfterConsume;
    }

    // Check for blocking commands that returned nil — block the
    // client instead of sending the nil response immediately.
    let should_block = matches!(
        response,
        RespFrame::Array(None) | RespFrame::BulkString(None)
    ) || waitaof_should_block(argv, &response)
        || wait_should_block(argv, &response);
    if should_block
        && let Some(blocked) =
            try_build_blocked_state(argv, ts).and_then(|BlockedState { op, deadline_ms }| {
                Some(BlockedState {
                    op: resolve_blocked_op(op, runtime, ts)?,
                    deadline_ms,
                })
            })
    {
        // Redis behavior: if the keys already have data, we shouldn't block.
        // try_build_blocked_state only returns Some if it's a blocking command.
        if let Some(immediate_response) = try_fulfill_blocked(&blocked.op, runtime, ts) {
            encode_client_reply(&immediate_response, client_resp3, &mut conn.write_buf);
        } else {
            conn.blocked = Some(blocked);
            blocked_tokens.insert(token);
            if let Some(blocked) = &conn.blocked {
                blocked_wake_index.insert(token, blocked);
            }
            runtime.mark_client_blocked(runtime.client_id());
            return ProcessArgvAction::BreakAfterConsume;
        }
    } else if suppress_client_network_reply(runtime, argv, &response) {
        // Redis treats REPLCONF ACK/GETACK as internal control
        // frames and does not send a direct reply on client links.
    } else {
        encode_client_reply(&response, client_resp3, &mut conn.write_buf);
    }
    if let Some(follow_up) = replication_follow_up_bytes(runtime, argv, &response, ts) {
        conn.write_buf.extend_from_slice(&follow_up);
        if runtime.is_replica(runtime.client_id()) {
            conn.replication_sent_offset = Some(runtime.replication_primary_offset());
        }
    }

    drain_pending_pubsub_to_connection(runtime, conn);

    ProcessArgvAction::Continue
}

fn drain_pending_pubsub_to_connection(runtime: &mut Runtime, conn: &mut ClientConnection) {
    // Drain and deliver any pending pub/sub messages (including
    // shard pub/sub SMessage) generated by the command.
    for msg in runtime.drain_pending_pubsub() {
        let resp3 = runtime.client_session().resp_protocol_version() == 3;
        let frame = pubsub_message_to_frame_for_protocol(
            msg,
            runtime.client_session().resp_protocol_version(),
        );
        // (frankenredis-o90ga) See deliver_pubsub_messages: RESP3
        // clients need the RESP3 null inside flush invalidations.
        if resp3 {
            frame.encode_into_resp3(&mut conn.write_buf);
        } else {
            frame.encode_into(&mut conn.write_buf);
        }
    }
}

fn disconnect_if_output_limit_exceeded(
    conn: &mut ClientConnection,
    output_buffer_limit: usize,
    closing_tokens: &mut HashSet<Token>,
    token: Token,
) -> bool {
    if conn.pending_output_bytes() > output_buffer_limit {
        eprintln!("warn: client write buffer exceeded limit, disconnecting");
        conn.closing = true;
        closing_tokens.insert(token);
        return true;
    }
    false
}

fn handle_parse_error(
    err: RespParseError,
    conn: &mut ClientConnection,
    closing_tokens: &mut HashSet<Token>,
    token: Token,
) -> bool {
    if err == RespParseError::Incomplete {
        return true;
    }
    // Protocol error — send the specific message and disconnect,
    // matching upstream networking.c (e.g. "invalid multibulk
    // length", "invalid bulk length"). fr previously emitted a
    // generic "invalid frame" for every parse failure.
    // (frankenredis-w7xy8)
    let err_reply = RespFrame::Error(format!("ERR Protocol error: {err}"));
    err_reply.encode_into(&mut conn.write_buf);
    conn.closing = true;
    closing_tokens.insert(token);
    true
}

use fr_server::{InlineParseResult, should_try_inline_parsing, try_parse_inline};

/// (frankenredis-pkdgs) How often a Sentinel actively PINGs + INFOs each
/// monitored master. Upstream pings every `down-after/2` (<=1s) and INFOs every
/// 10s; a unified 1s probe keeps the observable instance fields (runid, flags,
/// ping/info-refresh times, discovered replicas) fresh without two schedules.
const SENTINEL_PROBE_INTERVAL_MS: u64 = 1000;

/// (frankenredis-pkdgs) Blocking PING + INFO of one monitored master. Returns
/// the INFO payload on success; any connect / IO / protocol failure is an `Err`
/// the caller folds into a link disconnect. Short timeouts stop a dead master
/// from stalling the event loop.
fn probe_sentinel_master(
    ip: &str,
    port: u16,
    parser_config: &ParserConfig,
    query_buffer_limit: usize,
    hello: Option<&str>,
) -> io::Result<String> {
    let addr: std::net::SocketAddr = format!("{ip}:{port}")
        .parse()
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "bad master addr"))?;
    let mut stream = StdTcpStream::connect_timeout(&addr, Duration::from_millis(200))?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(Duration::from_millis(300)))?;
    stream.set_write_timeout(Some(Duration::from_millis(300)))?;
    let mut read_buf = Vec::new();

    stream.write_all(&replica_handshake_frame(&[b"PING"]).to_bytes())?;
    let pong = read_frame_from_stream(
        &mut stream,
        &mut read_buf,
        parser_config,
        query_buffer_limit,
    )?;
    if !matches!(&pong, RespFrame::SimpleString(s) if s.eq_ignore_ascii_case("PONG")) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "master did not PONG",
        ));
    }

    // (frankenredis-pkdgs) Gossip our hello on the master's pub/sub channel so
    // peer sentinels discover us. Best-effort: the integer reply is drained but
    // a failure here still lets the INFO below decide link liveness.
    if let Some(hello) = hello {
        stream.write_all(
            &replica_handshake_frame(&[b"PUBLISH", b"__sentinel__:hello", hello.as_bytes()])
                .to_bytes(),
        )?;
        let _ = read_frame_from_stream(
            &mut stream,
            &mut read_buf,
            parser_config,
            query_buffer_limit,
        )?;
    }

    stream.write_all(&replica_handshake_frame(&[b"INFO"]).to_bytes())?;
    let info = read_frame_from_stream(
        &mut stream,
        &mut read_buf,
        parser_config,
        query_buffer_limit,
    )?;
    match info {
        RespFrame::BulkString(Some(bytes)) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        _ => Err(io::Error::new(
            ErrorKind::InvalidData,
            "master INFO not a bulk string",
        )),
    }
}

/// (frankenredis-pkdgs) Once per `SENTINEL_PROBE_INTERVAL_MS`, PING + INFO every
/// monitored master and fold the result into the sentinel state (runid, role,
/// link liveness, discovered replicas) via the fr-store/fr-sentinel primitives.
/// Without this, a sentinel registers a master but never contacts it, so
/// SENTINEL MASTER reports an empty "master,disconnected" instance forever.
/// (frankenredis-pkdgs) A persistent connection SUBSCRIBEd to a monitored
/// master's `__sentinel__:hello` channel, drained non-blocking each iteration to
/// receive peer sentinels' hello announcements.
struct SentinelHelloSub {
    stream: StdTcpStream,
    buf: Vec<u8>,
}

/// Open + SUBSCRIBE + switch to non-blocking. None on any connect/IO failure
/// (the caller retries next tick).
fn open_sentinel_hello_sub(ip: &str, port: u16) -> Option<SentinelHelloSub> {
    let addr: SocketAddr = format!("{ip}:{port}").parse().ok()?;
    let mut stream = StdTcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
    stream
        .write_all(&replica_handshake_frame(&[b"SUBSCRIBE", b"__sentinel__:hello"]).to_bytes())
        .ok()?;
    stream.set_nonblocking(true).ok()?;
    Some(SentinelHelloSub {
        stream,
        buf: Vec::new(),
    })
}

/// Non-blocking drain of pending bytes, parsing pub/sub `message` frames on
/// `__sentinel__:hello` and folding each payload via runtime.sentinel_process_hello.
/// Err means the connection is dead/garbled and should be reopened.
fn drain_sentinel_hello_sub(
    sub: &mut SentinelHelloSub,
    runtime: &mut Runtime,
    parser_config: &ParserConfig,
    now_ms: u64,
) -> io::Result<()> {
    let mut tmp = [0u8; 8192];
    loop {
        match sub.stream.read(&mut tmp) {
            Ok(0) => return Err(io::Error::new(ErrorKind::UnexpectedEof, "hello sub closed")),
            Ok(n) => sub.buf.extend_from_slice(&tmp[..n]),
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        // Cap the buffer to avoid unbounded growth on a chatty channel.
        if sub.buf.len() > 1 << 20 {
            return Err(io::Error::new(ErrorKind::InvalidData, "hello sub overflow"));
        }
    }
    loop {
        match fr_protocol::parse_frame_with_config(&sub.buf, parser_config) {
            Ok(parsed) => {
                handle_sentinel_hello_frame(&parsed.frame, runtime, now_ms);
                sub.buf.drain(..parsed.consumed);
            }
            Err(RespParseError::Incomplete) => break,
            Err(_) => return Err(io::Error::new(ErrorKind::InvalidData, "bad hello frame")),
        }
    }
    Ok(())
}

/// A pub/sub push is `["message", "__sentinel__:hello", "<payload>"]`; ignore the
/// `subscribe` confirmation and anything else.
fn handle_sentinel_hello_frame(frame: &RespFrame, runtime: &mut Runtime, now_ms: u64) {
    if let RespFrame::Array(Some(items)) = frame
        && items.len() == 3
        && let (
            RespFrame::BulkString(Some(kind)),
            RespFrame::BulkString(Some(chan)),
            RespFrame::BulkString(Some(payload)),
        ) = (&items[0], &items[1], &items[2])
        && kind.eq_ignore_ascii_case(b"message")
        && chan.as_slice() == b"__sentinel__:hello"
        && let Ok(s) = std::str::from_utf8(payload)
    {
        runtime.sentinel_process_hello(s, now_ms);
    }
}

fn run_sentinel_monitoring_tick(
    runtime: &mut Runtime,
    now_ms: u64,
    last_probe_ms: &mut u64,
    hello_subs: &mut HashMap<String, SentinelHelloSub>,
) {
    if !runtime.sentinel_mode() {
        return;
    }
    // Advance the sentinel clock on EVERY event-loop iteration (cheap tilt
    // check), not just on a probe tick: SENTINEL MASTER/SLAVES render their
    // "ms-ago" delta fields as (previous_time - last_event_time). Advancing it
    // only once per probe pinned those fields to ~0; tracking real now makes
    // them report the true elapsed time since the last ping/info (0..interval),
    // matching redis's mstime()-based deltas.
    runtime.sentinel_begin_tick(now_ms);

    let parser_config = runtime.parser_config();
    // Snapshot (name, ip, port) so the blocking probes hold no borrow of the
    // sentinel state across the network IO.
    let targets = runtime.sentinel_monitor_targets();

    // Receive half of gossip (every iteration): keep a hello subscription per
    // monitored master and drain it so peer sentinels' hellos are ingested as
    // they arrive. Reconcile against the current master set first.
    {
        let current: HashSet<&str> = targets.iter().map(|(n, _, _)| n.as_str()).collect();
        hello_subs.retain(|name, _| current.contains(name.as_str()));
        for (name, ip, port) in &targets {
            if !hello_subs.contains_key(name)
                && let Some(conn) = open_sentinel_hello_sub(ip, *port)
            {
                hello_subs.insert(name.clone(), conn);
            }
            if let Some(conn) = hello_subs.get_mut(name)
                && drain_sentinel_hello_sub(conn, runtime, &parser_config, now_ms).is_err()
            {
                hello_subs.remove(name);
            }
        }
    }

    // Probe half (network IO) stays throttled.
    if now_ms.saturating_sub(*last_probe_ms) < SENTINEL_PROBE_INTERVAL_MS {
        return;
    }
    *last_probe_ms = now_ms;
    let query_buffer_limit = runtime.server.query_buffer_limit;
    for (name, ip, port) in &targets {
        // Gossip a hello on this master's __sentinel__:hello channel when due,
        // so peer sentinels discover this instance. Decided (and rate-limited)
        // by the store before the blocking probe sends it.
        let hello = runtime.sentinel_take_hello_to_publish(name, now_ms);
        let info = probe_sentinel_master(
            ip,
            *port,
            &parser_config,
            query_buffer_limit,
            hello.as_deref(),
        )
        .ok();
        runtime.apply_sentinel_probe_result(name, now_ms, info.as_deref());
    }
}

fn replica_handshake_frame(args: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        args.iter()
            .map(|arg| RespFrame::BulkString(Some(arg.to_vec())))
            .collect(),
    ))
}

fn encode_replication_snapshot(snapshot: &[u8]) -> Vec<u8> {
    // (frankenredis-og1y6) Vendored Redis 7.2.4
    // replication.c::sendBulkToSlave writes the bulk preamble
    // ("$<len>\r\n") followed by the raw RDB bytes — no trailing CRLF.
    // Tools that use SYNC as a debug subscriber (e.g. redis-cli --rdb)
    // expect this exact framing; an extra trailing CRLF gets parsed as
    // the next reply's type byte and surfaces as the spurious
    // "Protocol error, got \"\\r\" as reply type byte" warning.
    // read_replication_snapshot_from_stream tolerates either form, so
    // dropping the trailing CRLF is safe for fr's own replica path.
    let mut out = Vec::with_capacity(snapshot.len().saturating_add(32));
    out.extend_from_slice(b"$");
    out.extend_from_slice(snapshot.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(snapshot);
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
                // (frankenredis-replro) Mark this as a primary-stream replay so
                // the replica's read-only gate exempts these propagated writes
                // (upstream exempts the CLIENT_MASTER link). Toggled per frame so
                // an early return can never leave the flag stuck on.
                runtime.server.applying_master_stream = true;
                let response =
                    runtime.execute_frame(frame.clone(), now_ms.saturating_add(frame_index));
                runtime.server.applying_master_stream = false;
                if let RespFrame::Error(message) = &response {
                    eprintln!("warn: replica replay command failed for frame {frame:?}: {message}");
                }
                if let Some(follow_up) = replication_stream_follow_up_bytes(&frame, &response) {
                    connection.write_buf.extend_from_slice(&follow_up);
                    validate_replica_write_buffer_limit(
                        connection,
                        runtime.replica_link_output_hard_limit(),
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
        if let Err(err) = validate_replica_write_buffer_limit(
            connection,
            runtime.replica_link_output_hard_limit(),
        ) {
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
    argv: &[Vec<u8>],
    response: &RespFrame,
    now_ms: u64,
) -> Option<Vec<u8>> {
    if !is_replication_sync_frame(argv) {
        return None;
    }
    let RespFrame::SimpleString(line) = response else {
        return None;
    };
    if line.starts_with("FULLRESYNC ") {
        let snapshot = runtime.encoded_rdb_snapshot(now_ms);
        return Some(encode_replication_snapshot(snapshot.as_slice()));
    }
    if matches!(
        fr_repl::parse_psync_reply(line),
        Ok(fr_repl::PsyncReply::Continue { .. })
    ) {
        let offset = psync_requested_offset(argv)?;
        return Some(runtime.encoded_aof_stream_from_offset(offset));
    }
    None
}

pub(crate) fn is_replication_sync_frame(argv: &[Vec<u8>]) -> bool {
    argv.first()
        .is_some_and(|cmd| cmd.eq_ignore_ascii_case(b"PSYNC") || cmd.eq_ignore_ascii_case(b"SYNC"))
}

fn psync_requested_offset(argv: &[Vec<u8>]) -> Option<u64> {
    let offset_bytes = argv.get(2)?;
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

fn parse_waitaof_deadline_argv(argv: &[Vec<u8>], now_ms: u64) -> Option<u64> {
    if argv.len() != 4 {
        return None;
    }
    let timeout_ms: i64 = std::str::from_utf8(&argv[3]).ok()?.parse().ok()?;
    if timeout_ms < 0 {
        return None;
    }
    if timeout_ms == 0 {
        return Some(u64::MAX);
    }
    now_ms.checked_add(timeout_ms as u64)
}

fn parse_wait_deadline_argv(argv: &[Vec<u8>], now_ms: u64) -> Option<u64> {
    if argv.len() != 3 {
        return None;
    }
    let timeout_ms: i64 = std::str::from_utf8(&argv[2]).ok()?.parse().ok()?;
    if timeout_ms < 0 {
        return None;
    }
    if timeout_ms == 0 {
        return Some(u64::MAX);
    }
    now_ms.checked_add(timeout_ms as u64)
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
fn try_build_blocked_state(argv: &[Vec<u8>], now_ms: u64) -> Option<BlockedState> {
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
            BlockingOp::BLmpop {
                argv: argv.to_vec(),
            }
        } else {
            BlockingOp::BZmpop {
                argv: argv.to_vec(),
            }
        };
        Some(BlockedState { op, deadline_ms })
    } else if cmd.eq_ignore_ascii_case(b"XREAD") || cmd.eq_ignore_ascii_case(b"XREADGROUP") {
        let deadline_ms = parse_xread_block_deadline_argv(argv, now_ms)?;
        let op = if cmd.eq_ignore_ascii_case(b"XREAD") {
            BlockingOp::BXread {
                argv: argv.to_vec(),
            }
        } else {
            BlockingOp::BXreadgroup {
                argv: argv.to_vec(),
            }
        };
        Some(BlockedState { op, deadline_ms })
    } else if cmd.eq_ignore_ascii_case(b"WAITAOF") {
        let deadline_ms = parse_waitaof_deadline_argv(argv, now_ms)?;
        Some(BlockedState {
            op: BlockingOp::Waitaof {
                argv: argv.to_vec(),
            },
            deadline_ms,
        })
    } else if cmd.eq_ignore_ascii_case(b"WAIT") {
        let deadline_ms = parse_wait_deadline_argv(argv, now_ms)?;
        Some(BlockedState {
            op: BlockingOp::Wait {
                argv: argv.to_vec(),
            },
            deadline_ms,
        })
    } else {
        None
    }
}

fn waitaof_response_satisfies(argv: &[Vec<u8>], response: &RespFrame) -> bool {
    if argv.len() != 4 {
        return false;
    }
    let required_local = match std::str::from_utf8(&argv[1])
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(value) => value,
        None => return false,
    };
    let required_replicas = match std::str::from_utf8(&argv[2])
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(value) => value,
        None => return false,
    };
    let RespFrame::Array(Some(items)) = response else {
        return false;
    };
    if items.len() != 2 {
        return false;
    }
    let (RespFrame::Integer(local_ack), RespFrame::Integer(replica_acks)) = (&items[0], &items[1])
    else {
        return false;
    };
    *local_ack >= required_local && *replica_acks >= required_replicas
}

fn waitaof_should_block(argv: &[Vec<u8>], response: &RespFrame) -> bool {
    let Some(command) = argv.first() else {
        return false;
    };
    if !command.eq_ignore_ascii_case(b"WAITAOF") {
        return false;
    }
    // Only block when WAITAOF actually executed and returned its [numlocal,
    // numreplicas] array below threshold. Any other reply means the command was
    // NOT run as WAITAOF — queued as `+QUEUED` inside MULTI/EXEC, or rejected
    // with an error (e.g. numlocal set while appendonly is off) — and must be
    // delivered as-is. (frankenredis: WAIT/WAITAOF in MULTI must not block)
    if !matches!(response, RespFrame::Array(Some(_))) {
        return false;
    }
    !waitaof_response_satisfies(argv, response)
}

fn wait_response_satisfies(argv: &[Vec<u8>], response: &RespFrame) -> bool {
    if argv.len() != 3 {
        return false;
    }
    let required_replicas: i64 = match std::str::from_utf8(&argv[1])
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(value) => value,
        None => return false,
    };
    let RespFrame::Integer(acked_replicas) = response else {
        return false;
    };
    *acked_replicas >= required_replicas
}

fn wait_should_block(argv: &[Vec<u8>], response: &RespFrame) -> bool {
    let Some(command) = argv.first() else {
        return false;
    };
    if !command.eq_ignore_ascii_case(b"WAIT") {
        return false;
    }
    // Only block when WAIT actually executed and returned an integer ack count
    // below the requested replica count. A non-integer reply means the command
    // was NOT run as WAIT — queued as `+QUEUED` inside MULTI/EXEC, or rejected
    // with an error (e.g. on a replica) — and must be delivered as-is.
    // (frankenredis: WAIT/WAITAOF in MULTI must not block)
    if !matches!(response, RespFrame::Integer(_)) {
        return false;
    }
    !wait_response_satisfies(argv, response)
}

fn blocked_timeout_response(op: &BlockingOp, runtime: &mut Runtime, now_ms: u64) -> RespFrame {
    match op {
        BlockingOp::BLmove { .. } => RespFrame::BulkString(None),
        BlockingOp::Waitaof { argv } | BlockingOp::Wait { argv } => runtime.execute_frame(
            RespFrame::Array(Some(
                argv.iter()
                    .map(|arg| RespFrame::BulkString(Some(arg.clone())))
                    .collect(),
            )),
            now_ms,
        ),
        _ => RespFrame::Array(None),
    }
}

struct CheckBlockedClientsContext<'a> {
    clients: &'a mut HashMap<Token, ClientConnection>,
    blocked_tokens: &'a mut HashSet<Token>,
    blocked_wake_index: &'a mut BlockedWakeIndex,
    closing_tokens: &'a mut HashSet<Token>,
    paused_tokens: &'a mut HashSet<Token>,
    runtime: &'a mut Runtime,
    poll: &'a mut Poll,
    write_tokens: &'a mut HashSet<Token>,
    deferred_tokens: &'a mut HashSet<Token>,
    ts: u64,
    writer_pool: Option<&'a WriterPool>,
}

/// Check all blocked clients. Unblock them if their keys have data or
/// their timeout has expired.
fn check_blocked_clients(ctx: CheckBlockedClientsContext<'_>) {
    let CheckBlockedClientsContext {
        clients,
        blocked_tokens,
        blocked_wake_index,
        closing_tokens,
        paused_tokens,
        runtime,
        poll,
        write_tokens,
        deferred_tokens,
        ts,
        writer_pool,
    } = ctx;
    if blocked_tokens.is_empty() {
        runtime.clear_ready_keys();
        blocked_wake_index.clear();
        return;
    }

    let ready_keys = runtime.drain_ready_keys();
    let active_blocked = blocked_wake_index.candidates(&ready_keys, ts);

    for token in active_blocked {
        let Some(conn) = clients.get_mut(&token) else {
            blocked_tokens.remove(&token);
            blocked_wake_index.remove(token);
            continue;
        };
        let Some(blocked) = &conn.blocked else {
            blocked_tokens.remove(&token);
            blocked_wake_index.remove(token);
            continue;
        };

        let mut should_check = ts >= blocked.deadline_ms
            || matches!(
                &blocked.op,
                BlockingOp::Waitaof { .. } | BlockingOp::Wait { .. }
            );
        if !should_check && blocked.op.any_key_ready(&ready_keys) {
            should_check = true;
        }

        if !should_check {
            continue;
        }

        // Check timeout first.
        if ts >= blocked.deadline_ms {
            let resp3 = conn.session.resp_protocol_version() == 3;
            encode_client_reply(
                &blocked_timeout_response(&blocked.op, runtime, ts),
                resp3,
                &mut conn.write_buf,
            );
            conn.blocked = None;
            blocked_tokens.remove(&token);
            blocked_wake_index.remove(token);
            runtime.mark_client_unblocked(conn.session.client_id);

            // Process any commands the client pipelined while blocked.
            if !conn.read_buf.is_empty() {
                let session = std::mem::take(&mut conn.session);
                let prev = runtime.swap_session(session);
                let budget_exhausted = process_buffered_frames(
                    token,
                    conn,
                    runtime,
                    blocked_tokens,
                    blocked_wake_index,
                    closing_tokens,
                    write_tokens,
                    paused_tokens,
                    ts,
                    ts.saturating_mul(1000),
                );
                record_deferred_buffered_token(token, conn, deferred_tokens, budget_exhausted);
                let updated_session = runtime.swap_session(prev);
                conn.session = updated_session;
            }

            drive_client_output(
                token,
                conn,
                OutputDriveContext {
                    runtime,
                    poll,
                    write_tokens,
                    closing_tokens,
                    writer_pool,
                },
                false,
            );
            continue;
        }

        // Try to fulfill the blocking operation.
        let session = std::mem::take(&mut conn.session);
        let prev = runtime.swap_session(session);

        let result = try_fulfill_blocked(&blocked.op, runtime, ts);

        if let Some(response) = result {
            // (frankenredis-pgplm) Session is swapped into `runtime` here, so
            // its negotiated protocol drives the RESP3 null encoding.
            let resp3 = runtime.client_session().resp_protocol_version() == 3;
            encode_client_reply(&response, resp3, &mut conn.write_buf);
            conn.blocked = None;
            blocked_tokens.remove(&token);
            blocked_wake_index.remove(token);
            runtime.mark_client_unblocked(runtime.client_id());

            // Process any commands the client pipelined while blocked.
            if !conn.read_buf.is_empty() {
                // The session is already swapped in here.
                let budget_exhausted = process_buffered_frames(
                    token,
                    conn,
                    runtime,
                    blocked_tokens,
                    blocked_wake_index,
                    closing_tokens,
                    write_tokens,
                    paused_tokens,
                    ts,
                    ts.saturating_mul(1000),
                );
                record_deferred_buffered_token(token, conn, deferred_tokens, budget_exhausted);
            }
        }

        let updated_session = runtime.swap_session(prev);
        conn.session = updated_session;

        // Always arm write interest if there's pending output, even if the
        // client re-blocked during pipelined command processing — the response
        // from the first unblock still needs to reach the client.
        drive_client_output(
            token,
            conn,
            OutputDriveContext {
                runtime,
                poll,
                write_tokens,
                closing_tokens,
                writer_pool,
            },
            false,
        );
    }
}

struct PendingClientUnblocksContext<'a> {
    clients: &'a mut HashMap<Token, ClientConnection>,
    client_id_to_token: &'a HashMap<u64, Token>,
    blocked_tokens: &'a mut HashSet<Token>,
    blocked_wake_index: &'a mut BlockedWakeIndex,
    closing_tokens: &'a mut HashSet<Token>,
    paused_tokens: &'a mut HashSet<Token>,
    runtime: &'a mut Runtime,
    poll: &'a mut Poll,
    write_tokens: &'a mut HashSet<Token>,
    deferred_tokens: &'a mut HashSet<Token>,
    ts: u64,
    writer_pool: Option<&'a WriterPool>,
}

fn apply_pending_client_unblocks(ctx: PendingClientUnblocksContext<'_>) {
    let PendingClientUnblocksContext {
        clients,
        client_id_to_token,
        blocked_tokens,
        blocked_wake_index,
        closing_tokens,
        paused_tokens,
        runtime,
        poll,
        write_tokens,
        deferred_tokens,
        ts,
        writer_pool,
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
            blocked_wake_index.remove(token);
            continue;
        };
        let Some(blocked) = &conn.blocked else {
            runtime.mark_client_unblocked(client_id);
            blocked_tokens.remove(&token);
            blocked_wake_index.remove(token);
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

        let resp3 = conn.session.resp_protocol_version() == 3;
        encode_client_reply(&response, resp3, &mut conn.write_buf);
        conn.blocked = None;
        blocked_tokens.remove(&token);
        blocked_wake_index.remove(token);
        runtime.mark_client_unblocked(client_id);

        if !conn.read_buf.is_empty() {
            let session = std::mem::take(&mut conn.session);
            let prev = runtime.swap_session(session);
            let budget_exhausted = process_buffered_frames(
                token,
                conn,
                runtime,
                blocked_tokens,
                blocked_wake_index,
                closing_tokens,
                write_tokens,
                paused_tokens,
                ts,
                ts.saturating_mul(1000),
            );
            record_deferred_buffered_token(token, conn, deferred_tokens, budget_exhausted);
            let updated_session = runtime.swap_session(prev);
            conn.session = updated_session;
        }

        drive_client_output(
            token,
            conn,
            OutputDriveContext {
                runtime,
                poll,
                write_tokens,
                closing_tokens,
                writer_pool,
            },
            false,
        );
    }
}

struct DeferredBufferedClientsContext<'a> {
    clients: &'a mut HashMap<Token, ClientConnection>,
    blocked_tokens: &'a mut HashSet<Token>,
    blocked_wake_index: &'a mut BlockedWakeIndex,
    closing_tokens: &'a mut HashSet<Token>,
    write_tokens: &'a mut HashSet<Token>,
    paused_tokens: &'a mut HashSet<Token>,
    deferred_tokens: &'a mut HashSet<Token>,
    runtime: &'a mut Runtime,
    poll: &'a mut Poll,
    ts: u64,
    ts_us: u64,
    writer_pool: Option<&'a WriterPool>,
}

fn process_deferred_buffered_clients(ctx: DeferredBufferedClientsContext<'_>) {
    let DeferredBufferedClientsContext {
        clients,
        blocked_tokens,
        blocked_wake_index,
        closing_tokens,
        write_tokens,
        paused_tokens,
        deferred_tokens,
        runtime,
        poll,
        ts,
        ts_us,
        writer_pool,
    } = ctx;

    if deferred_tokens.is_empty() || runtime.is_client_paused(ts) {
        return;
    }

    let tokens: Vec<Token> = deferred_tokens.iter().copied().collect();
    for token in tokens {
        let Some(conn) = clients.get_mut(&token) else {
            deferred_tokens.remove(&token);
            continue;
        };

        if conn.read_buf.is_empty() || conn.closing || conn.blocked.is_some() {
            deferred_tokens.remove(&token);
            continue;
        }

        let session = std::mem::take(&mut conn.session);
        let prev = runtime.swap_session(session);
        let write_buf_before = conn.write_buf.len();
        let budget_exhausted = process_buffered_frames(
            token,
            conn,
            runtime,
            blocked_tokens,
            blocked_wake_index,
            closing_tokens,
            write_tokens,
            paused_tokens,
            ts,
            ts_us,
        );
        let output_delta = conn.write_buf.len().saturating_sub(write_buf_before);
        runtime.track_net_output_bytes(output_delta as u64);
        let updated_session = runtime.swap_session(prev);
        conn.session = updated_session;
        runtime.record_client_session(&conn.session);

        record_deferred_buffered_token(token, conn, deferred_tokens, budget_exhausted);
        drive_client_output(
            token,
            conn,
            OutputDriveContext {
                runtime,
                poll,
                write_tokens,
                closing_tokens,
                writer_pool,
            },
            false,
        );
    }
}

/// Try to fulfill a blocked operation by checking if the watched keys have
/// data. Returns Some(response) if fulfilled, None if still blocked.
fn try_fulfill_blocked(op: &BlockingOp, runtime: &mut Runtime, now_ms: u64) -> Option<RespFrame> {
    match op {
        BlockingOp::BLpop { keys } => {
            for key in keys {
                // Only a key that currently holds a LIST may serve a list
                // waiter. A non-list write (SET/SADD/HSET/…) must NOT unblock a
                // BLPOP — upstream signals readiness only on list pushes and
                // dispatches serve-by-type, so a wrong-type/absent key keeps the
                // client blocked (→ nil on timeout) rather than erroring.
                if !runtime.peek_is_list(key, now_ms) {
                    continue;
                }
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
                if !runtime.peek_is_list(key, now_ms) {
                    continue;
                }
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
                // Only a key currently holding a SORTED SET may serve a zset
                // waiter; a non-zset write must not unblock BZPOPMAX.
                if !runtime.peek_is_zset(key, now_ms) {
                    continue;
                }
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
                if !runtime.peek_is_zset(key, now_ms) {
                    continue;
                }
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
            // Only serve when the SOURCE (the awaited key) currently holds a
            // list — a non-list write to source must not unblock. Once source is
            // a real list, propagate whatever LMOVE returns, INCLUDING a
            // WRONGTYPE error from a wrong-type DESTINATION (upstream serves the
            // woken client and surfaces the dst error). (frankenredis blocking-serve type gate)
            if !runtime.peek_is_list(source, now_ms) {
                return None;
            }
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
            // *MPOP has no destination: a serve-time WRONGTYPE means one of the
            // awaited keys was overwritten with a non-list/non-zset value, which
            // upstream never signals as ready — stay blocked, don't error.
            if matches!(response, RespFrame::Error(_)) {
                None
            } else if response != RespFrame::Array(None) {
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
            // A serve-time WRONGTYPE means an awaited key was overwritten with a
            // non-stream value (e.g. SET); upstream never signals such a key as
            // stream-ready, so the client stays blocked (→ nil on timeout). Other
            // errors (e.g. NOGROUP from a destroyed consumer group) still
            // propagate, matching upstream. (frankenredis blocking-serve type gate)
            if let RespFrame::Error(msg) = &response {
                if msg.starts_with("WRONGTYPE") {
                    return None;
                }
                return Some(response);
            }
            // XREAD/XREADGROUP returns Array(None) when no data is available.
            if response == RespFrame::Array(None) {
                None
            } else {
                Some(response)
            }
        }
        BlockingOp::Waitaof { argv } => {
            let frame = RespFrame::Array(Some(
                argv.iter()
                    .map(|arg| RespFrame::BulkString(Some(arg.clone())))
                    .collect(),
            ));
            let response = runtime.execute_frame(frame, now_ms);
            if matches!(response, RespFrame::Error(_))
                || waitaof_response_satisfies(argv, &response)
            {
                Some(response)
            } else {
                None
            }
        }
        BlockingOp::Wait { argv } => {
            let frame = RespFrame::Array(Some(
                argv.iter()
                    .map(|arg| RespFrame::BulkString(Some(arg.clone())))
                    .collect(),
            ));
            let response = runtime.execute_frame(frame, now_ms);
            if matches!(response, RespFrame::Error(_)) || wait_response_satisfies(argv, &response) {
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
    poll: &mut Poll,
    write_tokens: &mut HashSet<Token>,
    closing_tokens: &mut HashSet<Token>,
    writer_pool: Option<&WriterPool>,
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
                drive_client_output(
                    token,
                    conn,
                    OutputDriveContext {
                        runtime,
                        poll,
                        write_tokens,
                        closing_tokens,
                        writer_pool,
                    },
                    false,
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
    writer_pool: Option<&WriterPool>,
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
        if conn.pending_output_bytes() > runtime.effective_output_hard_limit(conn.session.client_id)
        {
            eprintln!(
                "warn: client write buffer exceeded limit during monitor delivery, disconnecting"
            );
            conn.closing = true;
            closing_tokens.insert(token);
            continue;
        }
        drive_client_output(
            token,
            conn,
            OutputDriveContext {
                runtime,
                poll,
                write_tokens,
                closing_tokens,
                writer_pool,
            },
            false,
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
    writer_pool: Option<&WriterPool>,
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
            let resp3 = conn.session.resp_protocol_version() == 3;
            let frame =
                pubsub_message_to_frame_for_protocol(msg, conn.session.resp_protocol_version());
            // (frankenredis-o90ga) RESP3 clients must receive the RESP3 null
            // type (`_\r\n`) inside a flush invalidation push — encode_into
            // would emit the RESP2 `$-1\r\n`. Null-free messages encode
            // identically under both, so this is safe for all pubsub frames.
            if resp3 {
                frame.encode_into_resp3(&mut conn.write_buf);
            } else {
                frame.encode_into(&mut conn.write_buf);
            }
        }

        if conn.pending_output_bytes() > runtime.effective_output_hard_limit(conn.session.client_id)
        {
            eprintln!(
                "warn: client write buffer exceeded limit during pubsub delivery, disconnecting"
            );
            conn.closing = true;
            closing_tokens.insert(token);
            continue;
        }

        drive_client_output(
            token,
            conn,
            OutputDriveContext {
                runtime,
                poll,
                write_tokens,
                closing_tokens,
                writer_pool,
            },
            false,
        );
    }
}

/// Check if a command is allowed in subscription mode. Returns Some(error) if rejected.
fn check_subscription_mode_gate(argv: &[Vec<u8>], _in_sub_mode: bool) -> Option<RespFrame> {
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
    // Upstream networking.c uses c->cmd->fullname when formatting
    // the subscribe-mode rejection — for any container subcommand
    // that expands to "parent|sub" (br-frankenredis-pubsublower,
    // frankenredis-gcll9). The runtime helper does the same in
    // fr-runtime::pubsub_blocked_command_name; the TCP-server gate
    // mirrors that logic here.
    const CONTAINERS: &[&[u8]] = &[
        b"acl",
        b"client",
        b"cluster",
        b"command",
        b"config",
        b"debug",
        b"function",
        b"latency",
        b"memory",
        b"module",
        b"object",
        b"pubsub",
        b"script",
        b"slowlog",
        b"xgroup",
        b"xinfo",
    ];
    let cmd_lower = String::from_utf8_lossy(cmd).to_ascii_lowercase();
    let cmd_str = if CONTAINERS.contains(&cmd_lower.as_bytes())
        && let Some(sub) = argv.get(1)
    {
        format!(
            "{cmd_lower}|{}",
            String::from_utf8_lossy(sub).to_ascii_lowercase()
        )
    } else {
        cmd_lower
    };
    Some(RespFrame::Error(format!(
        "ERR Can't execute '{cmd_str}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context"
    )))
}

fn command_frame_can_move_to_argv(frame: &RespFrame) -> bool {
    let RespFrame::Array(Some(items)) = frame else {
        return false;
    };
    !items.is_empty()
        && items.iter().all(|item| {
            matches!(
                item,
                RespFrame::BulkString(Some(_)) | RespFrame::SimpleString(_) | RespFrame::Integer(_)
            )
        })
}

fn is_quit_frame(argv: &[Vec<u8>]) -> bool {
    argv.first()
        .is_some_and(|command| command.eq_ignore_ascii_case(b"QUIT"))
}

/// Encode a command reply to a client, choosing the RESP3 null encoding
/// (`_`) when the client negotiated RESP3. Frames are never mutated, so the
/// caller's block-detection comparisons against `BulkString(None)` /
/// `Array(None)` are unaffected. (frankenredis-pgplm)
#[inline]
fn encode_client_reply(frame: &RespFrame, resp3: bool, out: &mut Vec<u8>) {
    if resp3 {
        frame.encode_into_resp3(out);
    } else {
        frame.encode_into(out);
    }
}

fn suppress_client_network_reply(
    runtime: &Runtime,
    argv: &[Vec<u8>],
    response: &RespFrame,
) -> bool {
    if runtime.suppress_current_network_reply() {
        return true;
    }
    if matches!(response, RespFrame::Error(_)) {
        return false;
    }
    frame_matches_suppressed_replication_reply(argv)
}

fn frame_matches_suppressed_replication_reply(argv: &[Vec<u8>]) -> bool {
    match argv {
        [command, subcommand, argument] => {
            if !command.eq_ignore_ascii_case(b"REPLCONF") {
                return false;
            }
            if subcommand.eq_ignore_ascii_case(b"ACK") {
                return true;
            }
            subcommand.eq_ignore_ascii_case(b"GETACK") && argument == b"*"
        }
        [command] => command.eq_ignore_ascii_case(b"SYNC"),
        _ => false,
    }
}

struct OutputDriveContext<'a> {
    runtime: &'a mut Runtime,
    poll: &'a mut Poll,
    write_tokens: &'a mut HashSet<Token>,
    closing_tokens: &'a mut HashSet<Token>,
    writer_pool: Option<&'a WriterPool>,
}

fn drive_client_output(
    token: Token,
    conn: &mut ClientConnection,
    ctx: OutputDriveContext<'_>,
    allow_sync_fallback: bool,
) {
    if conn.writer_in_flight() {
        ctx.write_tokens.insert(token);
        conn.session.output_buffer_bytes = conn.pending_output_bytes();
        ensure_main_writable_disarmed(token, conn, ctx.poll);
        return;
    }

    if conn.write_buf.is_empty() {
        ctx.write_tokens.remove(&token);
        conn.session.output_buffer_bytes = 0;
        ensure_main_writable_disarmed(token, conn, ctx.poll);
        return;
    }

    // (frankenredis-fi8qp) Write-readiness scheduling: try ONE inline
    // non-blocking write before the writer-pool handoff. For the overwhelmingly
    // common small-reply case the socket accepts the whole buffer in a single
    // `write`, so we skip the writer-pool channel enqueue, the worker wakeup, and
    // its eventfd `Waker::wake()` syscall — the per-batch overhead the pipelined
    // GET/SET path pays on every reply today. A partial / `WouldBlock` write
    // falls through to hand the unwritten suffix to the writer pool (below) or to
    // arm WRITABLE: identical bytes, identical per-client ordering (nothing is in
    // flight — `writer_in_flight()` was false above). Gated on `allow_sync_fallback`
    // so callers that must not write synchronously keep the prior behaviour.
    if allow_sync_fallback {
        match conn.try_flush() {
            Ok(true) => {
                ctx.runtime.note_write_event();
                ctx.write_tokens.remove(&token);
                conn.session.output_buffer_bytes = 0;
                ensure_main_writable_disarmed(token, conn, ctx.poll);
                return;
            }
            Ok(false) => {
                // Partial write: `conn.write_buf` now holds the unwritten suffix;
                // fall through to offload that remainder (writer pool / WRITABLE).
            }
            Err(_) => {
                conn.write_failed = true;
                conn.closing = true;
                ctx.closing_tokens.insert(token);
                return;
            }
        }
    }

    if let Some(pool) = ctx.writer_pool
        && let Some(writer_stream) = conn.writer_stream.take()
    {
        let bytes = std::mem::take(&mut conn.write_buf);
        let byte_len = bytes.len();
        match pool.try_enqueue(token, writer_stream, bytes) {
            Ok(()) => {
                conn.writer_in_flight_bytes = byte_len;
                conn.session.output_buffer_bytes = conn.pending_output_bytes();
                ctx.write_tokens.insert(token);
                ensure_main_writable_disarmed(token, conn, ctx.poll);
                return;
            }
            Err(mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job)) => {
                conn.writer_stream = Some(job.stream);
                conn.write_buf = job.bytes;
            }
        }
    }

    if !allow_sync_fallback {
        arm_main_writable(token, conn, ctx.poll, ctx.write_tokens);
        conn.session.output_buffer_bytes = conn.pending_output_bytes();
        return;
    }

    match conn.try_flush() {
        Ok(true) => {
            ctx.runtime.note_write_event();
            ctx.write_tokens.remove(&token);
            ensure_main_writable_disarmed(token, conn, ctx.poll);
        }
        Ok(false) => {
            arm_main_writable(token, conn, ctx.poll, ctx.write_tokens);
        }
        Err(_) => {
            conn.write_failed = true;
            conn.closing = true;
            ctx.closing_tokens.insert(token);
        }
    }
    conn.session.output_buffer_bytes = conn.pending_output_bytes();
}

fn ensure_main_writable_disarmed(token: Token, conn: &mut ClientConnection, poll: &mut Poll) {
    if conn.main_writable_armed {
        let _ = poll
            .registry()
            .reregister(&mut conn.stream, token, Interest::READABLE);
        conn.main_writable_armed = false;
    }
}

fn arm_main_writable(
    token: Token,
    conn: &mut ClientConnection,
    poll: &mut Poll,
    write_tokens: &mut HashSet<Token>,
) {
    if conn.has_pending_output() {
        write_tokens.insert(token);
        if !conn.main_writable_armed {
            let _ = poll.registry().reregister(
                &mut conn.stream,
                token,
                Interest::READABLE | Interest::WRITABLE,
            );
            conn.main_writable_armed = true;
        }
    } else {
        write_tokens.remove(&token);
        ensure_main_writable_disarmed(token, conn, poll);
    }
}

fn drain_writer_completions(
    writer_pool: Option<&WriterPool>,
    clients: &mut HashMap<Token, ClientConnection>,
    runtime: &mut Runtime,
    poll: &mut Poll,
    write_tokens: &mut HashSet<Token>,
    closing_tokens: &mut HashSet<Token>,
) {
    let Some(pool) = writer_pool else {
        return;
    };

    loop {
        let completion = match pool.try_recv() {
            Ok(completion) => completion,
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => break,
        };
        let Some(conn) = clients.get_mut(&completion.token) else {
            continue;
        };

        conn.writer_stream = Some(completion.stream);
        conn.writer_in_flight_bytes = 0;

        match completion.status {
            WriterCompletionStatus::Drained => {
                runtime.note_write_event();
                if conn.write_buf.is_empty() {
                    write_tokens.remove(&completion.token);
                    ensure_main_writable_disarmed(completion.token, conn, poll);
                    conn.session.output_buffer_bytes = 0;
                } else {
                    drive_client_output(
                        completion.token,
                        conn,
                        OutputDriveContext {
                            runtime,
                            poll,
                            write_tokens,
                            closing_tokens,
                            writer_pool: Some(pool),
                        },
                        true,
                    );
                }
            }
            WriterCompletionStatus::WouldBlock => {
                prefix_writer_unsent_bytes(conn, completion.bytes);
                conn.session.output_buffer_bytes = conn.pending_output_bytes();
                arm_main_writable(completion.token, conn, poll, write_tokens);
            }
            WriterCompletionStatus::Failed(err) => {
                eprintln!("warn: client write error: {err}");
                conn.write_failed = true;
                conn.closing = true;
                conn.session.output_buffer_bytes = conn.pending_output_bytes();
                closing_tokens.insert(completion.token);
            }
        }
    }
}

fn prefix_writer_unsent_bytes(conn: &mut ClientConnection, mut unsent: Vec<u8>) {
    if unsent.is_empty() {
        return;
    }
    if conn.write_buf.is_empty() {
        conn.write_buf = unsent;
    } else {
        unsent.extend_from_slice(&conn.write_buf);
        conn.write_buf = unsent;
    }
}

fn handle_writable(
    token: Token,
    clients: &mut HashMap<Token, ClientConnection>,
    runtime: &mut Runtime,
    write_tokens: &mut HashSet<Token>,
    closing_tokens: &mut HashSet<Token>,
    poll: &mut Poll,
    writer_pool: Option<&WriterPool>,
) {
    let Some(conn) = clients.get_mut(&token) else {
        return;
    };

    // (frankenredis-k96mc) Only a flush of PENDING output is a write event. A
    // stray WRITABLE readiness on an already-drained buffer is not a
    // writeToClient call upstream, so it must not be counted.
    drive_client_output(
        token,
        conn,
        OutputDriveContext {
            runtime,
            poll,
            write_tokens,
            closing_tokens,
            writer_pool,
        },
        true,
    );
}

#[cfg(test)]
mod tests {
    use crate::{
        BlockingOp, CheckBlockedClientsContext, InlineParseResult, PendingClientUnblocksContext,
        REPLICA_ACK_INTERVAL_MS, REPLICA_RECONNECT_BACKOFF_MS, ReplicaPrimaryConnection,
        ReplicaSyncState, StartupConfig, apply_pending_client_unblocks, check_blocked_clients,
        check_subscription_mode_gate, command_frame_can_move_to_argv,
        consume_complete_replication_prefix, drain_replica_stream, drive_replica_sync,
        encode_eof_marked_replication_snapshot, encode_replication_snapshot, find_crlf,
        frame_matches_suppressed_replication_reply, is_quit_frame, parse_blocking_deadline,
        parse_xread_block_deadline_argv, process_buffered_frames, read_frame_from_stream,
        read_replication_snapshot_from_stream, replica_handshake_frame,
        replica_handshake_read_timeout, replication_follow_up_bytes, resolve_xread_block_argv,
        server_help_text, should_try_inline_parsing, startup_config_from_directives,
        sync_replica_with_primary, try_build_blocked_state, try_fulfill_blocked, wait_should_block,
        waitaof_should_block,
    };
    use fr_config::RuntimePolicy;
    use fr_protocol::{ParserConfig, RespFrame};
    use fr_runtime::Runtime;
    use mio::Token;
    use std::io::{ErrorKind, Write};
    use std::net::{TcpListener as StdTcpListener, TcpStream as StdTcpStream};
    use std::thread;

    fn test_argv(frame: RespFrame) -> Vec<Vec<u8>> {
        fr_command::frame_to_argv(&frame).expect("test command frame should produce argv")
    }

    #[test]
    fn borrowed_plain_get_packet_parser_accepts_canonical_get() {
        let input = b"*2\r\n$3\r\ngEt\r\n$3\r\nkey\r\n*1\r\n$4\r\nPING\r\n";
        let parsed = crate::parse_borrowed_plain_get_packet(input, &ParserConfig::default())
            .expect("canonical GET packet should parse");

        assert_eq!(parsed.key, b"key");
        assert_eq!(parsed.consumed, b"*2\r\n$3\r\ngEt\r\n$3\r\nkey\r\n".len());
    }

    #[test]
    fn borrowed_plain_get_packet_parser_defers_noncanonical_or_limited_inputs() {
        let cfg = ParserConfig::default();
        assert!(
            crate::parse_borrowed_plain_get_packet(b"*02\r\n$3\r\nGET\r\n$1\r\nk\r\n", &cfg)
                .is_none(),
            "noncanonical multibulk length stays on the generic parser"
        );
        assert!(
            crate::parse_borrowed_plain_get_packet(
                b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n",
                &ParserConfig {
                    max_array_len: 1,
                    ..ParserConfig::default()
                },
            )
            .is_none(),
            "array-limit errors stay on the generic parser"
        );
        assert!(
            crate::parse_borrowed_plain_get_packet(b"*2\r\n$3\r\nGET\r\n$2\r\nk\r\n", &cfg)
                .is_none(),
            "malformed bulk bodies stay on the generic parser"
        );
    }

    #[test]
    fn borrowed_plain_set_packet_parser_accepts_canonical_set() {
        let input = b"*3\r\n$3\r\nsEt\r\n$3\r\nkey\r\n$5\r\nvalue\r\n*1\r\n$4\r\nPING\r\n";
        let parsed = crate::parse_borrowed_plain_set_packet(input, &ParserConfig::default())
            .expect("canonical SET packet should parse");

        assert_eq!(parsed.key, b"key");
        assert_eq!(parsed.value, b"value");
        assert_eq!(
            parsed.consumed,
            b"*3\r\n$3\r\nsEt\r\n$3\r\nkey\r\n$5\r\nvalue\r\n".len()
        );
    }

    #[test]
    fn borrowed_plain_set_packet_parser_defers_noncanonical_or_limited_inputs() {
        let cfg = ParserConfig::default();
        assert!(
            crate::parse_borrowed_plain_set_packet(
                b"*03\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n",
                &cfg
            )
            .is_none(),
            "noncanonical multibulk length stays on the generic parser"
        );
        assert!(
            crate::parse_borrowed_plain_set_packet(
                b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n",
                &ParserConfig {
                    max_array_len: 2,
                    ..ParserConfig::default()
                },
            )
            .is_none(),
            "array-limit errors stay on the generic parser"
        );
        assert!(
            crate::parse_borrowed_plain_set_packet(
                b"*3\r\n$3\r\nSET\r\n$2\r\nk\r\n$1\r\nv\r\n",
                &cfg
            )
            .is_none(),
            "malformed bulk bodies stay on the generic parser"
        );
    }

    #[test]
    fn borrowed_plain_hset_packet_parser_accepts_canonical_single_field_hset() {
        let input =
            b"*4\r\n$4\r\nhSeT\r\n$3\r\nkey\r\n$5\r\nfield\r\n$5\r\nvalue\r\n*1\r\n$4\r\nPING\r\n";
        let parsed = crate::parse_borrowed_plain_hset_packet(input, &ParserConfig::default())
            .expect("canonical single-field HSET packet should parse");

        assert_eq!(parsed.key, b"key");
        assert_eq!(parsed.field, b"field");
        assert_eq!(parsed.value, b"value");
        assert_eq!(
            parsed.consumed,
            b"*4\r\n$4\r\nhSeT\r\n$3\r\nkey\r\n$5\r\nfield\r\n$5\r\nvalue\r\n".len()
        );
    }

    #[test]
    fn borrowed_plain_hset_packet_parser_defers_noncanonical_or_limited_inputs() {
        let cfg = ParserConfig::default();
        assert!(
            crate::parse_borrowed_plain_hset_packet(
                b"*04\r\n$4\r\nHSET\r\n$1\r\nk\r\n$1\r\nf\r\n$1\r\nv\r\n",
                &cfg
            )
            .is_none(),
            "noncanonical multibulk length stays on the generic parser"
        );
        assert!(
            crate::parse_borrowed_plain_hset_packet(
                b"*6\r\n$4\r\nHSET\r\n$1\r\nk\r\n$1\r\nf\r\n$1\r\nv\r\n$1\r\ng\r\n$1\r\nw\r\n",
                &cfg
            )
            .is_none(),
            "multi-field HSET stays on the generic parser"
        );
        assert!(
            crate::parse_borrowed_plain_hset_packet(
                b"*4\r\n$4\r\nHSET\r\n$1\r\nk\r\n$1\r\nf\r\n$1\r\nv\r\n",
                &ParserConfig {
                    max_array_len: 3,
                    ..ParserConfig::default()
                },
            )
            .is_none(),
            "array-limit errors stay on the generic parser"
        );
        assert!(
            crate::parse_borrowed_plain_hset_packet(
                b"*4\r\n$4\r\nHSET\r\n$2\r\nk\r\n$1\r\nf\r\n$1\r\nv\r\n",
                &cfg
            )
            .is_none(),
            "malformed bulk bodies stay on the generic parser"
        );
    }

    #[test]
    fn any_key_ready_matches_keys_membership_for_every_blocking_op() {
        use std::collections::HashSet;
        let ops = [
            BlockingOp::BLpop {
                keys: vec![b"a".to_vec(), b"b".to_vec()],
            },
            BlockingOp::BRpop {
                keys: vec![b"x".to_vec()],
            },
            BlockingOp::BZpopMax {
                keys: vec![b"z1".to_vec(), b"z2".to_vec()],
            },
            BlockingOp::BZpopMin {
                keys: vec![b"b".to_vec()],
            },
            BlockingOp::BLmove {
                source: b"src".to_vec(),
                destination: b"dst".to_vec(),
                wherefrom: b"LEFT".to_vec(),
                whereto: b"RIGHT".to_vec(),
            },
            // BLMPOP: [timeout, numkeys, k1, k2, LEFT, COUNT, 1]
            BlockingOp::BLmpop {
                argv: vec![
                    b"0".to_vec(),
                    b"2".to_vec(),
                    b"a".to_vec(),
                    b"k2".to_vec(),
                    b"LEFT".to_vec(),
                    b"COUNT".to_vec(),
                    b"1".to_vec(),
                ],
            },
            BlockingOp::BZmpop {
                argv: vec![
                    b"0".to_vec(),
                    b"1".to_vec(),
                    b"z2".to_vec(),
                    b"MIN".to_vec(),
                ],
            },
            // XREAD ... STREAMS s1 s2 0 0  -> keys s1, s2
            BlockingOp::BXread {
                argv: vec![
                    b"BLOCK".to_vec(),
                    b"0".to_vec(),
                    b"STREAMS".to_vec(),
                    b"s1".to_vec(),
                    b"s2".to_vec(),
                    b"0".to_vec(),
                    b"0".to_vec(),
                ],
            },
            BlockingOp::BXreadgroup {
                argv: vec![b"STREAMS".to_vec(), b"src".to_vec(), b">".to_vec()],
            },
            BlockingOp::Wait {
                argv: vec![b"1".to_vec(), b"100".to_vec()],
            },
            BlockingOp::Waitaof {
                argv: vec![b"1".to_vec(), b"0".to_vec(), b"0".to_vec()],
            },
        ];
        // Exercise empty, partial, full, and disjoint ready-sets.
        let ready_sets: Vec<HashSet<Vec<u8>>> = vec![
            HashSet::new(),
            ["a".as_bytes().to_vec()].into_iter().collect(),
            ["b".as_bytes().to_vec(), "z2".as_bytes().to_vec()]
                .into_iter()
                .collect(),
            ["src".as_bytes().to_vec()].into_iter().collect(),
            ["s2".as_bytes().to_vec()].into_iter().collect(),
            ["k2".as_bytes().to_vec()].into_iter().collect(),
            ["nope".as_bytes().to_vec()].into_iter().collect(),
        ];
        for op in &ops {
            for ready in &ready_sets {
                // Oracle: the old allocating path — clone keys, probe membership.
                let oracle = op.keys().iter().any(|k| ready.contains(k));
                assert_eq!(
                    op.any_key_ready(ready),
                    oracle,
                    "any_key_ready diverged from keys() for {op:?} / {ready:?}",
                );
            }
        }
    }

    #[test]
    fn any_key_ready_alloc_free_scan_agrees_with_keys_clone_at_scale() {
        use std::collections::HashSet;
        use std::time::Instant;
        // Tick scanning many blocked clients, each waiting on a few keys,
        // none ready (the common case: one unrelated key became ready, every
        // other blocked client re-scans). Doubles as a large-scale agreement
        // check; the ratio is informational (alloc-elimination is a FLOOR
        // improvement — the structural O(blocked)->O(ready+due) win lives in
        // the timeout-heap + reverse key-index, tracked by frankenredis-4pbq8).
        let blocked: Vec<BlockingOp> = (0..2000u32)
            .map(|i| BlockingOp::BLpop {
                keys: vec![
                    format!("queue:{i}:hi").into_bytes(),
                    format!("queue:{i}:lo").into_bytes(),
                    format!("queue:{i}:bk").into_bytes(),
                ],
            })
            .collect();
        let ready: HashSet<Vec<u8>> = ["unrelated".as_bytes().to_vec()].into_iter().collect();
        let iters = 400u32;

        // OLD: clone every client's keys each tick, then probe.
        let t0 = Instant::now();
        let mut hits0 = 0u64;
        for _ in 0..iters {
            for op in &blocked {
                if op.keys().iter().any(|k| ready.contains(k)) {
                    hits0 += 1;
                }
            }
        }
        let old_ns = t0.elapsed().as_nanos().max(1);

        // NEW: borrow-and-probe, zero allocation.
        let t1 = Instant::now();
        let mut hits1 = 0u64;
        for _ in 0..iters {
            for op in &blocked {
                if op.any_key_ready(&ready) {
                    hits1 += 1;
                }
            }
        }
        let new_ns = t1.elapsed().as_nanos().max(1);

        assert_eq!(hits0, hits1, "scan results diverged");
        let ratio = old_ns as f64 / new_ns as f64;
        println!(
            "blocked-tick scan (2000 clients x3 keys, miss): keys-clone {} ms -> borrow {} ms = {ratio:.2}x",
            old_ns / 1_000_000,
            new_ns / 1_000_000,
        );
        // New path must never be slower than the allocating one.
        assert!(
            new_ns <= old_ns + old_ns / 10,
            "alloc-free scan regressed: {ratio:.2}x"
        );
    }

    fn blocked_state(op: BlockingOp, deadline_ms: u64) -> crate::BlockedState {
        crate::BlockedState { op, deadline_ms }
    }

    #[test]
    fn blocked_wake_index_returns_only_ready_key_waiters() {
        use std::collections::HashSet;

        let mut index = crate::BlockedWakeIndex::default();
        for i in 0..2000usize {
            let state = blocked_state(
                BlockingOp::BLpop {
                    keys: vec![format!("queue:{i}").into_bytes()],
                },
                u64::MAX,
            );
            index.insert(Token(i + 1), &state);
        }

        let ready: HashSet<Vec<u8>> = [b"queue:1733".to_vec()].into_iter().collect();
        assert_eq!(index.candidates(&ready, 10), vec![Token(1734)]);
    }

    #[test]
    fn blocked_wake_index_preserves_per_key_fifo() {
        use std::collections::HashSet;

        let mut index = crate::BlockedWakeIndex::default();
        for token in [Token(11), Token(12), Token(13)] {
            let state = blocked_state(
                BlockingOp::BRpop {
                    keys: vec![b"queue".to_vec()],
                },
                u64::MAX,
            );
            index.insert(token, &state);
        }

        let ready: HashSet<Vec<u8>> = [b"queue".to_vec()].into_iter().collect();
        assert_eq!(
            index.candidates(&ready, 1),
            vec![Token(11), Token(12), Token(13)]
        );
    }

    #[test]
    fn blocked_wake_index_pops_only_due_timeouts() {
        let mut index = crate::BlockedWakeIndex::default();
        let finite = blocked_state(
            BlockingOp::BLpop {
                keys: vec![b"finite".to_vec()],
            },
            50,
        );
        let forever = blocked_state(
            BlockingOp::BLpop {
                keys: vec![b"forever".to_vec()],
            },
            u64::MAX,
        );
        index.insert(Token(1), &finite);
        index.insert(Token(2), &forever);

        assert!(
            index
                .candidates(&std::collections::HashSet::new(), 49)
                .is_empty()
        );
        assert_eq!(
            index.candidates(&std::collections::HashSet::new(), 50),
            vec![Token(1)]
        );
        assert!(
            index
                .candidates(&std::collections::HashSet::new(), 51)
                .is_empty()
        );
    }

    #[test]
    fn blocked_wake_index_wait_ops_are_tick_candidates() {
        let mut index = crate::BlockedWakeIndex::default();
        let wait = blocked_state(
            BlockingOp::Wait {
                argv: vec![b"WAIT".to_vec(), b"1".to_vec(), b"100".to_vec()],
            },
            100,
        );
        let waitaof = blocked_state(
            BlockingOp::Waitaof {
                argv: vec![
                    b"WAITAOF".to_vec(),
                    b"1".to_vec(),
                    b"0".to_vec(),
                    b"100".to_vec(),
                ],
            },
            100,
        );
        index.insert(Token(1), &wait);
        index.insert(Token(2), &waitaof);

        assert_eq!(
            index.candidates(&std::collections::HashSet::new(), 1),
            vec![Token(1), Token(2)]
        );
    }

    #[test]
    fn blocked_wake_index_ignores_stale_reinserted_token() {
        use std::collections::HashSet;

        let mut index = crate::BlockedWakeIndex::default();
        let first = blocked_state(
            BlockingOp::BLpop {
                keys: vec![b"old".to_vec()],
            },
            u64::MAX,
        );
        let second = blocked_state(
            BlockingOp::BLpop {
                keys: vec![b"new".to_vec()],
            },
            u64::MAX,
        );
        index.insert(Token(7), &first);
        index.remove(Token(7));
        index.insert(Token(7), &second);

        let old_ready: HashSet<Vec<u8>> = [b"old".to_vec()].into_iter().collect();
        let new_ready: HashSet<Vec<u8>> = [b"new".to_vec()].into_iter().collect();
        assert!(index.candidates(&old_ready, 1).is_empty());
        assert_eq!(index.candidates(&new_ready, 1), vec![Token(7)]);
    }

    #[test]
    fn blocked_wake_index_avoids_full_blocked_scan_at_scale() {
        use std::collections::HashSet;
        use std::time::Instant;

        let blocked: Vec<(Token, BlockingOp)> = (0..10_000usize)
            .map(|i| {
                (
                    Token(i + 1),
                    BlockingOp::BLpop {
                        keys: vec![
                            format!("queue:{i}:a").into_bytes(),
                            format!("queue:{i}:b").into_bytes(),
                            format!("queue:{i}:c").into_bytes(),
                        ],
                    },
                )
            })
            .collect();
        let ready: HashSet<Vec<u8>> = [b"queue:7333:b".to_vec()].into_iter().collect();
        let iters = 200u32;

        let t0 = Instant::now();
        let mut scan_hits = Vec::new();
        for _ in 0..iters {
            scan_hits.clear();
            for (token, op) in &blocked {
                if op.any_key_ready(&ready) {
                    scan_hits.push(*token);
                }
            }
        }
        let scan_ns = t0.elapsed().as_nanos().max(1);

        let mut index = crate::BlockedWakeIndex::default();
        for (token, op) in &blocked {
            index.insert(*token, &blocked_state(op.clone(), u64::MAX));
        }

        let t1 = Instant::now();
        let mut indexed_hits = Vec::new();
        for _ in 0..iters {
            indexed_hits = index.candidates(&ready, 1);
        }
        let index_ns = t1.elapsed().as_nanos().max(1);

        assert_eq!(scan_hits, vec![Token(7334)]);
        assert_eq!(indexed_hits, scan_hits);
        let ratio = scan_ns as f64 / index_ns as f64;
        println!(
            "blocked wake candidates (10000 clients x3 keys, one ready): scan {} ms -> index {} us = {ratio:.2}x",
            scan_ns / 1_000_000,
            index_ns / 1_000,
        );
        assert!(
            index_ns.saturating_mul(5) < scan_ns,
            "indexed wake candidates insufficiently faster: {ratio:.2}x",
        );
    }

    #[test]
    fn server_bootstrap_creates_runtime() {
        let _strict = Runtime::new(RuntimePolicy::default());
        let _hardened = Runtime::new(RuntimePolicy::hardened());
    }

    #[test]
    fn frame_suppression_matcher_preserves_replication_control_cases() {
        fn bulk(bytes: &[u8]) -> RespFrame {
            RespFrame::BulkString(Some(bytes.to_vec()))
        }

        fn array(items: Vec<RespFrame>) -> RespFrame {
            RespFrame::Array(Some(items))
        }

        assert!(frame_matches_suppressed_replication_reply(&test_argv(
            array(vec![bulk(b"REPLCONF"), bulk(b"ACK"), bulk(b"123"),])
        )));
        assert!(frame_matches_suppressed_replication_reply(&test_argv(
            array(vec![
                bulk(b"replconf"),
                bulk(b"getack"),
                RespFrame::SimpleString("*".to_string()),
            ])
        )));
        assert!(frame_matches_suppressed_replication_reply(&test_argv(
            array(vec![RespFrame::SimpleString("sync".to_string()),])
        )));

        assert!(!frame_matches_suppressed_replication_reply(&test_argv(
            array(vec![bulk(b"HGET"), bulk(b"hash"), bulk(b"field"),])
        )));
        assert!(!frame_matches_suppressed_replication_reply(&test_argv(
            array(vec![bulk(b"REPLCONF"), bulk(b"GETACK"), bulk(b"1"),])
        )));
        assert!(!frame_matches_suppressed_replication_reply(&test_argv(
            array(vec![bulk(b"REPLCONF"), bulk(b"ACK"),])
        )));
        assert!(!frame_matches_suppressed_replication_reply(&test_argv(
            array(vec![
                bulk(b"REPLCONF"),
                bulk(b"ACK"),
                bulk(b"1"),
                bulk(b"extra"),
            ])
        )));
        assert!(!frame_matches_suppressed_replication_reply(&test_argv(
            array(vec![RespFrame::Integer(0),])
        )));
    }

    #[test]
    fn quit_frame_matcher_borrows_first_command_token() {
        fn bulk(bytes: &[u8]) -> RespFrame {
            RespFrame::BulkString(Some(bytes.to_vec()))
        }

        fn array(items: Vec<RespFrame>) -> RespFrame {
            RespFrame::Array(Some(items))
        }

        assert!(is_quit_frame(&test_argv(array(vec![bulk(b"QUIT")]))));
        assert!(is_quit_frame(&test_argv(array(vec![bulk(b"quit")]))));
        assert!(is_quit_frame(&test_argv(array(vec![
            RespFrame::SimpleString("QuIt".to_string(),)
        ]))));

        assert!(!is_quit_frame(&test_argv(array(vec![bulk(b"HGET")]))));
        assert!(!command_frame_can_move_to_argv(&array(Vec::new())));
        assert!(!command_frame_can_move_to_argv(&RespFrame::BulkString(
            Some(b"QUIT".to_vec())
        )));
        assert!(!is_quit_frame(&test_argv(array(vec![RespFrame::Integer(
            0
        )]))));
        assert!(!command_frame_can_move_to_argv(&array(vec![
            RespFrame::BulkString(None)
        ])));
    }

    #[test]
    fn wait_block_matchers_preserve_command_filter_and_satisfaction() {
        fn bulk(bytes: &[u8]) -> RespFrame {
            RespFrame::BulkString(Some(bytes.to_vec()))
        }

        fn array(items: Vec<RespFrame>) -> RespFrame {
            RespFrame::Array(Some(items))
        }

        let non_wait = test_argv(array(vec![bulk(b"HGET"), bulk(b"key")]));
        assert!(!wait_should_block(&non_wait, &RespFrame::Integer(0)));
        assert!(!waitaof_should_block(
            &non_wait,
            &RespFrame::Array(Some(vec![RespFrame::Integer(0), RespFrame::Integer(0)])),
        ));

        let wait = test_argv(array(vec![bulk(b"WAIT"), bulk(b"2"), bulk(b"100")]));
        assert!(wait_should_block(&wait, &RespFrame::Integer(1)));
        assert!(!wait_should_block(&wait, &RespFrame::Integer(2)));

        let waitaof = test_argv(array(vec![
            bulk(b"WAITAOF"),
            bulk(b"1"),
            bulk(b"2"),
            bulk(b"100"),
        ]));
        assert!(waitaof_should_block(
            &waitaof,
            &RespFrame::Array(Some(vec![RespFrame::Integer(1), RespFrame::Integer(1)])),
        ));
        assert!(!waitaof_should_block(
            &waitaof,
            &RespFrame::Array(Some(vec![RespFrame::Integer(1), RespFrame::Integer(2)])),
        ));

        let simple_wait = test_argv(array(vec![
            RespFrame::SimpleString("wait".to_string()),
            bulk(b"1"),
            bulk(b"0"),
        ]));
        assert!(wait_should_block(&simple_wait, &RespFrame::Integer(0)));

        // A `+QUEUED` reply (WAIT/WAITAOF queued inside MULTI) must NOT block —
        // the command runs non-blocking at EXEC time. Likewise an error reply
        // (e.g. WAIT on a replica, WAITAOF with numlocal but appendonly off)
        // must be delivered as-is, not blocked on. Regression for the bug where
        // these matchers keyed only on argv[0] and treated any non-executed
        // reply as "unsatisfied → block". (verified vs redis 7.2.4)
        let queued = RespFrame::SimpleString("QUEUED".to_string());
        assert!(!wait_should_block(&wait, &queued));
        assert!(!waitaof_should_block(&waitaof, &queued));
        let wait_err = RespFrame::Error("ERR WAIT cannot be used with replica instances.".into());
        assert!(!wait_should_block(&wait, &wait_err));
        let waitaof_err = RespFrame::Error(
            "ERR WAITAOF cannot be used when numlocal is set but appendonly is disabled.".into(),
        );
        assert!(!waitaof_should_block(&waitaof, &waitaof_err));
    }

    #[test]
    fn startup_config_from_directives_extracts_basic_redis_conf_subset() {
        let parsed = fr_config::parse_redis_config(
            "bind 127.0.0.1 ::1\n\
             port 6381\n\
             requirepass \"top secret\"\n\
             masteruser repl\n\
             masterauth repl-secret\n\
             replicaof primary.local 6380\n\
             dir /tmp/frankenredis-startup\n\
             dbfilename startup.rdb\n\
             appendonly yes\n\
             appenddirname aof-from-config\n\
             appendfilename startup.aof\n\
             aclfile /tmp/frankenredis-startup/users.acl\n\
             timeout 30\n",
        )
        .expect("parse config file");

        let config = startup_config_from_directives(&parsed.directives)
            .expect("extract startup config subset");

        assert_eq!(
            config,
            StartupConfig {
                bind_addr: Some("127.0.0.1".to_string()),
                port: Some(6381),
                requirepass: Some(Some(b"top secret".to_vec())),
                masteruser: Some(Some("repl".to_string())),
                masterauth: Some(Some("repl-secret".to_string())),
                replicaof: Some(Some(("primary.local".to_string(), 6380))),
                dir: Some("/tmp/frankenredis-startup".to_string()),
                dbfilename: Some("startup.rdb".to_string()),
                appendonly: Some(true),
                appenddirname: Some("aof-from-config".to_string()),
                appendfilename: Some("startup.aof".to_string()),
                aclfile: Some("/tmp/frankenredis-startup/users.acl".to_string()),
                enable_debug_command: None,
            }
        );
        assert_eq!(
            config.configured_rdb_path(),
            Some("/tmp/frankenredis-startup/startup.rdb".to_string())
        );
        assert_eq!(
            config.configured_aof_path(),
            Some("/tmp/frankenredis-startup/aof-from-config/startup.aof".to_string())
        );
    }

    #[test]
    fn startup_config_from_directives_accepts_slaveof_no_one_alias() {
        let parsed =
            fr_config::parse_redis_config("slaveof no one\n").expect("parse slaveof config");

        let config = startup_config_from_directives(&parsed.directives)
            .expect("extract startup config subset");

        assert_eq!(config.replicaof, Some(None));
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
        assert!(help.contains("Runtime mode: strict or hardened (default: strict)"));
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
        let argv = test_argv(frame);

        let follow_up = replication_follow_up_bytes(&mut runtime, &argv, &response, 2)
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
        // (frankenredis-og1y6) Vendored Redis 7.2.4 sendBulkToSlave does
        // NOT append a trailing CRLF after the RDB bulk bytes — clients
        // that read the next reply byte (e.g. redis-cli --rdb) would
        // otherwise parse the stray \r as a malformed reply type byte.
        assert!(
            trailing.is_empty(),
            "snapshot payload should not be followed by CRLF (vendored sendBulkToSlave omits it); got {trailing:?}"
        );
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
        let argv = test_argv(frame);

        assert_eq!(
            replication_follow_up_bytes(&mut runtime, &argv, &response, 0),
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
        let argv = test_argv(frame);

        assert_eq!(
            replication_follow_up_bytes(&mut runtime, &argv, &response, 0),
            None
        );
    }

    #[test]
    fn psync_continue_emits_aof_backlog_tail() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        // (frankenredis-rl0qz) capture_aof_record is gated on
        // master+no-replicas+no-aof — the SETs below would not advance
        // primary_offset (and would not be captured into the backlog
        // buffer) without first latching the any-replica-ever-connected
        // flag. Trigger it via PSYNC ? -1 (the same handshake real
        // replicas use), which calls ensure_replica internally.
        runtime.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"PSYNC".to_vec())),
                RespFrame::BulkString(Some(b"?".to_vec())),
                RespFrame::BulkString(Some(b"-1".to_vec())),
            ])),
            0,
        );
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
        let argv = test_argv(frame);

        let follow_up = replication_follow_up_bytes(&mut runtime, &argv, &response, 3)
            .expect("psync continue should emit backlog");
        let psync2_response = RespFrame::SimpleString(
            "CONTINUE 0000000000000000000000000000000000000000".to_string(),
        );
        assert_eq!(
            replication_follow_up_bytes(&mut runtime, &argv, &psync2_response, 3),
            Some(follow_up.clone())
        );
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
                // ROLE reply now emits -1 for any replica state other than
                // "connected" (fr-runtime::handle_role_command:9437). Upstream
                // Redis follows the same convention: unknown/pre-handshake
                // offset is reported as -1, not 0. (br-frankenredis-ea1j)
                RespFrame::Integer(-1),
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
        // A tiny slave-class hard limit forces the replica-link write guard to
        // trip (0 would mean unlimited under redis semantics). (frankenredis-8sb0l)
        runtime.server.client_output_buffer_limits.slave.hard = 1;
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
    fn main_writable_interest_tracks_only_real_writable_arm() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = StdTcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut stream = mio::net::TcpStream::from_std(server);
        let mut poll = mio::Poll::new().unwrap();
        let token = Token(crate::MAX_LISTENERS + 71);
        poll.registry()
            .register(&mut stream, token, mio::Interest::READABLE)
            .unwrap();

        let runtime = Runtime::default_strict();
        let session = runtime.new_session();
        let mut conn = crate::ClientConnection::new(stream, session, 1_000);
        let mut write_tokens = std::collections::HashSet::new();

        crate::ensure_main_writable_disarmed(token, &mut conn, &mut poll);
        assert!(!conn.main_writable_armed);

        conn.write_buf.extend_from_slice(b"+OK\r\n");
        crate::arm_main_writable(token, &mut conn, &mut poll, &mut write_tokens);
        assert!(conn.main_writable_armed);
        assert!(write_tokens.contains(&token));

        crate::arm_main_writable(token, &mut conn, &mut poll, &mut write_tokens);
        assert!(conn.main_writable_armed);
        assert!(write_tokens.contains(&token));

        conn.write_buf.clear();
        crate::arm_main_writable(token, &mut conn, &mut poll, &mut write_tokens);
        assert!(!conn.main_writable_armed);
        assert!(!write_tokens.contains(&token));
    }

    #[test]
    fn inline_command_parsing() {
        let parsed = fr_server::try_parse_inline(b"SET key value\r\n").expect("parse inline");
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
        let args = fr_server::split_inline_args(b"SET key \"hello world\"").expect("parse quoted");
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], b"SET");
        assert_eq!(args[1], b"key");
        assert_eq!(args[2], b"hello world");
    }

    #[test]
    fn inline_splits_on_cr_and_lf_like_sdssplitargs() {
        // Upstream sds.c::sdssplitargs separates tokens on ' ' / '\t' / '\r' /
        // '\n' anywhere in the line, not just at the trailing CRLF. An embedded
        // CR must split the token: `SET\rk\rv` parses as three args, matching
        // redis (which then runs it as `SET k v`).
        let args = fr_server::split_inline_args(b"SET\rk\rv").expect("parse cr-separated");
        assert_eq!(args, vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
        let args = fr_server::split_inline_args(b"PING\nPING").expect("parse lf-separated");
        assert_eq!(args, vec![b"PING".to_vec(), b"PING".to_vec()]);
        // A CR inside a quoted run is preserved, not treated as a separator.
        let args = fr_server::split_inline_args(b"SET qk \"a\rb\"").expect("parse quoted cr");
        assert_eq!(
            args,
            vec![b"SET".to_vec(), b"qk".to_vec(), b"a\rb".to_vec()]
        );
    }

    #[test]
    fn inline_unbalanced_double_quotes_rejected() {
        let result = fr_server::split_inline_args(b"SET key \"unclosed");
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "ERR Protocol error: unbalanced quotes in request"
        );
    }

    #[test]
    fn inline_unbalanced_single_quotes_rejected() {
        let result = fr_server::split_inline_args(b"SET key 'unclosed");
        assert!(result.is_err());
    }

    #[test]
    fn inline_unbalanced_quotes_via_try_parse() {
        // Unbalanced quotes is an inline protocol error: it surfaces as an Err
        // so handle_parse_error replies and closes the connection, matching
        // upstream processInlineBuffer's setProtocolError.
        let result = fr_server::try_parse_inline(b"SET key \"unclosed\r\n");
        assert_eq!(
            result,
            Err(fr_protocol::RespParseError::UnbalancedInlineQuotes)
        );
        assert_eq!(
            fr_protocol::RespParseError::UnbalancedInlineQuotes.to_string(),
            "unbalanced quotes in request"
        );
    }

    #[test]
    fn inline_incomplete_returns_error() {
        let result = fr_server::try_parse_inline(b"SET key value");
        assert!(result.is_err());
    }

    #[test]
    fn blank_inline_line_is_consumed_without_command() {
        let result = fr_server::try_parse_inline(b"\r\n").expect("blank line should parse");
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
    fn parse_blocking_deadline_handles_signed_zero_and_subnormals() {
        // (frankenredis-p0rr) Edge cases that are easy to silently
        // regress — pin them as a single test so a future refactor
        // that uses sign-bit checks or restructures the zero/finite
        // gates can't accidentally turn a 'block forever' request into
        // a 'time out immediately' one.

        // -0.0 == 0.0 in IEEE-754, so negative-zero literals must
        // hit the zero-timeout (block forever) path, NOT the
        // negative-rejection path.
        assert_eq!(parse_blocking_deadline(b"-0", 123), Some(u64::MAX));
        assert_eq!(parse_blocking_deadline(b"-0.0", 123), Some(u64::MAX));
        assert_eq!(parse_blocking_deadline(b"-0e0", 123), Some(u64::MAX));

        // Scientific-notation zeros must also map to block-forever,
        // not to a far-future deadline.
        assert_eq!(parse_blocking_deadline(b"0e100", 123), Some(u64::MAX));
        assert_eq!(parse_blocking_deadline(b"0.0e0", 123), Some(u64::MAX));
        assert_eq!(parse_blocking_deadline(b"0.0e-100", 123), Some(u64::MAX));

        // Subnormal/underflow positives must still produce a valid
        // bounded deadline (rounded up to >=1ms by the ceil step) —
        // never None and never the block-forever sentinel.
        for tiny in ["1e-300", "1e-323", "1e-200", "0.000000000001"] {
            let deadline = parse_blocking_deadline(tiny.as_bytes(), 1_000)
                .unwrap_or_else(|| panic!("subnormal {tiny} should yield a deadline"));
            assert_ne!(
                deadline,
                u64::MAX,
                "subnormal {tiny} must NOT map to block-forever"
            );
            assert!(
                deadline > 1_000,
                "subnormal {tiny} should round up to a future deadline > now_ms, got {deadline}"
            );
        }

        // Plain positive zero stays at block-forever (regression
        // guard alongside the variants above).
        assert_eq!(parse_blocking_deadline(b"0", 123), Some(u64::MAX));
        assert_eq!(parse_blocking_deadline(b"0.0", 123), Some(u64::MAX));
    }

    #[test]
    fn blocking_state_builder_rejects_nonfinite_blpop_timeout() {
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"BLPOP".to_vec())),
            RespFrame::BulkString(Some(b"queue".to_vec())),
            RespFrame::BulkString(Some(b"inf".to_vec())),
        ]));
        let argv = test_argv(frame);
        assert!(try_build_blocked_state(&argv, 1_000).is_none());
    }

    #[test]
    fn blocking_state_builder_rounds_fractional_blpop_timeout_up() {
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"BLPOP".to_vec())),
            RespFrame::BulkString(Some(b"queue".to_vec())),
            RespFrame::BulkString(Some(b"0.0001".to_vec())),
        ]));
        let argv = test_argv(frame);
        let blocked = try_build_blocked_state(&argv, 1_000).expect("must block");
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
        // A non-zset write (SET) to the awaited key must NOT serve/unblock a
        // BZPOPMAX waiter: upstream signals readiness only on zset adds and
        // dispatches serve-by-type, so the client stays blocked (→ nil on
        // timeout) rather than receiving a spurious WRONGTYPE. (verified vs
        // redis 7.2.4: BZPOPMIN/BZPOPMAX woken by SET → nil)
        assert!(
            response.is_none(),
            "expected to stay blocked (None) on a non-zset key, got {response:?}"
        );
    }

    #[test]
    fn xread_block_stays_blocked_when_key_becomes_wrong_type() {
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
        // A non-stream write (SET) to the awaited key must NOT serve/unblock an
        // XREAD BLOCK waiter with a spurious WRONGTYPE: upstream signals
        // stream-readiness only on XADD, so the client stays blocked (→ nil on
        // timeout). (verified vs redis 7.2.4: XREAD BLOCK woken by SET → nil)
        assert!(
            response.is_none(),
            "expected to stay blocked (None) on a non-stream key, got {response:?}"
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

    // (frankenredis) Over-limit connections must receive the upstream
    // "-ERR max number of clients reached" reply before the socket closes,
    // matching networking.c::acceptCommonHandler — not a bare TCP reset.
    #[test]
    fn accept_over_maxclients_replies_error_before_close() {
        use mio::{Poll, Token};
        use std::collections::HashMap;
        use std::io::Read as _;
        use std::time::Duration;

        let mut runtime = Runtime::default_strict();
        runtime.server.max_clients = 1;

        let mut poll = Poll::new().unwrap();
        let listener = mio::net::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let mut clients: HashMap<Token, crate::ClientConnection> = HashMap::new();
        let mut client_id_to_token: HashMap<u64, Token> = HashMap::new();
        let mut next_handle = crate::MAX_LISTENERS;

        // Occupy the single allowed client slot so the next accept is over-limit.
        let _filler_client = StdTcpStream::connect(addr).unwrap();
        std::thread::sleep(Duration::from_millis(40));
        let (filler_srv, _) = listener.accept().unwrap();
        let sess = runtime.new_session();
        let cid = sess.client_id;
        clients.insert(
            Token(100),
            crate::ClientConnection::new(filler_srv, sess, 1_000),
        );
        client_id_to_token.insert(cid, Token(100));
        assert_eq!(clients.len(), 1);

        // The over-limit client connects and sends a request the server never reads.
        let mut over_client = StdTcpStream::connect(addr).unwrap();
        over_client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let _ = over_client.write_all(b"*1\r\n$4\r\nPING\r\n");
        std::thread::sleep(Duration::from_millis(40));

        crate::accept_connections(
            &listener,
            &mut poll,
            &mut clients,
            &mut client_id_to_token,
            &mut next_handle,
            &mut runtime,
            false,
        );

        // The over-limit connection was rejected (not admitted) and got the reply.
        assert_eq!(
            clients.len(),
            1,
            "over-limit connection must not be admitted"
        );
        let mut buf = [0u8; 64];
        let n = over_client.read(&mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"-ERR max number of clients reached\r\n",
            "client must receive the upstream error before close"
        );
    }

    // (frankenredis) A command deferred by CLIENT PAUSE must auto-execute once the
    // pause window expires, even with no further socket I/O on that connection —
    // mio is edge-triggered, so the bytes buffered in read_buf would otherwise
    // hang forever. Drives release_expired_client_pause across the deadline.
    #[test]
    fn client_pause_releases_deferred_command_after_deadline() {
        use mio::{Poll, Token};
        use std::collections::{HashMap, HashSet};
        use std::io::Read as _;
        use std::time::Duration;

        let mut runtime = Runtime::default_strict();
        let mut poll = Poll::new().unwrap();
        let listener = mio::net::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let mut client = StdTcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        std::thread::sleep(Duration::from_millis(40));
        let (srv, _) = listener.accept().unwrap();

        let session = runtime.new_session();
        let token = Token(crate::MAX_LISTENERS + 1);
        let mut conn = crate::ClientConnection::new(srv, session, 1_000);
        // A write command sitting in read_buf, deferred while paused.
        conn.read_buf.extend_from_slice(
            &RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SET".to_vec())),
                RespFrame::BulkString(Some(b"pk".to_vec())),
                RespFrame::BulkString(Some(b"pv".to_vec())),
            ]))
            .to_bytes(),
        );

        let mut clients: HashMap<Token, crate::ClientConnection> = HashMap::new();
        clients.insert(token, conn);
        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens: HashSet<Token> = HashSet::new();
        paused_tokens.insert(token);
        let mut deferred_tokens = HashSet::new();

        // Pause active until ts=1000.
        runtime.server.client_pause_deadline_ms = 1_000;
        runtime.server.client_pause_all = true;

        // Before the deadline: nothing is released, the command stays buffered.
        crate::release_expired_client_pause(
            &mut clients,
            &mut runtime,
            &mut poll,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            &mut deferred_tokens,
            500,
            500_000,
            None,
        );
        assert!(
            paused_tokens.contains(&token),
            "still paused before deadline"
        );
        assert!(
            !clients[&token].read_buf.is_empty(),
            "deferred command must stay buffered before the deadline"
        );

        // After the deadline: the command executes and its reply flushes.
        crate::release_expired_client_pause(
            &mut clients,
            &mut runtime,
            &mut poll,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            &mut deferred_tokens,
            2_000,
            2_000_000,
            None,
        );
        assert!(
            paused_tokens.is_empty(),
            "pause must be cleared after deadline"
        );
        assert!(
            clients[&token].read_buf.is_empty(),
            "deferred command must be consumed after release"
        );
        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"GET".to_vec())),
                    RespFrame::BulkString(Some(b"pk".to_vec())),
                ])),
                2_000,
            ),
            RespFrame::BulkString(Some(b"pv".to_vec())),
            "the released SET must have executed"
        );
        let mut buf = [0u8; 16];
        let n = client.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"+OK\r\n", "client must receive the SET reply");
    }

    #[test]
    fn xread_blocked_client_unblocks_when_xadd_marks_stream_ready() {
        use crate::ClientConnection;
        use mio::{Poll, Token};
        use std::collections::{HashMap, HashSet};
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();
        let ts = 1_000;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();

        let session = runtime.new_session();
        let client_id = session.client_id;
        let token = Token(1); // ubs:ignore
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);
        conn.read_buf.extend_from_slice(
            &RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"XREAD".to_vec())),
                RespFrame::BulkString(Some(b"BLOCK".to_vec())),
                RespFrame::BulkString(Some(b"0".to_vec())),
                RespFrame::BulkString(Some(b"STREAMS".to_vec())),
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::BulkString(Some(b"0-0".to_vec())),
            ]))
            .to_bytes(),
        );

        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();

        let session = std::mem::take(&mut conn.session);
        let prev = runtime.swap_session(session);
        process_buffered_frames(
            token,
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts,
            ts.saturating_mul(1000),
        );
        let updated_session = runtime.swap_session(prev);
        conn.session = updated_session;

        let blocked = conn.blocked.as_ref().expect("xread should block");
        assert!(matches!(blocked.op, BlockingOp::BXread { .. }));
        assert_eq!(blocked.deadline_ms, u64::MAX);
        assert!(blocked_tokens.contains(&token));
        assert!(conn.write_buf.is_empty());
        assert!(runtime.server.blocked_client_ids.contains(&client_id));

        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"XADD".to_vec())),
                    RespFrame::BulkString(Some(b"s".to_vec())),
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::BulkString(Some(b"field".to_vec())),
                    RespFrame::BulkString(Some(b"value".to_vec())),
                ])),
                ts + 1,
            ),
            RespFrame::BulkString(Some(b"1000-0".to_vec()))
        );

        let mut clients = HashMap::from([(token, conn)]);
        let mut poll = Poll::new().unwrap();
        let mut deferred_tokens = HashSet::new();
        check_blocked_clients(CheckBlockedClientsContext {
            clients: &mut clients,
            blocked_tokens: &mut blocked_tokens,
            blocked_wake_index: &mut blocked_wake_index,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            deferred_tokens: &mut deferred_tokens,
            ts: ts + 2,
            writer_pool: None,
        });

        let conn = clients.get_mut(&token).unwrap();
        assert!(conn.try_flush().unwrap());

        let conn = clients.get(&token).unwrap();
        assert!(conn.blocked.is_none());
        assert!(!blocked_tokens.contains(&token));
        assert!(!runtime.server.blocked_client_ids.contains(&client_id));

        let mut read_buf = Vec::new();
        let reply = read_frame_from_stream(
            &mut peer,
            &mut read_buf,
            &ParserConfig::default(),
            runtime.server.query_buffer_limit,
        )
        .unwrap();
        assert_eq!(
            reply,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"1000-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field".to_vec())),
                        RespFrame::BulkString(Some(b"value".to_vec())),
                    ])),
                ]))])),
            ]))]))
        );
    }

    #[test]
    fn xreadgroup_blocked_client_unblocks_when_xadd_marks_stream_ready() {
        use crate::ClientConnection;
        use mio::{Poll, Token};
        use std::collections::{HashMap, HashSet};
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();
        let ts = 2_000;

        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"XGROUP".to_vec())),
                    RespFrame::BulkString(Some(b"CREATE".to_vec())),
                    RespFrame::BulkString(Some(b"s".to_vec())),
                    RespFrame::BulkString(Some(b"g1".to_vec())),
                    RespFrame::BulkString(Some(b"0".to_vec())),
                    RespFrame::BulkString(Some(b"MKSTREAM".to_vec())),
                ])),
                ts,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();

        let session = runtime.new_session();
        let client_id = session.client_id;
        let token = Token(1); // ubs:ignore
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);
        conn.read_buf.extend_from_slice(
            &RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"XREADGROUP".to_vec())),
                RespFrame::BulkString(Some(b"GROUP".to_vec())),
                RespFrame::BulkString(Some(b"g1".to_vec())),
                RespFrame::BulkString(Some(b"c1".to_vec())),
                RespFrame::BulkString(Some(b"BLOCK".to_vec())),
                RespFrame::BulkString(Some(b"0".to_vec())),
                RespFrame::BulkString(Some(b"STREAMS".to_vec())),
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::BulkString(Some(b">".to_vec())),
            ]))
            .to_bytes(),
        );

        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();

        let session = std::mem::take(&mut conn.session);
        let prev = runtime.swap_session(session);
        process_buffered_frames(
            token,
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts,
            ts.saturating_mul(1000),
        );
        let updated_session = runtime.swap_session(prev);
        conn.session = updated_session;

        let blocked = conn.blocked.as_ref().expect("xreadgroup should block");
        assert!(matches!(blocked.op, BlockingOp::BXreadgroup { .. }));
        assert_eq!(blocked.deadline_ms, u64::MAX);
        assert!(blocked_tokens.contains(&token));
        assert!(conn.write_buf.is_empty());
        assert!(runtime.server.blocked_client_ids.contains(&client_id));

        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"XADD".to_vec())),
                    RespFrame::BulkString(Some(b"s".to_vec())),
                    RespFrame::BulkString(Some(b"2000-0".to_vec())),
                    RespFrame::BulkString(Some(b"field".to_vec())),
                    RespFrame::BulkString(Some(b"value".to_vec())),
                ])),
                ts + 1,
            ),
            RespFrame::BulkString(Some(b"2000-0".to_vec()))
        );

        let mut clients = HashMap::from([(token, conn)]);
        let mut poll = Poll::new().unwrap();
        let mut deferred_tokens = HashSet::new();
        check_blocked_clients(CheckBlockedClientsContext {
            clients: &mut clients,
            blocked_tokens: &mut blocked_tokens,
            blocked_wake_index: &mut blocked_wake_index,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            deferred_tokens: &mut deferred_tokens,
            ts: ts + 2,
            writer_pool: None,
        });

        let conn = clients.get_mut(&token).unwrap();
        assert!(conn.try_flush().unwrap());

        let conn = clients.get(&token).unwrap();
        assert!(conn.blocked.is_none());
        assert!(!blocked_tokens.contains(&token));
        assert!(!runtime.server.blocked_client_ids.contains(&client_id));

        let mut read_buf = Vec::new();
        let reply = read_frame_from_stream(
            &mut peer,
            &mut read_buf,
            &ParserConfig::default(),
            runtime.server.query_buffer_limit,
        )
        .unwrap();
        assert_eq!(
            reply,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"s".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"2000-0".to_vec())),
                    RespFrame::Array(Some(vec![
                        RespFrame::BulkString(Some(b"field".to_vec())),
                        RespFrame::BulkString(Some(b"value".to_vec())),
                    ])),
                ]))])),
            ]))]))
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
        let argv = test_argv(frame);
        assert!(try_build_blocked_state(&argv, 1_000).is_none());
    }

    #[test]
    fn blocking_state_builder_accepts_resp3_integer_timeout() {
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"BLPOP".to_vec())),
            RespFrame::BulkString(Some(b"list".to_vec())),
            RespFrame::Integer(0),
        ]));
        let argv = test_argv(frame);
        let blocked = try_build_blocked_state(&argv, 1_000).expect("must block");
        assert!(matches!(blocked.op, BlockingOp::BLpop { .. }));
        let BlockingOp::BLpop { keys } = blocked.op else {
            return;
        };
        assert_eq!(keys, vec![b"list".to_vec()]);
        assert_eq!(blocked.deadline_ms, u64::MAX);
    }

    #[test]
    fn inline_parser_gate_recognizes_all_resp_prefixes() {
        // Upstream: only '*' (multibulk) stays on the RESP parser path; every
        // other first byte — including the RESP2 reply and RESP3 type prefixes
        // — is the start of an inline command. (frankenredis-c6vt7)
        assert!(
            !should_try_inline_parsing(b'*'),
            "'*' must stay on the RESP multibulk parser path"
        );
        for prefix in *b"+-:$~%#,_(=|>!" {
            assert!(
                should_try_inline_parsing(prefix),
                "non-'*' prefix {prefix:?} must be treated as inline like redis"
            );
        }

        assert!(should_try_inline_parsing(b'P'));
        assert!(should_try_inline_parsing(b' '));
    }

    #[test]
    fn process_buffered_frames_defers_after_per_client_frame_budget() {
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
        let (_server_stream, _server_addr) = listener.accept().unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);

        let ping = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"PING".to_vec()))]));
        let ping_bytes = ping.to_bytes();
        for _ in 0..=crate::MAX_FRAMES_PER_CLIENT_TICK {
            conn.read_buf.extend_from_slice(&ping_bytes);
        }

        let token = Token(1); // ubs:ignore
        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();

        let prev = runtime.swap_session(std::mem::take(&mut conn.session));
        let budget_exhausted = crate::process_buffered_frames(
            token,
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts,
            ts.saturating_mul(1000),
        );
        conn.session = runtime.swap_session(prev);

        assert!(budget_exhausted);
        assert_eq!(conn.read_buf, ping_bytes);
        assert_eq!(
            conn.write_buf.len(),
            b"+PONG\r\n".len() * crate::MAX_FRAMES_PER_CLIENT_TICK
        );
        assert!(write_tokens.contains(&token));
        assert!(blocked_tokens.is_empty());
        assert!(closing_tokens.is_empty());
        assert!(paused_tokens.is_empty());
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
        let psync_argv = test_argv(psync_frame);

        if let Some(follow_up) =
            replication_follow_up_bytes(&mut runtime, &psync_argv, &response, ts)
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
        let mut closing_tokens = HashSet::new();
        let mut poll = mio::Poll::new().unwrap();

        propagate_writes_to_replicas(
            &mut clients,
            &mut runtime,
            &mut poll,
            &mut write_tokens,
            &mut closing_tokens,
            None,
        );

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
        let psync_argv = test_argv(psync_frame.clone());

        if let Some(follow_up) =
            replication_follow_up_bytes(&mut primary, &psync_argv, &response, ts)
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
        let psync_argv = test_argv(psync_frame);

        if let Some(follow_up) =
            replication_follow_up_bytes(&mut replica_rt, &psync_argv, &response_sub, ts)
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
        let mut closing_tokens = HashSet::new();
        let mut poll = mio::Poll::new().unwrap();

        propagate_writes_to_replicas(
            &mut clients,
            &mut primary,
            &mut poll,
            &mut write_tokens,
            &mut closing_tokens,
            None,
        );

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
        let mut sub_closing_tokens = HashSet::new();

        propagate_writes_to_replicas(
            &mut sub_clients,
            &mut replica_rt,
            &mut poll,
            &mut sub_write_tokens,
            &mut sub_closing_tokens,
            None,
        );

        let sub_conn = sub_clients.get(&sub_token).unwrap();
        let expected_bytes = encode_aof_stream(&[AofRecord {
            argv: vec![b"SET".to_vec(), b"foo".to_vec(), b"bar".to_vec()],
        }]);

        assert_eq!(sub_conn.write_buf, expected_bytes);
    }

    // (frankenredis-flbs4) Under CLIENT PAUSE ALL, upstream server.c::processCommand
    // defers EVERY non-replica command including CLIENT UNPAUSE — there is no
    // bypass, so a PAUSE ALL is only liftable by its own deadline. fr must defer
    // the in-pause UNPAUSE (leave it buffered, send no reply, stay paused), not
    // execute it early.
    #[test]
    fn client_unpause_is_deferred_under_pause_all() {
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
        let (_server_stream, _server_addr) = listener.accept().unwrap();
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
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts + 1,
            (ts + 1).saturating_mul(1000),
        );

        // The UNPAUSE is deferred: token parked, bytes left buffered, no reply,
        // and the pause is still in effect (only its own deadline lifts it).
        assert!(paused_tokens.contains(&Token(1)));
        assert!(
            !conn.read_buf.is_empty(),
            "in-pause UNPAUSE must stay buffered, not execute"
        );
        assert!(conn.write_buf.is_empty(), "no reply should be produced");
        assert!(runtime.is_client_paused(ts + 1), "pause must remain active");

        // It only runs once the pause window naturally expires (deadline 2000),
        // at which point it is a no-op (pause already over).
        assert!(!runtime.is_client_paused(2_001));
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

        let ping = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"PING".to_vec()))]));
        conn.read_buf.extend_from_slice(&ping.to_bytes());

        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts + 1,
            (ts + 1).saturating_mul(1000),
        );

        assert!(paused_tokens.contains(&Token(1)));
        assert!(conn.write_buf.is_empty());
        assert!(!conn.read_buf.is_empty());
    }

    #[test]
    fn client_reply_off_suppresses_network_replies_until_on() {
        use crate::ClientConnection;
        use fr_protocol::RespFrame;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::HashSet;
        use std::net::{TcpListener, TcpStream};

        fn frame(parts: &[&[u8]]) -> RespFrame {
            RespFrame::Array(Some(
                parts
                    .iter()
                    .map(|part| RespFrame::BulkString(Some(part.to_vec())))
                    .collect(),
            ))
        }

        let mut runtime = Runtime::default_strict();
        let ts = 20;
        let session = runtime.new_session();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (mut server_stream, _server_addr) = listener.accept().unwrap();
        server_stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);

        let pipeline = [
            frame(&[b"CLIENT", b"REPLY", b"OFF"]),
            frame(&[b"NOPE"]),
            frame(&[b"CLIENT", b"REPLY", b"ON"]),
            frame(&[b"PING"]),
        ];
        for frame in pipeline {
            conn.read_buf.extend_from_slice(&frame.to_bytes());
        }

        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let prev = runtime.swap_session(std::mem::take(&mut conn.session));
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts,
            ts.saturating_mul(1000),
        );
        conn.session = runtime.swap_session(prev);

        assert!(conn.try_flush().unwrap());

        let mut response = [0_u8; 12];
        std::io::Read::read_exact(&mut server_stream, &mut response).unwrap();
        assert_eq!(response, *b"+OK\r\n+PONG\r\n");
    }

    #[test]
    fn client_reply_skip_suppresses_current_and_next_network_reply() {
        use crate::ClientConnection;
        use fr_protocol::RespFrame;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::HashSet;
        use std::net::{TcpListener, TcpStream};

        fn frame(parts: &[&[u8]]) -> RespFrame {
            RespFrame::Array(Some(
                parts
                    .iter()
                    .map(|part| RespFrame::BulkString(Some(part.to_vec())))
                    .collect(),
            ))
        }

        let mut runtime = Runtime::default_strict();
        let ts = 21;
        let session = runtime.new_session();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (mut server_stream, _server_addr) = listener.accept().unwrap();
        server_stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);

        let pipeline = [
            frame(&[b"CLIENT", b"REPLY", b"SKIP"]),
            frame(&[b"NOPE"]),
            frame(&[b"PING"]),
        ];
        for frame in pipeline {
            conn.read_buf.extend_from_slice(&frame.to_bytes());
        }

        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let prev = runtime.swap_session(std::mem::take(&mut conn.session));
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts,
            ts.saturating_mul(1000),
        );
        conn.session = runtime.swap_session(prev);

        assert!(conn.try_flush().unwrap());

        let mut response = [0_u8; 7];
        std::io::Read::read_exact(&mut server_stream, &mut response).unwrap();
        assert_eq!(response, *b"+PONG\r\n");
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
            RespFrame::BulkString(Some(b"SET".to_vec())),
            RespFrame::BulkString(Some(b"k".to_vec())),
            RespFrame::BulkString(Some(b"v".to_vec())),
        ]));
        conn.read_buf.extend_from_slice(&set_frame.to_bytes());

        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let prev = runtime.swap_session(std::mem::take(&mut conn.session));
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts + 1,
            (ts + 1).saturating_mul(1000),
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
    fn process_buffered_frames_uses_microsecond_clock_for_time() {
        use crate::ClientConnection;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::HashSet;
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();
        let ts_ms = 1_778_390_164_120;
        let ts_us = 1_778_390_164_118_366;
        let session = runtime.new_session();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (_server_stream, _server_addr) = listener.accept().unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts_ms);

        let time = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"TIME".to_vec()))]));
        conn.read_buf.extend_from_slice(&time.to_bytes());

        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let prev = runtime.swap_session(std::mem::take(&mut conn.session));
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts_ms,
            ts_us,
        );
        conn.session = runtime.swap_session(prev);

        let parsed = fr_protocol::parse_frame(&conn.write_buf).expect("parse reply");
        assert_eq!(
            parsed.frame,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1778390164".to_vec())),
                RespFrame::BulkString(Some(b"118366".to_vec())),
            ]))
        );
    }

    #[test]
    fn process_buffered_frames_invalidates_cached_get_gate_after_generic_state_change() {
        use crate::ClientConnection;
        use fr_runtime::Runtime;
        use mio::Token;
        use std::collections::HashSet;
        use std::net::{TcpListener, TcpStream};

        let mut runtime = Runtime::default_strict();
        assert_eq!(
            runtime.execute_frame(
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"SET".to_vec())),
                    RespFrame::BulkString(Some(b"k".to_vec())),
                    RespFrame::BulkString(Some(b"v".to_vec())),
                ])),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        let ts = 2;
        let session = runtime.new_session();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (_server_stream, _server_addr) = listener.accept().unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);

        let get = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"GET".to_vec())),
            RespFrame::BulkString(Some(b"k".to_vec())),
        ]));
        let select_db1 = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"SELECT".to_vec())),
            RespFrame::BulkString(Some(b"1".to_vec())),
        ]));
        conn.read_buf.extend_from_slice(&get.to_bytes());
        conn.read_buf.extend_from_slice(&select_db1.to_bytes());
        conn.read_buf.extend_from_slice(&get.to_bytes());

        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let prev = runtime.swap_session(std::mem::take(&mut conn.session));
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts,
            ts.saturating_mul(1000),
        );
        conn.session = runtime.swap_session(prev);

        let mut expected = Vec::new();
        expected.extend_from_slice(&RespFrame::BulkString(Some(b"v".to_vec())).to_bytes());
        expected.extend_from_slice(&RespFrame::SimpleString("OK".to_string()).to_bytes());
        expected.extend_from_slice(&RespFrame::BulkString(None).to_bytes());
        assert_eq!(conn.write_buf, expected);
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

        let quit = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"QUIT".to_vec()))]));
        conn.read_buf.extend_from_slice(&quit.to_bytes());

        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let prev = runtime.swap_session(std::mem::take(&mut conn.session));
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts,
            ts.saturating_mul(1000),
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
        // A tiny normal-class hard limit forces the per-client write guard to
        // trip after the PING reply (0 would mean unlimited). (frankenredis-8sb0l)
        runtime.server.client_output_buffer_limits.normal.hard = 1;
        let session = runtime.new_session();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (_server_stream, _server_addr) = listener.accept().unwrap();
        let mut conn = ClientConnection::new(mio::net::TcpStream::from_std(stream), session, ts);
        conn.read_buf.extend_from_slice(b"*1\r\n$4\r\nPING\r\n");

        let mut blocked_tokens = HashSet::new();
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        crate::process_buffered_frames(
            Token(1),
            &mut conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            ts,
            ts.saturating_mul(1000),
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
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        if let Some(blocked) = &blocked_conn.blocked {
            blocked_wake_index.insert(blocked_token, blocked);
        }
        let mut closing_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();

        crate::process_buffered_frames(
            Token(2),
            &mut requester_conn,
            &mut runtime,
            &mut blocked_tokens,
            &mut blocked_wake_index,
            &mut closing_tokens,
            &mut write_tokens,
            &mut paused_tokens,
            1,
            1_000,
        );
        assert!(requester_conn.try_flush().unwrap());
        let mut requester_reply = [0_u8; 4];
        std::io::Read::read_exact(&mut requester_peer, &mut requester_reply).unwrap();
        assert_eq!(requester_reply, *b":1\r\n");

        let mut clients = HashMap::from([(blocked_token, blocked_conn)]);
        let client_id_to_token = HashMap::from([(blocked_client_id, blocked_token)]); // ubs:ignore
        let mut poll = Poll::new().unwrap();
        let mut deferred_tokens = HashSet::new();

        apply_pending_client_unblocks(PendingClientUnblocksContext {
            clients: &mut clients,
            client_id_to_token: &client_id_to_token,
            blocked_tokens: &mut blocked_tokens,
            blocked_wake_index: &mut blocked_wake_index,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            deferred_tokens: &mut deferred_tokens,
            ts: 2,
            writer_pool: None,
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
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        if let Some(blocked) = clients
            .get(&blocked_token)
            .and_then(|conn| conn.blocked.as_ref())
        {
            blocked_wake_index.insert(blocked_token, blocked);
        }
        let mut closing_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut poll = Poll::new().unwrap();
        let mut deferred_tokens = HashSet::new();

        apply_pending_client_unblocks(PendingClientUnblocksContext {
            clients: &mut clients,
            client_id_to_token: &client_id_to_token,
            blocked_tokens: &mut blocked_tokens,
            blocked_wake_index: &mut blocked_wake_index,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            deferred_tokens: &mut deferred_tokens,
            ts: 11,
            writer_pool: None,
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
        let mut blocked_wake_index = crate::BlockedWakeIndex::default();
        if let Some(blocked) = clients
            .get(&blocked_token)
            .and_then(|conn| conn.blocked.as_ref())
        {
            blocked_wake_index.insert(blocked_token, blocked);
        }
        let mut closing_tokens = HashSet::new();
        let mut paused_tokens = HashSet::new();
        let mut write_tokens = HashSet::new();
        let mut poll = Poll::new().unwrap();
        let mut deferred_tokens = HashSet::new();

        check_blocked_clients(CheckBlockedClientsContext {
            clients: &mut clients,
            blocked_tokens: &mut blocked_tokens,
            blocked_wake_index: &mut blocked_wake_index,
            closing_tokens: &mut closing_tokens,
            paused_tokens: &mut paused_tokens,
            runtime: &mut runtime,
            poll: &mut poll,
            write_tokens: &mut write_tokens,
            deferred_tokens: &mut deferred_tokens,
            ts: 21,
            writer_pool: None,
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

    // (frankenredis-nnbig) The pub/sub-context gate must only fire when
    // the command is known and its generic arity is OK. Upstream processCommand
    // does command LOOKUP (unknown -> "unknown command") and the generic ARITY
    // check (server.c:3787) BEFORE the pub/sub-context gate (server.c:4072), so a
    // wrong-arity or unknown command issued while subscribed surfaces its own
    // error, not "...allowed in this context". This pins the exact predicate the
    // fast-path gate in process_argv_frame applies: arity-ok AND gate-rejects.
    #[test]
    fn subscribe_mode_gate_runs_arity_before_context_gate_nnbig() {
        let argv = |parts: &[&str]| -> Vec<Vec<u8>> {
            parts.iter().map(|p| p.as_bytes().to_vec()).collect()
        };
        // gate_fires == the exact condition used in process_argv_frame's RESP2
        // subscription gate: known + arity-ok AND on the deny-list.
        let gate_fires = |parts: &[&str]| -> bool {
            let a = argv(parts);
            fr_command::check_command_arity(a.first().map(Vec::as_slice).unwrap_or(b""), a.len())
                .is_ok()
                && check_subscription_mode_gate(&a, true).is_some()
        };

        // Wrong-arity / unknown -> gate SKIPPED, command reaches dispatch so its
        // own unknown/arity error surfaces (matching upstream order).
        assert!(!gate_fires(&["GET"]), "GET with no key is wrong-arity");
        assert!(
            !gate_fires(&["SET", "k"]),
            "SET missing value is wrong-arity"
        );
        assert!(
            !gate_fires(&["GET", "k", "x"]),
            "GET with extra arg is wrong-arity"
        );
        assert!(!gate_fires(&["FOOBARNOTACMD", "x"]), "unknown command");
        assert!(
            !gate_fires(&["DEBUG"]),
            "DEBUG with no subcommand is wrong-arity"
        );

        // Valid-arity deny-listed commands -> gate FIRES (subscribe-context error).
        assert!(gate_fires(&["GET", "k"]));
        assert!(gate_fires(&["SET", "k", "v"]));
        assert!(gate_fires(&["INCR", "k"]));

        // Allow-listed pub/sub commands -> gate never fires regardless of arity.
        assert!(!gate_fires(&["SUBSCRIBE", "c"]));
        assert!(!gate_fires(&["UNSUBSCRIBE"]));
        assert!(!gate_fires(&["PING"]));
        assert!(!gate_fires(&["RESET"]));
        assert!(!gate_fires(&["QUIT"]));
    }
}
