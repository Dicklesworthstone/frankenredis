//! Differential gate: FUNCTION LOAD must embed a non-UTF8 source byte verbatim.
//! (frankenredis-7qmmr)
//!
//! Upstream `FUNCTION LOAD` compiles the library body and, on a syntax error,
//! reports `Error compiling function: user_function:<line>: unexpected symbol
//! near '<X>'` where `<X>` is the offending source byte COPIED VERBATIM. When
//! that byte is not valid UTF-8 the reply is not a valid UTF-8 string either.
//!
//! frankenredis builds that message through a Lua lexer whose error type is
//! `String`, which cannot hold a lone `0xFF`, so the byte was widened with
//! `b as char` and re-encoded on the wire as `0xC3 0xBF`. The reply was the
//! right ERROR CLASS but the wrong BYTES.
//!
//! This gate drives both engines and compares the raw reply bytes, so the
//! divergence is caught at the only layer where it is visible — a decoded or
//! lossy comparison would call `\xc3\xbf` and `\xff` equal.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn encode_command(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", parts.len()).into_bytes();
    for part in parts {
        out.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
        out.extend_from_slice(part);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Read one CRLF-terminated line — enough for the `-ERR ...` replies this gate
/// compares, and deliberately byte-level so nothing is normalised on the way.
fn read_line(stream: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut byte = [0_u8; 1];
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match stream.read(&mut byte) {
            Ok(0) => panic!("server closed the connection mid-reply"),
            Ok(_) => {
                out.push(byte[0]);
                if out.ends_with(b"\r\n") {
                    return out;
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                assert!(Instant::now() < deadline, "timed out reading reply");
                thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!("read from server: {err}"),
        }
    }
}

fn exchange(stream: &mut TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    stream
        .write_all(&encode_command(parts))
        .expect("write command");
    read_line(stream)
}

fn reserve_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    // (frankenredis-pve7s) This binary is where the race was actually OBSERVED:
    // `wait_for_port` succeeded and the following `connect_client` was refused,
    // which is the signature of another process having taken the port and then
    // closed -- a merely slow server would have failed inside `wait_for_port`
    // instead. Binding :0 and reading the port drops the listener before the
    // server binds, leaving exactly that window. A per-binary monotonic counter
    // removes the test-vs-test case; the bind is only a liveness probe.
    static NEXT_PORT: AtomicU16 = AtomicU16::new(36_000);
    for _ in 0..500 {
        let port = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        if port < 36_000 {
            continue;
        }
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("could not reserve a free TCP port");
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
    let stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set read timeout");
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
    let dir = unique_temp_dir("frankenredis-function-nonutf8");
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

/// A library whose body contains `byte` where Lua expects a symbol.
fn library_with_byte(byte: u8) -> Vec<u8> {
    let mut body = b"#!lua name=probe\nlocal x = ".to_vec();
    body.push(byte);
    body.extend_from_slice(b"\n");
    body
}

#[test]
fn function_load_compile_error_embeds_the_offending_byte_verbatim() {
    let fr_port = reserve_port();
    let redis_port = reserve_port();
    let _fr = spawn_frankenredis(fr_port);
    let _redis = spawn_legacy_redis(redis_port);
    let mut fr = connect_client(fr_port);
    let mut redis = connect_client(redis_port);

    // Bytes that are not valid standalone UTF-8. 0xFF and 0xFE never appear in
    // well-formed UTF-8 at all; 0xC3 and 0x80 are a lead and a continuation
    // byte, which the lexer sees individually.
    for byte in [0xFF_u8, 0xFE, 0xC3, 0x80, 0x90] {
        let library = library_with_byte(byte);
        let fr_reply = exchange(&mut fr, &[b"FUNCTION", b"LOAD", &library]);
        let redis_reply = exchange(&mut redis, &[b"FUNCTION", b"LOAD", &library]);

        assert_eq!(
            fr_reply,
            redis_reply,
            "FUNCTION LOAD reply diverged for body byte {byte:#04x}\n  fr    = {:?}\n  redis = {:?}",
            String::from_utf8_lossy(&fr_reply),
            String::from_utf8_lossy(&redis_reply)
        );

        // The point of the gate: the raw byte must be present, and its UTF-8
        // re-encoding must not be.
        assert!(
            fr_reply.contains(&byte),
            "reply for {byte:#04x} did not contain the raw byte: {fr_reply:?}"
        );
        if byte >= 0x80 {
            let utf8_widened = char::from(byte).to_string().into_bytes();
            assert_eq!(
                utf8_widened.len(),
                2,
                "a high byte widens to two UTF-8 bytes"
            );
            assert!(
                !fr_reply
                    .windows(2)
                    .any(|window| window == utf8_widened.as_slice()),
                "reply for {byte:#04x} still carries the UTF-8-widened form {utf8_widened:?}"
            );
        }
    }
}

/// An ASCII syntax error must be unaffected by the byte-preserving path.
#[test]
fn function_load_ascii_compile_error_is_unchanged() {
    let fr_port = reserve_port();
    let redis_port = reserve_port();
    let _fr = spawn_frankenredis(fr_port);
    let _redis = spawn_legacy_redis(redis_port);
    let mut fr = connect_client(fr_port);
    let mut redis = connect_client(redis_port);

    for body in [
        &b"#!lua name=probe\nlocal x = @\n"[..],
        &b"#!lua name=probe\nlocal x = ~\n"[..],
        &b"#!lua name=probe\nif then end\n"[..],
    ] {
        let fr_reply = exchange(&mut fr, &[b"FUNCTION", b"LOAD", body]);
        let redis_reply = exchange(&mut redis, &[b"FUNCTION", b"LOAD", body]);
        assert_eq!(
            fr_reply,
            redis_reply,
            "ASCII compile error diverged\n  fr    = {:?}\n  redis = {:?}",
            String::from_utf8_lossy(&fr_reply),
            String::from_utf8_lossy(&redis_reply)
        );
    }
}

/// A well-formed library must still load, and a metadata error must still win
/// precedence over the compile error.
#[test]
fn function_load_success_and_metadata_precedence_are_unchanged() {
    let fr_port = reserve_port();
    let redis_port = reserve_port();
    let _fr = spawn_frankenredis(fr_port);
    let _redis = spawn_legacy_redis(redis_port);
    let mut fr = connect_client(fr_port);
    let mut redis = connect_client(redis_port);

    let good = b"#!lua name=oklib\nredis.register_function('f', function() return 1 end)\n";
    assert_eq!(
        exchange(&mut fr, &[b"FUNCTION", b"LOAD", good]),
        exchange(&mut redis, &[b"FUNCTION", b"LOAD", good]),
        "a well-formed library must load identically"
    );

    // Bad metadata AND a bad body: the metadata error wins on both engines.
    let mut both_bad = b"#!lua name=bad\xff\xfe\nlocal x = ".to_vec();
    both_bad.push(0xFF);
    both_bad.extend_from_slice(b"\n");
    assert_eq!(
        exchange(&mut fr, &[b"FUNCTION", b"LOAD", &both_bad]),
        exchange(&mut redis, &[b"FUNCTION", b"LOAD", &both_bad]),
        "metadata-error precedence must be unchanged"
    );

    // No shebang at all.
    let no_header = b"local x = 1\n";
    assert_eq!(
        exchange(&mut fr, &[b"FUNCTION", b"LOAD", no_header]),
        exchange(&mut redis, &[b"FUNCTION", b"LOAD", no_header]),
        "missing-shebang error must be unchanged"
    );
}
