//! Differential fuzz target: diff fr-runtime against a persistent
//! vendored `redis-server` instance side-by-side. Complements the
//! existing self-referential fuzz targets with a cross-impl oracle so
//! wording / arity / nil-vs-int / sort-order divergences surface
//! immediately. (br-frankenredis-1749)
//!
//! Design notes
//! ------------
//! * `UPSTREAM` is a once-init `OnceLock<Option<Oracle>>`. The first
//!   call spawns `legacy_redis_code/redis/src/redis-server` on a free
//!   port, establishes a single long-lived TCP connection, and pins
//!   both so every fuzz iteration reuses them. Spawn failure (binary
//!   missing, port exhaustion, PING never returns PONG) yields `None`
//!   and the target no-ops for the rest of the process — libfuzzer
//!   does not see "crashes" from a missing oracle.
//! * Between cases we FLUSHALL on both sides. No cross-case state
//!   leakage.
//! * Structure-aware `CommandSeq` covers a minimal but meaningful
//!   subset of string / list / set / hash / zset / key commands. Set
//!   and hash responses (HGETALL, SMEMBERS) are canonicalized by
//!   sorting before the byte-level compare. KEYS-style globbing and
//!   time-dependent commands (EXPIRE/TTL) are out of scope for this
//!   first slice and can be added without touching the harness.
//! * Divergences panic with a structured payload. Under libfuzzer,
//!   panics feed the crash corpus; under `cargo test` the assertion
//!   message is the triage artifact.
//! * `FUZZ_DIFFERENTIAL_SKIP=1` unconditionally short-circuits the
//!   target — useful when running the harness on workers that do not
//!   vendor `redis-server`.

#![no_main]

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use arbitrary::Arbitrary;
use fr_protocol::{RespFrame, parse_frame};
use fr_runtime::Runtime;
use libfuzzer_sys::fuzz_target;

/// Maximum commands per fuzz case. Bounded so a single case cannot
/// swamp the upstream TCP buffer or starve libfuzzer's throughput.
const MAX_COMMANDS_PER_CASE: usize = 32;
/// Maximum argv size per command.
const MAX_ARG_LEN: usize = 64;
/// TCP read/write timeout. Short enough that a wedged upstream bails
/// out loudly rather than stalling the fuzzer; long enough that a busy
/// machine spawning redis-server has headroom.
const TCP_TIMEOUT: Duration = Duration::from_secs(5);

// ── Structured input ────────────────────────────────────────────────

#[derive(Debug, Arbitrary)]
struct CommandSeq {
    commands: Vec<FuzzCommand>,
}

#[derive(Debug, Arbitrary, Clone)]
enum FuzzCommand {
    // String
    Set { key: Arg, value: Arg },
    Get { key: Arg },
    Del { keys: Vec<Arg> },
    Incr { key: Arg },
    IncrBy { key: Arg, delta: i32 },
    Decr { key: Arg },
    Append { key: Arg, value: Arg },
    Strlen { key: Arg },
    Exists { keys: Vec<Arg> },
    // List
    LPush { key: Arg, values: Vec<Arg> },
    RPush { key: Arg, values: Vec<Arg> },
    LPop { key: Arg },
    RPop { key: Arg },
    LLen { key: Arg },
    LRange { key: Arg, start: i16, stop: i16 },
    // Set
    SAdd { key: Arg, members: Vec<Arg> },
    SRem { key: Arg, members: Vec<Arg> },
    SMembers { key: Arg },
    SCard { key: Arg },
    SIsMember { key: Arg, member: Arg },
    // Hash
    HSet { key: Arg, field: Arg, value: Arg },
    HGet { key: Arg, field: Arg },
    HDel { key: Arg, fields: Vec<Arg> },
    HLen { key: Arg },
    HGetAll { key: Arg },
    // Sorted set
    ZAdd { key: Arg, score: i16, member: Arg },
    ZRem { key: Arg, members: Vec<Arg> },
    ZScore { key: Arg, member: Arg },
    ZCard { key: Arg },
    ZRange { key: Arg, start: i16, stop: i16 },
}

#[derive(Debug, Arbitrary, Clone)]
struct Arg(Vec<u8>);

impl Arg {
    fn bytes(&self, cap: usize) -> &[u8] {
        &self.0[..self.0.len().min(cap)]
    }
}

// ── Command → argv ─────────────────────────────────────────────────

impl FuzzCommand {
    /// Render the command as a `Vec<Vec<u8>>` argv, truncated to
    /// avoid pathological blow-up. Returns `None` if the case would
    /// exceed the guardrails.
    fn to_argv(&self) -> Option<Vec<Vec<u8>>> {
        use FuzzCommand::*;
        let mut argv: Vec<Vec<u8>> = Vec::new();
        let key_or_trim = |a: &Arg| a.bytes(MAX_ARG_LEN).to_vec();
        match self {
            Set { key, value } => {
                argv.push(b"SET".to_vec());
                argv.push(key_or_trim(key));
                argv.push(key_or_trim(value));
            }
            Get { key } => {
                argv.push(b"GET".to_vec());
                argv.push(key_or_trim(key));
            }
            Del { keys } => {
                if keys.is_empty() || keys.len() > 8 {
                    return None;
                }
                argv.push(b"DEL".to_vec());
                for k in keys {
                    argv.push(key_or_trim(k));
                }
            }
            Incr { key } => {
                argv.push(b"INCR".to_vec());
                argv.push(key_or_trim(key));
            }
            IncrBy { key, delta } => {
                argv.push(b"INCRBY".to_vec());
                argv.push(key_or_trim(key));
                argv.push(delta.to_string().into_bytes());
            }
            Decr { key } => {
                argv.push(b"DECR".to_vec());
                argv.push(key_or_trim(key));
            }
            Append { key, value } => {
                argv.push(b"APPEND".to_vec());
                argv.push(key_or_trim(key));
                argv.push(key_or_trim(value));
            }
            Strlen { key } => {
                argv.push(b"STRLEN".to_vec());
                argv.push(key_or_trim(key));
            }
            Exists { keys } => {
                if keys.is_empty() || keys.len() > 8 {
                    return None;
                }
                argv.push(b"EXISTS".to_vec());
                for k in keys {
                    argv.push(key_or_trim(k));
                }
            }
            LPush { key, values } | RPush { key, values } => {
                if values.is_empty() || values.len() > 8 {
                    return None;
                }
                argv.push(match self {
                    LPush { .. } => b"LPUSH".to_vec(),
                    _ => b"RPUSH".to_vec(),
                });
                argv.push(key_or_trim(key));
                for v in values {
                    argv.push(key_or_trim(v));
                }
            }
            LPop { key } => {
                argv.push(b"LPOP".to_vec());
                argv.push(key_or_trim(key));
            }
            RPop { key } => {
                argv.push(b"RPOP".to_vec());
                argv.push(key_or_trim(key));
            }
            LLen { key } => {
                argv.push(b"LLEN".to_vec());
                argv.push(key_or_trim(key));
            }
            LRange { key, start, stop } => {
                argv.push(b"LRANGE".to_vec());
                argv.push(key_or_trim(key));
                argv.push(start.to_string().into_bytes());
                argv.push(stop.to_string().into_bytes());
            }
            SAdd { key, members } | SRem { key, members } => {
                if members.is_empty() || members.len() > 8 {
                    return None;
                }
                argv.push(match self {
                    SAdd { .. } => b"SADD".to_vec(),
                    _ => b"SREM".to_vec(),
                });
                argv.push(key_or_trim(key));
                for m in members {
                    argv.push(key_or_trim(m));
                }
            }
            SMembers { key } => {
                argv.push(b"SMEMBERS".to_vec());
                argv.push(key_or_trim(key));
            }
            SCard { key } => {
                argv.push(b"SCARD".to_vec());
                argv.push(key_or_trim(key));
            }
            SIsMember { key, member } => {
                argv.push(b"SISMEMBER".to_vec());
                argv.push(key_or_trim(key));
                argv.push(key_or_trim(member));
            }
            HSet { key, field, value } => {
                argv.push(b"HSET".to_vec());
                argv.push(key_or_trim(key));
                argv.push(key_or_trim(field));
                argv.push(key_or_trim(value));
            }
            HGet { key, field } => {
                argv.push(b"HGET".to_vec());
                argv.push(key_or_trim(key));
                argv.push(key_or_trim(field));
            }
            HDel { key, fields } => {
                if fields.is_empty() || fields.len() > 8 {
                    return None;
                }
                argv.push(b"HDEL".to_vec());
                argv.push(key_or_trim(key));
                for f in fields {
                    argv.push(key_or_trim(f));
                }
            }
            HLen { key } => {
                argv.push(b"HLEN".to_vec());
                argv.push(key_or_trim(key));
            }
            HGetAll { key } => {
                argv.push(b"HGETALL".to_vec());
                argv.push(key_or_trim(key));
            }
            ZAdd { key, score, member } => {
                argv.push(b"ZADD".to_vec());
                argv.push(key_or_trim(key));
                argv.push(score.to_string().into_bytes());
                argv.push(key_or_trim(member));
            }
            ZRem { key, members } => {
                if members.is_empty() || members.len() > 8 {
                    return None;
                }
                argv.push(b"ZREM".to_vec());
                argv.push(key_or_trim(key));
                for m in members {
                    argv.push(key_or_trim(m));
                }
            }
            ZScore { key, member } => {
                argv.push(b"ZSCORE".to_vec());
                argv.push(key_or_trim(key));
                argv.push(key_or_trim(member));
            }
            ZCard { key } => {
                argv.push(b"ZCARD".to_vec());
                argv.push(key_or_trim(key));
            }
            ZRange { key, start, stop } => {
                argv.push(b"ZRANGE".to_vec());
                argv.push(key_or_trim(key));
                argv.push(start.to_string().into_bytes());
                argv.push(stop.to_string().into_bytes());
            }
        }
        Some(argv)
    }

    /// What shape of canonicalization to apply before diffing the reply.
    fn reply_shape(&self) -> ReplyShape {
        match self {
            FuzzCommand::SMembers { .. } => ReplyShape::FlatSet,
            FuzzCommand::HGetAll { .. } => ReplyShape::FieldValuePairs,
            _ => ReplyShape::Ordered,
        }
    }
}

#[derive(Clone, Copy)]
enum ReplyShape {
    /// Preserve reply order as-is. Applies to everything whose RESP
    /// layout is already deterministic (GET, LRANGE, ZRANGE, counts…).
    Ordered,
    /// Unordered set of bulk-string members — sort bytes-wise.
    FlatSet,
    /// Interleaved field/value pairs — sort by field but keep each
    /// (field, value) adjacent.
    FieldValuePairs,
}

fn argv_to_frame(argv: Vec<Vec<u8>>) -> RespFrame {
    RespFrame::Array(Some(
        argv.into_iter()
            .map(|b| RespFrame::BulkString(Some(b)))
            .collect(),
    ))
}

// ── Upstream oracle ────────────────────────────────────────────────

/// Process-wide spawned `redis-server` + persistent TCP connection.
struct Oracle {
    _child: Child,
    stream: TcpStream,
}

fn upstream() -> Option<&'static Mutex<Oracle>> {
    // Double-check layout: we want a lazy `Option<Oracle>` because the
    // spawn can legitimately fail (binary missing) and we don't want
    // to retry it every iteration.
    static CELL: OnceLock<Option<Mutex<Oracle>>> = OnceLock::new();
    CELL.get_or_init(spawn_oracle).as_ref()
}

fn spawn_oracle() -> Option<Mutex<Oracle>> {
    if std::env::var_os("FUZZ_DIFFERENTIAL_SKIP").is_some() {
        return None;
    }
    let root = project_root();
    let binary = root.join("legacy_redis_code/redis/src/redis-server");
    if !binary.is_file() {
        return None;
    }
    let port = pick_free_port()?;
    let tmp_dir =
        std::env::temp_dir().join(format!("fuzz_diff_runtime_{}_{}", std::process::id(), port));
    std::fs::create_dir_all(&tmp_dir).ok()?;
    let child = Command::new(&binary)
        .arg("--port")
        .arg(port.to_string())
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--dir")
        .arg(&tmp_dir)
        .arg("--appendonly")
        .arg("no")
        .arg("--save")
        .arg("")
        .arg("--daemonize")
        .arg("no")
        .arg("--protected-mode")
        .arg("no")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        if Instant::now() > deadline {
            // Reap the stray child — we took ownership via `child`.
            let mut c = child;
            let _ = c.kill();
            let _ = c.wait();
            return None;
        }
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            s.set_read_timeout(Some(TCP_TIMEOUT)).ok();
            s.set_write_timeout(Some(TCP_TIMEOUT)).ok();
            // Sanity-ping before we trust the stream.
            let mut s = s;
            let ping = argv_to_frame(vec![b"PING".to_vec()]).to_bytes();
            if let Ok(RespFrame::SimpleString(reply)) = roundtrip_tcp(&mut s, &ping)
                && reply == "PONG"
            {
                break s;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Some(Mutex::new(Oracle {
        _child: child,
        stream,
    }))
}

fn project_root() -> PathBuf {
    // fuzz/ → project root is one level up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn pick_free_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    Some(port)
}

/// Send one RESP-encoded command and parse one RESP frame back.
fn roundtrip_tcp(stream: &mut TcpStream, request: &[u8]) -> Result<RespFrame, &'static str> {
    stream.write_all(request).map_err(|_| "tcp write")?;
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 4096];
    loop {
        match parse_frame(&buf) {
            Ok(parsed) => return Ok(parsed.frame),
            Err(fr_protocol::RespParseError::Incomplete) => {}
            Err(_) => return Err("tcp parse"),
        }
        let n = stream.read(&mut chunk).map_err(|_| "tcp read")?;
        if n == 0 {
            let _ = stream.shutdown(Shutdown::Both);
            return Err("tcp eof");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

// ── Canonicalization ───────────────────────────────────────────────

/// Normalize RESP frames so set/map-shaped replies compare equal
/// regardless of iteration order.
fn canonicalize(frame: &RespFrame, shape: ReplyShape) -> RespFrame {
    match (shape, frame) {
        (ReplyShape::Ordered, _) => frame.clone(),
        (ReplyShape::FlatSet, RespFrame::Array(Some(items))) => {
            let mut sorted = items.clone();
            sorted.sort_by_key(frame_sort_key);
            RespFrame::Array(Some(sorted))
        }
        (ReplyShape::FieldValuePairs, RespFrame::Array(Some(items))) if items.len() % 2 == 0 => {
            let mut pairs: Vec<(Vec<u8>, RespFrame)> = items
                .chunks_exact(2)
                .map(|c| {
                    let k = match &c[0] {
                        RespFrame::BulkString(Some(b)) => b.clone(),
                        other => frame_sort_key(other),
                    };
                    (k, c[1].clone())
                })
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = Vec::with_capacity(items.len());
            for (k, v) in pairs {
                out.push(RespFrame::BulkString(Some(k)));
                out.push(v);
            }
            RespFrame::Array(Some(out))
        }
        (_, other) => other.clone(),
    }
}

fn frame_sort_key(frame: &RespFrame) -> Vec<u8> {
    match frame {
        RespFrame::BulkString(Some(b)) => b.clone(),
        RespFrame::BulkString(None) => Vec::new(),
        RespFrame::SimpleString(s) => s.as_bytes().to_vec(),
        RespFrame::Integer(n) => n.to_string().into_bytes(),
        RespFrame::Error(s) => s.as_bytes().to_vec(),
        // Composite frames (Array / Sequence / RESP3 Map / Push)
        // sort by their full-encoded byte form; that's stable and
        // captures both shape and contents.
        RespFrame::Array(_)
        | RespFrame::Sequence(_)
        | RespFrame::Map(_)
        | RespFrame::Push(_) => frame.to_bytes(),
    }
}

/// Fold error replies so minor wording mismatches (e.g. the trailing
/// space or casing of `ERR` vs `WRONGTYPE`) do not count as a hard
/// divergence — we compare the error kind prefix only. This keeps the
/// fuzzer honest about *behavioral* parity without drowning in prose
/// drift. Full error-text parity is tracked in the conformance corpus.
fn error_kind(frame: &RespFrame) -> Option<String> {
    match frame {
        RespFrame::Error(s) => Some(
            s.split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase(),
        ),
        _ => None,
    }
}

fn frames_equivalent(ours: &RespFrame, upstream: &RespFrame) -> bool {
    if ours == upstream {
        return true;
    }
    // Errors: accept if the leading token (ERR, WRONGTYPE, ...) matches.
    if let (Some(a), Some(b)) = (error_kind(ours), error_kind(upstream)) {
        return a == b;
    }
    false
}

// ── Harness ────────────────────────────────────────────────────────

fn run_case(oracle: &Mutex<Oracle>, seq: CommandSeq) {
    if seq.commands.is_empty() || seq.commands.len() > MAX_COMMANDS_PER_CASE {
        return;
    }

    let mut oracle = match oracle.lock() {
        Ok(g) => g,
        Err(_) => return, // Poisoned — skip.
    };

    // Clean slate on both sides.
    let mut local = Runtime::default_strict();
    let flush = argv_to_frame(vec![b"FLUSHALL".to_vec()]);
    let _ = local.execute_frame(flush.clone(), 1);
    if roundtrip_tcp(&mut oracle.stream, &flush.to_bytes()).is_err() {
        return;
    }

    let mut now_ms: u64 = 1_000;
    for cmd in &seq.commands {
        let Some(argv) = cmd.to_argv() else {
            continue;
        };
        let frame = argv_to_frame(argv);
        let wire = frame.to_bytes();

        let ours_raw = local.execute_frame(frame.clone(), now_ms);
        let upstream_raw = match roundtrip_tcp(&mut oracle.stream, &wire) {
            Ok(f) => f,
            Err(_) => return, // TCP died — fail soft.
        };

        let shape = cmd.reply_shape();
        let ours = canonicalize(&ours_raw, shape);
        let upstream = canonicalize(&upstream_raw, shape);

        assert!(
            frames_equivalent(&ours, &upstream),
            "differential divergence on {argv:?}\n  ours:     {ours_raw:?}\n  upstream: {upstream_raw:?}",
            argv = cmd,
        );

        now_ms += 1;
    }
}

fuzz_target!(|seq: CommandSeq| {
    if let Some(oracle) = upstream() {
        run_case(oracle, seq);
    }
});
