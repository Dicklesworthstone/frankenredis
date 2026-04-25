//! Differential RESP2 round-trip gate against the vendored upstream
//! `redis-server` (legacy_redis_code/redis/src/redis-server).
//!
//! For each canonical command, this test:
//!   1. Sends the command to a spawned upstream server and captures the
//!      raw TCP reply bytes.
//!   2. Calls `parse_frame(bytes)` — must succeed with `consumed` equal to
//!      `bytes.len()` (no trailing, no partial).
//!   3. Calls `frame.to_bytes()` — must equal the original reply bytes
//!      byte-for-byte.
//!   4. Optionally re-parses the re-encoded bytes and asserts structural
//!      equality with the first parse.
//!
//! If the vendored binary is missing (most rch workers won't have it
//! copied explicitly) the test prints a [SKIP] log and returns.
//!
//! Env knobs mirror the fr-conformance harness:
//!   FR_CONFORMANCE_SKIP_LIVE_ORACLE=1  force silent skip.
//!
//! (br-frankenredis-0zyf — first increment: round-trip gate for a
//! canonical-command corpus; subsequent increments will add the
//! DISCREPANCIES.md / COVERAGE.md accounting and XFAIL infra.)

use fr_protocol::{RespFrame, parse_frame};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Self-contained VendoredRedis handle. Duplicates the spawner pattern
/// used in fr-conformance (lib.rs VendoredRedis struct, br-frankenredis-5pnv).
/// Kept local to this crate because fr-protocol must not depend on
/// fr-conformance (which itself depends on fr-protocol).
struct VendoredRedis {
    child: Child,
    port: u16,
    _tmp_dir: PathBuf,
}

impl VendoredRedis {
    fn spawn() -> Option<Self> {
        if std::env::var_os("FR_CONFORMANCE_SKIP_LIVE_ORACLE").is_some() {
            return None;
        }
        // fr-protocol crate dir → ../.. → repo root.
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..");
        let binary = repo_root
            .join("legacy_redis_code")
            .join("redis")
            .join("src")
            .join("redis-server");
        if !binary.exists() {
            return None;
        }
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").ok()?;
            let port = listener.local_addr().ok()?.port();
            drop(listener);
            port
        };
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let tmp_dir = std::env::temp_dir().join(format!("fr_proto_oracle_{ts}_{port}"));
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
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Some(Self {
                    child,
                    port,
                    _tmp_dir: tmp_dir,
                });
            }
            sleep(Duration::from_millis(50));
        }
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        None
    }
}

impl Drop for VendoredRedis {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Encode argv as a RESP2 multibulk request.
fn encode_request(argv: &[&[u8]]) -> Vec<u8> {
    use std::io::Write as _;
    let mut out = Vec::new();
    let _ = write!(&mut out, "*{}\r\n", argv.len());
    for arg in argv {
        let _ = write!(&mut out, "${}\r\n", arg.len());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Open a fresh TCP connection, send the request, read until `parse_frame`
/// can consume a full frame (or timeout), and return the captured bytes.
fn send_and_capture(port: u16, argv: &[&[u8]]) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect oracle");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set write timeout");
    stream
        .write_all(&encode_request(argv))
        .expect("write request");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // Try a parse on the running buffer — succeed fast when a complete
        // frame has arrived; otherwise keep reading.
        if !buf.is_empty() {
            match parse_frame(&buf) {
                Ok(result) => {
                    // Trim to exactly the consumed prefix so the round-trip
                    // assertions below compare apples-to-apples.
                    buf.truncate(result.consumed);
                    return buf;
                }
                Err(fr_protocol::RespParseError::Incomplete) => {}
                Err(err) => {
                    panic!("parse_frame failed on partial buffer for argv={argv:?}: {err:?}")
                }
            }
        }
        if Instant::now() > deadline {
            panic!("oracle did not emit a complete frame in time for argv={argv:?}");
        }
        match stream.read(&mut chunk) {
            Ok(0) => break, // server closed; return whatever we have
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                sleep(Duration::from_millis(10));
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("stream.read for argv={argv:?}: {e:?}"),
        }
    }
    buf
}

fn assert_roundtrip(label: &str, bytes: &[u8]) {
    let parsed = parse_frame(bytes)
        .unwrap_or_else(|err| panic!("{label}: parse_frame failed: {err:?} on {bytes:?}"));
    assert_eq!(
        parsed.consumed,
        bytes.len(),
        "{label}: parse_frame consumed {} of {} bytes",
        parsed.consumed,
        bytes.len()
    );
    let reencoded = parsed.frame.to_bytes();
    assert_eq!(
        reencoded.as_slice(),
        bytes,
        "{label}: re-encode diverged from oracle bytes"
    );
    let reparsed = parse_frame(&reencoded)
        .unwrap_or_else(|err| panic!("{label}: re-parse of our own encoded bytes failed: {err:?}"));
    assert_eq!(
        reparsed.frame, parsed.frame,
        "{label}: structural equality broken across re-parse"
    );
}

#[test]
fn live_oracle_roundtrip_canonical_corpus() {
    let Some(oracle) = VendoredRedis::spawn() else {
        eprintln!(
            "[SKIP] vendored redis-server unavailable; \
             set FR_CONFORMANCE_SKIP_LIVE_ORACLE=1 to silence this log"
        );
        return;
    };

    // Seed state for commands that read existing keys.
    // Each (label, argv) tuple is a fresh request on a fresh connection.
    let seed: &[(&str, &[&[u8]])] = &[
        (
            "seed:DEL",
            &[
                b"DEL", b"k:str", b"k:int", b"k:list", b"k:hash", b"k:set", b"k:zset",
            ],
        ),
        ("seed:SET str", &[b"SET", b"k:str", b"hello"]),
        ("seed:SET int", &[b"SET", b"k:int", b"42"]),
        ("seed:RPUSH list", &[b"RPUSH", b"k:list", b"a", b"b", b"c"]),
        (
            "seed:HSET hash",
            &[b"HSET", b"k:hash", b"f1", b"v1", b"f2", b"v2"],
        ),
        ("seed:SADD set", &[b"SADD", b"k:set", b"x", b"y", b"z"]),
        (
            "seed:ZADD zset",
            &[b"ZADD", b"k:zset", b"1", b"a", b"2", b"b", b"3", b"c"],
        ),
    ];
    for (label, argv) in seed {
        let bytes = send_and_capture(oracle.port, argv);
        assert!(!bytes.is_empty(), "{label}: empty reply from oracle");
        assert_roundtrip(label, &bytes);
    }

    // Actual canonical-command corpus. Each case exercises a distinct
    // reply shape from RESP2:
    //   + simple-string
    //   - error
    //   : integer
    //   $ bulk-string (incl. nil-bulk)
    //   * array (incl. empty array, nested array, nil-array)
    let corpus: &[(&str, &[&[u8]])] = &[
        // Simple strings
        ("PING", &[b"PING"]),
        ("PING with arg", &[b"PING", b"hello"]),
        ("SET OK", &[b"SET", b"k:str", b"hello"]),
        (
            "SET with NX-already-exists-nil",
            &[b"SET", b"k:str", b"new", b"NX"],
        ),
        ("CLIENT SETNAME", &[b"CLIENT", b"SETNAME", b"cc-redis-test"]),
        ("CLIENT GETNAME", &[b"CLIENT", b"GETNAME"]),
        ("ECHO", &[b"ECHO", b"round-trip-me"]),
        ("SELECT 0", &[b"SELECT", b"0"]),
        // Errors
        ("WRONGTYPE on list key", &[b"GET", b"k:list"]),
        ("Unknown command", &[b"NOSUCHCOMMAND", b"x"]),
        ("Wrong arity GET", &[b"GET"]),
        ("Wrong arity SET", &[b"SET", b"only-one-arg"]),
        ("Invalid integer SELECT", &[b"SELECT", b"not-a-number"]),
        // Integers
        ("STRLEN", &[b"STRLEN", b"k:str"]),
        ("INCR", &[b"INCR", b"k:int"]),
        ("INCRBY", &[b"INCRBY", b"k:int", b"7"]),
        ("DECR", &[b"DECR", b"k:int"]),
        (
            "EXISTS multiple",
            &[b"EXISTS", b"k:str", b"k:int", b"k:missing"],
        ),
        ("TTL no-expire", &[b"TTL", b"k:str"]),
        ("TTL missing", &[b"TTL", b"k:never"]),
        ("PTTL no-expire", &[b"PTTL", b"k:str"]),
        ("DEL none", &[b"DEL", b"k:never1", b"k:never2"]),
        ("DBSIZE", &[b"DBSIZE"]),
        ("LLEN", &[b"LLEN", b"k:list"]),
        ("HLEN", &[b"HLEN", b"k:hash"]),
        ("SCARD", &[b"SCARD", b"k:set"]),
        ("ZCARD", &[b"ZCARD", b"k:zset"]),
        ("PERSIST no-ttl", &[b"PERSIST", b"k:str"]),
        ("EXPIRE", &[b"EXPIRE", b"k:str", b"60"]),
        (
            "EXPIRE with XX flag absent",
            &[b"EXPIRE", b"k:str", b"30", b"XX"],
        ),
        // Bulk strings
        ("GET existing", &[b"GET", b"k:str"]),
        ("GET missing", &[b"GET", b"k:missing"]),
        ("TYPE string", &[b"TYPE", b"k:str"]),
        ("TYPE missing", &[b"TYPE", b"k:missing"]),
        ("TYPE hash", &[b"TYPE", b"k:hash"]),
        ("TYPE zset", &[b"TYPE", b"k:zset"]),
        ("GETRANGE full", &[b"GETRANGE", b"k:str", b"0", b"-1"]),
        ("HGET existing field", &[b"HGET", b"k:hash", b"f1"]),
        ("HGET missing field", &[b"HGET", b"k:hash", b"nofield"]),
        ("LINDEX 0", &[b"LINDEX", b"k:list", b"0"]),
        ("LINDEX oob", &[b"LINDEX", b"k:list", b"99"]),
        // Arrays
        ("LRANGE full", &[b"LRANGE", b"k:list", b"0", b"-1"]),
        ("LRANGE empty window", &[b"LRANGE", b"k:list", b"10", b"20"]),
        (
            "LRANGE negative window",
            &[b"LRANGE", b"k:list", b"-2", b"-1"],
        ),
        ("HGETALL", &[b"HGETALL", b"k:hash"]),
        ("HKEYS", &[b"HKEYS", b"k:hash"]),
        ("HVALS", &[b"HVALS", b"k:hash"]),
        (
            "HMGET present+missing",
            &[b"HMGET", b"k:hash", b"f1", b"f2", b"nofield"],
        ),
        ("SMEMBERS", &[b"SMEMBERS", b"k:set"]),
        ("SINTER single", &[b"SINTER", b"k:set"]),
        ("ZRANGE", &[b"ZRANGE", b"k:zset", b"0", b"-1"]),
        (
            "ZRANGE WITHSCORES",
            &[b"ZRANGE", b"k:zset", b"0", b"-1", b"WITHSCORES"],
        ),
        (
            "ZRANGEBYSCORE inf",
            &[b"ZRANGEBYSCORE", b"k:zset", b"-inf", b"+inf"],
        ),
        ("KEYS glob", &[b"KEYS", b"k:*"]),
        ("OBJECT ENCODING str", &[b"OBJECT", b"ENCODING", b"k:str"]),
        (
            "OBJECT ENCODING missing",
            &[b"OBJECT", b"ENCODING", b"k:missing"],
        ),
        ("CONFIG GET fixed", &[b"CONFIG", b"GET", b"maxmemory"]),
        ("DEBUG OBJECT missing", &[b"DEBUG", b"OBJECT", b"k:missing"]),
        // Nil-bulk & nil-array edges
        ("LPOP missing", &[b"LPOP", b"k:never-list"]),
        (
            "RPOPLPUSH missing",
            &[b"RPOPLPUSH", b"k:never-list", b"k:never-list-2"],
        ),
    ];

    for (label, argv) in corpus {
        let bytes = send_and_capture(oracle.port, argv);
        assert!(!bytes.is_empty(), "{label}: empty reply from oracle");
        assert_roundtrip(label, &bytes);
    }
}

/// Upstream-bytes equality cross-check: explicitly confirm that a few
/// well-known simple replies have the exact wire shape our parser claims
/// they do. Catches accidental drift in our encoder — e.g., stray
/// whitespace, missing CRLF, or simple-vs-bulk confusion.
#[test]
fn live_oracle_byte_exact_frames_for_canonical_replies() {
    let Some(oracle) = VendoredRedis::spawn() else {
        eprintln!("[SKIP] vendored redis-server unavailable");
        return;
    };

    // PING → +PONG\r\n
    let ping = send_and_capture(oracle.port, &[b"PING"]);
    assert_eq!(ping, b"+PONG\r\n");
    let parsed = parse_frame(&ping).expect("parse PING reply");
    match &parsed.frame {
        RespFrame::SimpleString(s) => assert_eq!(s, "PONG"),
        other => panic!("expected SimpleString(PONG), got {other:?}"),
    }

    // SET k v → +OK\r\n
    let set = send_and_capture(oracle.port, &[b"SET", b"exact:k", b"v"]);
    assert_eq!(set, b"+OK\r\n");

    // STRLEN on existing key → :N\r\n where N > 0
    let _ = send_and_capture(oracle.port, &[b"SET", b"exact:s", b"abcdef"]);
    let strlen = send_and_capture(oracle.port, &[b"STRLEN", b"exact:s"]);
    assert_eq!(strlen, b":6\r\n");
    match parse_frame(&strlen).expect("parse STRLEN").frame {
        RespFrame::Integer(n) => assert_eq!(n, 6),
        other => panic!("expected Integer(6), got {other:?}"),
    }

    // GET missing → $-1\r\n (null bulk)
    let get_missing = send_and_capture(oracle.port, &[b"GET", b"exact:never"]);
    assert_eq!(get_missing, b"$-1\r\n");
    match parse_frame(&get_missing).expect("parse nil bulk").frame {
        RespFrame::BulkString(None) => {}
        other => panic!("expected BulkString(None), got {other:?}"),
    }

    // EXISTS on missing → :0\r\n
    let exists = send_and_capture(oracle.port, &[b"EXISTS", b"exact:never"]);
    assert_eq!(exists, b":0\r\n");

    // LRANGE on missing list → *0\r\n (empty array, not nil)
    let lrange = send_and_capture(oracle.port, &[b"LRANGE", b"exact:never-list", b"0", b"-1"]);
    assert_eq!(lrange, b"*0\r\n");
    match parse_frame(&lrange).expect("parse empty array").frame {
        RespFrame::Array(Some(v)) => assert!(v.is_empty()),
        other => panic!("expected empty Array, got {other:?}"),
    }
}
