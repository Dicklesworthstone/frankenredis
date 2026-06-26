#![forbid(unsafe_code)]

use std::env;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const HOST: &str = "127.0.0.1";
const COMMANDS_PER_ITER: usize = 16;
const SMALL_INTSET_MEMBERS: usize = 512;
const MEDIUM_GENERIC_MEMBERS: usize = 2048;
const LARGE_GENERIC_MEMBERS: usize = 4096;
const SINTERCARD_LIMIT: &str = "16";
const OPS: [(&str, &str, &str, &str); 5] = [
    ("SINTERSTORE_SMALL", "SINTERSTORE", "small", "large"),
    ("SDIFFSTORE_SMALL", "SDIFFSTORE", "small", "large_miss"),
    ("SUNIONSTORE_SMALL", "SUNIONSTORE", "small", "large_miss"),
    ("SINTERSTORE_LARGE", "SINTERSTORE", "large", "medium"),
    ("SDIFFSTORE_LARGE", "SDIFFSTORE", "large", "medium"),
];

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

    fn read_status_or_integer_reply(&mut self) {
        let line = self.read_line();
        match line.first().copied() {
            Some(b'+') | Some(b':') => {}
            Some(b'-') => fatal(format!(
                "benchmark command returned error: {}",
                lossy(&line)
            )),
            _ => fatal(format!("unexpected benchmark reply: {}", lossy(&line))),
        }
    }

    fn read_line(&mut self) -> Vec<u8> {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|pair| pair == b"\r\n") {
                let line = self.buf[..pos].to_vec();
                self.buf.drain(..pos + 2);
                return line;
            }
            let mut tmp = [0u8; 8192];
            let read = self.stream.read(&mut tmp).expect("read benchmark reply");
            if read == 0 {
                fatal("server closed benchmark connection");
            }
            self.buf.extend_from_slice(&tmp[..read]);
        }
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn fatal(message: impl AsRef<str>) -> ! {
    eprintln!("{}", message.as_ref());
    std::process::exit(2);
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
}

fn set_algebra_vs_redis(c: &mut Criterion) {
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

    let redis_port = free_port(env_u16("FR_REDIS_SETALG_BENCH_PORT").unwrap_or(43_451));
    let fr_port = free_port(redis_port + 1);
    let _redis = spawn_redis(&redis_bin, redis_port);
    let _fr = spawn_frankenredis(&fr_bin, fr_port);
    wait_for_ping(redis_port);
    wait_for_ping(fr_port);

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
        setup_dataset(&mut client);
    }

    let mut group = c.benchmark_group("set_algebra_vs_redis");
    group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));

    for (label, op, lhs, rhs) in OPS {
        let packet = set_algebra_packet(op, lhs, rhs, COMMANDS_PER_ITER);
        for engine in engines {
            let id = BenchmarkId::new(label, engine.name);
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
    }

    for (op, packet) in [
        (
            "SINTERCARD_LIMIT2",
            sintercard_limit_packet(&["small", "large"], SINTERCARD_LIMIT, COMMANDS_PER_ITER),
        ),
        (
            "SINTERCARD_LIMIT3",
            sintercard_limit_packet(
                &["small", "medium", "large"],
                SINTERCARD_LIMIT,
                COMMANDS_PER_ITER,
            ),
        ),
    ] {
        for engine in engines {
            let id = BenchmarkId::new(op, engine.name);
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
    }

    group.finish();
}

fn setup_dataset(client: &mut Client) {
    client.run_packet(&command(&[bytes("FLUSHALL")]), 1);
    client.run_packet(
        &command(&[
            bytes("CONFIG"),
            bytes("SET"),
            bytes("set-max-intset-entries"),
            bytes("512"),
        ]),
        1,
    );

    let mut small = vec![bytes("SADD"), bytes("small")];
    for value in 0..SMALL_INTSET_MEMBERS {
        small.push(value.to_string().into_bytes());
    }
    client.run_packet(&command(&small), 1);

    let mut large = vec![bytes("SADD"), bytes("large")];
    for value in 0..LARGE_GENERIC_MEMBERS {
        large.push(value.to_string().into_bytes());
    }
    client.run_packet(&command(&large), 1);

    let mut medium = vec![bytes("SADD"), bytes("medium")];
    for value in 0..MEDIUM_GENERIC_MEMBERS {
        medium.push(value.to_string().into_bytes());
    }
    client.run_packet(&command(&medium), 1);

    let mut large_miss = vec![bytes("SADD"), bytes("large_miss")];
    for value in 10_000..10_000 + LARGE_GENERIC_MEMBERS {
        large_miss.push(value.to_string().into_bytes());
    }
    client.run_packet(&command(&large_miss), 1);
}

fn set_algebra_packet(op: &str, lhs: &str, rhs: &str, count: usize) -> Vec<u8> {
    let mut packet = Vec::new();
    for index in 0..count {
        let dst = format!("dst:{op}:{index}");
        packet.extend_from_slice(&command(&[
            bytes(op),
            dst.into_bytes(),
            bytes(lhs),
            bytes(rhs),
        ]));
    }
    packet
}

fn sintercard_limit_packet(keys: &[&str], limit: &str, count: usize) -> Vec<u8> {
    let mut packet = Vec::new();
    for _ in 0..count {
        let mut args = Vec::with_capacity(keys.len() + 3);
        args.push(bytes("SINTERCARD"));
        args.push(keys.len().to_string().into_bytes());
        for key in keys {
            args.push(bytes(key));
        }
        args.push(bytes("LIMIT"));
        args.push(bytes(limit));
        packet.extend_from_slice(&command(&args));
    }
    packet
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
                && buf[..read].windows(4).any(|part| part == b"PONG")
            {
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
    targets = set_algebra_vs_redis
}
criterion_main!(benches);
