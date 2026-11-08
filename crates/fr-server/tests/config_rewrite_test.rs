use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fr_protocol::{RespFrame, parse_frame};

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
    // (frankenredis-pve7s) See the note in client_kill_test.rs: binding :0 and
    // reading the port drops the listener before the server binds, leaving a
    // window in which any other process can take it. A per-binary monotonic
    // counter removes the test-vs-test case; the bind is only a liveness probe.
    // Base is unique per test binary.
    static NEXT_PORT: AtomicU16 = AtomicU16::new(35_000);
    for _ in 0..500 {
        let port = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        if port < 35_000 {
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
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("set read timeout");
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

fn spawn_frankenredis_with_config(port: u16, config_path: &Path) -> ManagedChild {
    let log_dir = unique_temp_dir("frankenredis-config-rewrite-log");
    let log_path = log_dir.join("stderr.log");
    let log_file = fs::File::create(log_path).expect("create stderr log file");
    let mut command = Command::new(env!("CARGO_BIN_EXE_frankenredis"));
    command
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--mode")
        .arg("strict")
        .arg("--config")
        .arg(config_path)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));
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

#[test]
fn config_rewrite_updates_file() {
    let port = reserve_port();
    let temp_dir = unique_temp_dir("frankenredis-config-rewrite");
    let config_path = temp_dir.join("frankenredis.conf");
    fs::write(&config_path, "").expect("write initial config file");

    let _server = spawn_frankenredis_with_config(port, &config_path);
    let mut client = connect_client(port);

    assert_eq!(
        send_command(&mut client, &[b"CONFIG", b"SET", b"timeout", b"123"]),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        send_command(&mut client, &[b"CONFIG", b"REWRITE"]),
        RespFrame::SimpleString("OK".to_string())
    );

    wait_until(
        Duration::from_secs(2),
        || {
            fs::read_to_string(&config_path)
                .map(|content| content.contains("timeout 123"))
                .unwrap_or(false)
        },
        "CONFIG REWRITE did not persist the timeout setting",
    );

    let content = fs::read_to_string(&config_path).expect("read rewritten config");
    assert!(
        content.contains("timeout 123"),
        "config file should contain rewritten parameter, got: {content}"
    );

    send_shutdown_nosave(port);
}

#[test]
fn shutdown_save_failure_preserves_server_data_and_resumes_pipelined_commands() {
    let port = reserve_port();
    let temp_dir = unique_temp_dir("fr-shutdown-failure");
    let config_path = temp_dir.join("frankenredis.conf");
    let rdb_path = temp_dir.join("dump.rdb");
    let backup_path = temp_dir.join("previous.rdb");
    let pid_path = temp_dir.join("server.pid");
    #[cfg(unix)]
    let socket_path = temp_dir.join("server.sock");
    let config = format!(
        "dir {}\ndbfilename dump.rdb\nsave \"\"\npidfile {}\n",
        temp_dir.display(),
        pid_path.display()
    );
    #[cfg(unix)]
    let config = format!("{config}unixsocket {}\n", socket_path.display());
    fs::write(&config_path, config).expect("write shutdown failure config");

    let mut server = spawn_frankenredis_with_config(port, &config_path);
    let mut client = connect_client(port);
    assert_eq!(
        send_command(&mut client, &[b"INCR", b"counter"]),
        RespFrame::Integer(1)
    );
    assert_eq!(
        send_command(&mut client, &[b"SAVE"]),
        RespFrame::SimpleString("OK".to_string())
    );
    let previous_snapshot = fs::read(&rdb_path).expect("read previous snapshot");
    let pid_contents = fs::read(&pid_path).expect("read live pid file");
    assert_eq!(
        send_command(&mut client, &[b"INCR", b"counter"]),
        RespFrame::Integer(2)
    );
    // A directory at the final filename makes rename fail even when tests run
    // as root. Preserve the original snapshot for a byte-for-byte check.
    fs::rename(&rdb_path, &backup_path).expect("preserve previous snapshot");
    fs::create_dir(&rdb_path).expect("obstruct final snapshot filename");

    let mut pipeline = encode_command(&[b"SHUTDOWN", b"SAVE"]);
    pipeline.extend(encode_command(&[b"GET", b"counter"]));
    pipeline.extend(encode_command(&[b"PING"]));
    client.write_all(&pipeline).expect("send shutdown pipeline");
    let expected = b"-ERR Errors trying to SHUTDOWN. Check logs.\r\n$1\r\n2\r\n+PONG\r\n";
    let mut actual = vec![0; expected.len()];
    client
        .read_exact(&mut actual)
        .expect("shutdown failure and queued commands must reply");
    assert_eq!(actual.as_slice(), expected);
    assert!(server.child.try_wait().expect("poll child").is_none());
    assert_eq!(
        fs::read(&pid_path).expect("pid file survives"),
        pid_contents
    );
    assert_eq!(
        fs::read(&backup_path).expect("previous snapshot survives"),
        previous_snapshot
    );
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let mut unix_client = UnixStream::connect(&socket_path).expect("Unix listener survives");
        unix_client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set Unix client timeout");
        unix_client
            .write_all(&encode_command(&[b"GET", b"counter"]))
            .expect("read retained data over Unix socket");
        let mut reply = [0; 7];
        unix_client.read_exact(&mut reply).expect("read Unix reply");
        assert_eq!(&reply, b"$1\r\n2\r\n");
    }
    assert_eq!(
        send_command(&mut client, &[b"SHUTDOWN", b"ABORT"]),
        RespFrame::Error("ERR No shutdown in progress.".to_string())
    );
    assert_eq!(
        send_command(&mut client, &[b"INCR", b"counter"]),
        RespFrame::Integer(3)
    );

    // FORCE is explicit consent to exit despite this same snapshot failure.
    // A successful shutdown closes the connection without sending +OK.
    client
        .write_all(&encode_command(&[b"SHUTDOWN", b"SAVE", b"FORCE"]))
        .expect("request forced shutdown");
    let mut reply = [0; 64];
    assert_eq!(client.read(&mut reply).expect("read shutdown EOF"), 0);
    wait_until(
        Duration::from_secs(5),
        || {
            server
                .child
                .try_wait()
                .expect("poll forced shutdown")
                .is_some()
        },
        "forced shutdown did not exit",
    );
    assert!(
        server
            .child
            .wait()
            .expect("wait for forced shutdown")
            .success()
    );
    assert!(!pid_path.exists());
    #[cfg(unix)]
    assert!(!socket_path.exists());
    assert_eq!(
        fs::read(&backup_path).expect("forced shutdown preserves previous snapshot"),
        previous_snapshot
    );
}

#[test]
fn shutdown_default_skips_disabled_snapshots_but_explicit_save_persists() {
    let temp_dir = unique_temp_dir("fr-shutdown-save");
    for explicit_save in [false, true] {
        let name = if explicit_save { "save" } else { "default" };
        let instance_dir = temp_dir.join(name);
        fs::create_dir(&instance_dir).expect("create shutdown instance directory");
        let config_path = instance_dir.join("frankenredis.conf");
        let rdb_path = instance_dir.join("dump.rdb");
        fs::write(
            &config_path,
            format!(
                "dir {}\ndbfilename dump.rdb\nsave \"\"\n",
                instance_dir.display()
            ),
        )
        .expect("write shutdown save config");
        let port = reserve_port();
        let mut server = spawn_frankenredis_with_config(port, &config_path);
        let mut client = connect_client(port);
        assert_eq!(
            send_command(&mut client, &[b"INCR", b"counter"]),
            RespFrame::Integer(1)
        );
        let shutdown: &[&[u8]] = if explicit_save {
            &[b"SHUTDOWN", b"SAVE"]
        } else {
            &[b"SHUTDOWN"]
        };
        client
            .write_all(&encode_command(shutdown))
            .expect("request shutdown");
        let mut reply = [0; 64];
        assert_eq!(client.read(&mut reply).expect("read shutdown EOF"), 0);
        wait_until(
            Duration::from_secs(5),
            || server.child.try_wait().expect("poll shutdown").is_some(),
            "shutdown did not exit",
        );
        assert!(server.child.wait().expect("wait for shutdown").success());
        assert_eq!(rdb_path.is_file(), explicit_save);

        let restart_port = reserve_port();
        let _restarted = spawn_frankenredis_with_config(restart_port, &config_path);
        let mut restarted_client = connect_client(restart_port);
        assert_eq!(
            send_command(&mut restarted_client, &[b"GET", b"counter"]),
            RespFrame::BulkString(explicit_save.then(|| b"1".to_vec()))
        );
        send_shutdown_nosave(restart_port);
    }
}
