//! End-to-end TCP tests that spin up a minimal FrankenRedis server,
//! connect via TCP, send RESP commands, and verify responses.
//! Tests the actual networking stack including RESP framing.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

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

    loop {
        let n = stream.read(&mut buf).expect("read from server");
        assert!(n > 0, "server closed connection unexpectedly");
        accumulated.extend_from_slice(&buf[..n]);
        match parse_frame(&accumulated) {
            Ok(parsed) => return parsed.frame,
            Err(_) => continue, // incomplete, read more
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
