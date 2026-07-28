#![forbid(unsafe_code)]

//! Same-invocation A/A + A/B + live-incumbent gate for server throughput.
//!
//! The harness deliberately drives many established connections from persistent
//! client shards. Every shard writes its clients before reading their replies;
//! independent shards overlap so pipeline depth 1 can saturate the server rather
//! than the client. By default, two byte-identical mio processes provide the
//! null control and the same FrankenRedis ELF with the runtime flag is the
//! candidate. A command-shape experiment may instead put all three FrankenRedis
//! processes on io_uring and select a frozen control route by environment before
//! the first packet. Vendored Redis is always the live incumbent.
//!
//! Run only through strict remote RCH on one explicitly selected worker:
//!
//! `RCH_WORKER=<worker> RCH_REQUIRE_REMOTE=1 env -u CARGO_TARGET_DIR rch exec --
//! cargo test --profile release-perf -p fr-server --features io-uring-writes
//! --test io_uring_submission_ab -- --ignored --exact
//! io_uring_submission_same_elf_null_then_ab --nocapture --test-threads=1`

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hint::black_box;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLIENTS: usize = 50;
// Four shards became client-bound below one microsecond per command at P16, and
// even five left the ECHO floor at only 85.298% median server utilization. Nine
// shards use the worker's remaining physical cores while keeping a disjoint
// server core; the utilization guard remains authoritative.
const DEFAULT_CLIENT_THREADS: usize = 9;
// Two complete 24-permutation order cycles keep every physical arm in every
// position equally often. A partial tail can bias an otherwise identical A/A
// pair; the median validity check caught that on the first XTRIM floor run.
const DEFAULT_SAMPLES: usize = 48;
const DEFAULT_OPS_PER_SAMPLE: usize = 200_000;
const DEFAULT_PROFILE_SECONDS: u64 = 3;
// One group is only CLIENTS * pipeline operations. Twenty-five groups left a
// sub-microsecond floor dominated by client-channel handoffs even with nine
// pinned shards; 125 keeps each arm slice below one second while amortizing the
// barrier enough to drive the server continuously.
const INTERLEAVE_GROUPS: usize = 125;
const QUIET_CORE_MAX_PCT: f64 = 5.0;
const QUIET_CORE_PREFLIGHT_ATTEMPTS: usize = 20;
const MIN_SERVER_UTIL_PCT: f64 = 90.0;
const IO_URING_FLAG: &str = "--io-uring-output";
const BITPOS_RANGE_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_BITPOS_RANGE_FLOOR_ORIG";
const BITFIELD_RO_TWO_GET_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_BITFIELD_RO_TWO_GET_FLOOR_ORIG";
const OBJECT_ENCODING_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_OBJECT_ENCODING_FLOOR_ORIG";
const OBJECT_REFCOUNT_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_OBJECT_REFCOUNT_FLOOR_ORIG";
const DBSIZE_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_DBSIZE_FLOOR_ORIG";
const ECHO_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_ECHO_FLOOR_ORIG";
const WAIT_ZERO_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_WAIT_ZERO_FLOOR_ORIG";
const XTRIM_MINID_NOOP_CONTROL_ENV: &str = "FR_PERF_AB_XTRIM_MINID_NOOP_ORIG";
const XTRIM_MINID_NOOP_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_XTRIM_MINID_NOOP_FLOOR_ORIG";
const XTRIM_MINID_NOOP_PREFILL_ENTRIES: usize = 1_000;
const SHUTDOWN: &[u8] = b"*2\r\n$8\r\nSHUTDOWN\r\n$6\r\nNOSAVE\r\n";
const SET: &[u8] = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n";
const SET_REPLY: &[u8] = b"+OK\r\n";
const GET: &[u8] = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n";
const GET_REPLY: &[u8] = b"$1\r\nv\r\n";
const BITPOS_RANGE_PREFILL: &[u8] =
    b"*3\r\n$3\r\nSET\r\n$8\r\nbitpos:k\r\n$8\r\n\0\0\0\0\0\0\0\x80\r\n";
const BITPOS_RANGE: &[u8] =
    b"*5\r\n$6\r\nBITPOS\r\n$8\r\nbitpos:k\r\n$1\r\n1\r\n$1\r\n0\r\n$1\r\n7\r\n";
const BITPOS_RANGE_REPLY: &[u8] = b":56\r\n";
const BITFIELD_RO_TWO_GET_PREFILL: &[u8] =
    b"*3\r\n$3\r\nSET\r\n$10\r\nbitfield:k\r\n$2\r\n\x12\x34\r\n";
const BITFIELD_RO_TWO_GET: &[u8] = b"*8\r\n$11\r\nBITFIELD_RO\r\n$10\r\nbitfield:k\r\n\
$3\r\nGET\r\n$2\r\nu8\r\n$1\r\n0\r\n$3\r\nGET\r\n$2\r\nu8\r\n$1\r\n8\r\n";
const BITFIELD_RO_TWO_GET_REPLY: &[u8] = b"*2\r\n:18\r\n:52\r\n";
const OBJECT_ENCODING_PREFILL: &[u8] = b"*3\r\n$3\r\nSET\r\n$8\r\nobject:k\r\n$2\r\n42\r\n";
const OBJECT_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$8\r\nobject:k\r\n";
const OBJECT_ENCODING_REPLY: &[u8] = b"$3\r\nint\r\n";
const OBJECT_REFCOUNT_PREFILL: &[u8] = b"*3\r\n$3\r\nSET\r\n$8\r\nobject:k\r\n$5\r\nvalue\r\n";
const OBJECT_REFCOUNT: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nREFCOUNT\r\n$8\r\nobject:k\r\n";
const OBJECT_REFCOUNT_REPLY: &[u8] = b":1\r\n";
const DBSIZE: &[u8] = b"*1\r\n$6\r\nDBSIZE\r\n";
const DBSIZE_REPLY: &[u8] = b":1\r\n";
const ECHO: &[u8] = b"*2\r\n$4\r\nECHO\r\n$1\r\nx\r\n";
const ECHO_REPLY: &[u8] = b"$1\r\nx\r\n";
const UNWATCH: &[u8] = b"*1\r\n$7\r\nUNWATCH\r\n";
const UNWATCH_REPLY: &[u8] = b"+OK\r\n";
const WAIT_ZERO: &[u8] = b"*3\r\n$4\r\nWAIT\r\n$1\r\n0\r\n$1\r\n0\r\n";
const WAIT_ZERO_REPLY: &[u8] = b":0\r\n";
const XTRIM_MINID_NOOP: &[u8] =
    b"*5\r\n$5\r\nXTRIM\r\n$2\r\nxs\r\n$5\r\nMINID\r\n$1\r\n~\r\n$3\r\n0-0\r\n";
const XTRIM_MINID_NOOP_REPLY: &[u8] = b":0\r\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arm {
    MioA,
    MioB,
    IoUring,
    Redis,
}

impl Arm {
    const ALL: [Self; 4] = [Self::MioA, Self::MioB, Self::IoUring, Self::Redis];

    const fn index(self) -> usize {
        match self {
            Self::MioA => 0,
            Self::MioB => 1,
            Self::IoUring => 2,
            Self::Redis => 3,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::MioA => "mio_a",
            Self::MioB => "mio_b",
            Self::IoUring => "io_uring",
            Self::Redis => "redis",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    Set,
    Get,
    Mixed,
    BitposRange,
    BitfieldRoTwoGet,
    ObjectEncoding,
    ObjectRefcount,
    Dbsize,
    Echo,
    Unwatch,
    WaitZero,
    XtrimMinidNoop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandFloorAb {
    None,
    BitposRange,
    BitfieldRoTwoGet,
    ObjectEncoding,
    ObjectRefcount,
    Dbsize,
    Echo,
    WaitZero,
    XtrimMinidNoop,
    XtrimMinidNoopFloor,
}

impl Workload {
    const fn name(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Get => "get",
            Self::Mixed => "mixed",
            Self::BitposRange => "bitpos-range",
            Self::BitfieldRoTwoGet => "bitfield-ro-two-get",
            Self::ObjectEncoding => "object-encoding",
            Self::ObjectRefcount => "object-refcount",
            Self::Dbsize => "dbsize",
            Self::Echo => "echo",
            Self::Unwatch => "unwatch",
            Self::WaitZero => "wait-zero",
            Self::XtrimMinidNoop => "xtrim-minid-noop",
        }
    }

    const fn profile_targets(self) -> &'static [&'static str] {
        match self {
            Self::BitposRange => &[
                "frankenredis::process_buffered_frames",
                "execute_plain_bitpos_borrowed",
                "bitpos_impl",
                "bitpos_full_bytes",
                "parse_borrowed_plain_bitpos_range_packet",
            ],
            Self::BitfieldRoTwoGet => &[
                "frankenredis::process_buffered_frames",
                "fr_command::bitfield_ro_cmd",
                "bitfield_get_batch",
                "parse_command_args_borrowed_into",
                "copy_borrowed_argv_into_scratch",
            ],
            Self::ObjectEncoding => &[
                "frankenredis::process_buffered_frames",
                "execute_plain_object_encoding_borrowed_into",
                "parse_borrowed_plain_object_encoding_packet",
                "object_encoding",
            ],
            Self::ObjectRefcount => &[
                "frankenredis::process_buffered_frames",
                "execute_plain_object_refcount_borrowed",
                "parse_borrowed_plain_object_refcount_packet",
                "object_refcount",
            ],
            Self::Dbsize => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_dbsize",
                "execute_plain_dbsize_borrowed",
                "parse_borrowed_plain_dbsize_packet",
                "dbsize_in_db",
            ],
            Self::Echo => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_echo_into",
                "execute_plain_echo_borrowed_into",
                "parse_borrowed_plain_echo_packet",
            ],
            Self::Unwatch => &[
                "frankenredis::process_buffered_frames",
                "execute_plain_unwatch_borrowed_into",
                "parse_borrowed_plain_unwatch_packet",
            ],
            Self::WaitZero => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_wait_zero",
                "execute_plain_wait_borrowed",
                "parse_borrowed_plain_key_arg1_packet",
            ],
            Self::XtrimMinidNoop => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_xtrim_minid_noop",
                "execute_plain_xtrim_minid_noop_borrowed",
                "fr_command::xtrim",
                "fr_store::Store::xtrim_minid_approx",
                "xtrim_minid_noop_guard_enabled",
            ],
            Self::Set | Self::Get | Self::Mixed => &[],
        }
    }

    fn parse_list() -> Vec<Self> {
        let value = std::env::var("FR_URING_AB_WORKLOADS").unwrap_or_else(|_| "set".to_owned());
        value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| match item {
                "set" => Self::Set,
                "get" => Self::Get,
                "mixed" => Self::Mixed,
                "bitpos-range" => Self::BitposRange,
                "bitfield-ro-two-get" => Self::BitfieldRoTwoGet,
                "object-encoding" => Self::ObjectEncoding,
                "object-refcount" => Self::ObjectRefcount,
                "dbsize" => Self::Dbsize,
                "echo" => Self::Echo,
                "unwatch" => Self::Unwatch,
                "wait-zero" => Self::WaitZero,
                "xtrim-minid-noop" => Self::XtrimMinidNoop,
                other => panic!("unknown FR_URING_AB_WORKLOADS item: {other}"),
            })
            .collect()
    }
}

struct ExchangeCase {
    request: Vec<u8>,
    response: Vec<u8>,
}

struct WorkloadPackets {
    even: ExchangeCase,
    odd: ExchangeCase,
}

impl WorkloadPackets {
    fn new(workload: Workload, pipeline: usize) -> Self {
        assert!(pipeline > 0, "pipeline depth must be positive");
        match workload {
            Workload::Set => {
                let case = repeated_case(SET, SET_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::Get => {
                let case = repeated_case(GET, GET_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::Mixed => Self {
                even: mixed_case(pipeline, false),
                odd: mixed_case(pipeline, true),
            },
            Workload::BitposRange => {
                let case = repeated_case(BITPOS_RANGE, BITPOS_RANGE_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::BitfieldRoTwoGet => {
                let case = repeated_case(BITFIELD_RO_TWO_GET, BITFIELD_RO_TWO_GET_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ObjectEncoding => {
                let case = repeated_case(OBJECT_ENCODING, OBJECT_ENCODING_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ObjectRefcount => {
                let case = repeated_case(OBJECT_REFCOUNT, OBJECT_REFCOUNT_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::Dbsize => {
                let case = repeated_case(DBSIZE, DBSIZE_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::Echo => {
                let case = repeated_case(ECHO, ECHO_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::Unwatch => {
                let case = repeated_case(UNWATCH, UNWATCH_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::WaitZero => {
                let case = repeated_case(WAIT_ZERO, WAIT_ZERO_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::XtrimMinidNoop => {
                let case = repeated_case(XTRIM_MINID_NOOP, XTRIM_MINID_NOOP_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
        }
    }
}

fn repeated_case(request: &[u8], response: &[u8], pipeline: usize) -> ExchangeCase {
    ExchangeCase {
        request: request.repeat(pipeline),
        response: response.repeat(pipeline),
    }
}

fn mixed_case(pipeline: usize, start_with_get: bool) -> ExchangeCase {
    let mut request = Vec::with_capacity(pipeline * SET.len().max(GET.len()));
    let mut response = Vec::with_capacity(pipeline * SET_REPLY.len().max(GET_REPLY.len()));
    for index in 0..pipeline {
        let get = (index % 2 == 0) == start_with_get;
        if get {
            request.extend_from_slice(GET);
            response.extend_from_slice(GET_REPLY);
        } else {
            request.extend_from_slice(SET);
            response.extend_from_slice(SET_REPLY);
        }
    }
    ExchangeCase { request, response }
}

enum ClientCommand {
    Run {
        packets: Arc<WorkloadPackets>,
        groups: usize,
        odd_first: bool,
    },
    Shutdown,
}

struct ClientWorker {
    command: Sender<ClientCommand>,
    complete: Receiver<()>,
    handle: Option<thread::JoinHandle<()>>,
}

struct ClientDriver {
    workers: Vec<ClientWorker>,
}

impl ClientDriver {
    fn new(port: u16, client_threads: usize) -> Self {
        assert!(
            (1..=CLIENTS).contains(&client_threads),
            "client thread count must be in 1..={CLIENTS}"
        );
        let mut workers = Vec::with_capacity(client_threads);
        for worker_index in 0..client_threads {
            let client_count =
                CLIENTS / client_threads + usize::from(worker_index < CLIENTS % client_threads);
            let clients = (0..client_count).map(|_| connect(port)).collect();
            let (command_tx, command_rx) = mpsc::channel();
            let (complete_tx, complete_rx) = mpsc::channel();
            let handle = thread::Builder::new()
                .name(format!("bench-client-{worker_index}"))
                .spawn(move || {
                    client_worker(clients, command_rx, complete_tx);
                })
                .expect("spawn benchmark client worker");
            workers.push(ClientWorker {
                command: command_tx,
                complete: complete_rx,
                handle: Some(handle),
            });
        }
        Self { workers }
    }

    fn run(&self, packets: &Arc<WorkloadPackets>, groups: usize, odd_first: bool) -> Duration {
        let start = Instant::now();
        for worker in &self.workers {
            worker
                .command
                .send(ClientCommand::Run {
                    packets: Arc::clone(packets),
                    groups,
                    odd_first,
                })
                .expect("dispatch benchmark client work");
        }
        for worker in &self.workers {
            worker
                .complete
                .recv()
                .expect("benchmark client worker completed");
        }
        start.elapsed()
    }
}

impl Drop for ClientDriver {
    fn drop(&mut self) {
        for worker in &self.workers {
            let _ = worker.command.send(ClientCommand::Shutdown);
        }
        for worker in &mut self.workers {
            if let Some(handle) = worker.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

fn client_worker(
    mut clients: Vec<TcpStream>,
    commands: Receiver<ClientCommand>,
    complete: Sender<()>,
) {
    while let Ok(command) = commands.recv() {
        let ClientCommand::Run {
            packets,
            groups,
            odd_first,
        } = command
        else {
            return;
        };
        let mut response = Vec::new();
        for group in 0..groups {
            let odd = (group % 2 == 1) ^ odd_first;
            let case = if odd { &packets.odd } else { &packets.even };
            let request = black_box(case.request.as_slice());
            for client in &mut clients {
                client.write_all(request).expect("write request group");
            }
            response.resize(case.response.len(), 0);
            for client in &mut clients {
                client
                    .read_exact(&mut response)
                    .expect("read complete response group");
                assert_eq!(
                    response, case.response,
                    "server returned bytes that diverge from the RESP oracle"
                );
                black_box(response.as_slice());
            }
        }
        complete
            .send(())
            .expect("report benchmark client completion");
    }
}

struct Server {
    arm: Arm,
    child: Child,
    port: u16,
    clients: Option<ClientDriver>,
    stderr_path: PathBuf,
}

impl Server {
    fn spawn(
        fr_binary: &Path,
        redis_binary: &Path,
        arm: Arm,
        root: &Path,
        server_core: usize,
        client_threads: usize,
        command_floor_ab: CommandFloorAb,
    ) -> Self {
        let runtime_dir = root.join(arm.name());
        fs::create_dir_all(&runtime_dir).expect("create unique server runtime directory");
        let stderr_path = runtime_dir.join("stderr.log");
        let stderr = File::create(&stderr_path).expect("create unique server stderr log");
        let port = free_port();
        let mut command = Command::new("taskset");
        command
            .args(["-c", &server_core.to_string()])
            .arg(if matches!(arm, Arm::Redis) {
                redis_binary
            } else {
                fr_binary
            })
            .args(["--bind", "127.0.0.1", "--port", &port.to_string()]);
        if matches!(arm, Arm::Redis) {
            command.args(["--save", "", "--appendonly", "no"]);
        }
        if !matches!(arm, Arm::Redis)
            && (matches!(arm, Arm::IoUring) || !matches!(command_floor_ab, CommandFloorAb::None))
        {
            command.arg(
                std::env::var("FR_URING_AB_FLAG").unwrap_or_else(|_| IO_URING_FLAG.to_owned()),
            );
        }
        if matches!(command_floor_ab, CommandFloorAb::BitposRange)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(BITPOS_RANGE_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::BitfieldRoTwoGet)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(BITFIELD_RO_TWO_GET_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::ObjectEncoding)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(OBJECT_ENCODING_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::ObjectRefcount)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(OBJECT_REFCOUNT_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::Dbsize)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(DBSIZE_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::Echo) && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(ECHO_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::WaitZero)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(WAIT_ZERO_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::XtrimMinidNoop)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(XTRIM_MINID_NOOP_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::XtrimMinidNoopFloor)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(XTRIM_MINID_NOOP_FLOOR_CONTROL_ENV, "1");
        }
        command
            .current_dir(&runtime_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));

        let child = command.spawn().expect("spawn benchmark server arm");
        let mut server = Self {
            arm,
            child,
            port,
            clients: None,
            stderr_path,
        };
        server.wait_until_ready();
        server.clients = Some(ClientDriver::new(port, client_threads));
        server
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn cpu_ns(&self) -> u64 {
        fs::read_to_string(format!("/proc/{}/schedstat", self.pid()))
            .expect("read server process schedstat")
            .split_whitespace()
            .next()
            .expect("schedstat contains execution time")
            .parse::<u64>()
            .expect("parse server CPU nanoseconds")
    }

    fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("poll server startup") {
                let stderr = fs::read_to_string(&self.stderr_path).unwrap_or_default();
                panic!(
                    "{} server exited during startup with {status}: {stderr}",
                    self.arm.name()
                );
            }
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let stderr = fs::read_to_string(&self.stderr_path).unwrap_or_default();
        panic!(
            "{} server on port {} did not become ready: {stderr}",
            self.arm.name(),
            self.port
        );
    }

    fn assert_flag_reached_process(&self) {
        assert!(
            !matches!(self.arm, Arm::Redis),
            "Redis does not accept the FrankenRedis io_uring flag"
        );
        let cmdline = fs::read(format!("/proc/{}/cmdline", self.pid()))
            .expect("read candidate process command line");
        let flag = std::env::var("FR_URING_AB_FLAG").unwrap_or_else(|_| IO_URING_FLAG.to_owned());
        assert!(
            cmdline
                .split(|byte| *byte == 0)
                .any(|arg| arg == flag.as_bytes()),
            "{} process did not receive {flag}",
            self.arm.name()
        );
        println!(
            "FRANKENREDIS_FLAG arm={} pid={} flag={flag}",
            self.arm.name(),
            self.pid()
        );
    }

    fn assert_environment_value(&self, name: &str, expected: Option<&str>) {
        assert!(
            !matches!(self.arm, Arm::Redis),
            "Redis environment is outside the same-ELF control contract"
        );
        let environ = fs::read(format!("/proc/{}/environ", self.pid()))
            .expect("read FrankenRedis process environment");
        let prefix = format!("{name}=");
        let actual = environ.split(|byte| *byte == 0).find_map(|entry| {
            entry
                .strip_prefix(prefix.as_bytes())
                .map(|value| String::from_utf8_lossy(value).into_owned())
        });
        assert_eq!(
            actual.as_deref(),
            expected,
            "{} process environment diverged for {name}",
            self.arm.name()
        );
        println!(
            "FRANKENREDIS_ENV arm={} pid={} name={name} value={:?}",
            self.arm.name(),
            self.pid(),
            actual
        );
    }

    fn executing_elf_sha256(&self) -> String {
        hash_path(&PathBuf::from(format!("/proc/{}/exe", self.child.id())))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.clients.take();
        if matches!(self.child.try_wait(), Ok(None)) {
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", self.port)) {
                let _ = stream.write_all(SHUTDOWN);
            }
            for _ in 0..100 {
                match self.child.try_wait() {
                    Ok(Some(_)) | Err(_) => return,
                    Ok(None) => thread::sleep(Duration::from_millis(10)),
                }
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("read ephemeral port")
        .port()
}

fn connect(port: u16) -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect benchmark client");
    stream
        .set_nodelay(true)
        .expect("set benchmark client TCP_NODELAY");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set benchmark client read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .expect("set benchmark client write timeout");
    stream
}

fn time_block(
    server: &mut Server,
    packets: &Arc<WorkloadPackets>,
    groups: usize,
    odd_first: bool,
) -> Duration {
    server
        .clients
        .as_ref()
        .expect("benchmark clients initialized")
        .run(packets, groups, odd_first)
}

fn exchange_one(server: &mut Server, request: &[u8], expected: &[u8]) {
    let mut stream = connect(server.port);
    stream.write_all(request).expect("write setup request");
    let mut response = vec![0_u8; expected.len()];
    stream
        .read_exact(&mut response)
        .expect("read setup response");
    assert_eq!(
        response,
        expected,
        "setup reply diverged for arm={}",
        server.arm.name()
    );
}

fn xtrim_minid_noop_prefill() -> ExchangeCase {
    let mut request = Vec::new();
    let mut response = Vec::new();

    // Make each prefill idempotent without accepting an arm-dependent DEL
    // reply: SET guarantees that DEL removes exactly one key.
    request.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$2\r\nxs\r\n$4\r\nseed\r\n");
    response.extend_from_slice(SET_REPLY);
    request.extend_from_slice(b"*2\r\n$3\r\nDEL\r\n$2\r\nxs\r\n");
    response.extend_from_slice(b":1\r\n");

    for id in 1..=XTRIM_MINID_NOOP_PREFILL_ENTRIES {
        let stream_id = format!("{id}-0");
        let command = format!(
            "*5\r\n$4\r\nXADD\r\n$2\r\nxs\r\n${}\r\n{stream_id}\r\n\
$1\r\nf\r\n$1\r\nv\r\n",
            stream_id.len()
        );
        request.extend_from_slice(command.as_bytes());
        let reply = format!("${}\r\n{stream_id}\r\n", stream_id.len());
        response.extend_from_slice(reply.as_bytes());
    }
    ExchangeCase { request, response }
}

fn prefill(servers: &mut [Server; 4], workload: Workload) {
    let xtrim_minid_noop =
        matches!(workload, Workload::XtrimMinidNoop).then(xtrim_minid_noop_prefill);
    for server in servers.iter_mut() {
        exchange_one(server, SET, SET_REPLY);
        if matches!(workload, Workload::BitposRange) {
            exchange_one(server, BITPOS_RANGE_PREFILL, SET_REPLY);
        } else if matches!(workload, Workload::BitfieldRoTwoGet) {
            exchange_one(server, BITFIELD_RO_TWO_GET_PREFILL, SET_REPLY);
        } else if matches!(workload, Workload::ObjectEncoding) {
            exchange_one(server, OBJECT_ENCODING_PREFILL, SET_REPLY);
        } else if matches!(workload, Workload::ObjectRefcount) {
            exchange_one(server, OBJECT_REFCOUNT_PREFILL, SET_REPLY);
        } else if let Some(case) = &xtrim_minid_noop {
            exchange_one(server, &case.request, &case.response);
        }
    }
}

fn prefill_and_warm(
    servers: &mut [Server; 4],
    workload: Workload,
    pipeline: usize,
    packets: &Arc<WorkloadPackets>,
) {
    prefill(servers, workload);
    let warm_ops = 20_000_usize;
    let warm_groups = warm_ops.div_ceil(CLIENTS * pipeline).max(8);
    for arm in Arm::ALL {
        time_block(
            &mut servers[arm.index()],
            packets,
            warm_groups,
            matches!(workload, Workload::Mixed),
        );
    }
}

#[derive(Debug)]
struct Sample {
    mio_a_ns: f64,
    mio_b_ns: f64,
    io_uring_ns: f64,
    redis_ns: f64,
    null_ratio: f64,
    self_speedup: f64,
    competitive_speedup: f64,
    mio_a_cpu_ns: u64,
    mio_b_cpu_ns: u64,
    io_uring_cpu_ns: u64,
    redis_cpu_ns: u64,
    io_uring_cpu_util_pct: f64,
    redis_cpu_util_pct: f64,
    cpu_null_ratio: f64,
    cpu_self_speedup: f64,
    cpu_competitive_speedup: f64,
}

fn measure_configuration(
    servers: &mut [Server; 4],
    workload: Workload,
    pipeline: usize,
    client_threads: usize,
    samples: usize,
    ops_per_sample: usize,
) -> Vec<Sample> {
    // The 24 permutations rotate across samples. Within a sample, each arm runs
    // only INTERLEAVE_GROUPS client groups before control passes to the next arm,
    // so host-frequency and queue drift cannot alias onto a multi-second block.
    const ORDERS: [[Arm; 4]; 24] = [
        [Arm::MioA, Arm::MioB, Arm::IoUring, Arm::Redis],
        [Arm::MioA, Arm::MioB, Arm::Redis, Arm::IoUring],
        [Arm::MioA, Arm::IoUring, Arm::MioB, Arm::Redis],
        [Arm::MioA, Arm::IoUring, Arm::Redis, Arm::MioB],
        [Arm::MioA, Arm::Redis, Arm::MioB, Arm::IoUring],
        [Arm::MioA, Arm::Redis, Arm::IoUring, Arm::MioB],
        [Arm::MioB, Arm::MioA, Arm::IoUring, Arm::Redis],
        [Arm::MioB, Arm::MioA, Arm::Redis, Arm::IoUring],
        [Arm::MioB, Arm::IoUring, Arm::MioA, Arm::Redis],
        [Arm::MioB, Arm::IoUring, Arm::Redis, Arm::MioA],
        [Arm::MioB, Arm::Redis, Arm::MioA, Arm::IoUring],
        [Arm::MioB, Arm::Redis, Arm::IoUring, Arm::MioA],
        [Arm::IoUring, Arm::MioA, Arm::MioB, Arm::Redis],
        [Arm::IoUring, Arm::MioA, Arm::Redis, Arm::MioB],
        [Arm::IoUring, Arm::MioB, Arm::MioA, Arm::Redis],
        [Arm::IoUring, Arm::MioB, Arm::Redis, Arm::MioA],
        [Arm::IoUring, Arm::Redis, Arm::MioA, Arm::MioB],
        [Arm::IoUring, Arm::Redis, Arm::MioB, Arm::MioA],
        [Arm::Redis, Arm::MioA, Arm::MioB, Arm::IoUring],
        [Arm::Redis, Arm::MioA, Arm::IoUring, Arm::MioB],
        [Arm::Redis, Arm::MioB, Arm::MioA, Arm::IoUring],
        [Arm::Redis, Arm::MioB, Arm::IoUring, Arm::MioA],
        [Arm::Redis, Arm::IoUring, Arm::MioA, Arm::MioB],
        [Arm::Redis, Arm::IoUring, Arm::MioB, Arm::MioA],
    ];
    assert!(
        samples.is_multiple_of(ORDERS.len()),
        "sample count must contain complete 24-order cycles; got {samples}"
    );

    let packets = Arc::new(WorkloadPackets::new(workload, pipeline));
    prefill_and_warm(servers, workload, pipeline, &packets);
    let groups = ops_per_sample.div_ceil(CLIENTS * pipeline).max(1);
    let actual_ops = groups * CLIENTS * pipeline;
    let mut output = Vec::with_capacity(samples);

    println!(
        "CONFIG workload={} pipeline={pipeline} clients={CLIENTS} client_threads={client_threads} \
samples={samples} \
groups_per_arm_sample={groups} interleave_groups={INTERLEAVE_GROUPS} \
ops_per_arm_sample={actual_ops}",
        workload.name()
    );
    for sample_index in 0..samples {
        // The two mio processes are byte-identical controls, but fixed logical
        // labels let a persistent process-instance bias shift the A/A median.
        // Swap their identities every sample and use an even sample count, so
        // each physical process contributes equally to both sides of the null.
        let swap_controls = sample_index % 2 == 1;
        let mio_a_slot = usize::from(swap_controls);
        let mio_b_slot = usize::from(!swap_controls);
        let mut elapsed = [Duration::ZERO; 4];
        let mut cpu_elapsed = [0_u64; 4];
        let mut groups_done = 0usize;
        let mut interleave_index = 0usize;
        while groups_done < groups {
            let block_groups = (groups - groups_done).min(INTERLEAVE_GROUPS);
            let order = ORDERS[(sample_index + interleave_index) % ORDERS.len()];
            for arm in order {
                let server_slot = match arm {
                    Arm::MioA => mio_a_slot,
                    Arm::MioB => mio_b_slot,
                    Arm::IoUring => Arm::IoUring.index(),
                    Arm::Redis => Arm::Redis.index(),
                };
                let cpu_before = servers[server_slot].cpu_ns();
                let block_elapsed = time_block(
                    &mut servers[server_slot],
                    &packets,
                    block_groups,
                    (groups_done % 2 == 1) ^ (sample_index % 2 == 1),
                );
                let cpu_after = servers[server_slot].cpu_ns();
                elapsed[arm.index()] += block_elapsed;
                cpu_elapsed[arm.index()] += cpu_after - cpu_before;
            }
            groups_done += block_groups;
            interleave_index += 1;
        }
        let mio_a_cpu_ns = cpu_elapsed[Arm::MioA.index()];
        let mio_b_cpu_ns = cpu_elapsed[Arm::MioB.index()];
        let io_uring_cpu_ns = cpu_elapsed[Arm::IoUring.index()];
        let redis_cpu_ns = cpu_elapsed[Arm::Redis.index()];
        assert!(
            mio_a_cpu_ns > 0 && mio_b_cpu_ns > 0 && io_uring_cpu_ns > 0 && redis_cpu_ns > 0,
            "each server arm must accrue CPU time"
        );
        let mio_a_ns = elapsed[Arm::MioA.index()].as_nanos() as f64;
        let mio_b_ns = elapsed[Arm::MioB.index()].as_nanos() as f64;
        let io_uring_ns = elapsed[Arm::IoUring.index()].as_nanos() as f64;
        let redis_ns = elapsed[Arm::Redis.index()].as_nanos() as f64;
        let mio_center_ns = (mio_a_ns * mio_b_ns).sqrt();
        let result = Sample {
            mio_a_ns,
            mio_b_ns,
            io_uring_ns,
            redis_ns,
            null_ratio: mio_a_ns / mio_b_ns,
            self_speedup: mio_center_ns / io_uring_ns,
            competitive_speedup: redis_ns / io_uring_ns,
            mio_a_cpu_ns,
            mio_b_cpu_ns,
            io_uring_cpu_ns,
            redis_cpu_ns,
            io_uring_cpu_util_pct: io_uring_cpu_ns as f64 / io_uring_ns * 100.0,
            redis_cpu_util_pct: redis_cpu_ns as f64 / redis_ns * 100.0,
            cpu_null_ratio: mio_a_cpu_ns as f64 / mio_b_cpu_ns as f64,
            cpu_self_speedup: (mio_a_cpu_ns as f64 * mio_b_cpu_ns as f64).sqrt()
                / io_uring_cpu_ns as f64,
            cpu_competitive_speedup: redis_cpu_ns as f64 / io_uring_cpu_ns as f64,
        };
        println!(
            "SAMPLE workload={} pipeline={pipeline} sample={} order={:?} \
control_slots={} \
control_a_ns_per_op={:.3} control_b_ns_per_op={:.3} candidate_ns_per_op={:.3} \
redis_ns_per_op={:.3} null_control_a_over_b={:.9} \
control_geomean_over_candidate={:.9} candidate_over_redis={:.9} \
control_a_cpu_ns={} control_b_cpu_ns={} candidate_cpu_ns={} redis_cpu_ns={} \
candidate_cpu_util_pct={:.3} redis_cpu_util_pct={:.3} \
cpu_null_control_a_over_b={:.9} cpu_control_geomean_over_candidate={:.9} \
cpu_candidate_over_redis={:.9}",
            workload.name(),
            sample_index + 1,
            ORDERS[sample_index % ORDERS.len()],
            if swap_controls { "BA" } else { "AB" },
            result.mio_a_ns / actual_ops as f64,
            result.mio_b_ns / actual_ops as f64,
            result.io_uring_ns / actual_ops as f64,
            result.redis_ns / actual_ops as f64,
            result.null_ratio,
            result.self_speedup,
            result.competitive_speedup,
            result.mio_a_cpu_ns,
            result.mio_b_cpu_ns,
            result.io_uring_cpu_ns,
            result.redis_cpu_ns,
            result.io_uring_cpu_util_pct,
            result.redis_cpu_util_pct,
            result.cpu_null_ratio,
            result.cpu_self_speedup,
            result.cpu_competitive_speedup,
        );
        output.push(result);
    }
    output
}

fn quantile(samples: &[f64], q: f64) -> f64 {
    assert!(!samples.is_empty(), "quantile requires samples");
    assert!((0.0..=1.0).contains(&q), "quantile must be in [0, 1]");
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let position = q * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        sorted[lower]
    } else {
        let fraction = position - lower as f64;
        sorted[lower] + (sorted[upper] - sorted[lower]) * fraction
    }
}

fn median(samples: &[f64]) -> f64 {
    quantile(samples, 0.5)
}

fn mean_cv_pct(samples: &[f64]) -> f64 {
    assert!(samples.len() >= 2, "CV requires at least two samples");
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / (samples.len() - 1) as f64;
    variance.sqrt() / mean * 100.0
}

fn bootstrap_median_ci(samples: &[f64]) -> (f64, f64) {
    assert!(
        samples.len() >= 8,
        "median CI requires at least eight paired samples"
    );
    const REPLICATES: usize = 20_000;
    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ samples.len() as u64;
    let mut resample = vec![0.0; samples.len()];
    let mut medians = Vec::with_capacity(REPLICATES);
    for _ in 0..REPLICATES {
        for value in &mut resample {
            // Deterministic xorshift64*: reproducible CI, no RNG dependency.
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let draw = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            *value = samples[(draw as usize) % samples.len()];
        }
        medians.push(median(&resample));
    }
    (quantile(&medians, 0.025), quantile(&medians, 0.975))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Keep,
    Reject,
    Hold,
    Invalid,
}

fn adjudicate_ratios(
    metric: &str,
    ratio_name: &str,
    workload: Workload,
    pipeline: usize,
    null: &[f64],
    candidate: &[f64],
) -> Verdict {
    let null_median = median(null);
    let (null_ci_low, null_ci_high) = bootstrap_median_ci(null);
    let candidate_median = median(candidate);
    let (candidate_ci_low, candidate_ci_high) = bootstrap_median_ci(candidate);
    let null_radius = (null_ci_low - 1.0).abs().max((null_ci_high - 1.0).abs());
    let gate_low = 1.0 - 2.0 * null_radius;
    let gate_high = 1.0 + 2.0 * null_radius;
    let null_cv_pct = mean_cv_pct(null);
    let candidate_cv_pct = mean_cv_pct(candidate);
    let invalid = (null_median - 1.0).abs() > 0.02;
    let verdict = if invalid {
        Verdict::Invalid
    } else if candidate_ci_low > gate_high && candidate_median >= 1.01 {
        Verdict::Keep
    } else if candidate_ci_high < gate_low {
        Verdict::Reject
    } else {
        Verdict::Hold
    };
    println!(
        "MEDIAN_CI_GATE metric={metric} workload={} pipeline={pipeline} verdict={verdict:?} \
null_median={null_median:.9} null_ci95=[{null_ci_low:.9},{null_ci_high:.9}] \
null_cv_pct={null_cv_pct:.6} margin2x=[{gate_low:.9},{gate_high:.9}] \
{ratio_name}_median={candidate_median:.9} \
candidate_ci95=[{candidate_ci_low:.9},{candidate_ci_high:.9}] \
candidate_cv_pct={candidate_cv_pct:.6}",
        workload.name()
    );
    assert!(
        !invalid,
        "INVALID A/A: metric={metric} workload={} pipeline={pipeline} \
null median {null_median:.9} exposes position bias or core contamination",
        workload.name()
    );
    verdict
}

fn adjudicate(
    workload: Workload,
    pipeline: usize,
    samples: &[Sample],
) -> (Verdict, Verdict, Verdict, Verdict) {
    let io_uring_util = samples
        .iter()
        .map(|sample| sample.io_uring_cpu_util_pct)
        .collect::<Vec<_>>();
    let redis_util = samples
        .iter()
        .map(|sample| sample.redis_cpu_util_pct)
        .collect::<Vec<_>>();
    let io_uring_util_median = median(&io_uring_util);
    let redis_util_median = median(&redis_util);
    println!(
        "SERVER_SATURATION_GUARD workload={} pipeline={pipeline} \
io_uring_cpu_util_median_pct={io_uring_util_median:.3} \
redis_cpu_util_median_pct={redis_util_median:.3} minimum_pct={MIN_SERVER_UTIL_PCT:.3}",
        workload.name()
    );
    assert!(
        io_uring_util_median >= MIN_SERVER_UTIL_PCT && redis_util_median >= MIN_SERVER_UTIL_PCT,
        "CLIENT-BOUND workload={} pipeline={pipeline}: server utilization \
must reach {MIN_SERVER_UTIL_PCT:.1}% before wall throughput is admissible; \
io_uring={io_uring_util_median:.3}% redis={redis_util_median:.3}%",
        workload.name()
    );
    let wall_null = samples
        .iter()
        .map(|sample| sample.null_ratio)
        .collect::<Vec<_>>();
    let wall_candidate = samples
        .iter()
        .map(|sample| sample.self_speedup)
        .collect::<Vec<_>>();
    let wall_competitive = samples
        .iter()
        .map(|sample| sample.competitive_speedup)
        .collect::<Vec<_>>();
    let cpu_null = samples
        .iter()
        .map(|sample| sample.cpu_null_ratio)
        .collect::<Vec<_>>();
    let cpu_candidate = samples
        .iter()
        .map(|sample| sample.cpu_self_speedup)
        .collect::<Vec<_>>();
    let cpu_competitive = samples
        .iter()
        .map(|sample| sample.cpu_competitive_speedup)
        .collect::<Vec<_>>();
    (
        adjudicate_ratios(
            "wall_ns_per_op",
            "control_geomean_over_candidate",
            workload,
            pipeline,
            &wall_null,
            &wall_candidate,
        ),
        adjudicate_ratios(
            "cpu_ns_per_fixed_work",
            "cpu_control_geomean_over_candidate",
            workload,
            pipeline,
            &cpu_null,
            &cpu_candidate,
        ),
        adjudicate_ratios(
            "wall_ns_per_op",
            "candidate_over_redis",
            workload,
            pipeline,
            &wall_null,
            &wall_competitive,
        ),
        adjudicate_ratios(
            "cpu_ns_per_fixed_work",
            "cpu_candidate_over_redis",
            workload,
            pipeline,
            &cpu_null,
            &cpu_competitive,
        ),
    )
}

fn profile_io_uring_path(
    candidate: &mut Server,
    root: &Path,
    profile_seconds: u64,
    workload: Workload,
    pipeline: usize,
) {
    let data = root.join(format!(
        "io_uring_profile_{}_p{pipeline}.data",
        workload.name()
    ));
    assert!(!data.exists(), "refusing to overwrite {}", data.display());
    let mut perf = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "record",
            "-q",
            "-F",
            "997",
            "-e",
            "cycles",
            "-g",
            "--call-graph",
            "fp",
            "-p",
            &candidate.pid().to_string(),
            "-o",
        ])
        .arg(&data)
        .args(["--", "sleep", &profile_seconds.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn perf record");
    thread::sleep(Duration::from_millis(500));
    if let Some(status) = perf.try_wait().expect("poll perf record") {
        let mut stderr = String::new();
        perf.stderr
            .take()
            .expect("capture early perf stderr")
            .read_to_string(&mut stderr)
            .expect("read early perf stderr");
        panic!("perf record exited before profile workload: status={status} stderr={stderr}");
    }

    let packets = Arc::new(WorkloadPackets::new(workload, pipeline));
    while perf
        .try_wait()
        .expect("poll perf record workload")
        .is_none()
    {
        time_block(candidate, &packets, 32, false);
    }
    let perf_output = perf.wait_with_output().expect("wait for perf record");
    assert!(
        perf_output.status.success(),
        "perf record failed: {}",
        String::from_utf8_lossy(&perf_output.stderr)
    );

    let report = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "report",
            "--stdio",
            "--no-children",
            "--percent-limit",
            "0",
            "--call-graph",
            "none",
            "--sort",
            "overhead,symbol,dso",
            "-i",
        ])
        .arg(&data)
        .output()
        .expect("run perf report");
    assert!(
        report.status.success(),
        "perf report failed: {}",
        String::from_utf8_lossy(&report.stderr)
    );
    let report = String::from_utf8(report.stdout).expect("perf report is UTF-8");
    let lost = report
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .expect("perf report states lost-sample count");
    assert!(
        lost.trim_end().ends_with(" 0"),
        "profile lost samples: {lost}"
    );

    let owned_targets = ["BatchWriter>::submit_owned", "BatchWriter>::drain_owned"];
    let surface_targets = [
        owned_targets[0],
        owned_targets[1],
        "frankenredis::submit_uring_batch",
        "frankenredis::drain_uring_completions",
        "io_uring::submit::Submitter>::submit_and_wait",
        "io_uring_enter",
    ];
    let mut matched = Vec::new();
    let mut owned_self_pct = 0.0_f64;
    let mut surface_self_pct = 0.0_f64;
    for line in report.lines() {
        if surface_targets.iter().any(|target| line.contains(target))
            && let Some(raw_pct) = line.split_whitespace().next()
            && let Ok(pct) = raw_pct.trim_end_matches('%').parse::<f64>()
        {
            surface_self_pct += pct;
            if owned_targets.iter().any(|target| line.contains(target)) {
                owned_self_pct += pct;
            }
            matched.push(line.trim().to_owned());
        }
    }
    assert!(
        owned_self_pct > 0.0,
        "profile did not attribute non-zero self-time to owned submit/CQ drain"
    );
    assert!(
        surface_self_pct < 100.0,
        "invalid aggregate io_uring self-time: {surface_self_pct}%"
    );
    let amdahl_ceiling = 1.0 / (1.0 - surface_self_pct / 100.0);
    println!(
        "PROFILE_REACHABILITY target=async_owned_io_uring_output \
workload={} pipeline={pipeline} \
owned_self_pct={owned_self_pct:.4} surface_self_pct={surface_self_pct:.4} \
amdahl_elimination_ceiling={amdahl_ceiling:.6}x lost_samples=0 rows={matched:?}",
        workload.name()
    );

    let command_targets = workload.profile_targets();
    if !command_targets.is_empty() {
        let mut command_rows = Vec::new();
        let mut command_self_pct = 0.0_f64;
        for line in report.lines() {
            if command_targets.iter().any(|target| line.contains(target))
                && let Some(raw_pct) = line.split_whitespace().next()
                && let Ok(pct) = raw_pct.trim_end_matches('%').parse::<f64>()
            {
                command_self_pct += pct;
                command_rows.push(line.trim().to_owned());
            }
        }
        assert!(
            command_self_pct > 0.0,
            "profile did not attribute non-zero self-time to workload={} targets={command_targets:?}",
            workload.name()
        );
        assert!(
            command_self_pct < 100.0,
            "invalid aggregate command self-time: {command_self_pct}%"
        );
        let command_amdahl_ceiling = 1.0 / (1.0 - command_self_pct / 100.0);
        println!(
            "PROFILE_COMMAND_SURFACE workload={} pipeline={pipeline} \
targets={command_targets:?} self_pct={command_self_pct:.4} \
amdahl_elimination_ceiling={command_amdahl_ceiling:.6}x \
lost_samples=0 rows={command_rows:?}",
            workload.name()
        );
    }
}

fn parse_cpu_list(text: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for item in text.trim().split(',') {
        if let Some((start, end)) = item.split_once('-') {
            let start = start.parse::<usize>().expect("parse CPU range start");
            let end = end.parse::<usize>().expect("parse CPU range end");
            cpus.extend(start..=end);
        } else if !item.is_empty() {
            cpus.push(item.parse::<usize>().expect("parse CPU number"));
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    cpus
}

fn allowed_cpus() -> Vec<usize> {
    let status = fs::read_to_string("/proc/self/status").expect("read process CPU allowance");
    let allowed = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .map(str::trim)
        .expect("Cpus_allowed_list is present");
    parse_cpu_list(allowed)
}

fn sibling_group(cpu: usize) -> Vec<usize> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    fs::read_to_string(path)
        .map(|text| parse_cpu_list(&text))
        .unwrap_or_else(|_| vec![cpu])
}

fn read_core_ticks() -> HashMap<usize, (u64, u64)> {
    let stat = fs::read_to_string("/proc/stat").expect("read per-core CPU counters");
    stat.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let label = fields.next()?;
            let cpu = label.strip_prefix("cpu")?.parse::<usize>().ok()?;
            let ticks = fields
                .take(8)
                .map(|value| value.parse::<u64>().expect("parse /proc/stat CPU tick"))
                .collect::<Vec<_>>();
            assert!(
                ticks.len() >= 4,
                "per-core /proc/stat row has fewer than four counters: {line:?}"
            );
            let idle = ticks[3] + ticks.get(4).copied().unwrap_or(0);
            Some((cpu, (ticks.iter().sum(), idle)))
        })
        .collect()
}

fn observed_core_loads() -> HashMap<usize, f64> {
    let before = read_core_ticks();
    thread::sleep(Duration::from_millis(500));
    let after = read_core_ticks();
    after
        .into_iter()
        .filter_map(|(cpu, (total_after, idle_after))| {
            let (total_before, idle_before) = before.get(&cpu).copied()?;
            let total = total_after.saturating_sub(total_before);
            let idle = idle_after.saturating_sub(idle_before).min(total);
            (total != 0).then_some((cpu, 100.0 * (total - idle) as f64 / total as f64))
        })
        .collect()
}

fn choose_and_pin_cores(client_threads: usize) -> (Vec<usize>, usize) {
    let allowed = allowed_cpus();
    for attempt in 1..=QUIET_CORE_PREFLIGHT_ATTEMPTS {
        let loads = observed_core_loads();
        let quiet = allowed
            .iter()
            .copied()
            .filter(|cpu| {
                sibling_group(*cpu)
                    .iter()
                    .all(|sibling| loads.get(sibling).copied().unwrap_or(0.0) < QUIET_CORE_MAX_PCT)
            })
            .collect::<Vec<_>>();
        let mut claimed_siblings = HashSet::new();
        let mut client_cores = Vec::with_capacity(client_threads);
        for cpu in &quiet {
            let siblings = sibling_group(*cpu);
            if siblings
                .iter()
                .all(|sibling| !claimed_siblings.contains(sibling))
            {
                client_cores.push(*cpu);
                claimed_siblings.extend(siblings);
            }
            if client_cores.len() == client_threads {
                break;
            }
        }
        let server_core = (client_cores.len() == client_threads)
            .then(|| {
                quiet.iter().rev().copied().find(|cpu| {
                    sibling_group(*cpu)
                        .iter()
                        .all(|sibling| !claimed_siblings.contains(sibling))
                })
            })
            .flatten();
        let Some(server_core) = server_core else {
            if attempt == QUIET_CORE_PREFLIGHT_ATTEMPTS {
                panic!(
                    "worker did not expose {client_threads} quiet physical client CPUs plus a \
disjoint quiet server CPU after {attempt} attempts: allowed={allowed:?} loads={loads:?} \
quiet={quiet:?} client_cores={client_cores:?} claimed_siblings={claimed_siblings:?}"
                );
            }
            println!(
                "CPU_PREFLIGHT_RETRY attempt={attempt}/{QUIET_CORE_PREFLIGHT_ATTEMPTS} \
allowed={allowed:?} loads={loads:?} quiet={quiet:?} \
client_cores={client_cores:?} claimed_siblings={claimed_siblings:?}"
            );
            continue;
        };
        let client_mask = client_cores
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let pin = Command::new("taskset")
            .args(["-apc", &client_mask, &std::process::id().to_string()])
            .output()
            .expect("pin benchmark client process");
        assert!(
            pin.status.success(),
            "client taskset failed: {}",
            String::from_utf8_lossy(&pin.stderr)
        );
        println!(
            "CPU_PREFLIGHT attempts={attempt} client_cores={client_cores:?} \
client_siblings={claimed_siblings:?} \
server={server_core} server_siblings={:?} allowed={allowed:?} loads={loads:?}",
            sibling_group(server_core)
        );
        return (client_cores, server_core);
    }
    unreachable!("quiet-core preflight loop returns or panics");
}

fn unique_root() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "fr_io_uring_submission_ab_{}_{stamp}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("create unique A/B root");
    root
}

fn hash_path(path: &Path) -> String {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .expect("run sha256sum");
    assert!(
        output.status.success(),
        "sha256sum failed for {}",
        path.display()
    );
    String::from_utf8(output.stdout)
        .expect("sha256sum output is UTF-8")
        .split_whitespace()
        .next()
        .expect("sha256sum emitted digest")
        .to_owned()
}

fn parse_usize_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .map(|value| {
            value
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid {name}"))
        })
        .unwrap_or(default)
}

fn parse_u64_env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .map(|value| {
            value
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("invalid {name}"))
        })
        .unwrap_or(default)
}

fn parse_bool_env(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) if value == "1" => true,
        Ok(value) if value == "0" => false,
        Ok(value) => panic!("{name} must be 0 or 1, got {value:?}"),
        Err(std::env::VarError::NotPresent) => false,
        Err(error) => panic!("invalid {name}: {error}"),
    }
}

fn parse_pipelines() -> Vec<usize> {
    if std::env::var_os("FR_URING_AB_P1_ONLY").is_some() {
        return vec![1];
    }
    let value = std::env::var("FR_URING_AB_PIPELINES").unwrap_or_else(|_| "1".to_owned());
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid pipeline depth: {item}"))
        })
        .collect()
}

fn command_output(command: &str, args: &[&str]) -> Output {
    Command::new(command)
        .args(args)
        .output()
        .unwrap_or_else(|_| panic!("run {command}"))
}

#[test]
#[ignore = "strict-remote pinned-worker performance gate; run explicitly"]
fn io_uring_submission_same_elf_null_then_ab() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_frankenredis"));
    let redis_binary = std::env::var_os("FR_URING_REDIS_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../legacy_redis_code/redis/src/redis-server")
        });
    assert!(
        redis_binary.is_file(),
        "vendored Redis executable is missing: {}",
        redis_binary.display()
    );
    let harness = std::env::current_exe().expect("locate running harness ELF");
    // First benchmark-authored line: the executing harness identifies its own
    // ELF before any child process is started.
    println!(
        "HARNESS_ELF_SELF_REPORT sha256={} arms=mio_a,mio_b,io_uring,redis",
        hash_path(&harness)
    );
    let redis_version = command_output(
        redis_binary
            .to_str()
            .expect("vendored Redis executable path must be UTF-8"),
        &["--version"],
    );
    assert!(
        redis_version.status.success(),
        "vendored Redis --version must succeed"
    );
    let redis_version = String::from_utf8(redis_version.stdout).expect("Redis version is UTF-8");
    assert!(
        redis_version.contains("v=7.2.4"),
        "expected vendored Redis 7.2.4, got {redis_version:?}"
    );
    println!("INCUMBENT_VERSION {}", redis_version.trim());

    let hostname = command_output("hostname", &[]);
    assert!(hostname.status.success(), "hostname failed");
    let hostname = String::from_utf8_lossy(&hostname.stdout).trim().to_owned();
    let kernel = command_output("uname", &["-r"]);
    assert!(kernel.status.success(), "uname failed");
    let kernel = String::from_utf8_lossy(&kernel.stdout).trim().to_owned();
    let disabled = fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
        .unwrap_or_else(|_| "unknown".into());
    println!(
        "WORKER_ID host={hostname} kernel={kernel} io_uring_disabled={}",
        disabled.trim()
    );
    println!(
        "DECISION_CONTRACT same_invocation_aa=true live_redis_arm=true \
bootstrap_median_ci_gate=true cv_provenance_only=true never_cv_gate=true"
    );

    let samples = parse_usize_env("FR_URING_AB_SAMPLES", DEFAULT_SAMPLES);
    assert!(samples >= 8, "median CI requires at least eight samples");
    let client_threads = parse_usize_env("FR_URING_AB_CLIENT_THREADS", DEFAULT_CLIENT_THREADS);
    assert!(
        (1..=CLIENTS).contains(&client_threads),
        "FR_URING_AB_CLIENT_THREADS must be in 1..={CLIENTS}"
    );
    let ops_per_sample = parse_usize_env("FR_URING_AB_OPS_PER_SAMPLE", DEFAULT_OPS_PER_SAMPLE);
    let profile_seconds = parse_u64_env("FR_URING_AB_PROFILE_SECONDS", DEFAULT_PROFILE_SECONDS);
    let workloads = Workload::parse_list();
    let pipelines = parse_pipelines();
    let bitpos_range_floor_ab = parse_bool_env("FR_URING_AB_BITPOS_RANGE_FLOOR");
    let bitfield_ro_two_get_floor_ab = parse_bool_env("FR_URING_AB_BITFIELD_RO_TWO_GET_FLOOR");
    let object_encoding_floor_ab = parse_bool_env("FR_URING_AB_OBJECT_ENCODING_FLOOR");
    let object_refcount_floor_ab = parse_bool_env("FR_URING_AB_OBJECT_REFCOUNT_FLOOR");
    let dbsize_floor_ab = parse_bool_env("FR_URING_AB_DBSIZE_FLOOR");
    let echo_floor_ab = parse_bool_env("FR_URING_AB_ECHO_FLOOR");
    let wait_zero_floor_ab = parse_bool_env("FR_URING_AB_WAIT_ZERO_FLOOR");
    let xtrim_minid_noop_ab = parse_bool_env("FR_URING_AB_XTRIM_MINID_NOOP");
    let xtrim_minid_noop_floor_ab = parse_bool_env("FR_URING_AB_XTRIM_MINID_NOOP_FLOOR");
    assert!(!workloads.is_empty(), "at least one workload is required");
    assert!(!pipelines.is_empty(), "at least one pipeline is required");
    #[cfg(not(feature = "perf-ab-bitpos-range-floor"))]
    assert!(
        !bitpos_range_floor_ab,
        "FR_URING_AB_BITPOS_RANGE_FLOOR=1 requires \
--features perf-ab-bitpos-range-floor"
    );
    #[cfg(not(feature = "perf-ab-bitfield-ro-two-get-floor"))]
    assert!(
        !bitfield_ro_two_get_floor_ab,
        "FR_URING_AB_BITFIELD_RO_TWO_GET_FLOOR=1 requires \
--features perf-ab-bitfield-ro-two-get-floor"
    );
    #[cfg(not(feature = "perf-ab-object-encoding-floor"))]
    assert!(
        !object_encoding_floor_ab,
        "FR_URING_AB_OBJECT_ENCODING_FLOOR=1 requires \
--features perf-ab-object-encoding-floor"
    );
    #[cfg(not(feature = "perf-ab-object-refcount-floor"))]
    assert!(
        !object_refcount_floor_ab,
        "FR_URING_AB_OBJECT_REFCOUNT_FLOOR=1 requires \
--features perf-ab-object-refcount-floor"
    );
    #[cfg(not(feature = "perf-ab-dbsize-floor"))]
    assert!(
        !dbsize_floor_ab,
        "FR_URING_AB_DBSIZE_FLOOR=1 requires --features perf-ab-dbsize-floor"
    );
    #[cfg(not(feature = "perf-ab-echo-floor"))]
    assert!(
        !echo_floor_ab,
        "FR_URING_AB_ECHO_FLOOR=1 requires --features perf-ab-echo-floor"
    );
    #[cfg(not(feature = "perf-ab-wait-zero-floor"))]
    assert!(
        !wait_zero_floor_ab,
        "FR_URING_AB_WAIT_ZERO_FLOOR=1 requires --features perf-ab-wait-zero-floor"
    );
    #[cfg(not(feature = "perf-ab-xtrim-minid-noop"))]
    assert!(
        !xtrim_minid_noop_ab,
        "FR_URING_AB_XTRIM_MINID_NOOP=1 requires --features perf-ab-xtrim-minid-noop"
    );
    #[cfg(not(feature = "perf-ab-xtrim-minid-noop-floor"))]
    assert!(
        !xtrim_minid_noop_floor_ab,
        "FR_URING_AB_XTRIM_MINID_NOOP_FLOOR=1 requires \
--features perf-ab-xtrim-minid-noop-floor"
    );
    assert!(
        usize::from(bitpos_range_floor_ab)
            + usize::from(bitfield_ro_two_get_floor_ab)
            + usize::from(object_encoding_floor_ab)
            + usize::from(object_refcount_floor_ab)
            + usize::from(dbsize_floor_ab)
            + usize::from(echo_floor_ab)
            + usize::from(wait_zero_floor_ab)
            + usize::from(xtrim_minid_noop_ab)
            + usize::from(xtrim_minid_noop_floor_ab)
            <= 1,
        "run only one command-shape floor experiment per invocation"
    );
    let command_floor_ab = if bitpos_range_floor_ab {
        CommandFloorAb::BitposRange
    } else if bitfield_ro_two_get_floor_ab {
        CommandFloorAb::BitfieldRoTwoGet
    } else if object_encoding_floor_ab {
        CommandFloorAb::ObjectEncoding
    } else if object_refcount_floor_ab {
        CommandFloorAb::ObjectRefcount
    } else if dbsize_floor_ab {
        CommandFloorAb::Dbsize
    } else if echo_floor_ab {
        CommandFloorAb::Echo
    } else if wait_zero_floor_ab {
        CommandFloorAb::WaitZero
    } else if xtrim_minid_noop_ab {
        CommandFloorAb::XtrimMinidNoop
    } else if xtrim_minid_noop_floor_ab {
        CommandFloorAb::XtrimMinidNoopFloor
    } else {
        CommandFloorAb::None
    };
    if bitpos_range_floor_ab {
        assert_eq!(
            workloads,
            [Workload::BitposRange],
            "the BITPOS range floor A/B must isolate the exact profiled workload"
        );
    }
    if bitfield_ro_two_get_floor_ab {
        assert_eq!(
            workloads,
            [Workload::BitfieldRoTwoGet],
            "the BITFIELD_RO two-GET floor A/B must isolate the exact profiled workload"
        );
    }
    if object_encoding_floor_ab {
        assert_eq!(
            workloads,
            [Workload::ObjectEncoding],
            "the OBJECT ENCODING floor A/B must isolate the exact profiled workload"
        );
    }
    if object_refcount_floor_ab {
        assert_eq!(
            workloads,
            [Workload::ObjectRefcount],
            "the OBJECT REFCOUNT floor A/B must isolate the exact profiled workload"
        );
    }
    if dbsize_floor_ab {
        assert_eq!(
            workloads,
            [Workload::Dbsize],
            "the DBSIZE floor A/B must isolate the exact profiled workload"
        );
    }
    if echo_floor_ab {
        assert_eq!(
            workloads,
            [Workload::Echo],
            "the ECHO floor A/B must isolate the exact profiled workload"
        );
    }
    if wait_zero_floor_ab {
        assert_eq!(
            workloads,
            [Workload::WaitZero],
            "the WAIT 0 0 floor A/B must isolate the exact profiled workload"
        );
    }
    if xtrim_minid_noop_ab {
        assert_eq!(
            workloads,
            [Workload::XtrimMinidNoop],
            "the XTRIM MINID no-op A/B must isolate the exact profiled workload"
        );
    }
    if xtrim_minid_noop_floor_ab {
        assert_eq!(
            workloads,
            [Workload::XtrimMinidNoop],
            "the XTRIM MINID no-op floor A/B must isolate the exact profiled workload"
        );
    }

    let perf_version = command_output("perf", &["--version"]);
    assert!(
        perf_version.status.success(),
        "worker must provide perf for profile attribution"
    );
    let (_client_cores, server_core) = choose_and_pin_cores(client_threads);
    let root = unique_root();
    println!("ARTIFACT_ROOT {}", root.display());

    let mut servers = [
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::MioA,
            &root,
            server_core,
            client_threads,
            command_floor_ab,
        ),
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::MioB,
            &root,
            server_core,
            client_threads,
            command_floor_ab,
        ),
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::IoUring,
            &root,
            server_core,
            client_threads,
            command_floor_ab,
        ),
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::Redis,
            &root,
            server_core,
            client_threads,
            command_floor_ab,
        ),
    ];
    let server_hashes = Arm::ALL.map(|arm| servers[arm.index()].executing_elf_sha256());
    assert_eq!(
        server_hashes[Arm::MioA.index()],
        server_hashes[Arm::MioB.index()],
        "A/A controls must execute the same ELF"
    );
    assert_eq!(
        server_hashes[Arm::MioA.index()],
        server_hashes[Arm::IoUring.index()],
        "control and candidate must execute the same FrankenRedis ELF"
    );
    for arm in Arm::ALL {
        println!(
            "SERVER_ELF_SELF_REPORT arm={} pid={} sha256={}",
            arm.name(),
            servers[arm.index()].child.id(),
            server_hashes[arm.index()]
        );
    }
    if bitpos_range_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_bitpos_range_floor \
control_b=io_uring+frozen_pre_bitpos_range_floor \
candidate=io_uring+bitpos_range_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(BITPOS_RANGE_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(BITPOS_RANGE_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(BITPOS_RANGE_FLOOR_CONTROL_ENV, None);
    } else if bitfield_ro_two_get_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_bitfield_ro_two_get_floor \
control_b=io_uring+frozen_pre_bitfield_ro_two_get_floor \
candidate=io_uring+bitfield_ro_two_get_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(BITFIELD_RO_TWO_GET_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(BITFIELD_RO_TWO_GET_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(BITFIELD_RO_TWO_GET_FLOOR_CONTROL_ENV, None);
    } else if object_encoding_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_object_encoding_floor \
control_b=io_uring+frozen_pre_object_encoding_floor \
candidate=io_uring+object_encoding_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(OBJECT_ENCODING_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(OBJECT_ENCODING_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(OBJECT_ENCODING_FLOOR_CONTROL_ENV, None);
    } else if object_refcount_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_object_refcount_floor \
control_b=io_uring+frozen_pre_object_refcount_floor \
candidate=io_uring+object_refcount_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(OBJECT_REFCOUNT_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(OBJECT_REFCOUNT_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(OBJECT_REFCOUNT_FLOOR_CONTROL_ENV, None);
    } else if dbsize_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_dbsize_floor \
control_b=io_uring+frozen_pre_dbsize_floor \
candidate=io_uring+dbsize_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()].assert_environment_value(DBSIZE_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()].assert_environment_value(DBSIZE_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()].assert_environment_value(DBSIZE_FLOOR_CONTROL_ENV, None);
    } else if echo_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_echo_floor \
control_b=io_uring+frozen_pre_echo_floor \
candidate=io_uring+echo_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()].assert_environment_value(ECHO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()].assert_environment_value(ECHO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()].assert_environment_value(ECHO_FLOOR_CONTROL_ENV, None);
    } else if wait_zero_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_wait_zero_floor \
control_b=io_uring+frozen_pre_wait_zero_floor \
candidate=io_uring+wait_zero_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()].assert_environment_value(WAIT_ZERO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()].assert_environment_value(WAIT_ZERO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()].assert_environment_value(WAIT_ZERO_FLOOR_CONTROL_ENV, None);
    } else if xtrim_minid_noop_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_xtrim_minid_noop_guard \
control_b=io_uring+frozen_pre_xtrim_minid_noop_guard \
candidate=io_uring+xtrim_minid_noop_guard incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(XTRIM_MINID_NOOP_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(XTRIM_MINID_NOOP_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()].assert_environment_value(XTRIM_MINID_NOOP_CONTROL_ENV, None);
    } else if xtrim_minid_noop_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+guarded_generic_xtrim_minid_noop \
control_b=io_uring+guarded_generic_xtrim_minid_noop \
candidate=io_uring+xtrim_minid_noop_dispatch_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(XTRIM_MINID_NOOP_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(XTRIM_MINID_NOOP_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(XTRIM_MINID_NOOP_FLOOR_CONTROL_ENV, None);
    } else {
        println!(
            "ARM_SEMANTICS control_a=mio control_b=mio \
candidate=io_uring incumbent=vendored_redis_7.2.4"
        );
        servers[Arm::IoUring.index()].assert_flag_reached_process();
    }

    let mut verdicts = Vec::new();
    for workload in workloads {
        for pipeline in &pipelines {
            // Read-only/profiled workloads may require seeded server state.
            // Prime every arm before profiling, then measure_configuration
            // re-primes all four arms immediately before its warmup.
            prefill(&mut servers, workload);
            profile_io_uring_path(
                &mut servers[Arm::IoUring.index()],
                &root,
                profile_seconds,
                workload,
                *pipeline,
            );
            let measured = measure_configuration(
                &mut servers,
                workload,
                *pipeline,
                client_threads,
                samples,
                ops_per_sample,
            );
            verdicts.push((
                workload.name(),
                *pipeline,
                adjudicate(workload, *pipeline, &measured),
            ));
        }
    }

    let final_loads = observed_core_loads();
    println!("FINAL_CORE_LOAD_SNAPSHOT {final_loads:?}");
    println!("VERDICT_MATRIX {verdicts:?}");
}
