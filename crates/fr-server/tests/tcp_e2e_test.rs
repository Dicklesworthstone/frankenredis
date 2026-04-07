//! End-to-end TCP tests that spin up a minimal FrankenRedis server,
//! connect via TCP, send RESP commands, and verify responses.
//! Tests the actual networking stack including RESP framing.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fr_config::RuntimePolicy;
use fr_protocol::{ParserConfig, RespFrame, parse_frame};
use fr_runtime::Runtime;

/// Encode a command as RESP array of bulk strings.
fn encode_command(parts: &[&[u8]]) -> Vec<u8> {
    RespFrame::Array(Some(
        parts
            .iter()
            .map(|p| RespFrame::BulkString(Some(p.to_vec())))
            .collect(),
    ))
    .to_bytes()
}

/// Read a complete RESP frame from a stream.
fn read_response(stream: &mut TcpStream) -> RespFrame {
    let mut buf = vec![0u8; 65536];
    let mut accumulated = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);

    loop {
        match stream.read(&mut buf) {
            Ok(0) => panic!("server closed connection unexpectedly"),
            Ok(n) => {
                accumulated.extend_from_slice(&buf[..n]);
                match parse_frame(&accumulated) {
                    Ok(parsed) => return parsed.frame,
                    Err(_) => continue, // incomplete, read more
                }
            }
            Err(ref err)
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
    log_path: Option<PathBuf>,
}

impl ManagedChild {
    fn spawn(mut command: Command, log_path: Option<PathBuf>) -> Self {
        let child = command.spawn().expect("spawn child process");
        Self { child, log_path }
    }

    fn log_contents(&self) -> Option<String> {
        self.log_path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_legacy_redis(port: u16) -> ManagedChild {
    let dir = unique_temp_dir("frankenredis-legacy");
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
    let child = ManagedChild::spawn(command, None);
    wait_for_port(port);
    child
}

fn spawn_frankenredis(port: u16, primary_port: Option<u16>) -> ManagedChild {
    let log_dir = unique_temp_dir("frankenredis-server-log");
    let log_path = log_dir.join("stderr.log");
    let log_file = std::fs::File::create(&log_path).expect("create replica log file");
    let mut command = Command::new(env!("CARGO_BIN_EXE_frankenredis"));
    command
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--mode")
        .arg("strict")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));
    if let Some(primary_port) = primary_port {
        command
            .arg("--replicaof")
            .arg("127.0.0.1")
            .arg(primary_port.to_string());
    }
    let child = ManagedChild::spawn(command, Some(log_path));
    wait_for_port(port);
    child
}

fn fetch_info_replication(port: u16) -> Option<String> {
    let mut client = TcpStream::connect(format!("127.0.0.1:{port}")).ok()?;
    client.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    let response = send_command(&mut client, &[b"INFO", b"replication"]);
    match response {
        RespFrame::BulkString(Some(bytes)) => String::from_utf8(bytes).ok(),
        _ => None,
    }
}

fn fetch_string_value(port: u16, key: &[u8]) -> Option<Vec<u8>> {
    let mut client = TcpStream::connect(format!("127.0.0.1:{port}")).ok()?;
    client.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    match send_command(&mut client, &[b"GET", key]) {
        RespFrame::BulkString(Some(bytes)) => Some(bytes),
        RespFrame::BulkString(None) => None,
        _ => None,
    }
}

fn send_shutdown_nosave(port: u16) {
    if let Ok(mut client) = TcpStream::connect(format!("127.0.0.1:{port}")) {
        let _ = client.set_read_timeout(Some(Duration::from_millis(250)));
        let _ = client.write_all(&encode_command(&[b"SHUTDOWN", b"NOSAVE"]));
    }
}

/// Start a minimal single-client server on a random port.
/// Returns the port number. The server handles one connection
/// then exits when the client disconnects.
fn start_single_client_server() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let handle = thread::spawn(move || {
        listener.set_nonblocking(false).expect("set blocking mode");
        let (mut stream, _) = listener.accept().expect("accept client");
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

        let mut runtime = Runtime::new(RuntimePolicy::default());
        let parser = ParserConfig::default();
        let mut buf = vec![0u8; 65536];
        let mut read_buf = Vec::new();

        loop {
            let n = match stream.read(&mut buf) {
                Ok(0) => break, // client disconnected
                Ok(n) => n,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                Err(e) => panic!("server read error: {e}"),
            };
            read_buf.extend_from_slice(&buf[..n]);

            // Process all complete frames in the buffer
            while let Ok(parsed) = fr_protocol::parse_frame_with_config(&read_buf, &parser) {
                let consumed = parsed.consumed;
                let now_ms = 0;
                let response = runtime.execute_frame(parsed.frame, now_ms);
                stream
                    .write_all(&response.to_bytes())
                    .expect("write response");
                read_buf.drain(..consumed);
            }
        }
    });

    (port, handle)
}

#[test]
fn tcp_ping_pong() {
    let (port, server) = start_single_client_server();

    let mut client = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    client.set_read_timeout(Some(Duration::from_secs(5))).ok();

    // Send PING
    client.write_all(&encode_command(&[b"PING"])).unwrap();
    let resp = read_response(&mut client);
    assert_eq!(resp, RespFrame::SimpleString("PONG".to_string()));

    drop(client);
    server.join().expect("server thread");
}

#[test]
fn tcp_set_get_roundtrip() {
    let (port, server) = start_single_client_server();

    let mut client = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    client.set_read_timeout(Some(Duration::from_secs(5))).ok();

    // SET
    client
        .write_all(&encode_command(&[b"SET", b"tcp_key", b"tcp_value"]))
        .unwrap();
    let set_resp = read_response(&mut client);
    assert_eq!(set_resp, RespFrame::SimpleString("OK".to_string()));

    // GET
    client
        .write_all(&encode_command(&[b"GET", b"tcp_key"]))
        .unwrap();
    let get_resp = read_response(&mut client);
    assert_eq!(get_resp, RespFrame::BulkString(Some(b"tcp_value".to_vec())));

    drop(client);
    server.join().expect("server thread");
}

#[test]
fn tcp_multiple_commands_pipelined() {
    let (port, server) = start_single_client_server();

    let mut client = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    client.set_read_timeout(Some(Duration::from_secs(5))).ok();

    // Pipeline: send SET + GET in one write
    let mut pipeline = Vec::new();
    pipeline.extend_from_slice(&encode_command(&[b"SET", b"pipe_key", b"pipe_val"]));
    pipeline.extend_from_slice(&encode_command(&[b"GET", b"pipe_key"]));
    client.write_all(&pipeline).unwrap();

    let set_resp = read_response(&mut client);
    assert_eq!(set_resp, RespFrame::SimpleString("OK".to_string()));

    let get_resp = read_response(&mut client);
    assert_eq!(get_resp, RespFrame::BulkString(Some(b"pipe_val".to_vec())));

    drop(client);
    server.join().expect("server thread");
}

#[test]
fn tcp_error_response() {
    let (port, server) = start_single_client_server();

    let mut client = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    client.set_read_timeout(Some(Duration::from_secs(5))).ok();

    // Send WRONGTYPE: SET a string, then LPUSH on it
    client
        .write_all(&encode_command(&[b"SET", b"str_key", b"val"]))
        .unwrap();
    let _set = read_response(&mut client);

    client
        .write_all(&encode_command(&[b"LPUSH", b"str_key", b"item"]))
        .unwrap();
    let err = read_response(&mut client);
    assert!(
        matches!(err, RespFrame::Error(ref e) if e.contains("WRONGTYPE")),
        "expected WRONGTYPE error, got: {err:?}"
    );

    drop(client);
    server.join().expect("server thread");
}

#[test]
fn tcp_dbsize_and_flushdb() {
    let (port, server) = start_single_client_server();

    let mut client = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    client.set_read_timeout(Some(Duration::from_secs(5))).ok();

    // DBSIZE on empty store
    client.write_all(&encode_command(&[b"DBSIZE"])).unwrap();
    let dbsize0 = read_response(&mut client);
    assert_eq!(dbsize0, RespFrame::Integer(0));

    // Add keys
    client
        .write_all(&encode_command(&[b"SET", b"k1", b"v1"]))
        .unwrap();
    let _ = read_response(&mut client);
    client
        .write_all(&encode_command(&[b"SET", b"k2", b"v2"]))
        .unwrap();
    let _ = read_response(&mut client);

    // DBSIZE should be 2
    client.write_all(&encode_command(&[b"DBSIZE"])).unwrap();
    let dbsize2 = read_response(&mut client);
    assert_eq!(dbsize2, RespFrame::Integer(2));

    // FLUSHDB
    client.write_all(&encode_command(&[b"FLUSHDB"])).unwrap();
    let flush = read_response(&mut client);
    assert_eq!(flush, RespFrame::SimpleString("OK".to_string()));

    // DBSIZE should be 0
    client.write_all(&encode_command(&[b"DBSIZE"])).unwrap();
    let dbsize_after = read_response(&mut client);
    assert_eq!(dbsize_after, RespFrame::Integer(0));

    drop(client);
    server.join().expect("server thread");
}

#[test]
fn tcp_replicaof_command_connects_to_legacy_primary_and_replicates_writes() {
    let primary_port = reserve_port();
    let replica_port = reserve_port();
    let _primary = spawn_legacy_redis(primary_port);
    let replica = spawn_frankenredis(replica_port, None);

    let mut replica_client = connect_client(replica_port);
    let primary_port_text = primary_port.to_string();
    assert_eq!(
        send_command(
            &mut replica_client,
            &[b"REPLICAOF", b"127.0.0.1", primary_port_text.as_bytes()],
        ),
        RespFrame::SimpleString("OK".to_string())
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_info = None;
    let mut link_up = false;
    while Instant::now() < deadline {
        last_info = fetch_info_replication(replica_port);
        if last_info.as_ref().is_some_and(|info| {
            info.contains("role:slave\r\n")
                && info.contains("master_host:127.0.0.1\r\n")
                && info.contains(&format!("master_port:{primary_port}\r\n"))
                && info.contains("master_link_status:up\r\n")
        }) {
            link_up = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        link_up,
        "replica never reported an active primary link after REPLICAOF; latest INFO: {last_info:?}; replica log: {:?}",
        replica.log_contents()
    );

    let mut primary_client = connect_client(primary_port);
    assert_eq!(
        send_command(
            &mut primary_client,
            &[b"SET", b"external-repl-key", b"replicated"]
        ),
        RespFrame::SimpleString("OK".to_string())
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut replicated = false;
    let mut last_info_after_write = None;
    while Instant::now() < deadline {
        if fetch_string_value(replica_port, b"external-repl-key")
            .is_some_and(|value| value == b"replicated")
        {
            replicated = true;
            break;
        }
        last_info_after_write = fetch_info_replication(replica_port);
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        replicated,
        "replica never observed the primary write; latest INFO: {last_info_after_write:?}; replica log: {:?}",
        replica.log_contents()
    );

    send_shutdown_nosave(replica_port);
    send_shutdown_nosave(primary_port);
}

#[test]
fn tcp_replicaof_cli_flag_bootstraps_replica_link_on_startup() {
    let primary_port = reserve_port();
    let replica_port = reserve_port();
    let _primary = spawn_legacy_redis(primary_port);
    let replica = spawn_frankenredis(replica_port, Some(primary_port));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_info = None;
    let mut link_up = false;
    while Instant::now() < deadline {
        last_info = fetch_info_replication(replica_port);
        if last_info.as_ref().is_some_and(|info| {
            info.contains("role:slave\r\n")
                && info.contains("master_host:127.0.0.1\r\n")
                && info.contains(&format!("master_port:{primary_port}\r\n"))
                && info.contains("master_link_status:up\r\n")
        }) {
            link_up = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        link_up,
        "replica CLI flag never established a primary link; latest INFO: {last_info:?}; replica log: {:?}",
        replica.log_contents()
    );

    let mut primary_client = connect_client(primary_port);
    assert_eq!(
        send_command(
            &mut primary_client,
            &[b"SET", b"cli-repl-key", b"from-primary"]
        ),
        RespFrame::SimpleString("OK".to_string())
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut replicated = false;
    let mut last_info_after_write = None;
    while Instant::now() < deadline {
        if fetch_string_value(replica_port, b"cli-repl-key")
            .is_some_and(|value| value == b"from-primary")
        {
            replicated = true;
            break;
        }
        last_info_after_write = fetch_info_replication(replica_port);
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        replicated,
        "replica started with --replicaof never applied the replicated write; latest INFO: {last_info_after_write:?}; replica log: {:?}",
        replica.log_contents()
    );

    send_shutdown_nosave(replica_port);
    send_shutdown_nosave(primary_port);
}
