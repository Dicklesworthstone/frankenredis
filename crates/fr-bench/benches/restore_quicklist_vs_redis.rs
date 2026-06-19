#![forbid(unsafe_code)]

use std::env;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const HOST: &str = "127.0.0.1";
const COMMANDS_PER_ITER: usize = 8;
const LIST_MEMBERS: usize = 96;
const MEMBER_LEN: usize = 40;
const RDB_TYPE_LIST_QUICKLIST_2: u8 = 18;

#[derive(Clone, Copy)]
struct Engine {
    name: &'static str,
    port: u16,
}

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
}

enum Reply {
    Simple,
    Error(Vec<u8>),
    Integer,
    Bulk(Option<Vec<u8>>),
}

impl Client {
    fn connect(port: u16) -> Self {
        let stream = TcpStream::connect((HOST, port)).expect("connect benchmark server");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .expect("set write timeout");
        Self {
            stream,
            buf: Vec::with_capacity(1 << 16),
        }
    }

    fn run_packet(&mut self, packet: &[u8], replies: usize) {
        self.stream
            .write_all(packet)
            .expect("write benchmark packet");
        for _ in 0..replies {
            self.read_status_or_integer_reply();
        }
    }

    fn request(&mut self, args: &[Vec<u8>]) -> Reply {
        self.stream
            .write_all(&command(args))
            .expect("write benchmark command");
        self.read_reply()
    }

    fn read_status_or_integer_reply(&mut self) {
        match self.read_reply() {
            Reply::Simple | Reply::Integer => {}
            Reply::Error(line) => fatal(format!(
                "benchmark command returned error: {}",
                lossy(&line)
            )),
            Reply::Bulk(_) => fatal("unexpected bulk reply where status/integer was expected"),
        }
    }

    fn read_reply(&mut self) -> Reply {
        let line = self.read_line();
        match line.first().copied() {
            Some(b'+') => Reply::Simple,
            Some(b'-') => Reply::Error(strip_reply_prefix(&line, b'-').to_vec()),
            Some(b':') => {
                let _ = parse_i64(strip_reply_prefix(&line, b':'), "integer reply");
                Reply::Integer
            }
            Some(b'$') => {
                let len = parse_i64(strip_reply_prefix(&line, b'$'), "bulk length");
                if len < 0 {
                    return Reply::Bulk(None);
                }
                let len = usize::try_from(len).expect("bulk length should fit usize");
                let body = self.read_exact_from_stream(len);
                let crlf = self.read_exact_from_stream(2);
                if crlf != b"\r\n" {
                    fatal("bulk reply missing trailing CRLF");
                }
                Reply::Bulk(Some(body))
            }
            _ => fatal(format!("unexpected benchmark reply: {}", lossy(&line))),
        }
    }

    fn read_line(&mut self) -> Vec<u8> {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|pair| pair == b"\r\n") {
                let line = self
                    .buf
                    .get(..pos)
                    .unwrap_or_else(|| fatal("reply line split out of bounds"))
                    .to_vec();
                self.buf.drain(..pos + 2);
                return line;
            }
            self.read_more();
        }
    }

    fn read_exact_from_stream(&mut self, len: usize) -> Vec<u8> {
        while self.buf.len() < len {
            self.read_more();
        }
        self.buf.drain(..len).collect()
    }

    fn read_more(&mut self) {
        let mut tmp = [0u8; 8192];
        let read = self.stream.read(&mut tmp).expect("read benchmark reply");
        if read == 0 {
            fatal("server closed benchmark connection");
        }
        let chunk = tmp
            .get(..read)
            .unwrap_or_else(|| fatal("reply chunk split out of bounds"));
        self.buf.extend_from_slice(chunk);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
}

fn restore_quicklist_vs_redis(c: &mut Criterion) {
    let redis_bin = redis_server_bin();
    let fr_bin = fr_server_bin();
    assert!(
        redis_bin.is_file(),
        "REDIS_SERVER_BIN not found: {}",
        redis_bin.display()
    );
    assert!(
        fr_bin.is_file(),
        "FR_SERVER_BIN not found: {}",
        fr_bin.display()
    );

    let redis_port = free_port(env_u16("FR_RESTORE_QUICKLIST_BENCH_PORT").unwrap_or(43_751));
    let fr_port = free_port(redis_port + 1);
    let _redis = spawn_redis(&redis_bin, redis_port);
    let _fr = spawn_frankenredis(&fr_bin, fr_port);
    wait_for_ping(redis_port);
    wait_for_ping(fr_port);

    let payload = redis_quicklist2_payload(redis_port);
    let first = payload.first().copied().unwrap_or_default();
    assert_eq!(
        first, RDB_TYPE_LIST_QUICKLIST_2,
        "Redis DUMP payload should be quicklist2 type 18, got {first}"
    );

    let engines = [
        Engine {
            name: "redis-7.2.4",
            port: redis_port,
        },
        Engine {
            name: "frankenredis",
            port: fr_port,
        },
    ];

    for engine in engines {
        let mut client = Client::connect(engine.port);
        client.run_packet(&command(&[bytes("FLUSHALL")]), 1);
    }

    let packet = restore_packet(&payload, COMMANDS_PER_ITER);
    let mut group = c.benchmark_group("restore_quicklist_vs_redis");
    group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));

    for engine in engines {
        let id = BenchmarkId::new("quicklist2_packed_restore", engine.name);
        group.bench_with_input(id, &engine, |b, engine| {
            let mut client = Client::connect(engine.port);
            b.iter_custom(|iters| {
                let start = Instant::now();
                for _ in 0..iters {
                    client.run_packet(&packet, COMMANDS_PER_ITER);
                }
                start.elapsed()
            });
        });
    }

    group.finish();
}

fn redis_quicklist2_payload(port: u16) -> Vec<u8> {
    let mut client = Client::connect(port);
    client.run_packet(&command(&[bytes("FLUSHALL")]), 1);

    let mut rpush = vec![bytes("RPUSH"), bytes("source")];
    for index in 0..LIST_MEMBERS {
        rpush.push(list_member(index));
    }
    client.run_packet(&command(&rpush), 1);

    match client.request(&[bytes("DUMP"), bytes("source")]) {
        Reply::Bulk(Some(payload)) => payload,
        Reply::Bulk(None) => fatal("Redis returned nil DUMP payload"),
        Reply::Error(line) => fatal(format!("Redis DUMP failed: {}", lossy(&line))),
        Reply::Simple | Reply::Integer => fatal("Redis DUMP returned non-bulk reply"),
    }
}

fn restore_packet(payload: &[u8], count: usize) -> Vec<u8> {
    let mut packet = Vec::new();
    for index in 0..count {
        packet.extend_from_slice(&command(&[
            bytes("RESTORE"),
            format!("dst:{index}").into_bytes(),
            bytes("0"),
            payload.to_vec(),
            bytes("REPLACE"),
        ]));
    }
    packet
}

fn list_member(index: usize) -> Vec<u8> {
    let mut member = format!("member:{index:04}:").into_bytes();
    member.resize(MEMBER_LEN, b'x');
    member
}

fn command(args: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn bytes(value: &str) -> Vec<u8> {
    value.as_bytes().to_vec()
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parse_i64(bytes: &[u8], label: &str) -> i64 {
    let text = std::str::from_utf8(bytes).unwrap_or_else(|_| fatal(format!("invalid {label}")));
    text.parse::<i64>()
        .unwrap_or_else(|_| fatal(format!("invalid {label}: {text}")))
}

fn strip_reply_prefix(line: &[u8], prefix: u8) -> &[u8] {
    line.strip_prefix(&[prefix])
        .unwrap_or_else(|| fatal("missing reply prefix"))
}

fn fatal(message: impl AsRef<str>) -> ! {
    eprintln!("{}", message.as_ref());
    std::process::exit(2);
}

fn redis_server_bin() -> PathBuf {
    env::var_os("REDIS_SERVER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let local = Path::new("legacy_redis_code/redis/src/redis-server");
            if local.exists() {
                local.to_path_buf()
            } else {
                PathBuf::from("/dp/frankenredis/legacy_redis_code/redis/src/redis-server")
            }
        })
}

fn fr_server_bin() -> PathBuf {
    env::var_os("FR_SERVER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let target_dir = env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("target"));
            target_dir.join("release/frankenredis")
        })
}

fn spawn_redis(bin: &Path, port: u16) -> Server {
    let child = Command::new(bin)
        .arg("--port")
        .arg(port.to_string())
        .arg("--save")
        .arg("")
        .arg("--appendonly")
        .arg("no")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn redis-server");
    Server { child }
}

fn spawn_frankenredis(bin: &Path, port: u16) -> Server {
    let child = Command::new(bin)
        .arg("--port")
        .arg(port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn frankenredis");
    Server { child }
}

fn wait_for_ping(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect((HOST, port)) {
            let _ = stream.write_all(&command(&[bytes("PING")]));
            let mut buf = [0u8; 64];
            if let Ok(read) = stream.read(&mut buf)
                && buf
                    .get(..read)
                    .unwrap_or_default()
                    .windows(4)
                    .any(|part| part == b"PONG")
            {
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    fatal(format!("server did not answer PING on port {port}"));
}

fn free_port(start: u16) -> u16 {
    for port in start..start.saturating_add(500) {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        if TcpListener::bind(addr).is_ok() {
            return port;
        }
    }
    fatal(format!("no free port near {start}"));
}

fn env_u16(name: &str) -> Option<u16> {
    env::var(name).ok()?.parse().ok()
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = restore_quicklist_vs_redis
}
criterion_main!(benches);
