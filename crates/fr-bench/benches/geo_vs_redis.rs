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

    fn run_status_packet(&mut self, packet: &[u8], replies: usize) {
        or_exit(self.stream.write_all(packet), "write benchmark packet");
        for _ in 0..replies {
            self.read_status_or_integer_reply();
        }
    }

    fn run_geohash_packet(&mut self, packet: &[u8], replies: usize, members_per_reply: usize) {
        or_exit(self.stream.write_all(packet), "write benchmark packet");
        for _ in 0..replies {
            self.read_array_bulk_reply(members_per_reply);
        }
    }

    fn read_status_or_integer_reply(&mut self) {
        let line = self.read_line();
        match line.first().copied() {
            Some(b'+') | Some(b':') => {}
            Some(b'-') => fail(format_args!(
                "benchmark command returned error: {}",
                String::from_utf8_lossy(&line)
            )),
            _ => fail(format_args!(
                "unexpected benchmark reply: {}",
                String::from_utf8_lossy(&line)
            )),
        }
    }

    fn read_array_bulk_reply(&mut self, members: usize) {
        let array = self.read_line();
        let expected = format!("*{members}");
        if array.as_slice() != expected.as_bytes() {
            fail(format_args!(
                "unexpected GEOHASH array reply: {}",
                String::from_utf8_lossy(&array)
            ));
        }
        for _ in 0..members {
            let header = self.read_line();
            if header.as_slice() == b"$-1" {
                continue;
            }
            if header.first() != Some(&b'$') {
                fail(format_args!(
                    "unexpected GEOHASH bulk header: {}",
                    String::from_utf8_lossy(&header)
                ));
            }
            let Some(len_bytes) = header.get(1..) else {
                fail("missing GEOHASH bulk length");
            };
            let len = parse_bulk_len(len_bytes);
            self.read_exact_bulk(len);
        }
    }

    fn read_exact_bulk(&mut self, len: usize) {
        while self.buf.len() < len + 2 {
            let mut tmp = [0u8; 8192];
            let read = or_exit(self.stream.read(&mut tmp), "read benchmark bulk");
            if read == 0 {
                fail("server closed benchmark connection");
            }
            self.buf.extend(tmp.iter().copied().take(read));
        }
        if self.buf.get(len..len + 2) != Some(b"\r\n") {
            fail("malformed GEOHASH bulk trailer");
        }
        self.buf.drain(..len + 2);
    }

    fn read_line(&mut self) -> Vec<u8> {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|pair| pair == b"\r\n") {
                let line: Vec<u8> = self.buf.drain(..pos).collect();
                self.buf.drain(..2);
                return line;
            }
            let mut tmp = [0u8; 8192];
            let read = or_exit(self.stream.read(&mut tmp), "read benchmark reply");
            if read == 0 {
                fail("server closed benchmark connection");
            }
            self.buf.extend(tmp.iter().copied().take(read));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
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

fn parse_bulk_len(bytes: &[u8]) -> usize {
    let mut len = 0usize;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            fail("non-decimal GEOHASH bulk length");
        }
        len = len
            .checked_mul(10)
            .and_then(|value| value.checked_add(usize::from(byte - b'0')))
            .unwrap_or_else(|| fail("GEOHASH bulk length overflow"));
    }
    len
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
}

fn geo_vs_redis(c: &mut Criterion) {
    let redis_bin = redis_server_bin();
    let fr_bin = fr_server_bin();
    if !redis_bin.is_file() {
        fail(format_args!(
            "REDIS_SERVER_BIN not found: {}",
            redis_bin.display()
        ));
    }
    if !fr_bin.is_file() {
        fail(format_args!(
            "FR_SERVER_BIN not found: {}",
            fr_bin.display()
        ));
    }

    let redis_port = free_port(env_u16("FR_REDIS_GEO_BENCH_PORT").unwrap_or(43_651));
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

    let mut group = c.benchmark_group("geo_vs_redis");
    group.throughput(Throughput::Elements(COMMANDS_PER_ITER_U64));

    for case in [
        (
            "GEOHASH_1",
            geohash_packet(&[b"Palermo"], COMMANDS_PER_ITER),
            1,
        ),
        (
            "GEOHASH_4",
            geohash_packet(
                &[b"Palermo", b"Catania", b"SanFrancisco", b"London"],
                COMMANDS_PER_ITER,
            ),
            4,
        ),
    ] {
        for engine in engines {
            let id = BenchmarkId::new(case.0, engine.name);
            group.bench_with_input(id, &engine, |b, engine| {
                let mut client = Client::connect(engine.port);
                b.iter_custom(|iters| {
                    let start = Instant::now();
                    for _ in 0..iters {
                        client.run_geohash_packet(&case.1, COMMANDS_PER_ITER, case.2);
                    }
                    start.elapsed()
                });
            });
        }
    }

    group.finish();
}

fn setup_dataset(client: &mut Client) {
    client.run_status_packet(&command(&[b"FLUSHALL".as_slice()]), 1);
    client.run_status_packet(
        &command(&[
            b"GEOADD".as_slice(),
            b"geo".as_slice(),
            b"13.361389".as_slice(),
            b"38.115556".as_slice(),
            b"Palermo".as_slice(),
            b"15.087269".as_slice(),
            b"37.502669".as_slice(),
            b"Catania".as_slice(),
            b"-122.4194".as_slice(),
            b"37.7749".as_slice(),
            b"SanFrancisco".as_slice(),
            b"-0.1278".as_slice(),
            b"51.5074".as_slice(),
            b"London".as_slice(),
        ]),
        1,
    );
}

fn geohash_packet(members: &[&[u8]], count: usize) -> Vec<u8> {
    let mut args = Vec::with_capacity(members.len() + 2);
    args.push(b"GEOHASH".as_slice());
    args.push(b"geo".as_slice());
    args.extend_from_slice(members);
    let command = command(&args);
    let mut packet = Vec::with_capacity(command.len() * count);
    for _ in 0..count {
        packet.extend_from_slice(&command);
    }
    packet
}

fn command(args: &[&[u8]]) -> Vec<u8> {
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
    let child = or_exit(
        Command::new(bin)
            .arg("--port")
            .arg(port.to_string())
            .arg("--save")
            .arg("")
            .arg("--appendonly")
            .arg("no")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(),
        "spawn redis-server",
    );
    Server { child }
}

fn spawn_frankenredis(bin: &Path, port: u16) -> Server {
    let child = or_exit(
        Command::new(bin)
            .arg("--port")
            .arg(port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(),
        "spawn frankenredis",
    );
    Server { child }
}

fn wait_for_ping(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect((HOST, port)) {
            let _ = stream.write_all(&command(&[b"PING".as_slice()]));
            let mut buf = [0u8; 64];
            if let Ok(read) = stream.read(&mut buf)
                && buf
                    .get(..read)
                    .is_some_and(|reply| reply.windows(4).any(|part| part == b"PONG"))
            {
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
            let _ = stream.shutdown(Shutdown::Both);
        }
        thread::sleep(Duration::from_millis(50));
    }
    fail(format_args!("server did not answer PING on port {port}"));
}

fn free_port(start: u16) -> u16 {
    for port in start..start.saturating_add(500) {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        if TcpListener::bind(addr).is_ok() {
            return port;
        }
    }
    fail(format_args!("no free port near {start}"));
}

fn env_u16(name: &str) -> Option<u16> {
    env::var(name).ok()?.parse().ok()
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = geo_vs_redis
}
criterion_main!(benches);
