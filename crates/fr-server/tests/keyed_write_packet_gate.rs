//! Port-based differential gate for the N-pair keyed-write fast-path packets.
//! (frankenredis-rzajb)
//!
//! HSET and MSET are each served by a ladder of byte-prefix fast-path parsers —
//! dedicated 1/2-pair forms, then a multi form covering a bounded pair count,
//! then a fall-through to the generic argv path. Every rung is a separate
//! parser with its own `*N` prefix literal, so a mistake on any one of them
//! affects only that pair count and is invisible to a test that exercises a
//! single N.
//!
//! This gate drives every rung against a live frankenredis AND a live Redis
//! 7.2.4 on ephemeral ports, and requires the two to agree BYTE FOR BYTE — on
//! the write reply, on the readback, on HLEN, and on OBJECT ENCODING.
//!
//! On the pair-count boundaries: the bead text guessed "N=4..16 pairs, N=17/20
//! generic fallback". The parsers as written serve HSET for 1..=8 pairs
//! (`*4`..`*18`) and MSET for 1..=32 pairs, so the real fast-path/generic seam
//! is at 9 pairs for HSET and 33 for MSET. This gate walks the ACTUAL seam
//! rather than the assumed one, and covers both sides of it — a gate aimed at
//! N=17 would sit entirely in HSET's generic path and prove nothing about the
//! fast path it was written to lock.

use fr_protocol::{RespFrame, parse_frame};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn encode_command(parts: &[&[u8]]) -> Vec<u8> {
    RespFrame::Array(Some(
        parts
            .iter()
            .map(|part| RespFrame::BulkString(Some(part.to_vec())))
            .collect(),
    ))
    .to_bytes()
}

/// Read exactly `frames` complete RESP frames and return the RAW bytes.
///
/// Raw bytes rather than parsed frames is the point of this gate: a decoded
/// comparison would paper over encoding differences (integer vs bulk, RESP2 nil
/// spelling, element ordering) that a client would actually observe.
fn read_raw_frames(stream: &mut TcpStream, frames: usize) -> Vec<u8> {
    let mut accumulated = Vec::new();
    let mut buf = vec![0_u8; 65_536];
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        let mut complete = 0usize;
        let mut rest: &[u8] = &accumulated;
        while !rest.is_empty() {
            match parse_frame(rest) {
                Ok(parsed) => {
                    complete += 1;
                    rest = &rest[parsed.consumed..];
                }
                Err(_) => break,
            }
        }
        if complete >= frames && rest.is_empty() {
            return accumulated;
        }

        match stream.read(&mut buf) {
            Ok(0) => panic!("server closed the connection with {complete}/{frames} frames read"),
            Ok(n) => accumulated.extend_from_slice(&buf[..n]),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                assert!(
                    Instant::now() < deadline,
                    "timed out after {complete}/{frames} frames"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!("read from server: {err}"),
        }
    }
}

/// Send one command and return the raw reply bytes.
fn exchange(stream: &mut TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    stream
        .write_all(&encode_command(parts))
        .expect("write command");
    read_raw_frames(stream, 1)
}

/// Send a pre-encoded pipeline and return the raw bytes of `frames` replies.
fn exchange_pipeline(stream: &mut TcpStream, payload: &[u8], frames: usize) -> Vec<u8> {
    stream.write_all(payload).expect("write pipeline");
    read_raw_frames(stream, frames)
}

fn reserve_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn wait_for_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("port {port} did not become ready in time");
}

fn connect_client(port: u16) -> TcpStream {
    let stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to server");
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");
    stream
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonical project root")
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}

struct ManagedChild {
    child: Child,
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_frankenredis(port: u16) -> ManagedChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_frankenredis"));
    command
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--mode")
        .arg("strict")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = ManagedChild {
        child: command.spawn().expect("spawn frankenredis"),
    };
    wait_for_port(port);
    child
}

fn spawn_legacy_redis(port: u16) -> ManagedChild {
    let dir = unique_temp_dir("frankenredis-keyed-write-gate");
    let mut command = Command::new(project_root().join("legacy_redis_code/redis/src/redis-server"));
    command
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--save")
        .arg("")
        .arg("--appendonly")
        .arg("no")
        .arg("--protected-mode")
        .arg("no")
        .arg("--dir")
        .arg(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = ManagedChild {
        child: command.spawn().expect("spawn legacy redis-server"),
    };
    wait_for_port(port);
    child
}

/// Both engines, connected, with a clean keyspace.
struct Pair {
    _fr: ManagedChild,
    _redis: ManagedChild,
    fr: TcpStream,
    redis: TcpStream,
}

impl Pair {
    fn spawn() -> Self {
        let fr_port = reserve_port();
        let redis_port = reserve_port();
        let _fr = spawn_frankenredis(fr_port);
        let _redis = spawn_legacy_redis(redis_port);
        let mut fr = connect_client(fr_port);
        let mut redis = connect_client(redis_port);
        assert_eq!(exchange(&mut fr, &[b"FLUSHALL"]), b"+OK\r\n");
        assert_eq!(exchange(&mut redis, &[b"FLUSHALL"]), b"+OK\r\n");
        Self {
            _fr,
            _redis,
            fr,
            redis,
        }
    }

    /// Run one command on both engines and require byte-identical replies.
    fn assert_same(&mut self, parts: &[&[u8]], context: &str) -> Vec<u8> {
        let fr = exchange(&mut self.fr, parts);
        let redis = exchange(&mut self.redis, parts);
        assert_eq!(
            fr,
            redis,
            "{context}: reply bytes diverged\n  fr    = {:?}\n  redis = {:?}",
            String::from_utf8_lossy(&fr),
            String::from_utf8_lossy(&redis)
        );
        fr
    }

    fn assert_same_pipeline(&mut self, payload: &[u8], frames: usize, context: &str) {
        let fr = exchange_pipeline(&mut self.fr, payload, frames);
        let redis = exchange_pipeline(&mut self.redis, payload, frames);
        assert_eq!(
            fr,
            redis,
            "{context}: pipelined reply bytes diverged\n  fr    = {:?}\n  redis = {:?}",
            String::from_utf8_lossy(&fr),
            String::from_utf8_lossy(&redis)
        );
    }

    /// Compare HGETALL at the strongest assertion the encoding actually
    /// entitles us to.
    ///
    /// A listpack-encoded hash stores its fields in insertion order, and both
    /// engines walk that order, so the reply must be byte-identical.
    ///
    /// A hashtable-encoded hash has NO defined HGETALL ordering. Redis emits
    /// dict-bucket order, which is a function of its own hash seed and rehash
    /// state — it is not part of the command contract, real clients must not
    /// depend on it, and redis itself will emit a different order after a
    /// rehash. Requiring byte-equality there would be asserting an
    /// implementation detail rather than a parity property, so the field/value
    /// SET is compared instead. That is the whole contract, and it is still
    /// strong: a dropped, duplicated, mis-paired or corrupted field all fail.
    ///
    /// (Observed concretely while writing this gate: for a 4-pair
    /// hashtable-encoded hash frankenredis returned field0..field3 and redis
    /// returned field3, field2, field1, field0 — same pairs, different order.)
    fn assert_hgetall_matches(&mut self, key: &[u8], context: &str) {
        let encoding = self.assert_same(
            &[b"OBJECT", b"ENCODING", key],
            &format!("{context}: OBJECT ENCODING"),
        );
        let ordered = encoding.windows(8).any(|w| w == b"listpack");

        let fr = exchange(&mut self.fr, &[b"HGETALL", key]);
        let redis = exchange(&mut self.redis, &[b"HGETALL", key]);

        if ordered {
            assert_eq!(
                fr,
                redis,
                "{context}: listpack HGETALL must be byte-identical\n  fr    = {:?}\n  redis = {:?}",
                String::from_utf8_lossy(&fr),
                String::from_utf8_lossy(&redis)
            );
            return;
        }

        let mut fr_pairs = bulk_pairs(&fr);
        let mut redis_pairs = bulk_pairs(&redis);
        assert_eq!(
            fr_pairs.len(),
            redis_pairs.len(),
            "{context}: hashtable HGETALL returned a different number of pairs"
        );
        fr_pairs.sort();
        redis_pairs.sort();
        assert_eq!(
            fr_pairs, redis_pairs,
            "{context}: hashtable HGETALL field/value set diverged"
        );
    }
}

/// Decode a flat RESP array of bulk strings into (field, value) pairs.
fn bulk_pairs(raw: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let parsed = parse_frame(raw).expect("HGETALL reply must be a complete frame");
    let elements = match parsed.frame {
        RespFrame::Array(Some(elements)) => elements,
        other => panic!("HGETALL reply was not an array: {other:?}"),
    };
    assert!(
        elements.len().is_multiple_of(2),
        "HGETALL returned an odd number of elements"
    );
    let (pairs, remainder) = elements.as_chunks::<2>();
    assert!(remainder.is_empty(), "HGETALL element count was not even");
    pairs
        .iter()
        .map(|chunk| {
            let field = match &chunk[0] {
                RespFrame::BulkString(Some(bytes)) => bytes.clone(),
                other => panic!("HGETALL field was not a bulk string: {other:?}"),
            };
            let value = match &chunk[1] {
                RespFrame::BulkString(Some(bytes)) => bytes.clone(),
                other => panic!("HGETALL value was not a bulk string: {other:?}"),
            };
            (field, value)
        })
        .collect()
}

/// `HSET key f0 v0 .. f{n-1} v{n-1}` argv for `pairs` field/value pairs.
fn hset_argv(key: &[u8], pairs: usize, value_len: usize) -> Vec<Vec<u8>> {
    let mut argv: Vec<Vec<u8>> = vec![b"HSET".to_vec(), key.to_vec()];
    for i in 0..pairs {
        argv.push(format!("field{i}").into_bytes());
        argv.push(vec![b'a' + (i % 26) as u8; value_len]);
    }
    argv
}

/// `MSET k0 v0 .. k{n-1} v{n-1}` argv for `pairs` key/value pairs.
fn mset_argv(prefix: &str, pairs: usize, value_len: usize) -> Vec<Vec<u8>> {
    let mut argv: Vec<Vec<u8>> = vec![b"MSET".to_vec()];
    for i in 0..pairs {
        argv.push(format!("{prefix}:{i}").into_bytes());
        argv.push(vec![b'a' + (i % 26) as u8; value_len]);
    }
    argv
}

fn borrow(argv: &[Vec<u8>]) -> Vec<&[u8]> {
    argv.iter().map(Vec::as_slice).collect()
}

/// HSET across every rung of the parser ladder plus both sides of the seam.
///
/// 1..=8 pairs are the fast paths (dedicated 1 and 2, multi 3..8); 9 and 10
/// fall through to the generic argv path. Each N is a distinct `*N` prefix
/// literal, so this walks them all rather than sampling.
#[test]
fn hset_pair_ladder_matches_live_redis_byte_for_byte() {
    let mut pair = Pair::spawn();

    for n in 1..=10usize {
        let key = format!("hash:{n}").into_bytes();
        let argv = hset_argv(&key, n, 8);

        // The write reply is the number of NEW fields — an integer whose
        // encoding must match exactly.
        pair.assert_same(&borrow(&argv), &format!("HSET with {n} pairs"));

        // Readback: every field must be present, in the same order, with the
        // same values.
        pair.assert_hgetall_matches(&key, &format!("after {n}-pair HSET"));
        pair.assert_same(&[b"HLEN", &key], &format!("HLEN after {n}-pair HSET"));

        // Re-applying the same pairs must report zero new fields, which
        // exercises the update path through the same parser rung.
        pair.assert_same(&borrow(&argv), &format!("idempotent HSET with {n} pairs"));
        pair.assert_same(&[b"HLEN", &key], &format!("HLEN after idempotent {n}-pair"));
    }
}

/// MSET across the dedicated rungs and into the multi form.
#[test]
fn mset_pair_ladder_matches_live_redis_byte_for_byte() {
    let mut pair = Pair::spawn();

    for n in 1..=12usize {
        let argv = mset_argv(&format!("str{n}"), n, 8);
        pair.assert_same(&borrow(&argv), &format!("MSET with {n} pairs"));

        // MGET the same keys back, in order.
        let mut mget: Vec<Vec<u8>> = vec![b"MGET".to_vec()];
        for i in 0..n {
            mget.push(format!("str{n}:{i}").into_bytes());
        }
        pair.assert_same(&borrow(&mget), &format!("MGET after {n}-pair MSET"));

        // A missing key in the middle must produce an identically-spelled nil.
        let mut mixed: Vec<Vec<u8>> = vec![b"MGET".to_vec()];
        mixed.push(format!("str{n}:0").into_bytes());
        mixed.push(format!("str{n}:absent").into_bytes());
        pair.assert_same(
            &borrow(&mixed),
            &format!("MGET with a hole after {n} pairs"),
        );
    }
}

/// The fast-path/generic seam itself.
///
/// HSET serves at most 8 pairs on the fast path. 8 and 9 differ only in taking
/// different code paths inside frankenredis, so requiring both to match redis
/// is what proves the seam is invisible from the wire.
#[test]
fn hset_fast_path_and_generic_fallback_agree_across_the_seam() {
    let mut pair = Pair::spawn();

    for n in [7usize, 8, 9, 16, 20] {
        let key = format!("seam:{n}").into_bytes();
        let argv = hset_argv(&key, n, 8);
        pair.assert_same(
            &borrow(&argv),
            &format!("HSET with {n} pairs across the fast-path seam"),
        );
        pair.assert_same(&[b"HLEN", &key], &format!("HLEN at the {n}-pair seam"));
        pair.assert_hgetall_matches(&key, &format!("at the {n}-pair seam"));
    }
}

/// A single write carrying many different pair counts back to back.
///
/// Each fast-path parser reports how many bytes it consumed, and the next
/// packet is parsed from exactly there. A consumed-length error on any rung
/// would desynchronise everything after it in the same buffer — which a
/// one-command-per-write test cannot detect, because the socket boundary hides
/// the mistake.
#[test]
fn mixed_pair_count_pipeline_stays_in_sync_with_live_redis() {
    let mut pair = Pair::spawn();

    let mut payload = Vec::new();
    let mut frames = 0usize;
    for n in [1usize, 5, 2, 8, 3, 9, 4, 6, 7, 2, 8, 1] {
        let key = format!("pipe:{frames}").into_bytes();
        let argv = hset_argv(&key, n, 6);
        payload.extend_from_slice(&encode_command(&borrow(&argv)));
        frames += 1;
        payload.extend_from_slice(&encode_command(&[b"HLEN", &key]));
        frames += 1;

        let mset = mset_argv(&format!("pipestr{frames}"), n, 6);
        payload.extend_from_slice(&encode_command(&borrow(&mset)));
        frames += 1;
    }
    // A trailing PING proves the parser landed exactly on the end of the last
    // packet rather than a byte either side of it.
    payload.extend_from_slice(&encode_command(&[b"PING"]));
    frames += 1;

    pair.assert_same_pipeline(&payload, frames, "mixed-N keyed-write pipeline");
}

/// Long values must drive the same encoding transition on both engines.
///
/// A hash stays listpack-encoded only while every value is under
/// hash-max-listpack-value (64 by default). Crossing that inside a multi-pair
/// fast-path write must convert exactly as redis does — the fast path writes
/// through a different code path than the generic one, so this is where a
/// missed conversion would hide.
#[test]
fn long_value_encoding_transition_matches_live_redis() {
    let mut pair = Pair::spawn();

    // Well under the listpack value limit: stays listpack at every pair count.
    for n in [1usize, 4, 8] {
        let key = format!("short:{n}").into_bytes();
        pair.assert_same(&borrow(&hset_argv(&key, n, 16)), "short-value HSET");
        pair.assert_same(
            &[b"OBJECT", b"ENCODING", &key],
            &format!("short-value encoding at {n} pairs"),
        );
    }

    // Over the 64-byte listpack value limit: must convert to hashtable.
    for n in [1usize, 4, 8, 9] {
        let key = format!("long:{n}").into_bytes();
        pair.assert_same(&borrow(&hset_argv(&key, n, 200)), "long-value HSET");
        pair.assert_same(&[b"HLEN", &key], &format!("long-value HLEN at {n} pairs"));
        pair.assert_hgetall_matches(&key, &format!("long-value at {n} pairs"));
    }

    // A hash that starts short and is then grown past the limit by a later
    // multi-pair write must end up converted, not stuck listpack.
    let key = b"grow".to_vec();
    pair.assert_same(&borrow(&hset_argv(&key, 4, 8)), "grow: initial short write");
    pair.assert_same(&[b"OBJECT", b"ENCODING", &key], "grow: encoding before");
    let mut grow: Vec<Vec<u8>> = vec![b"HSET".to_vec(), key.clone()];
    for i in 0..4usize {
        grow.push(format!("long{i}").into_bytes());
        grow.push(vec![b'z'; 200]);
    }
    pair.assert_same(&borrow(&grow), "grow: long-value write");
    pair.assert_same(&[b"OBJECT", b"ENCODING", &key], "grow: encoding after");
    pair.assert_same(&[b"HLEN", &key], "grow: HLEN after");
}

/// Empty and binary payloads through the same rungs.
///
/// Zero-length bulks and values containing CR, LF and NUL are where a
/// hand-rolled byte-prefix parser is most likely to mis-measure a length.
#[test]
fn binary_and_empty_payloads_match_live_redis() {
    let mut pair = Pair::spawn();

    for n in [1usize, 3, 5, 8, 9] {
        let key = format!("bin:{n}").into_bytes();
        let mut argv: Vec<Vec<u8>> = vec![b"HSET".to_vec(), key.clone()];
        for i in 0..n {
            argv.push(format!("f{i}").into_bytes());
            argv.push(match i % 3 {
                0 => Vec::new(),
                1 => b"with\r\nCRLF".to_vec(),
                _ => vec![0_u8, b'x', 0_u8, b'y'],
            });
        }
        pair.assert_same(&borrow(&argv), &format!("binary HSET with {n} pairs"));
        pair.assert_hgetall_matches(&key, &format!("binary at {n} pairs"));
        pair.assert_same(&[b"HLEN", &key], &format!("binary HLEN at {n} pairs"));
    }
}
