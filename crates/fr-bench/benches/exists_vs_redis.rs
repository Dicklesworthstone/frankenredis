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
const COMMANDS_PER_ITER: usize = 128;
const COMMANDS_PER_ITER_U64: u64 = 128;
const DATASET_KEYS: [&[u8]; 8] = [b"k0", b"k1", b"k2", b"k3", b"k4", b"k5", b"k6", b"k7"];
const WORKLOADS: [(&str, [&[u8]; 8]); 3] = [
    (
        "exists8_all_hit",
        [b"k0", b"k1", b"k2", b"k3", b"k4", b"k5", b"k6", b"k7"],
    ),
    (
        "exists8_half_hit",
        [b"k0", b"m0", b"k1", b"m1", b"k2", b"m2", b"k3", b"m3"],
    ),
    (
        "exists8_duplicates",
        [b"k0", b"k0", b"k1", b"m0", b"k2", b"m1", b"k2", b"k3"],
    ),
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
        let stream = or_exit(TcpStream::connect((HOST, port)), "connect benchmark server");
        or_exit(
            stream.set_read_timeout(Some(Duration::from_secs(5))),
            "set read timeout",
        );
        or_exit(
            stream.set_write_timeout(Some(Duration::from_secs(5))),
            "set write timeout",
        );
        Self {
            stream,
            buf: Vec::with_capacity(8192),
        }
    }

    fn run_packet(&mut self, packet: &[u8], replies: usize) {
        or_exit(self.stream.write_all(packet), "write benchmark packet");
        self.read_integer_replies(replies);
    }

    fn read_integer_replies(&mut self, replies: usize) {
        self.buf.clear();
        let mut seen = 0usize;
        let mut scan_from = 0usize;
        while seen < replies {
            let mut tmp = [0u8; 8192];
            let read = or_exit(self.stream.read(&mut tmp), "read benchmark replies");
            if read == 0 {
                fail("server closed benchmark connection");
            }
            let bytes = match tmp.get(..read) {
                Some(bytes) => bytes,
                None => fail("benchmark reply read exceeded scratch buffer"),
            };
            self.buf.extend_from_slice(bytes);

            while let Some(scan) = self.buf.get(scan_from..) {
                let Some(pos) = find_crlf(scan) else {
                    break;
                };
                let line_end = scan_from + pos;
                let line = match self.buf.get(scan_from..line_end) {
                    Some(line) => line,
                    None => fail("benchmark reply line bounds were invalid"),
                };
                if line.first() != Some(&b':') && line != b"+OK" {
                    fail(format_args!(
                        "unexpected benchmark reply: {:?}",
                        String::from_utf8_lossy(line)
                    ));
                }
                seen += 1;
                scan_from = line_end + 2;
                if seen == replies {
                    break;
                }
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|pair| pair == b"\r\n")
}

fn or_exit<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => fail(format_args!("{context}: {error}")),
    }
}

fn fail(message: impl std::fmt::Display) -> ! {
    let _ = writeln!(std::io::stderr(), "{message}");
    std::process::exit(2);
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
}

fn exists_vs_redis(c: &mut Criterion) {
    let redis_bin = redis_server_bin();
    let fr_bin = fr_server_bin();
    if !redis_bin.is_file() {
        fail(format!(
            "REDIS_SERVER_BIN not found: {}",
            redis_bin.display()
        ));
    }
    if !fr_bin.is_file() {
        fail(format!("FR_SERVER_BIN not found: {}", fr_bin.display()));
    }

    let redis_port = free_port(env_u16("FR_REDIS_EXISTS_BENCH_PORT").unwrap_or(43_251));
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

    let mut group = c.benchmark_group("exists_vs_redis");
    group.throughput(Throughput::Elements(COMMANDS_PER_ITER_U64));

    for (name, keys) in WORKLOADS {
        let packet = exists_packet(&keys, COMMANDS_PER_ITER);
        for engine in engines {
            let id = BenchmarkId::new(name, engine.name);
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

    let move_missing_packet = move_missing_packet(COMMANDS_PER_ITER);
    for engine in engines {
        let id = BenchmarkId::new("move_missing", engine.name);
        group.bench_with_input(id, &engine, |b, engine| {
            let mut client = Client::connect(engine.port);
            b.iter_custom(|iters| {
                let start = Instant::now();
                for _ in 0..iters {
                    client.run_packet(&move_missing_packet, COMMANDS_PER_ITER);
                }
                start.elapsed()
            });
        });
    }

    let object_idletime_hit_packet = object_idletime_hit_packet(COMMANDS_PER_ITER);
    for engine in engines {
        let id = BenchmarkId::new("object_idletime_hit", engine.name);
        group.bench_with_input(id, &engine, |b, engine| {
            let mut client = Client::connect(engine.port);
            b.iter_custom(|iters| {
                let start = Instant::now();
                for _ in 0..iters {
                    client.run_packet(&object_idletime_hit_packet, COMMANDS_PER_ITER);
                }
                start.elapsed()
            });
        });
    }

    let watch_unwatch_packet = watch_unwatch_packet(COMMANDS_PER_ITER);
    for engine in engines {
        let id = BenchmarkId::new("watch_unwatch", engine.name);
        group.bench_with_input(id, &engine, |b, engine| {
            let mut client = Client::connect(engine.port);
            b.iter_custom(|iters| {
                let start = Instant::now();
                for _ in 0..iters {
                    client.run_packet(&watch_unwatch_packet, COMMANDS_PER_ITER * 2);
                }
                start.elapsed()
            });
        });
    }

    group.finish();
}

fn setup_dataset(client: &mut Client) {
    client.run_packet(&encode_command(&[b"FLUSHALL".as_slice()]), 1);
    for key in DATASET_KEYS {
        client.run_packet(
            &encode_command(&[b"SET".as_slice(), key, b"v".as_slice()]),
            1,
        );
    }
}

fn exists_packet(keys: &[&[u8]; 8], count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 96);
    for _ in 0..count {
        let mut args = Vec::with_capacity(9);
        args.push(b"EXISTS".as_slice());
        args.extend_from_slice(keys);
        packet.extend_from_slice(&encode_command(&args));
    }
    packet
}

fn move_missing_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 40);
    for _ in 0..count {
        packet.extend_from_slice(&encode_command(&[
            b"MOVE".as_slice(),
            b"missing".as_slice(),
            b"1".as_slice(),
        ]));
    }
    packet
}

fn object_idletime_hit_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 48);
    for _ in 0..count {
        packet.extend_from_slice(&encode_command(&[
            b"OBJECT".as_slice(),
            b"IDLETIME".as_slice(),
            b"k0".as_slice(),
        ]));
    }
    packet
}

fn watch_unwatch_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 56);
    for _ in 0..count {
        packet.extend_from_slice(&encode_command(&[b"WATCH".as_slice(), b"k0".as_slice()]));
        packet.extend_from_slice(&encode_command(&[b"UNWATCH".as_slice()]));
    }
    packet
}

fn encode_command(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    write_resp_len(&mut out, b'*', args.len());
    for arg in args {
        write_resp_len(&mut out, b'$', arg.len());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn write_resp_len(out: &mut Vec<u8>, prefix: u8, len: usize) {
    out.push(prefix);
    if write!(out, "{len}\r\n").is_err() {
        fail("failed to write RESP length into benchmark buffer");
    }
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
        .spawn();
    let child = or_exit(child, "spawn redis-server");
    Server { child }
}

fn spawn_frankenredis(bin: &Path, port: u16) -> Server {
    let child = Command::new(bin)
        .arg("--port")
        .arg(port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let child = or_exit(child, "spawn frankenredis");
    Server { child }
}

fn wait_for_ping(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect((HOST, port)) {
            let _ = stream.write_all(&encode_command(&[b"PING".as_slice()]));
            let mut buf = [0u8; 64];
            if let Ok(read) = stream.read(&mut buf)
                && buf
                    .get(..read)
                    .is_some_and(|reply| reply.windows(4).any(|part| part == b"PONG"))
            {
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    fail(format!("server did not answer PING on port {port}"));
}

fn free_port(start: u16) -> u16 {
    for port in start..start.saturating_add(500) {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        if TcpListener::bind(addr).is_ok() {
            return port;
        }
    }
    fail(format!("no free port near {start}"));
}

fn env_u16(name: &str) -> Option<u16> {
    env::var(name).ok()?.parse().ok()
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = exists_vs_redis
}
criterion_main!(benches);
