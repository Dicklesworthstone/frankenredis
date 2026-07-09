#![forbid(unsafe_code)]

use std::env;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const HOST: &str = "127.0.0.1";
const COMMANDS_PER_ITER: usize = 64;
const ARITIES: [usize; 6] = [1, 4, 5, 8, 12, 16];
const COMMANDS: [&str; 5] = ["LPUSH", "RPUSH", "SADD", "PFADD", "PFMERGE"];
const SMISMEMBER_ARITIES: [usize; 2] = [2, 3];
const REMOVE_COMMANDS: [RemoveCommand; 2] = [
    RemoveCommand {
        remove: "HDEL",
        prefill: "HSET",
    },
    RemoveCommand {
        remove: "SREM",
        prefill: "SADD",
    },
];

#[derive(Clone, Copy)]
struct RemoveCommand {
    remove: &'static str,
    prefill: &'static str,
}
const DELETE_ARITIES: [usize; 4] = [1, 4, 8, 16];
const DELETE_COMMANDS: [&str; 2] = ["HDEL", "SREM"];
const LINSERT_LIST_LEN: usize = 64;
const SPOP_COUNT_SET_SIZE: usize = 8;
const SPOP_COUNT_POP: usize = 4;
const DUMP_ZSET_KEYS: usize = 64;
const DUMP_ZSET_MEMBERS: usize = 64;
const GETEX_PERSIST_EXAT: &str = "4102444800";
const GETEX_ABS_EXAT: &str = "4102444800";
const GETEX_ABS_PXAT: &str = "4102444800000";
const GETSET_VALUE_LEN: usize = 4096;

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
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set write timeout");
        Self {
            stream,
            buf: Vec::with_capacity(8192),
        }
    }

    fn flushall(&mut self) {
        self.run_packet(&encode_command(&["FLUSHALL"]), 1);
    }

    fn run_packet(&mut self, packet: &[u8], replies: usize) {
        self.stream
            .write_all(packet)
            .expect("write benchmark packet");
        self.read_integer_replies(replies);
    }

    fn run_resp_packet(&mut self, packet: &[u8], replies: usize) {
        self.stream
            .write_all(packet)
            .expect("write benchmark packet");
        self.read_resp_replies(replies);
    }

    fn read_integer_replies(&mut self, replies: usize) {
        self.buf.clear();
        let mut seen = 0usize;
        let mut scan_from = 0usize;
        while seen < replies {
            let mut tmp = [0u8; 8192];
            let read = self.stream.read(&mut tmp).expect("read benchmark replies");
            assert!(read > 0, "server closed benchmark connection");
            self.buf.extend_from_slice(&tmp[..read]);

            while let Some(pos) = find_crlf(&self.buf[scan_from..]) {
                let line_end = scan_from + pos;
                let line = &self.buf[scan_from..line_end];
                assert!(
                    line.first() == Some(&b':') || line == b"+OK",
                    "unexpected benchmark reply: {:?}",
                    String::from_utf8_lossy(line)
                );
                seen += 1;
                scan_from = line_end + 2;
                if seen == replies {
                    break;
                }
            }
        }
    }

    fn read_resp_replies(&mut self, replies: usize) {
        self.buf.clear();
        let mut seen = 0usize;
        let mut scan_from = 0usize;
        while seen < replies {
            let mut tmp = [0u8; 8192];
            let read = self.stream.read(&mut tmp).expect("read benchmark replies");
            assert!(read > 0, "server closed benchmark connection");
            self.buf.extend_from_slice(&tmp[..read]);

            while seen < replies {
                let Some(next) = resp_frame_end(&self.buf, scan_from) else {
                    break;
                };
                seen += 1;
                scan_from = next;
            }
        }
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|pair| pair == b"\r\n")
}

fn resp_frame_end(buf: &[u8], start: usize) -> Option<usize> {
    let first = *buf.get(start)?;
    let line_end = start + find_crlf(&buf[start..])?;
    let line = &buf[start + 1..line_end];
    match first {
        b'+' | b'-' | b':' | b',' | b'_' => Some(line_end + 2),
        b'$' => {
            let len = parse_resp_len(line)?;
            if len < 0 {
                return Some(line_end + 2);
            }
            let payload_end = line_end + 2 + usize::try_from(len).ok()?;
            if buf.get(payload_end..payload_end + 2)? == b"\r\n" {
                Some(payload_end + 2)
            } else {
                None
            }
        }
        b'*' | b'~' => {
            let len = parse_resp_len(line)?;
            if len < 0 {
                return Some(line_end + 2);
            }
            let mut pos = line_end + 2;
            for _ in 0..usize::try_from(len).ok()? {
                pos = resp_frame_end(buf, pos)?;
            }
            Some(pos)
        }
        b'%' => {
            let len = parse_resp_len(line)?;
            if len < 0 {
                return Some(line_end + 2);
            }
            let mut pos = line_end + 2;
            for _ in 0..usize::try_from(len).ok()?.saturating_mul(2) {
                pos = resp_frame_end(buf, pos)?;
            }
            Some(pos)
        }
        _ => None,
    }
}

fn parse_resp_len(line: &[u8]) -> Option<i64> {
    std::str::from_utf8(line).ok()?.parse().ok()
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
}

fn keyed_write_vs_redis(c: &mut Criterion) {
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

    let redis_port = free_port(env_u16("FR_REDIS_BENCH_PORT").unwrap_or(43_151));
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

    // (BlackThrush) Optional same-worker A/B: when FR_SERVER_ORIG_BIN is set, spawn a
    // second frankenredis built from a prior commit as a "frankenredis-orig" engine so
    // the dispatch_floor group measures candidate vs orig vs redis in ONE process on
    // ONE worker — the only way to compare across the multi-agent rch fleet honestly.
    let orig_bin = env::var_os("FR_SERVER_ORIG_BIN").map(PathBuf::from);
    let orig_port = free_port(fr_port + 1);
    let _fr_orig = orig_bin.as_ref().map(|bin| {
        assert!(
            bin.is_file(),
            "FR_SERVER_ORIG_BIN not found: {}",
            bin.display()
        );
        let server = spawn_frankenredis(bin, orig_port);
        wait_for_ping(orig_port);
        server
    });
    let mut floor_engines = engines.to_vec();
    if _fr_orig.is_some() {
        floor_engines.push(Engine {
            name: "frankenredis-orig",
            port: orig_port,
        });
    }

    let mut group = c.benchmark_group("keyed_write_vs_redis");
    group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));

    for cmd in COMMANDS {
        for arity in ARITIES {
            let packet = pipelined_keyed_write_packet(cmd, arity, COMMANDS_PER_ITER);
            for engine in engines {
                let id = BenchmarkId::new(format!("{cmd}_{arity}v"), engine.name);
                group.bench_with_input(id, &engine, |b, engine| {
                    let mut client = Client::connect(engine.port);
                    b.iter_custom(|iters| {
                        client.flushall();
                        let start = Instant::now();
                        for _ in 0..iters {
                            client.run_packet(&packet, COMMANDS_PER_ITER);
                        }
                        let elapsed = start.elapsed();
                        client.flushall();
                        elapsed
                    });
                });
            }
        }
    }

    for cmd in DELETE_COMMANDS {
        for arity in DELETE_ARITIES {
            let (prefill_packet, delete_packet) =
                pipelined_keyed_delete_packets(cmd, arity, COMMANDS_PER_ITER);
            for engine in engines {
                let id = BenchmarkId::new(format!("{cmd}_{arity}v"), engine.name);
                group.bench_with_input(id, &engine, |b, engine| {
                    let mut client = Client::connect(engine.port);
                    b.iter_custom(|iters| {
                        client.flushall();
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            client.run_packet(&prefill_packet, COMMANDS_PER_ITER);
                            let start = Instant::now();
                            client.run_packet(&delete_packet, COMMANDS_PER_ITER);
                            elapsed += start.elapsed();
                        }
                        client.flushall();
                        elapsed
                    });
                });
            }
        }
    }

    group.finish();

    let mut remove_group = c.benchmark_group("keyed_remove_vs_redis");
    remove_group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));

    for cmd in REMOVE_COMMANDS {
        let prefill = pipelined_remove_prefill_packet(cmd, COMMANDS_PER_ITER);
        let remove = pipelined_keyed_remove_packet(cmd.remove, COMMANDS_PER_ITER);
        for engine in engines {
            let id = BenchmarkId::new(cmd.remove, engine.name);
            remove_group.bench_with_input(id, &engine, |b, engine| {
                let mut client = Client::connect(engine.port);
                b.iter_custom(|iters| {
                    client.flushall();
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iters {
                        client.run_packet(&prefill, COMMANDS_PER_ITER);
                        let start = Instant::now();
                        client.run_packet(&remove, COMMANDS_PER_ITER);
                        elapsed += start.elapsed();
                    }
                    client.flushall();
                    elapsed
                });
            });
        }
    }

    remove_group.finish();

    let mut linsert_group = c.benchmark_group("linsert_vs_redis");
    linsert_group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));

    let linsert_prefill = linsert_prefill_packet(LINSERT_LIST_LEN);
    let linsert_insert = pipelined_linsert_packet(COMMANDS_PER_ITER, LINSERT_LIST_LEN / 2);
    for engine in engines {
        linsert_group.bench_with_input(
            BenchmarkId::new("LINSERT_mid", engine.name),
            &engine,
            |b, engine| {
                let mut client = Client::connect(engine.port);
                b.iter_custom(|iters| {
                    client.flushall();
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iters {
                        client.run_packet(&linsert_prefill, 1);
                        let start = Instant::now();
                        client.run_packet(&linsert_insert, COMMANDS_PER_ITER);
                        elapsed += start.elapsed();
                    }
                    client.flushall();
                    elapsed
                });
            },
        );
    }

    linsert_group.finish();

    let mut spop_group = c.benchmark_group("spop_count_vs_redis");
    spop_group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));
    let spop_prefill = pipelined_spop_count_prefill_packet(COMMANDS_PER_ITER, SPOP_COUNT_SET_SIZE);
    let spop_count = pipelined_spop_count_packet(COMMANDS_PER_ITER, SPOP_COUNT_POP);
    for engine in engines {
        spop_group.bench_with_input(
            BenchmarkId::new(format!("SPOP_count{SPOP_COUNT_POP}"), engine.name),
            &engine,
            |b, engine| {
                let mut client = Client::connect(engine.port);
                b.iter_custom(|iters| {
                    client.flushall();
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iters {
                        client.run_packet(&spop_prefill, COMMANDS_PER_ITER);
                        let start = Instant::now();
                        client.run_resp_packet(&spop_count, COMMANDS_PER_ITER);
                        elapsed += start.elapsed();
                    }
                    client.flushall();
                    elapsed
                });
            },
        );
    }

    spop_group.finish();

    let mut getex_group = c.benchmark_group("getex_persist_vs_redis");
    getex_group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));
    let getex_prefill = pipelined_getex_persist_prefill_packet(COMMANDS_PER_ITER);
    let getex_persist = pipelined_getex_persist_packet(COMMANDS_PER_ITER);
    for engine in engines {
        getex_group.bench_with_input(
            BenchmarkId::new("GETEX_PERSIST", engine.name),
            &engine,
            |b, engine| {
                let mut client = Client::connect(engine.port);
                b.iter_custom(|iters| {
                    client.flushall();
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iters {
                        client.run_packet(&getex_prefill, COMMANDS_PER_ITER);
                        let start = Instant::now();
                        client.run_resp_packet(&getex_persist, COMMANDS_PER_ITER);
                        elapsed += start.elapsed();
                    }
                    client.flushall();
                    elapsed
                });
            },
        );
    }

    getex_group.finish();

    let mut getex_abs_group = c.benchmark_group("getex_absexpire_vs_redis");
    getex_abs_group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));
    for (label, unit, deadline) in [
        ("GETEX_EXAT", "EXAT", GETEX_ABS_EXAT),
        ("GETEX_PXAT", "PXAT", GETEX_ABS_PXAT),
    ] {
        let prefill = pipelined_getex_absexpire_prefill_packet(COMMANDS_PER_ITER);
        let expire = pipelined_getex_absexpire_packet(COMMANDS_PER_ITER, unit, deadline);
        for engine in engines {
            getex_abs_group.bench_with_input(
                BenchmarkId::new(label, engine.name),
                &engine,
                |b, engine| {
                    let mut client = Client::connect(engine.port);
                    b.iter_custom(|iters| {
                        client.flushall();
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            client.run_packet(&prefill, COMMANDS_PER_ITER);
                            let start = Instant::now();
                            client.run_resp_packet(&expire, COMMANDS_PER_ITER);
                            elapsed += start.elapsed();
                        }
                        client.flushall();
                        elapsed
                    });
                },
            );
        }
    }

    getex_abs_group.finish();

    let mut set_get_group = c.benchmark_group("set_get_vs_redis");
    set_get_group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));
    let set_get_prefill = pipelined_set_get_prefill_packet(COMMANDS_PER_ITER);
    let set_get = pipelined_set_get_packet(COMMANDS_PER_ITER);
    for engine in engines {
        set_get_group.bench_with_input(
            BenchmarkId::new("SET_GET", engine.name),
            &engine,
            |b, engine| {
                let mut client = Client::connect(engine.port);
                b.iter_custom(|iters| {
                    client.flushall();
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iters {
                        client.run_packet(&set_get_prefill, COMMANDS_PER_ITER);
                        let start = Instant::now();
                        client.run_resp_packet(&set_get, COMMANDS_PER_ITER);
                        elapsed += start.elapsed();
                    }
                    client.flushall();
                    elapsed
                });
            },
        );
    }

    set_get_group.finish();

    let mut getset_group = c.benchmark_group("getset_large_vs_redis");
    getset_group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));
    let getset_prefill = pipelined_getset_large_prefill_packet(COMMANDS_PER_ITER);
    let getset = pipelined_getset_large_packet(COMMANDS_PER_ITER);
    for engine in engines {
        getset_group.bench_with_input(
            BenchmarkId::new(format!("GETSET_{GETSET_VALUE_LEN}B"), engine.name),
            &engine,
            |b, engine| {
                let mut client = Client::connect(engine.port);
                b.iter_custom(|iters| {
                    client.flushall();
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iters {
                        client.run_packet(&getset_prefill, COMMANDS_PER_ITER);
                        let start = Instant::now();
                        client.run_resp_packet(&getset, COMMANDS_PER_ITER);
                        elapsed += start.elapsed();
                    }
                    client.flushall();
                    elapsed
                });
            },
        );
    }
    getset_group.finish();

    let mut smismember_group = c.benchmark_group("smismember_vs_redis");
    smismember_group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));
    let smismember_prefill = smismember_prefill_packet();
    for arity in SMISMEMBER_ARITIES {
        let smismember = pipelined_smismember_packet(arity, COMMANDS_PER_ITER);
        for engine in engines {
            smismember_group.bench_with_input(
                BenchmarkId::new(format!("SMISMEMBER_{arity}v"), engine.name),
                &engine,
                |b, engine| {
                    let mut client = Client::connect(engine.port);
                    b.iter_custom(|iters| {
                        client.flushall();
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            client.run_packet(&smismember_prefill, 1);
                            let start = Instant::now();
                            client.run_resp_packet(&smismember, COMMANDS_PER_ITER);
                            elapsed += start.elapsed();
                        }
                        client.flushall();
                        elapsed
                    });
                },
            );
        }
    }
    smismember_group.finish();

    let mut dispatch_floor_group = c.benchmark_group("dispatch_floor_vs_redis");
    dispatch_floor_group.throughput(Throughput::Elements(COMMANDS_PER_ITER as u64));
    let xlen_prefill = pipelined_xlen_prefill_packet(COMMANDS_PER_ITER);
    let xlen_packet = pipelined_xlen_packet(COMMANDS_PER_ITER);
    let zremrange_prefill = pipelined_zremrangebyrank_prefill_packet(COMMANDS_PER_ITER);
    let zremrange_packet = pipelined_zremrangebyrank_noop_packet(COMMANDS_PER_ITER);
    let zcount_prefill = pipelined_zcount_prefill_packet(COMMANDS_PER_ITER);
    let zcount_packet = pipelined_zcount_packet(COMMANDS_PER_ITER);
    let type_prefill = pipelined_type_prefill_packet(COMMANDS_PER_ITER);
    let type_packet = pipelined_type_packet(COMMANDS_PER_ITER);
    let pfcount_prefill = pipelined_pfcount_prefill_packet(COMMANDS_PER_ITER);
    let pfcount_packet = pipelined_pfcount_packet(COMMANDS_PER_ITER);
    let strlen_prefill = pipelined_strlen_prefill_packet(COMMANDS_PER_ITER);
    let strlen_packet = pipelined_strlen_packet(COMMANDS_PER_ITER);
    let llen_prefill = pipelined_llen_prefill_packet(COMMANDS_PER_ITER);
    let llen_packet = pipelined_llen_packet(COMMANDS_PER_ITER);
    let scard_prefill = pipelined_scard_prefill_packet(COMMANDS_PER_ITER);
    let scard_packet = pipelined_scard_packet(COMMANDS_PER_ITER);
    let hlen_prefill = pipelined_hlen_prefill_packet(COMMANDS_PER_ITER);
    let hlen_packet = pipelined_hlen_packet(COMMANDS_PER_ITER);
    let zcard_prefill = pipelined_zcard_prefill_packet(COMMANDS_PER_ITER);
    let zcard_packet = pipelined_zcard_packet(COMMANDS_PER_ITER);
    for (label, prefill, packet, prefill_is_resp, packet_is_resp) in [
        ("XLEN", &xlen_prefill, &xlen_packet, true, false),
        (
            "ZREMRANGEBYRANK_noop",
            &zremrange_prefill,
            &zremrange_packet,
            false,
            false,
        ),
        ("TYPE_string", &type_prefill, &type_packet, false, true),
        (
            "PFCOUNT_single",
            &pfcount_prefill,
            &pfcount_packet,
            false,
            false,
        ),
        ("ZCOUNT", &zcount_prefill, &zcount_packet, false, false),
        ("STRLEN", &strlen_prefill, &strlen_packet, false, false),
        ("LLEN", &llen_prefill, &llen_packet, false, false),
        ("SCARD", &scard_prefill, &scard_packet, false, false),
        ("HLEN", &hlen_prefill, &hlen_packet, false, false),
        ("ZCARD", &zcard_prefill, &zcard_packet, false, false),
    ] {
        for engine in floor_engines.iter().copied() {
            dispatch_floor_group.bench_with_input(
                BenchmarkId::new(label, engine.name),
                &engine,
                |b, engine| {
                    let mut client = Client::connect(engine.port);
                    b.iter_custom(|iters| {
                        client.flushall();
                        if prefill_is_resp {
                            client.run_resp_packet(prefill, COMMANDS_PER_ITER);
                        } else {
                            client.run_packet(prefill, COMMANDS_PER_ITER);
                        }
                        let start = Instant::now();
                        for _ in 0..iters {
                            if packet_is_resp {
                                client.run_resp_packet(packet, COMMANDS_PER_ITER);
                            } else {
                                client.run_packet(packet, COMMANDS_PER_ITER);
                            }
                        }
                        let elapsed = start.elapsed();
                        client.flushall();
                        elapsed
                    });
                },
            );
        }
    }
    dispatch_floor_group.finish();

    let mut dump_group = c.benchmark_group("dump_zset_vs_redis");
    dump_group.throughput(Throughput::Elements(DUMP_ZSET_KEYS as u64));
    let dump_prefill = pipelined_dump_zset_prefill_packet(DUMP_ZSET_KEYS, DUMP_ZSET_MEMBERS);
    let dump_packet = pipelined_dump_zset_packet(DUMP_ZSET_KEYS);
    for engine in engines {
        dump_group.bench_with_input(
            BenchmarkId::new(format!("DUMP_zset_{DUMP_ZSET_MEMBERS}m"), engine.name),
            &engine,
            |b, engine| {
                let mut client = Client::connect(engine.port);
                b.iter_custom(|iters| {
                    client.flushall();
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iters {
                        client.run_packet(&dump_prefill, DUMP_ZSET_KEYS);
                        let start = Instant::now();
                        client.run_resp_packet(&dump_packet, DUMP_ZSET_KEYS);
                        elapsed += start.elapsed();
                        client.flushall();
                    }
                    elapsed
                });
            },
        );
    }
    dump_group.finish();
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
            let _ = stream.write_all(&encode_command(&["PING"]));
            let mut buf = [0u8; 64];
            if let Ok(read) = stream.read(&mut buf)
                && buf[..read].windows(4).any(|part| part == b"PONG")
            {
                return;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("server did not answer PING on port {port}");
}

fn free_port(start: u16) -> u16 {
    for port in start..start.saturating_add(500) {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        if TcpListener::bind(addr).is_ok() {
            return port;
        }
    }
    panic!("no free port near {start}");
}

fn env_u16(name: &str) -> Option<u16> {
    env::var(name).ok()?.parse().ok()
}

fn pipelined_keyed_write_packet(cmd: &str, arity: usize, count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * (32 + arity * 8));
    for _ in 0..count {
        packet.extend_from_slice(&keyed_write_command(cmd, arity));
    }
    packet
}

fn keyed_write_command(cmd: &str, arity: usize) -> Vec<u8> {
    let mut args = Vec::with_capacity(arity + 2);
    args.push(cmd);
    args.push("k");
    let values = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "l", "m", "n", "o", "p", "q",
    ];
    for value in values.iter().take(arity) {
        args.push(value);
    }
    encode_command(&args)
}

fn pipelined_remove_prefill_packet(cmd: RemoveCommand, count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 48);
    for idx in 0..count {
        let member = remove_member(idx);
        if cmd.prefill == "HSET" {
            packet.extend_from_slice(&encode_command(&[cmd.prefill, "k", &member, "v"]));
        } else {
            packet.extend_from_slice(&encode_command(&[cmd.prefill, "k", &member]));
        }
    }
    packet
}

fn pipelined_keyed_remove_packet(cmd: &str, count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 40);
    for idx in 0..count {
        let member = remove_member(idx);
        packet.extend_from_slice(&encode_command(&[cmd, "k", &member]));
    }
    packet
}

fn remove_member(idx: usize) -> String {
    format!("m{idx:03}")
}

fn pipelined_keyed_delete_packets(cmd: &str, arity: usize, count: usize) -> (Vec<u8>, Vec<u8>) {
    let mut prefill = Vec::with_capacity(count * (48 + arity * 16));
    let mut delete = Vec::with_capacity(count * (40 + arity * 12));
    for index in 0..count {
        prefill.extend_from_slice(&keyed_delete_prefill_command(cmd, arity, index));
        delete.extend_from_slice(&keyed_delete_command(cmd, arity, index));
    }
    (prefill, delete)
}

fn keyed_delete_prefill_command(cmd: &str, arity: usize, index: usize) -> Vec<u8> {
    let mut args = Vec::with_capacity(arity * 2 + 2);
    if cmd == "HDEL" {
        args.push(b"HSET".to_vec());
        args.push(b"k".to_vec());
        for value_index in 0..arity {
            args.push(format!("f{index}_{value_index}").into_bytes());
            args.push(b"v".to_vec());
        }
    } else {
        args.push(b"SADD".to_vec());
        args.push(b"k".to_vec());
        for value_index in 0..arity {
            args.push(format!("m{index}_{value_index}").into_bytes());
        }
    }
    encode_command_vecs(&args)
}

fn keyed_delete_command(cmd: &str, arity: usize, index: usize) -> Vec<u8> {
    let mut args = Vec::with_capacity(arity + 2);
    args.push(cmd.as_bytes().to_vec());
    args.push(b"k".to_vec());
    let prefix = if cmd == "HDEL" { b'f' } else { b'm' };
    for value_index in 0..arity {
        args.push(format!("{}{index}_{value_index}", prefix as char).into_bytes());
    }
    encode_command_vecs(&args)
}

fn linsert_prefill_packet(list_len: usize) -> Vec<u8> {
    let mut args = Vec::with_capacity(list_len + 2);
    args.push(b"RPUSH".to_vec());
    args.push(b"k".to_vec());
    for index in 0..list_len {
        args.push(format!("v{index:03}").into_bytes());
    }
    encode_command_vecs(&args)
}

fn pipelined_linsert_packet(count: usize, pivot_index: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 56);
    let pivot = format!("v{pivot_index:03}");
    for index in 0..count {
        let element = format!("x{index:03}");
        packet.extend_from_slice(&encode_command(&[
            "LINSERT", "k", "BEFORE", &pivot, &element,
        ]));
    }
    packet
}

fn pipelined_spop_count_prefill_packet(count: usize, set_size: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * (48 + set_size * 8));
    for index in 0..count {
        let mut args = Vec::with_capacity(set_size + 2);
        args.push(b"SADD".to_vec());
        args.push(format!("s{index:03}").into_bytes());
        for member_index in 0..set_size {
            args.push(format!("m{index:03}_{member_index}").into_bytes());
        }
        packet.extend_from_slice(&encode_command_vecs(&args));
    }
    packet
}

fn pipelined_spop_count_packet(count: usize, pop_count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 48);
    let pop_count = pop_count.to_string();
    for index in 0..count {
        let key = format!("s{index:03}");
        packet.extend_from_slice(&encode_command(&["SPOP", &key, &pop_count]));
    }
    packet
}

fn pipelined_getex_persist_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 64);
    for index in 0..count {
        let key = format!("g{index:03}");
        let value = format!("v{index:03}");
        packet.extend_from_slice(&encode_command(&[
            "SET",
            &key,
            &value,
            "EXAT",
            GETEX_PERSIST_EXAT,
        ]));
    }
    packet
}

fn pipelined_getex_persist_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 48);
    for index in 0..count {
        let key = format!("g{index:03}");
        packet.extend_from_slice(&encode_command(&["GETEX", &key, "PERSIST"]));
    }
    packet
}

fn pipelined_getex_absexpire_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 48);
    for index in 0..count {
        let key = format!("a{index:03}");
        let value = format!("v{index:03}");
        packet.extend_from_slice(&encode_command(&["SET", &key, &value]));
    }
    packet
}

fn pipelined_set_get_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 48);
    for index in 0..count {
        let key = format!("sg{index:03}");
        let value = format!("old{index:03}");
        packet.extend_from_slice(&encode_command(&["SET", &key, &value]));
    }
    packet
}

fn pipelined_getex_absexpire_packet(count: usize, unit: &str, deadline: &str) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 64);
    for index in 0..count {
        let key = format!("a{index:03}");
        packet.extend_from_slice(&encode_command(&["GETEX", &key, unit, deadline]));
    }
    packet
}

fn pipelined_set_get_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 56);
    for index in 0..count {
        let key = format!("sg{index:03}");
        let value = format!("new{index:03}");
        packet.extend_from_slice(&encode_command(&["SET", &key, &value, "GET"]));
    }
    packet
}

fn pipelined_getset_large_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * (48 + GETSET_VALUE_LEN));
    let value = vec![b'x'; GETSET_VALUE_LEN];
    for index in 0..count {
        packet.extend_from_slice(&encode_command_vecs(&[
            b"SET".to_vec(),
            format!("gs{index:03}").into_bytes(),
            value.clone(),
        ]));
    }
    packet
}

fn pipelined_getset_large_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 64);
    for index in 0..count {
        packet.extend_from_slice(&encode_command_vecs(&[
            b"GETSET".to_vec(),
            format!("gs{index:03}").into_bytes(),
            b"n".to_vec(),
        ]));
    }
    packet
}

fn smismember_prefill_packet() -> Vec<u8> {
    encode_command(&["SADD", "s", "a", "b", "c", "d", "e", "f"])
}

fn pipelined_smismember_packet(arity: usize, count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * (48 + arity * 8));
    for index in 0..count {
        let miss = format!("miss{index:03}");
        match arity {
            2 => packet.extend_from_slice(&encode_command(&["SMISMEMBER", "s", "a", &miss])),
            3 => packet.extend_from_slice(&encode_command(&["SMISMEMBER", "s", "a", &miss, "c"])),
            _ => unreachable!("benchmark arity is fixed"),
        }
    }
    packet
}

fn pipelined_xlen_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 56);
    for index in 0..count {
        let key = format!("xlen:{index:03}");
        packet.extend_from_slice(&encode_command(&["XADD", &key, "1-0", "f", "v"]));
    }
    packet
}

fn pipelined_xlen_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 32);
    for index in 0..count {
        let key = format!("xlen:{index:03}");
        packet.extend_from_slice(&encode_command(&["XLEN", &key]));
    }
    packet
}

fn pipelined_zremrangebyrank_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 48);
    for index in 0..count {
        let key = format!("zr:{index:03}");
        packet.extend_from_slice(&encode_command(&["ZADD", &key, "0", "m"]));
    }
    packet
}

fn pipelined_zremrangebyrank_noop_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 56);
    for index in 0..count {
        let key = format!("zr:{index:03}");
        packet.extend_from_slice(&encode_command(&["ZREMRANGEBYRANK", &key, "10", "20"]));
    }
    packet
}

// (CrimsonHawk) ZCOUNT dispatch-floor bench: each key holds 16 members so the
// count exercises the rank walk while the pipelined `ZCOUNT key 0 16` isolates
// the dispatch cost the preclassifier removes.
fn pipelined_zcount_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 96);
    for index in 0..count {
        let key = format!("zc:{index:03}");
        let mut args: Vec<Vec<u8>> = vec![b"ZADD".to_vec(), key.into_bytes()];
        for m in 0..16u32 {
            args.push(m.to_string().into_bytes());
            args.push(format!("m{m}").into_bytes());
        }
        packet.extend_from_slice(&encode_command_vecs(&args));
    }
    packet
}

fn pipelined_zcount_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 32);
    for index in 0..count {
        let key = format!("zc:{index:03}");
        packet.extend_from_slice(&encode_command(&["ZCOUNT", &key, "0", "16"]));
    }
    packet
}

fn pipelined_type_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 40);
    for index in 0..count {
        let key = format!("typ:{index:03}");
        packet.extend_from_slice(&encode_command(&["SET", &key, "v"]));
    }
    packet
}

fn pipelined_type_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 32);
    for index in 0..count {
        let key = format!("typ:{index:03}");
        packet.extend_from_slice(&encode_command(&["TYPE", &key]));
    }
    packet
}

fn pipelined_pfcount_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 48);
    for index in 0..count {
        let key = format!("pf:{index:03}");
        packet.extend_from_slice(&encode_command(&["PFADD", &key, "m"]));
    }
    packet
}

fn pipelined_pfcount_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 32);
    for index in 0..count {
        let key = format!("pf:{index:03}");
        packet.extend_from_slice(&encode_command(&["PFCOUNT", &key]));
    }
    packet
}

// (BlackThrush) cardinality-cluster dispatch-floor benches. Each command is a
// single-key O(1) length/cardinality read (STRLEN/LLEN/SCARD/HLEN/ZCARD) that the
// preclassifier hoists out of the ~position-85 cascade arm to the dispatch floor.
fn pipelined_strlen_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 40);
    for index in 0..count {
        let key = format!("sl:{index:03}");
        packet.extend_from_slice(&encode_command(&["SET", &key, "value"]));
    }
    packet
}

fn pipelined_strlen_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 32);
    for index in 0..count {
        let key = format!("sl:{index:03}");
        packet.extend_from_slice(&encode_command(&["STRLEN", &key]));
    }
    packet
}

fn pipelined_llen_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 40);
    for index in 0..count {
        let key = format!("ll:{index:03}");
        packet.extend_from_slice(&encode_command(&["RPUSH", &key, "a", "b", "c"]));
    }
    packet
}

fn pipelined_llen_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 32);
    for index in 0..count {
        let key = format!("ll:{index:03}");
        packet.extend_from_slice(&encode_command(&["LLEN", &key]));
    }
    packet
}

fn pipelined_scard_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 40);
    for index in 0..count {
        let key = format!("sc:{index:03}");
        packet.extend_from_slice(&encode_command(&["SADD", &key, "1", "2", "3"]));
    }
    packet
}

fn pipelined_scard_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 32);
    for index in 0..count {
        let key = format!("sc:{index:03}");
        packet.extend_from_slice(&encode_command(&["SCARD", &key]));
    }
    packet
}

fn pipelined_hlen_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 48);
    for index in 0..count {
        let key = format!("hl:{index:03}");
        packet.extend_from_slice(&encode_command(&["HSET", &key, "f1", "v1", "f2", "v2"]));
    }
    packet
}

fn pipelined_hlen_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 32);
    for index in 0..count {
        let key = format!("hl:{index:03}");
        packet.extend_from_slice(&encode_command(&["HLEN", &key]));
    }
    packet
}

fn pipelined_zcard_prefill_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 48);
    for index in 0..count {
        let key = format!("zd:{index:03}");
        packet.extend_from_slice(&encode_command(&["ZADD", &key, "1", "a", "2", "b"]));
    }
    packet
}

fn pipelined_zcard_packet(count: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(count * 32);
    for index in 0..count {
        let key = format!("zd:{index:03}");
        packet.extend_from_slice(&encode_command(&["ZCARD", &key]));
    }
    packet
}

fn pipelined_dump_zset_prefill_packet(keys: usize, members: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(keys * (48 + members * 16));
    for key_index in 0..keys {
        let key = format!("dz:{key_index:03}");
        let mut args = Vec::with_capacity(members.saturating_mul(2).saturating_add(2));
        args.push(b"ZADD".to_vec());
        args.push(key.into_bytes());
        for member_index in 0..members {
            args.push(member_index.to_string().into_bytes());
            args.push(format!("m{member_index:03}").into_bytes());
        }
        packet.extend_from_slice(&encode_command_vecs(&args));
    }
    packet
}

fn pipelined_dump_zset_packet(keys: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(keys * 32);
    for key_index in 0..keys {
        let key = format!("dz:{key_index:03}");
        packet.extend_from_slice(&encode_command(&["DUMP", &key]));
    }
    packet
}

fn encode_command(args: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn encode_command_vecs(args: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = keyed_write_vs_redis
}
criterion_main!(benches);
