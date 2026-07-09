#![forbid(unsafe_code)]

use std::env;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const HOST: &str = "127.0.0.1";
const COMMANDS_PER_ITER: usize = 128;
const COMMANDS_PER_ITER_U64: u64 = 128;
const DATASET_BYTES: usize = 4096;

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

    fn run_bitfield_packet(&mut self, packet: &[u8], replies: usize) {
        or_exit(self.stream.write_all(packet), "write benchmark packet");
        self.read_array_integer_replies(replies);
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

    fn read_array_integer_replies(&mut self, replies: usize) {
        for _ in 0..replies {
            let array = self.read_line();
            if array.as_slice() != b"*1" {
                fail(format_args!(
                    "unexpected BITFIELD array reply: {}",
                    String::from_utf8_lossy(&array)
                ));
            }
            let value = self.read_line();
            if value.first() != Some(&b':') {
                fail(format_args!(
                    "unexpected BITFIELD value reply: {}",
                    String::from_utf8_lossy(&value)
                ));
            }
        }
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

fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
}

fn bitfield_vs_redis(c: &mut Criterion) {
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

    let redis_port = free_port(env_u16("FR_REDIS_BITFIELD_BENCH_PORT").unwrap_or(43_551));
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

    let mut group = c.benchmark_group("bitfield_vs_redis");
    group.throughput(Throughput::Elements(COMMANDS_PER_ITER_U64));

    for case in [
        (
            "BITFIELD_GET_u8_0",
            bitfield_get_packet(b"BITFIELD", COMMANDS_PER_ITER),
        ),
        (
            "BITFIELD_RO_GET_u8_0",
            bitfield_get_packet(b"BITFIELD_RO", COMMANDS_PER_ITER),
        ),
        (
            "BITFIELD_SET_u8_0_1",
            bitfield_set_packet(COMMANDS_PER_ITER),
        ),
    ] {
        for engine in engines {
            let id = BenchmarkId::new(case.0, engine.name);
            group.bench_with_input(id, &engine, |b, engine| {
                let mut client = Client::connect(engine.port);
                b.iter_custom(|iters| {
                    let start = Instant::now();
                    for _ in 0..iters {
                        client.run_bitfield_packet(&case.1, COMMANDS_PER_ITER);
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
    let value = vec![0xA5; DATASET_BYTES];
    client.run_status_packet(&command(&[b"SET".as_slice(), b"bf".as_slice(), &value]), 1);
}

fn bitfield_get_packet(command_name: &[u8], count: usize) -> Vec<u8> {
    let command = command(&[
        command_name,
        b"bf".as_slice(),
        b"GET".as_slice(),
        b"u8".as_slice(),
        b"0".as_slice(),
    ]);
    let mut packet = Vec::with_capacity(command.len() * count);
    for _ in 0..count {
        packet.extend_from_slice(&command);
    }
    packet
}

fn bitfield_set_packet(count: usize) -> Vec<u8> {
    let command = command(&[
        b"BITFIELD".as_slice(),
        b"bf".as_slice(),
        b"SET".as_slice(),
        b"u8".as_slice(),
        b"0".as_slice(),
        b"1".as_slice(),
    ]);
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
    if let Some(bin) = env::var_os("FR_SERVER_BIN") {
        return PathBuf::from(bin);
    }
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"));
    let bin = target_dir.join("release/frankenredis");
    ensure_default_fr_server_bin(&bin);
    bin
}

fn ensure_default_fr_server_bin(bin: &Path) {
    static SERVER_BUILD: OnceLock<()> = OnceLock::new();
    SERVER_BUILD.get_or_init(|| {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("fr-bench manifest lives under workspace/crates/fr-bench");
        let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(workspace)
            .args(["build", "--profile", "release", "-p", "fr-server"])
            .status()
            .expect("build fr-server benchmark binary");
        assert!(status.success(), "fr-server build failed before benchmark");
        assert!(
            bin.is_file(),
            "FR_SERVER_BIN not found after build: {}",
            bin.display()
        );
    });
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
    targets = bitfield_vs_redis
}
criterion_main!(benches);
