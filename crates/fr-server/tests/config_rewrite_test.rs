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
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
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
