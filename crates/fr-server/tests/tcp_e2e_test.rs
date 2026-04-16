//! End-to-end TCP tests that spin up a minimal FrankenRedis server,
//! connect via TCP, send RESP commands, and verify responses.
//! Tests the actual networking stack including RESP framing.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
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

struct BufferedTcpClient {
    stream: TcpStream,
    read_buf: Vec<u8>,
}

impl BufferedTcpClient {
    fn connect(port: u16) -> Self {
        Self {
            stream: connect_client(port),
            read_buf: Vec::new(),
        }
    }

    fn write_all(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).expect("write bytes to server");
    }

    fn read_response(&mut self) -> RespFrame {
        let mut buf = vec![0u8; 65536];
        let deadline = Instant::now() + Duration::from_secs(20);

        loop {
            if let Ok(parsed) = parse_frame(&self.read_buf) {
                let consumed = parsed.consumed;
                self.read_buf.drain(..consumed);
                return parsed.frame;
            }

            match self.stream.read(&mut buf) {
                Ok(0) => panic!("server closed connection unexpectedly"),
                Ok(n) => self.read_buf.extend_from_slice(&buf[..n]),
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

    fn read_responses(&mut self, count: usize) -> Vec<RespFrame> {
        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            frames.push(self.read_response());
        }
        frames
    }

    fn send_command(&mut self, parts: &[&[u8]]) -> RespFrame {
        self.write_all(&encode_command(parts));
        self.read_response()
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
    spawn_legacy_redis_with_requirepass(port, None)
}

fn spawn_legacy_redis_with_requirepass(port: u16, requirepass: Option<&str>) -> ManagedChild {
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
        .arg(dir);
    if let Some(requirepass) = requirepass {
        command.arg("--requirepass").arg(requirepass);
    }
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let child = ManagedChild::spawn(command, None);
    wait_for_port(port);
    child
}

fn spawn_legacy_redis_replica(port: u16, primary_port: u16) -> ManagedChild {
    let dir = unique_temp_dir("frankenredis-legacy-replica");
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
        .arg("--replicaof")
        .arg("127.0.0.1")
        .arg(primary_port.to_string())
        .arg("--dir")
        .arg(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = ManagedChild::spawn(command, None);
    wait_for_port(port);
    child
}

fn spawn_frankenredis(port: u16, primary_port: Option<u16>) -> ManagedChild {
    spawn_frankenredis_opts(port, primary_port, None, None)
}

fn spawn_frankenredis_opts(
    port: u16,
    primary_port: Option<u16>,
    aof_path: Option<&str>,
    rdb_path: Option<&str>,
) -> ManagedChild {
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
    if let Some(path) = aof_path {
        command.arg("--aof").arg(path);
    }
    if let Some(path) = rdb_path {
        command.arg("--rdb").arg(path);
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

fn parse_client_list_fields(line: &str) -> HashMap<String, String> {
    line.split_whitespace()
        .filter_map(|field| {
            let (key, value) = field.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

fn sample_client_list_fields(
    spawn: impl FnOnce(u16) -> ManagedChild,
) -> HashMap<String, String> {
    let port = reserve_port();
    let _server = spawn(port);

    let mut client = connect_client(port);
    assert_eq!(
        send_command(&mut client, &[b"CLIENT", b"SETNAME", b"tracked-client"]),
        RespFrame::SimpleString("OK".to_string())
    );

    thread::sleep(Duration::from_millis(2_100));

    let response = send_command(&mut client, &[b"CLIENT", b"LIST"]);
    let listing = match response {
        RespFrame::BulkString(Some(bytes)) => String::from_utf8(bytes).expect("client list utf8"),
        other => panic!("expected bulk client list, got {other:?}"),
    };
    let tracked_line = listing
        .lines()
        .find(|line| line.split_whitespace().any(|field| field == "name=tracked-client"))
        .unwrap_or_else(|| panic!("tracked client line missing from CLIENT LIST: {listing}"));
    parse_client_list_fields(tracked_line)
}

fn send_shutdown_nosave(port: u16) {
    if let Ok(mut client) = TcpStream::connect(format!("127.0.0.1:{port}")) {
        let _ = client.set_read_timeout(Some(Duration::from_millis(250)));
        let _ = client.write_all(&encode_command(&[b"SHUTDOWN", b"NOSAVE"]));
    }
}

fn assert_positive_integer_response(response: RespFrame) {
    match response {
        RespFrame::Integer(value) => assert!(value > 0, "expected positive integer, got {value}"),
        other => panic!("expected integer response, got {other:?}"),
    }
}

fn run_multi_client_workload(port: u16, pipeline_depth: usize) {
    const CLIENTS: usize = 10;
    const OPS_PER_CLIENT: usize = 100;
    assert!(pipeline_depth > 0, "pipeline depth must be positive");
    let barrier = Arc::new(Barrier::new(CLIENTS + 1));
    let mut handles = Vec::with_capacity(CLIENTS);

    for thread_id in 0..CLIENTS {
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let mut client = BufferedTcpClient::connect(port);
            barrier.wait();

            let mut batch_start = 0usize;
            while batch_start < OPS_PER_CLIENT {
                let batch_end = (batch_start + pipeline_depth).min(OPS_PER_CLIENT);
                let mut key_values = Vec::with_capacity(batch_end - batch_start);
                let mut set_pipeline = Vec::new();

                for op_index in batch_start..batch_end {
                    let key = format!("client_{thread_id}_key_{op_index}").into_bytes();
                    let value = format!("value_{thread_id}_{op_index}").into_bytes();
                    set_pipeline.extend_from_slice(&encode_command(&[
                        b"SET",
                        key.as_slice(),
                        value.as_slice(),
                    ]));
                    key_values.push((key, value));
                }

                client.write_all(&set_pipeline);
                for _ in batch_start..batch_end {
                    assert_eq!(
                        client.read_response(),
                        RespFrame::SimpleString("OK".to_string())
                    );
                }

                let mut get_pipeline = Vec::new();
                for (key, _) in &key_values {
                    get_pipeline.extend_from_slice(&encode_command(&[b"GET", key.as_slice()]));
                }
                client.write_all(&get_pipeline);
                for (_, value) in &key_values {
                    assert_eq!(
                        client.read_response(),
                        RespFrame::BulkString(Some(value.clone()))
                    );
                }

                let mut incr_pipeline = Vec::new();
                for _ in batch_start..batch_end {
                    incr_pipeline.extend_from_slice(&encode_command(&[b"INCR", b"global_counter"]));
                }
                client.write_all(&incr_pipeline);
                for _ in batch_start..batch_end {
                    assert_positive_integer_response(client.read_response());
                }

                let mut lpush_pipeline = Vec::new();
                for (key, _) in &key_values {
                    lpush_pipeline.extend_from_slice(&encode_command(&[
                        b"LPUSH",
                        b"global_list",
                        key.as_slice(),
                    ]));
                }
                client.write_all(&lpush_pipeline);
                for _ in batch_start..batch_end {
                    assert_positive_integer_response(client.read_response());
                }

                batch_start = batch_end;
            }
        }));
    }

    barrier.wait();
    for handle in handles {
        handle.join().expect("client workload thread");
    }

    let mut verifier = BufferedTcpClient::connect(port);
    assert_eq!(
        verifier.send_command(&[b"GET", b"global_counter"]),
        RespFrame::BulkString(Some((CLIENTS * OPS_PER_CLIENT).to_string().into_bytes()))
    );
    assert_eq!(
        verifier.send_command(&[b"LLEN", b"global_list"]),
        RespFrame::Integer((CLIENTS * OPS_PER_CLIENT) as i64)
    );
    assert_eq!(
        verifier.send_command(&[b"DBSIZE"]),
        RespFrame::Integer((CLIENTS * OPS_PER_CLIENT + 2) as i64)
    );

    for thread_id in 0..CLIENTS {
        for op_index in 0..OPS_PER_CLIENT {
            let key = format!("client_{thread_id}_key_{op_index}");
            let expected = format!("value_{thread_id}_{op_index}");
            assert_eq!(
                verifier.send_command(&[b"GET", key.as_bytes()]),
                RespFrame::BulkString(Some(expected.into_bytes()))
            );
        }
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
                Err(ref e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
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

    let mut client = BufferedTcpClient::connect(port);

    // Pipeline: send SET + GET in one write
    let mut pipeline = Vec::new();
    pipeline.extend_from_slice(&encode_command(&[b"SET", b"pipe_key", b"pipe_val"]));
    pipeline.extend_from_slice(&encode_command(&[b"GET", b"pipe_key"]));
    client.write_all(&pipeline);

    let responses = client.read_responses(2);
    assert_eq!(responses[0], RespFrame::SimpleString("OK".to_string()));
    assert_eq!(
        responses[1],
        RespFrame::BulkString(Some(b"pipe_val".to_vec()))
    );
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
fn tcp_replicaof_command_uses_masterauth_for_protected_legacy_primary() {
    let primary_port = reserve_port();
    let replica_port = reserve_port();
    let _primary = spawn_legacy_redis_with_requirepass(primary_port, Some("secret"));
    let replica = spawn_frankenredis(replica_port, None);

    let mut replica_client = connect_client(replica_port);
    assert_eq!(
        send_command(
            &mut replica_client,
            &[b"CONFIG", b"SET", b"masterauth", b"secret"],
        ),
        RespFrame::SimpleString("OK".to_string())
    );
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
        "replica never authenticated to the protected primary; latest INFO: {last_info:?}; replica log: {:?}",
        replica.log_contents()
    );

    let mut primary_client = connect_client(primary_port);
    assert_eq!(
        send_command(&mut primary_client, &[b"AUTH", b"secret"]),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        send_command(
            &mut primary_client,
            &[b"SET", b"protected-repl-key", b"replicated"]
        ),
        RespFrame::SimpleString("OK".to_string())
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut replicated = false;
    let mut last_info_after_write = None;
    while Instant::now() < deadline {
        if fetch_string_value(replica_port, b"protected-repl-key")
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
        "replica never observed the protected primary write; latest INFO: {last_info_after_write:?}; replica log: {:?}",
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

#[test]
fn tcp_frankenredis_min_replicas_gate_blocks_then_admits_writes() {
    exercise_min_replicas_write_gate(
        |port| spawn_frankenredis(port, None),
        |port, primary_port| spawn_frankenredis(port, Some(primary_port)),
    );
}

#[test]
fn tcp_min_replicas_gate_matches_legacy_redis_reference() {
    exercise_min_replicas_write_gate(spawn_legacy_redis, spawn_legacy_redis_replica);
}

#[test]
fn tcp_client_list_age_idle_matches_legacy_redis_reference() {
    let franken_fields = sample_client_list_fields(|port| spawn_frankenredis(port, None));
    let legacy_fields = sample_client_list_fields(spawn_legacy_redis);

    for key in ["age", "idle"] {
        assert!(
            franken_fields.contains_key(key),
            "frankenredis missing {key} field: {franken_fields:?}"
        );
        assert!(
            legacy_fields.contains_key(key),
            "legacy redis missing {key} field: {legacy_fields:?}"
        );
    }

    let franken_age = franken_fields["age"].parse::<u64>().expect("franken age");
    let legacy_age = legacy_fields["age"].parse::<u64>().expect("legacy age");
    let franken_idle = franken_fields["idle"].parse::<u64>().expect("franken idle");
    let legacy_idle = legacy_fields["idle"].parse::<u64>().expect("legacy idle");

    assert!(
        franken_age.abs_diff(legacy_age) <= 1,
        "age mismatch: frankenredis={franken_age}, legacy={legacy_age}, franken_fields={franken_fields:?}, legacy_fields={legacy_fields:?}"
    );
    assert!(
        franken_idle.abs_diff(legacy_idle) <= 1,
        "idle mismatch: frankenredis={franken_idle}, legacy={legacy_idle}, franken_fields={franken_fields:?}, legacy_fields={legacy_fields:?}"
    );
    assert!(
        franken_age >= franken_idle,
        "target age should be >= idle: {franken_fields:?}"
    );
}

#[test]
fn tcp_multi_client_concurrent_access_roundtrip() {
    let port = reserve_port();
    let _server = spawn_frankenredis(port, None);
    run_multi_client_workload(port, 1);
    send_shutdown_nosave(port);
}

#[test]
fn tcp_multi_client_concurrent_access_roundtrip_with_pipeline_depth_ten() {
    let port = reserve_port();
    let _server = spawn_frankenredis(port, None);
    run_multi_client_workload(port, 10);
    send_shutdown_nosave(port);
}

// ---------- Persistence restart tests ----------

#[test]
fn tcp_aof_restart_preserves_all_data() {
    let tmp = unique_temp_dir("frankenredis-aof-restart");
    let aof_file = tmp.join("test.aof");
    let aof_path = aof_file.to_str().unwrap();
    let port1 = reserve_port();

    // Phase 1: Start server with AOF, write data, then kill.
    {
        let _server = spawn_frankenredis_opts(port1, None, Some(aof_path), None);
        let mut client = connect_client(port1);

        for i in 0..20 {
            let key = format!("str-key-{i}");
            let val = format!("value-{i}");
            let resp = send_command(&mut client, &[b"SET", key.as_bytes(), val.as_bytes()]);
            assert_eq!(resp, RespFrame::SimpleString("OK".to_string()));
        }
        for i in 0..5 {
            let elem = format!("elem-{i}");
            send_command(&mut client, &[b"RPUSH", b"mylist", elem.as_bytes()]);
        }
        for i in 0..5 {
            let field = format!("field-{i}");
            let val = format!("hval-{i}");
            send_command(
                &mut client,
                &[b"HSET", b"myhash", field.as_bytes(), val.as_bytes()],
            );
        }
        for i in 0..5 {
            let member = format!("member-{i}");
            send_command(&mut client, &[b"SADD", b"myset", member.as_bytes()]);
        }
        for i in 0..5 {
            let score = format!("{}", (i + 1) * 10);
            let member = format!("zmem-{i}");
            send_command(
                &mut client,
                &[b"ZADD", b"myzset", score.as_bytes(), member.as_bytes()],
            );
        }

        let dbsize = send_command(&mut client, &[b"DBSIZE"]);
        assert_eq!(dbsize, RespFrame::Integer(24));

        // Flush AOF to disk before killing the server.
        let rewrite = send_command(&mut client, &[b"BGREWRITEAOF"]);
        assert!(
            matches!(rewrite, RespFrame::SimpleString(_)),
            "BGREWRITEAOF failed: {rewrite:?}"
        );

        drop(client);
        // _server dropped here — process killed, port freed.
    }

    assert!(aof_file.exists(), "AOF file was not created");
    assert!(aof_file.metadata().unwrap().len() > 0, "AOF file is empty");

    // Phase 2: Restart on new port with same AOF, verify all data survived.
    let port2 = reserve_port();
    {
        let _server = spawn_frankenredis_opts(port2, None, Some(aof_path), None);
        let mut client = connect_client(port2);

        let dbsize = send_command(&mut client, &[b"DBSIZE"]);
        assert_eq!(
            dbsize,
            RespFrame::Integer(24),
            "DBSIZE mismatch after AOF restart"
        );
        for i in 0..20 {
            let key = format!("str-key-{i}");
            let expected = format!("value-{i}");
            let resp = send_command(&mut client, &[b"GET", key.as_bytes()]);
            assert_eq!(
                resp,
                RespFrame::BulkString(Some(expected.into_bytes())),
                "string key {key} mismatch after AOF restart"
            );
        }
        assert_eq!(
            send_command(&mut client, &[b"LLEN", b"mylist"]),
            RespFrame::Integer(5),
            "list length mismatch"
        );
        assert_eq!(
            send_command(&mut client, &[b"HLEN", b"myhash"]),
            RespFrame::Integer(5),
            "hash length mismatch"
        );
        assert_eq!(
            send_command(&mut client, &[b"SCARD", b"myset"]),
            RespFrame::Integer(5),
            "set cardinality mismatch"
        );
        assert_eq!(
            send_command(&mut client, &[b"ZCARD", b"myzset"]),
            RespFrame::Integer(5),
            "zset cardinality mismatch"
        );
        assert_eq!(
            send_command(&mut client, &[b"ZSCORE", b"myzset", b"zmem-2"]),
            RespFrame::BulkString(Some(b"30".to_vec())),
            "zset score mismatch"
        );

        send_shutdown_nosave(port2);
    }
}

#[test]
fn tcp_rdb_restart_preserves_all_data() {
    let tmp = unique_temp_dir("frankenredis-rdb-restart");
    let rdb_file = tmp.join("test.rdb");
    let rdb_path = rdb_file.to_str().unwrap();
    let port1 = reserve_port();

    // Phase 1: Start server with RDB, write data, SAVE, then kill.
    {
        let _server = spawn_frankenredis_opts(port1, None, None, Some(rdb_path));
        let mut client = connect_client(port1);

        for i in 0..20 {
            let key = format!("rdb-key-{i}");
            let val = format!("rdb-val-{i}");
            send_command(&mut client, &[b"SET", key.as_bytes(), val.as_bytes()]);
        }
        for i in 0..5 {
            let elem = format!("rdb-elem-{i}");
            send_command(&mut client, &[b"RPUSH", b"rdb-list", elem.as_bytes()]);
        }
        for i in 0..5 {
            let field = format!("f{i}");
            let val = format!("v{i}");
            send_command(
                &mut client,
                &[b"HSET", b"rdb-hash", field.as_bytes(), val.as_bytes()],
            );
        }
        for i in 0..5 {
            let member = format!("s{i}");
            send_command(&mut client, &[b"SADD", b"rdb-set", member.as_bytes()]);
        }
        for i in 0..5 {
            let score = format!("{}", (i + 1) * 100);
            let member = format!("z{i}");
            send_command(
                &mut client,
                &[b"ZADD", b"rdb-zset", score.as_bytes(), member.as_bytes()],
            );
        }

        // Force RDB snapshot before kill.
        let save_resp = send_command(&mut client, &[b"SAVE"]);
        assert_eq!(save_resp, RespFrame::SimpleString("OK".to_string()));

        drop(client);
        // _server dropped here — process killed, port freed.
    }

    assert!(rdb_file.exists(), "RDB file was not created");
    assert!(rdb_file.metadata().unwrap().len() > 0, "RDB file is empty");

    // Phase 2: Restart on new port with same RDB, verify all data survived.
    let port2 = reserve_port();
    {
        let _server = spawn_frankenredis_opts(port2, None, None, Some(rdb_path));
        let mut client = connect_client(port2);

        let dbsize = send_command(&mut client, &[b"DBSIZE"]);
        assert_eq!(
            dbsize,
            RespFrame::Integer(24),
            "DBSIZE mismatch after RDB restart"
        );
        for i in 0..20 {
            let key = format!("rdb-key-{i}");
            let expected = format!("rdb-val-{i}");
            let resp = send_command(&mut client, &[b"GET", key.as_bytes()]);
            assert_eq!(
                resp,
                RespFrame::BulkString(Some(expected.into_bytes())),
                "string key {key} mismatch after RDB restart"
            );
        }
        assert_eq!(
            send_command(&mut client, &[b"LLEN", b"rdb-list"]),
            RespFrame::Integer(5),
            "list length mismatch"
        );
        assert_eq!(
            send_command(&mut client, &[b"HLEN", b"rdb-hash"]),
            RespFrame::Integer(5),
            "hash length mismatch"
        );
        assert_eq!(
            send_command(&mut client, &[b"SCARD", b"rdb-set"]),
            RespFrame::Integer(5),
            "set cardinality mismatch"
        );
        assert_eq!(
            send_command(&mut client, &[b"ZCARD", b"rdb-zset"]),
            RespFrame::Integer(5),
            "zset cardinality mismatch"
        );
        assert_eq!(
            send_command(&mut client, &[b"ZSCORE", b"rdb-zset", b"z3"]),
            RespFrame::BulkString(Some(b"400".to_vec())),
            "zset score mismatch"
        );

        send_shutdown_nosave(port2);
    }
}

// ---------- Pub/Sub cross-client tests ----------

fn pubsub_subscribe_frame(channel: &str, count: i64) -> RespFrame {
    RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"subscribe".to_vec())),
        RespFrame::BulkString(Some(channel.as_bytes().to_vec())),
        RespFrame::Integer(count),
    ]))
}

fn pubsub_message_frame(channel: &str, data: &str) -> RespFrame {
    RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"message".to_vec())),
        RespFrame::BulkString(Some(channel.as_bytes().to_vec())),
        RespFrame::BulkString(Some(data.as_bytes().to_vec())),
    ]))
}

#[test]
fn tcp_pubsub_basic_cross_client_delivery() {
    let port = reserve_port();
    let _server = spawn_frankenredis(port, None);

    let mut sub_client = BufferedTcpClient::connect(port);
    sub_client
        .stream
        .write_all(&encode_command(&[b"SUBSCRIBE", b"channel1"]))
        .unwrap();
    let confirms = sub_client.read_responses(1);
    assert_eq!(confirms[0], pubsub_subscribe_frame("channel1", 1));

    let mut pub_client = connect_client(port);
    let pub_resp = send_command(&mut pub_client, &[b"PUBLISH", b"channel1", b"hello"]);
    assert_eq!(pub_resp, RespFrame::Integer(1), "expected 1 subscriber");

    let msgs = sub_client.read_responses(1);
    assert_eq!(msgs[0], pubsub_message_frame("channel1", "hello"));

    sub_client
        .stream
        .write_all(&encode_command(&[b"UNSUBSCRIBE", b"channel1"]))
        .unwrap();
    let unsub = sub_client.read_responses(1);
    assert_eq!(
        unsub[0],
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"unsubscribe".to_vec())),
            RespFrame::BulkString(Some(b"channel1".to_vec())),
            RespFrame::Integer(0),
        ]))
    );

    let pub_resp2 = send_command(&mut pub_client, &[b"PUBLISH", b"channel1", b"gone"]);
    assert_eq!(
        pub_resp2,
        RespFrame::Integer(0),
        "expected 0 subscribers after unsubscribe"
    );

    send_shutdown_nosave(port);
}

#[test]
fn tcp_pubsub_multiple_subscribers() {
    let port = reserve_port();
    let _server = spawn_frankenredis(port, None);

    let mut sub_a = BufferedTcpClient::connect(port);
    sub_a
        .stream
        .write_all(&encode_command(&[b"SUBSCRIBE", b"chat"]))
        .unwrap();
    let _ = sub_a.read_responses(1);

    let mut sub_b = BufferedTcpClient::connect(port);
    sub_b
        .stream
        .write_all(&encode_command(&[b"SUBSCRIBE", b"chat"]))
        .unwrap();
    let _ = sub_b.read_responses(1);

    let mut pub_client = connect_client(port);
    let pub_resp = send_command(&mut pub_client, &[b"PUBLISH", b"chat", b"broadcast"]);
    assert_eq!(pub_resp, RespFrame::Integer(2), "expected 2 subscribers");

    let msg_a = sub_a.read_responses(1);
    assert_eq!(msg_a[0], pubsub_message_frame("chat", "broadcast"));
    let msg_b = sub_b.read_responses(1);
    assert_eq!(msg_b[0], pubsub_message_frame("chat", "broadcast"));

    send_shutdown_nosave(port);
}

#[test]
fn tcp_pubsub_pattern_subscribe() {
    let port = reserve_port();
    let _server = spawn_frankenredis(port, None);

    let mut sub_client = BufferedTcpClient::connect(port);
    sub_client
        .stream
        .write_all(&encode_command(&[b"PSUBSCRIBE", b"news.*"]))
        .unwrap();
    let confirms = sub_client.read_responses(1);
    assert_eq!(
        confirms[0],
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"psubscribe".to_vec())),
            RespFrame::BulkString(Some(b"news.*".to_vec())),
            RespFrame::Integer(1),
        ]))
    );

    let mut pub_client = connect_client(port);
    let pub_resp = send_command(&mut pub_client, &[b"PUBLISH", b"news.sports", b"goal!"]);
    assert_eq!(pub_resp, RespFrame::Integer(1));

    let msgs = sub_client.read_responses(1);
    assert_eq!(
        msgs[0],
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"pmessage".to_vec())),
            RespFrame::BulkString(Some(b"news.*".to_vec())),
            RespFrame::BulkString(Some(b"news.sports".to_vec())),
            RespFrame::BulkString(Some(b"goal!".to_vec())),
        ]))
    );

    let pub_resp2 = send_command(&mut pub_client, &[b"PUBLISH", b"weather.rain", b"wet"]);
    assert_eq!(
        pub_resp2,
        RespFrame::Integer(0),
        "non-matching channel should have 0 subscribers"
    );

    send_shutdown_nosave(port);
}

// ---------- Transaction isolation tests ----------

#[test]
fn tcp_watch_exec_aborts_on_concurrent_modification() {
    let port = reserve_port();
    let _server = spawn_frankenredis(port, None);

    // Initialize the key.
    let mut setup = connect_client(port);
    send_command(&mut setup, &[b"SET", b"watched-key", b"0"]);
    drop(setup);

    // Client A: WATCH, read, then MULTI/EXEC — but Client B modifies in between.
    let mut client_a = connect_client(port);
    let mut client_b = connect_client(port);

    // A watches the key.
    let watch_resp = send_command(&mut client_a, &[b"WATCH", b"watched-key"]);
    assert_eq!(watch_resp, RespFrame::SimpleString("OK".to_string()));

    // A reads current value.
    let val = send_command(&mut client_a, &[b"GET", b"watched-key"]);
    assert_eq!(val, RespFrame::BulkString(Some(b"0".to_vec())));

    // B modifies the key while A has it watched.
    let set_resp = send_command(&mut client_b, &[b"SET", b"watched-key", b"1"]);
    assert_eq!(set_resp, RespFrame::SimpleString("OK".to_string()));

    // A starts a transaction and tries to set the key.
    let multi_resp = send_command(&mut client_a, &[b"MULTI"]);
    assert_eq!(multi_resp, RespFrame::SimpleString("OK".to_string()));

    let queued = send_command(&mut client_a, &[b"SET", b"watched-key", b"2"]);
    assert_eq!(queued, RespFrame::SimpleString("QUEUED".to_string()));

    // EXEC should return null array (transaction aborted because watched key was modified).
    let exec_resp = send_command(&mut client_a, &[b"EXEC"]);
    assert_eq!(
        exec_resp,
        RespFrame::Array(None),
        "EXEC should return nil when WATCH detects modification"
    );

    // The value should be "1" (Client B's write), not "2" (Client A's aborted write).
    let final_val = send_command(&mut client_b, &[b"GET", b"watched-key"]);
    assert_eq!(
        final_val,
        RespFrame::BulkString(Some(b"1".to_vec())),
        "value should be Client B's write, not the aborted transaction"
    );

    send_shutdown_nosave(port);
}

#[test]
fn tcp_watch_exec_succeeds_without_concurrent_modification() {
    let port = reserve_port();
    let _server = spawn_frankenredis(port, None);

    let mut client = connect_client(port);
    send_command(&mut client, &[b"SET", b"counter", b"10"]);

    // WATCH + MULTI/EXEC with no interference should succeed.
    send_command(&mut client, &[b"WATCH", b"counter"]);
    send_command(&mut client, &[b"MULTI"]);
    send_command(&mut client, &[b"INCR", b"counter"]);
    let exec_resp = send_command(&mut client, &[b"EXEC"]);

    // EXEC should return array with INCR result.
    assert_eq!(
        exec_resp,
        RespFrame::Array(Some(vec![RespFrame::Integer(11)])),
        "EXEC should succeed when WATCH key is unmodified"
    );

    let val = send_command(&mut client, &[b"GET", b"counter"]);
    assert_eq!(val, RespFrame::BulkString(Some(b"11".to_vec())));

    send_shutdown_nosave(port);
}

// ---------- Cross-process replication tests ----------

/// Wait for a replica to report master_link_status:up in INFO replication.
fn wait_for_replica_sync(replica_port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(info) = fetch_info_replication(replica_port)
            && info.contains("master_link_status:up")
        {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "replica on port {replica_port} did not sync within {timeout:?}; last INFO: {:?}",
        fetch_info_replication(replica_port)
    );
}

fn exercise_min_replicas_write_gate<SP, SR>(spawn_primary: SP, spawn_replica: SR)
where
    SP: FnOnce(u16) -> ManagedChild,
    SR: Fn(u16, u16) -> ManagedChild,
{
    let primary_port = reserve_port();
    let replica_port = reserve_port();

    let _primary = spawn_primary(primary_port);
    let mut primary_client = connect_client(primary_port);

    assert_eq!(
        send_command(
            &mut primary_client,
            &[b"CONFIG", b"SET", b"min-replicas-to-write", b"1"],
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        send_command(&mut primary_client, &[b"SET", b"gate-key", b"blocked"]),
        RespFrame::Error("NOREPLICAS Not enough good replicas to write.".to_string())
    );

    let _replica = spawn_replica(replica_port, primary_port);
    wait_for_replica_sync(replica_port, Duration::from_secs(10));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut admitted = false;
    let mut last_primary_info = None;
    while Instant::now() < deadline {
        let reply = send_command(&mut primary_client, &[b"SET", b"gate-key", b"allowed"]);
        if reply == RespFrame::SimpleString("OK".to_string()) {
            admitted = true;
            break;
        }
        assert_eq!(
            reply,
            RespFrame::Error("NOREPLICAS Not enough good replicas to write.".to_string())
        );
        last_primary_info = fetch_info_replication(primary_port);
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        admitted,
        "primary on port {primary_port} never admitted writes after a healthy replica link; latest INFO: {last_primary_info:?}",
    );

    wait_until(
        Duration::from_secs(5),
        || fetch_string_value(replica_port, b"gate-key").is_some_and(|value| value == b"allowed"),
        &format!("replica on port {replica_port} never observed gated write"),
    );

    send_shutdown_nosave(replica_port);
    send_shutdown_nosave(primary_port);
}

#[test]
fn tcp_frankenredis_to_frankenredis_fullresync_and_live_streaming() {
    let primary_port = reserve_port();
    let replica_port = reserve_port();

    // Start primary.
    let _primary = spawn_frankenredis(primary_port, None);

    // Write initial data to primary before replica connects.
    let mut client = connect_client(primary_port);
    for i in 0..20 {
        let key = format!("initial-{i}");
        let val = format!("val-{i}");
        send_command(&mut client, &[b"SET", key.as_bytes(), val.as_bytes()]);
    }
    for i in 0..5 {
        let elem = format!("item-{i}");
        send_command(&mut client, &[b"RPUSH", b"repl-list", elem.as_bytes()]);
    }

    // Start replica pointing to primary.
    let _replica = spawn_frankenredis(replica_port, Some(primary_port));

    // Wait for replica to complete full resync.
    wait_for_replica_sync(replica_port, Duration::from_secs(10));

    // Verify initial data replicated via FULLRESYNC.
    let mut replica_client = connect_client(replica_port);
    for i in 0..20 {
        let key = format!("initial-{i}");
        let expected = format!("val-{i}");
        let val = send_command(&mut replica_client, &[b"GET", key.as_bytes()]);
        assert_eq!(
            val,
            RespFrame::BulkString(Some(expected.into_bytes())),
            "key {key} missing after FULLRESYNC"
        );
    }
    let llen = send_command(&mut replica_client, &[b"LLEN", b"repl-list"]);
    assert_eq!(llen, RespFrame::Integer(5), "list not replicated");

    // Phase 2: Live streaming — write more data to primary after replica is synced.
    for i in 0..10 {
        let key = format!("live-{i}");
        let val = format!("streamed-{i}");
        send_command(&mut client, &[b"SET", key.as_bytes(), val.as_bytes()]);
    }

    // Wait for live commands to propagate.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut live_replicated = false;
    while Instant::now() < deadline {
        if let Some(bytes) = fetch_string_value(replica_port, b"live-9")
            && bytes == b"streamed-9"
        {
            live_replicated = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        live_replicated,
        "live-streamed keys did not propagate to replica"
    );

    // Verify all live keys on replica.
    for i in 0..10 {
        let key = format!("live-{i}");
        let expected = format!("streamed-{i}");
        let val = send_command(&mut replica_client, &[b"GET", key.as_bytes()]);
        assert_eq!(
            val,
            RespFrame::BulkString(Some(expected.into_bytes())),
            "live key {key} not replicated"
        );
    }

    // Verify INCR propagation.
    send_command(&mut client, &[b"SET", b"counter", b"0"]);
    for _ in 0..50 {
        send_command(&mut client, &[b"INCR", b"counter"]);
    }

    // Wait for counter to reach 50 on replica.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut counter_replicated = false;
    while Instant::now() < deadline {
        if let Some(bytes) = fetch_string_value(replica_port, b"counter")
            && bytes == b"50"
        {
            counter_replicated = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        counter_replicated,
        "INCR counter did not propagate to replica; got {:?}",
        fetch_string_value(replica_port, b"counter")
    );

    send_shutdown_nosave(replica_port);
    send_shutdown_nosave(primary_port);
}

/// Test replica-of-replica chain: Primary → Replica1 → Replica2.
/// Verifies data propagates through the entire chain, including live streaming.
fn exercise_replica_of_replica_chain<SP, SR>(spawn_primary: SP, spawn_replica: SR)
where
    SP: FnOnce(u16) -> ManagedChild,
    SR: Fn(u16, u16) -> ManagedChild,
{
    let primary_port = reserve_port();
    let replica1_port = reserve_port();
    let replica2_port = reserve_port();

    // Start primary.
    let _primary = spawn_primary(primary_port);

    // Write initial data to primary before any replicas connect.
    let mut client = connect_client(primary_port);
    for i in 0..10 {
        let key = format!("chain-initial-{i}");
        let val = format!("initial-val-{i}");
        send_command(&mut client, &[b"SET", key.as_bytes(), val.as_bytes()]);
    }

    // Start replica1 pointing to primary.
    let _replica1 = spawn_replica(replica1_port, primary_port);
    wait_for_replica_sync(replica1_port, Duration::from_secs(10));

    // Verify replica1 has initial data.
    let mut replica1_client = connect_client(replica1_port);
    for i in 0..10 {
        let key = format!("chain-initial-{i}");
        let expected = format!("initial-val-{i}");
        let val = send_command(&mut replica1_client, &[b"GET", key.as_bytes()]);
        assert_eq!(
            val,
            RespFrame::BulkString(Some(expected.into_bytes())),
            "replica1 missing key {key}"
        );
    }

    // Start replica2 pointing to replica1 (chained replication).
    let _replica2 = spawn_replica(replica2_port, replica1_port);
    wait_for_replica_sync(replica2_port, Duration::from_secs(10));

    // Verify each hop reports the expected replication topology.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_primary_info = None;
    let mut last_replica1_info = None;
    let mut last_replica2_info = None;
    let mut topology_ready = false;
    while Instant::now() < deadline {
        last_primary_info = fetch_info_replication(primary_port);
        last_replica1_info = fetch_info_replication(replica1_port);
        last_replica2_info = fetch_info_replication(replica2_port);
        if last_primary_info.as_ref().is_some_and(|info| {
            info.contains("role:master\r\n")
                && info.contains("connected_slaves:1\r\n")
                && info.contains(&format!(
                    "slave0:ip=127.0.0.1,port={replica1_port},state=online,"
                ))
        }) && last_replica1_info.as_ref().is_some_and(|info| {
            info.contains("role:slave\r\n")
                && info.contains("master_host:127.0.0.1\r\n")
                && info.contains(&format!("master_port:{primary_port}\r\n"))
                && info.contains("master_link_status:up\r\n")
                && info.contains("connected_slaves:1\r\n")
                && info.contains(&format!(
                    "slave0:ip=127.0.0.1,port={replica2_port},state=online,"
                ))
        }) && last_replica2_info.as_ref().is_some_and(|info| {
            info.contains("role:slave\r\n")
                && info.contains("master_host:127.0.0.1\r\n")
                && info.contains(&format!("master_port:{replica1_port}\r\n"))
                && info.contains("master_link_status:up\r\n")
                && info.contains("connected_slaves:0\r\n")
        }) {
            topology_ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        topology_ready,
        "replication chain topology info never stabilized; primary={last_primary_info:?}; replica1={last_replica1_info:?}; replica2={last_replica2_info:?}"
    );

    // Verify replica2 has initial data through the chain.
    let mut replica2_client = connect_client(replica2_port);
    for i in 0..10 {
        let key = format!("chain-initial-{i}");
        let expected = format!("initial-val-{i}");
        let val = send_command(&mut replica2_client, &[b"GET", key.as_bytes()]);
        assert_eq!(
            val,
            RespFrame::BulkString(Some(expected.into_bytes())),
            "replica2 (chained) missing key {key}"
        );
    }

    // Verify ROLE on each node.
    let primary_role = send_command(&mut client, &[b"ROLE"]);
    if let RespFrame::Array(Some(items)) = &primary_role
        && let Some(RespFrame::BulkString(Some(role))) = items.first()
    {
        assert_eq!(role.as_slice(), b"master", "primary should report master");
    }

    let replica1_role = send_command(&mut replica1_client, &[b"ROLE"]);
    if let RespFrame::Array(Some(items)) = &replica1_role
        && let Some(RespFrame::BulkString(Some(role))) = items.first()
    {
        assert_eq!(role.as_slice(), b"slave", "replica1 should report slave");
    }

    let replica2_role = send_command(&mut replica2_client, &[b"ROLE"]);
    if let RespFrame::Array(Some(items)) = &replica2_role
        && let Some(RespFrame::BulkString(Some(role))) = items.first()
    {
        assert_eq!(role.as_slice(), b"slave", "replica2 should report slave");
    }

    // Live streaming test: write more data to primary and verify propagation through chain.
    for i in 0..5 {
        let key = format!("chain-live-{i}");
        let val = format!("live-val-{i}");
        send_command(&mut client, &[b"SET", key.as_bytes(), val.as_bytes()]);
    }

    // Wait for live data to propagate to replica2 through the chain.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut chain_propagated = false;
    while Instant::now() < deadline {
        if let Some(bytes) = fetch_string_value(replica2_port, b"chain-live-4")
            && bytes == b"live-val-4"
        {
            chain_propagated = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        chain_propagated,
        "live data did not propagate through replica chain"
    );

    // Verify all live keys on replica2.
    for i in 0..5 {
        let key = format!("chain-live-{i}");
        let expected = format!("live-val-{i}");
        let val = send_command(&mut replica2_client, &[b"GET", key.as_bytes()]);
        assert_eq!(
            val,
            RespFrame::BulkString(Some(expected.into_bytes())),
            "chain-live key {key} not propagated to replica2"
        );
    }

    // INCR propagation test through chain.
    send_command(&mut client, &[b"SET", b"chain-counter", b"0"]);
    for _ in 0..25 {
        send_command(&mut client, &[b"INCR", b"chain-counter"]);
    }

    // Wait for counter to reach 25 on replica2.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut counter_propagated = false;
    while Instant::now() < deadline {
        if let Some(bytes) = fetch_string_value(replica2_port, b"chain-counter")
            && bytes == b"25"
        {
            counter_propagated = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        counter_propagated,
        "INCR counter did not propagate through chain; got {:?}",
        fetch_string_value(replica2_port, b"chain-counter")
    );

    send_shutdown_nosave(replica2_port);
    send_shutdown_nosave(replica1_port);
    send_shutdown_nosave(primary_port);
}

#[test]
fn tcp_replica_of_replica_chain_replication() {
    exercise_replica_of_replica_chain(
        |port| spawn_frankenredis(port, None),
        |port, primary_port| spawn_frankenredis(port, Some(primary_port)),
    );
}

#[test]
fn tcp_replica_of_replica_chain_matches_legacy_redis_reference() {
    exercise_replica_of_replica_chain(spawn_legacy_redis, spawn_legacy_redis_replica);
}

#[test]
fn tcp_failover_command_promotes_target_replica_and_leaves_chain_in_place() {
    let original_master_port = reserve_port();
    let target_replica_port = reserve_port();
    let chained_replica_port = reserve_port();

    let _original_master = spawn_frankenredis(original_master_port, None);
    let _target_replica = spawn_frankenredis(target_replica_port, Some(original_master_port));
    let _chained_replica = spawn_frankenredis(chained_replica_port, Some(original_master_port));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut link_up = false;
    while Instant::now() < deadline {
        let info1 = fetch_info_replication(target_replica_port);
        let info2 = fetch_info_replication(chained_replica_port);
        if info1
            .as_ref()
            .is_some_and(|info| info.contains("master_link_status:up\r\n"))
            && info2
                .as_ref()
                .is_some_and(|info| info.contains("master_link_status:up\r\n"))
        {
            link_up = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(link_up, "replicas never synced to original master");

    let mut original_master_client = connect_client(original_master_port);
    assert_eq!(
        send_command(&mut original_master_client, &[b"SET", b"pre-failover", b"value"]),
        RespFrame::SimpleString("OK".to_string())
    );

    let target_replica_port_text = target_replica_port.to_string();
    assert_eq!(
        send_command(
            &mut original_master_client,
            &[
                b"FAILOVER",
                b"TO",
                b"127.0.0.1",
                target_replica_port_text.as_bytes(),
                b"FORCE",
                b"TIMEOUT",
                b"5000",
            ],
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    drop(original_master_client);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_original_info = None;
    let mut last_target_info = None;
    let mut last_chained_info = None;
    let mut topology_ready = false;
    while Instant::now() < deadline {
        last_original_info = fetch_info_replication(original_master_port);
        last_target_info = fetch_info_replication(target_replica_port);
        last_chained_info = fetch_info_replication(chained_replica_port);

        if last_original_info.as_ref().is_some_and(|info| {
            info.contains("role:slave\r\n")
                && info.contains("master_host:127.0.0.1\r\n")
                && info.contains(&format!("master_port:{target_replica_port}\r\n"))
                && info.contains("master_link_status:up\r\n")
                && info.contains("connected_slaves:1\r\n")
                && info.contains(&format!(
                    "slave0:ip=127.0.0.1,port={chained_replica_port},state=online,"
                ))
        }) && last_target_info.as_ref().is_some_and(|info| {
            info.contains("role:master\r\n")
                && info.contains("connected_slaves:1\r\n")
                && info.contains(&format!(
                    "slave0:ip=127.0.0.1,port={original_master_port},state=online,"
                ))
        }) && last_chained_info.as_ref().is_some_and(|info| {
            info.contains("role:slave\r\n")
                && info.contains("master_host:127.0.0.1\r\n")
                && info.contains(&format!("master_port:{original_master_port}\r\n"))
                && info.contains("master_link_status:up\r\n")
                && info.contains("connected_slaves:0\r\n")
        }) {
            topology_ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        topology_ready,
        "FAILOVER topology never stabilized; original={last_original_info:?}; target={last_target_info:?}; chained={last_chained_info:?}"
    );

    let mut target_master_client = connect_client(target_replica_port);
    assert_eq!(
        send_command(&mut target_master_client, &[b"SET", b"post-failover", b"value"]),
        RespFrame::SimpleString("OK".to_string())
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut propagated = false;
    while Instant::now() < deadline {
        if fetch_string_value(chained_replica_port, b"post-failover")
            .is_some_and(|value| value == b"value")
            && fetch_string_value(original_master_port, b"post-failover")
                .is_some_and(|value| value == b"value")
        {
            propagated = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        propagated,
        "post-failover write never reached chained topology; original={:?}; chained={:?}",
        fetch_string_value(original_master_port, b"post-failover"),
        fetch_string_value(chained_replica_port, b"post-failover")
    );

    send_shutdown_nosave(original_master_port);
    send_shutdown_nosave(target_replica_port);
    send_shutdown_nosave(chained_replica_port);
}

#[test]
fn tcp_sentinel_failover_integration() {
    // Proves the failover sequence orchestrated by Sentinel works correctly on FrankenRedis nodes
    let original_master_port = reserve_port();
    let replica1_port = reserve_port();
    let replica2_port = reserve_port();

    let _original_master = spawn_frankenredis(original_master_port, None);
    let _replica1 = spawn_frankenredis(replica1_port, Some(original_master_port));
    let _replica2 = spawn_frankenredis(replica2_port, Some(original_master_port));

    // Wait for replicas to connect and sync
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut link_up = false;
    while Instant::now() < deadline {
        let info1 = fetch_info_replication(replica1_port);
        let info2 = fetch_info_replication(replica2_port);
        if info1
            .as_ref()
            .is_some_and(|info| info.contains("master_link_status:up\r\n"))
            && info2
                .as_ref()
                .is_some_and(|info| info.contains("master_link_status:up\r\n"))
        {
            link_up = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(link_up, "replicas never synced to master");

    // Write some data to original master
    let mut client = connect_client(original_master_port);
    assert_eq!(
        send_command(&mut client, &[b"SET", b"sentinel_key", b"original"]),
        RespFrame::SimpleString("OK".to_string())
    );
    drop(client);

    // Wait for propagation
    thread::sleep(Duration::from_millis(200));

    // Sentinel decides to failover to replica1
    let mut sentinel_client1 = connect_client(replica1_port);
    assert_eq!(
        send_command(&mut sentinel_client1, &[b"REPLICAOF", b"NO", b"ONE"]),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        send_command(&mut sentinel_client1, &[b"CONFIG", b"REWRITE"]),
        RespFrame::SimpleString("OK".to_string())
    );
    drop(sentinel_client1);

    // Check replica1 is now master
    let info1 = fetch_info_replication(replica1_port).unwrap();
    assert!(info1.contains("role:master\r\n"));

    // Sentinel reconfigures replica2 to point to replica1
    let mut sentinel_client2 = connect_client(replica2_port);
    let replica1_port_str = replica1_port.to_string();
    assert_eq!(
        send_command(
            &mut sentinel_client2,
            &[b"REPLICAOF", b"127.0.0.1", replica1_port_str.as_bytes()]
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        send_command(&mut sentinel_client2, &[b"CONFIG", b"REWRITE"]),
        RespFrame::SimpleString("OK".to_string())
    );
    drop(sentinel_client2);

    // Wait for replica2 to sync with new master
    let deadline = Instant::now() + Duration::from_secs(5);
    link_up = false;
    while Instant::now() < deadline {
        let info2 = fetch_info_replication(replica2_port);
        if info2
            .as_ref()
            .is_some_and(|info| info.contains("master_link_status:up\r\n"))
        {
            link_up = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(link_up, "replica2 never synced to new master");

    // Write to new master and check propagation
    let mut client = connect_client(replica1_port);
    assert_eq!(
        send_command(&mut client, &[b"SET", b"sentinel_key", b"failed_over"]),
        RespFrame::SimpleString("OK".to_string())
    );
    drop(client);

    thread::sleep(Duration::from_millis(200));

    let mut client2 = connect_client(replica2_port);
    assert_eq!(
        send_command(&mut client2, &[b"GET", b"sentinel_key"]),
        RespFrame::BulkString(Some(b"failed_over".to_vec()))
    );

    send_shutdown_nosave(original_master_port);
    send_shutdown_nosave(replica1_port);
    send_shutdown_nosave(replica2_port);
}

#[test]
fn idle_client_disconnected_after_timeout() {
    let port = reserve_port();
    let _server = spawn_frankenredis(port, None);

    let mut client = connect_client(port);

    // Set timeout to 1 second
    let res = send_command(
        &mut client,
        &[b"CONFIG", b"SET", b"timeout", b"1"]
    );
    assert_eq!(res, RespFrame::SimpleString("OK".to_string()));

    // Wait slightly more than 1 second
    thread::sleep(Duration::from_millis(1500));

    // Client should have been disconnected by the server
    // Trying to send a PING might succeed in writing to the local socket buffer,
    // but reading the response should fail.
    client.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap_or(());
    
    let mut buf = [0u8; 1024];
    let read_res = client.read(&mut buf);
    assert!(read_res.unwrap_or(0) == 0, "Server should have closed connection");

    send_shutdown_nosave(port);
}
