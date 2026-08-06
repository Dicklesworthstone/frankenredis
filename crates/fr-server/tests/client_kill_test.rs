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

fn read_response(stream: &mut TcpStream) -> RespFrame {
    let mut buf = vec![0_u8; 65_536];
    let mut accumulated = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);

    loop {
        match stream.read(&mut buf) {
            Ok(0) => panic!("server closed connection unexpectedly"),
            Ok(n) => {
                accumulated.extend_from_slice(&buf[..n]);
                match parse_frame(&accumulated) {
                    Ok(parsed) => return parsed.frame,
                    Err(_) => continue,
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for server response"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("read from server: {err}"),
        }
    }
}

fn send_command(stream: &mut TcpStream, parts: &[&[u8]]) -> RespFrame {
    stream
        .write_all(&encode_command(parts))
        .expect("write command to server");
    read_response(stream)
}

fn reserve_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    // (frankenredis-pve7s) `bind("127.0.0.1:0")` then reading `.port()` drops the
    // listener at the end of that expression, leaving the port unbound until the
    // spawned server claims it. Anything on the host can take it in between --
    // including another test doing exactly the same thing, since two concurrent
    // binds to :0 can hand out the same freshly-released ephemeral port.
    //
    // A monotonic counter gives every test in this binary a distinct candidate,
    // which removes the test-vs-test collision outright; the bind below is only a
    // liveness probe to skip a port an unrelated process already holds. This is
    // the pattern frankenredis-vcv8o established in tcp_e2e_test.rs for the same
    // failure. The base is unique per test BINARY, because the counter is static
    // per binary and `cargo test` runs binaries concurrently.
    static NEXT_PORT: AtomicU16 = AtomicU16::new(34_000);
    for _ in 0..500 {
        let port = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        if port < 34_000 {
            continue;
        }
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("could not reserve a free TCP port");
}

fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(check(), "{message}");
}

fn wait_for_port(port: u16) {
    wait_until(
        Duration::from_secs(5),
        || TcpStream::connect(format!("127.0.0.1:{port}")).is_ok(),
        &format!("port {port} did not become ready in time"),
    );
}

fn connect_client(port: u16) -> TcpStream {
    let mut retries = 0_u8;
    loop {
        match TcpStream::connect(format!("127.0.0.1:{port}")) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .expect("set read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_millis(250)))
                    .expect("set write timeout");
                return stream;
            }
            Err(err) if retries < 50 => {
                let _ = err;
                retries = retries.saturating_add(1);
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => panic!("failed to connect to 127.0.0.1:{port}: {err}"),
        }
    }
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonical project root")
}

fn legacy_redis_server_path() -> PathBuf {
    project_root().join("legacy_redis_code/redis/src/redis-server")
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

impl ManagedChild {
    fn spawn(mut command: Command) -> Self {
        let child = command.spawn().expect("spawn child process");
        Self { child }
    }
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
    let child = ManagedChild::spawn(command);
    wait_for_port(port);
    child
}

fn spawn_legacy_redis(port: u16) -> ManagedChild {
    let dir = unique_temp_dir("frankenredis-legacy-client-kill");
    let mut command = Command::new(legacy_redis_server_path());
    command
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--save")
        .arg("")
        .arg("--appendonly")
        .arg("no")
        .arg("--repl-diskless-sync")
        .arg("no")
        .arg("--repl-diskless-sync-delay")
        .arg("0")
        .arg("--protected-mode")
        .arg("no")
        .arg("--dir")
        .arg(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = ManagedChild::spawn(command);
    wait_for_port(port);
    child
}

fn send_shutdown_nosave(port: u16) {
    if let Ok(mut client) = TcpStream::connect(format!("127.0.0.1:{port}")) {
        let _ = client.set_read_timeout(Some(Duration::from_millis(250)));
        let _ = client.write_all(&encode_command(&[b"SHUTDOWN", b"NOSAVE"]));
    }
}

fn disconnect_observed(stream: &mut TcpStream) -> bool {
    match stream.write_all(&encode_command(&[b"PING"])) {
        Ok(()) => {}
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotConnected
            ) =>
        {
            return true;
        }
        Err(err) => panic!("write to target after CLIENT KILL: {err}"),
    }

    let mut buf = [0_u8; 1024];
    match stream.read(&mut buf) {
        Ok(0) => true,
        Ok(n) => panic!(
            "expected target disconnect after CLIENT KILL, got {} bytes: {:?}",
            n,
            &buf[..n]
        ),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            true
        }
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            false
        }
        Err(err) => panic!("read from target after CLIENT KILL: {err}"),
    }
}

fn wait_for_disconnect(stream: &mut TcpStream) {
    wait_until(
        Duration::from_secs(2),
        || disconnect_observed(stream),
        "target client did not disconnect after CLIENT KILL",
    );
}

fn exercise_client_kill_by_id(spawn: impl FnOnce(u16) -> ManagedChild) -> i64 {
    let port = reserve_port();
    let _server = spawn(port);

    let mut target = connect_client(port);
    let mut killer = connect_client(port);

    let target_id = match send_command(&mut target, &[b"CLIENT", b"ID"]) {
        RespFrame::Integer(id) => id,
        other => panic!("expected integer CLIENT ID reply, got {other:?}"),
    };
    let target_id_arg = target_id.to_string();
    let kill_reply = send_command(
        &mut killer,
        &[b"CLIENT", b"KILL", b"ID", target_id_arg.as_bytes()],
    );
    let killed = match kill_reply {
        RespFrame::Integer(count) => count,
        other => panic!("expected integer CLIENT KILL reply, got {other:?}"),
    };

    wait_for_disconnect(&mut target);
    send_shutdown_nosave(port);
    killed
}

#[test]
fn client_kill_by_id_disconnects_target() {
    assert_eq!(exercise_client_kill_by_id(spawn_frankenredis), 1);
}

#[test]
fn client_kill_by_id_matches_legacy_redis_reference() {
    let legacy = exercise_client_kill_by_id(spawn_legacy_redis);
    let franken = exercise_client_kill_by_id(spawn_frankenredis);
    assert_eq!(franken, legacy);
}
