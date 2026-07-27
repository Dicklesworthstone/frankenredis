#![forbid(unsafe_code)]

//! Same-invocation A/A + A/B + live-incumbent gate for flagged io_uring output.
//!
//! The harness deliberately drives many established connections as a group:
//! every client writes before any client reads. At pipeline depth 1 this gives
//! the event loop a cross-connection submission batch. Two byte-identical mio
//! processes provide the null control, the same FrankenRedis ELF with the
//! runtime flag is the candidate, and vendored Redis is the live incumbent.
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
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLIENTS: usize = 50;
const DEFAULT_SAMPLES: usize = 32;
const DEFAULT_OPS_PER_SAMPLE: usize = 200_000;
const DEFAULT_PROFILE_SECONDS: u64 = 3;
const INTERLEAVE_GROUPS: usize = 25;
const QUIET_CORE_MAX_PCT: f64 = 5.0;
const IO_URING_FLAG: &str = "--io-uring-output";
const SHUTDOWN: &[u8] = b"*2\r\n$8\r\nSHUTDOWN\r\n$6\r\nNOSAVE\r\n";
const SET: &[u8] = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n";
const SET_REPLY: &[u8] = b"+OK\r\n";
const GET: &[u8] = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n";
const GET_REPLY: &[u8] = b"$1\r\nv\r\n";

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

#[derive(Clone, Copy, Debug)]
enum Workload {
    Set,
    Get,
    Mixed,
}

impl Workload {
    const fn name(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Get => "get",
            Self::Mixed => "mixed",
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

struct Server {
    arm: Arm,
    child: Child,
    port: u16,
    clients: Vec<TcpStream>,
    stderr_path: PathBuf,
}

impl Server {
    fn spawn(
        fr_binary: &Path,
        redis_binary: &Path,
        arm: Arm,
        root: &Path,
        server_core: usize,
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
        if matches!(arm, Arm::IoUring) {
            command.arg(
                std::env::var("FR_URING_AB_FLAG").unwrap_or_else(|_| IO_URING_FLAG.to_owned()),
            );
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
            clients: Vec::new(),
            stderr_path,
        };
        server.wait_until_ready();
        server.clients = (0..CLIENTS).map(|_| connect(port)).collect();
        server
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn cpu_ticks(&self) -> u64 {
        let stat = fs::read_to_string(format!("/proc/{}/stat", self.pid()))
            .expect("read server process stat");
        let close_paren = stat
            .rfind(')')
            .expect("server process stat contains command terminator");
        // Fields after the command start at proc(5) field 3 (`state`), so
        // indices 11 and 12 are fields 14/15: user and system CPU ticks.
        let fields = stat[close_paren + 1..]
            .split_whitespace()
            .collect::<Vec<_>>();
        assert!(fields.len() > 12, "server process stat has CPU fields");
        let user = fields[11].parse::<u64>().expect("parse server user ticks");
        let system = fields[12]
            .parse::<u64>()
            .expect("parse server system ticks");
        user + system
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
        if !matches!(self.arm, Arm::IoUring) {
            return;
        }
        let cmdline = fs::read(format!("/proc/{}/cmdline", self.pid()))
            .expect("read candidate process command line");
        let flag = std::env::var("FR_URING_AB_FLAG").unwrap_or_else(|_| IO_URING_FLAG.to_owned());
        assert!(
            cmdline
                .split(|byte| *byte == 0)
                .any(|arg| arg == flag.as_bytes()),
            "candidate process did not receive {flag}"
        );
        println!("CANDIDATE_FLAG pid={} flag={flag}", self.pid());
    }

    fn executing_elf_sha256(&self) -> String {
        hash_path(&PathBuf::from(format!("/proc/{}/exe", self.child.id())))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.clients.clear();
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

fn exchange_group(server: &mut Server, case: &ExchangeCase) {
    let request = black_box(case.request.as_slice());
    for client in &mut server.clients {
        client.write_all(request).expect("write request group");
    }
    let mut response = vec![0_u8; case.response.len()];
    for client in &mut server.clients {
        client
            .read_exact(&mut response)
            .expect("read complete response group");
        assert_eq!(
            response,
            case.response,
            "{} returned bytes that diverge from the RESP oracle",
            server.arm.name()
        );
        black_box(response.as_slice());
    }
}

fn time_block(
    server: &mut Server,
    packets: &WorkloadPackets,
    groups: usize,
    odd_first: bool,
) -> Duration {
    let start = Instant::now();
    for group in 0..groups {
        let odd = (group % 2 == 1) ^ odd_first;
        exchange_group(server, if odd { &packets.odd } else { &packets.even });
    }
    start.elapsed()
}

fn exchange_one(server: &mut Server, request: &[u8], expected: &[u8]) {
    let mut stream = connect(server.port);
    stream.write_all(request).expect("write setup request");
    let mut response = vec![0_u8; expected.len()];
    stream
        .read_exact(&mut response)
        .expect("read setup response");
    assert_eq!(response, expected);
}

fn prefill_and_warm(
    servers: &mut [Server; 4],
    workload: Workload,
    pipeline: usize,
    packets: &WorkloadPackets,
) {
    for server in servers.iter_mut() {
        exchange_one(server, SET, SET_REPLY);
    }
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
    mio_a_cpu_ticks: u64,
    mio_b_cpu_ticks: u64,
    io_uring_cpu_ticks: u64,
    redis_cpu_ticks: u64,
    cpu_null_ratio: f64,
    cpu_self_speedup: f64,
    cpu_competitive_speedup: f64,
}

fn measure_configuration(
    servers: &mut [Server; 4],
    workload: Workload,
    pipeline: usize,
    samples: usize,
    ops_per_sample: usize,
) -> Vec<Sample> {
    // Every permutation appears inside every measured sample. Each arm runs only
    // INTERLEAVE_GROUPS client groups before control passes to the next arm, so
    // host-frequency and queue drift cannot alias onto a multi-second arm block.
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

    let packets = WorkloadPackets::new(workload, pipeline);
    prefill_and_warm(servers, workload, pipeline, &packets);
    let groups = ops_per_sample.div_ceil(CLIENTS * pipeline).max(1);
    let actual_ops = groups * CLIENTS * pipeline;
    let mut output = Vec::with_capacity(samples);

    println!(
        "CONFIG workload={} pipeline={pipeline} clients={CLIENTS} samples={samples} \
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
        let cpu_before = std::array::from_fn::<_, 4, _>(|index| servers[index].cpu_ticks());
        let mut elapsed = [Duration::ZERO; 4];
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
                elapsed[arm.index()] += time_block(
                    &mut servers[server_slot],
                    &packets,
                    block_groups,
                    (groups_done % 2 == 1) ^ (sample_index % 2 == 1),
                );
            }
            groups_done += block_groups;
            interleave_index += 1;
        }
        let cpu_after = std::array::from_fn::<_, 4, _>(|index| servers[index].cpu_ticks());
        let cpu_delta =
            std::array::from_fn::<_, 4, _>(|index| cpu_after[index] - cpu_before[index]);
        let mio_a_cpu_ticks = cpu_delta[mio_a_slot];
        let mio_b_cpu_ticks = cpu_delta[mio_b_slot];
        let io_uring_cpu_ticks = cpu_delta[Arm::IoUring.index()];
        let redis_cpu_ticks = cpu_delta[Arm::Redis.index()];
        assert!(
            mio_a_cpu_ticks > 0
                && mio_b_cpu_ticks > 0
                && io_uring_cpu_ticks > 0
                && redis_cpu_ticks > 0,
            "each server arm must accrue CPU ticks"
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
            mio_a_cpu_ticks,
            mio_b_cpu_ticks,
            io_uring_cpu_ticks,
            redis_cpu_ticks,
            cpu_null_ratio: mio_a_cpu_ticks as f64 / mio_b_cpu_ticks as f64,
            cpu_self_speedup: (mio_a_cpu_ticks as f64 * mio_b_cpu_ticks as f64).sqrt()
                / io_uring_cpu_ticks as f64,
            cpu_competitive_speedup: redis_cpu_ticks as f64 / io_uring_cpu_ticks as f64,
        };
        println!(
            "SAMPLE workload={} pipeline={pipeline} sample={} order={:?} \
control_slots={} \
mio_a_ns_per_op={:.3} mio_b_ns_per_op={:.3} io_uring_ns_per_op={:.3} redis_ns_per_op={:.3} \
null_a_over_b={:.9} mio_over_io_uring={:.9} fr_io_uring_over_redis={:.9} \
mio_a_cpu_ticks={} mio_b_cpu_ticks={} io_uring_cpu_ticks={} redis_cpu_ticks={} \
cpu_null_a_over_b={:.9} cpu_mio_over_io_uring={:.9} \
cpu_fr_io_uring_over_redis={:.9}",
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
            result.mio_a_cpu_ticks,
            result.mio_b_cpu_ticks,
            result.io_uring_cpu_ticks,
            result.redis_cpu_ticks,
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
            "mio_over_io_uring",
            workload,
            pipeline,
            &wall_null,
            &wall_candidate,
        ),
        adjudicate_ratios(
            "cpu_ticks_per_fixed_work",
            "cpu_mio_over_io_uring",
            workload,
            pipeline,
            &cpu_null,
            &cpu_candidate,
        ),
        adjudicate_ratios(
            "wall_ns_per_op",
            "fr_io_uring_over_redis",
            workload,
            pipeline,
            &wall_null,
            &wall_competitive,
        ),
        adjudicate_ratios(
            "cpu_ticks_per_fixed_work",
            "cpu_fr_io_uring_over_redis",
            workload,
            pipeline,
            &cpu_null,
            &cpu_competitive,
        ),
    )
}

fn profile_io_uring_path(candidate: &mut Server, root: &Path, profile_seconds: u64) {
    let data = root.join("io_uring_profile.data");
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
    assert!(
        perf.try_wait().expect("poll perf record").is_none(),
        "perf record exited before profile workload"
    );

    let packets = WorkloadPackets::new(Workload::Set, 1);
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
owned_self_pct={owned_self_pct:.4} surface_self_pct={surface_self_pct:.4} \
amdahl_elimination_ceiling={amdahl_ceiling:.6}x lost_samples=0 rows={matched:?}"
    );
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

fn observed_core_loads() -> HashMap<usize, f64> {
    let output = Command::new("ps")
        .args(["-eLo", "psr=,pcpu=,pid=,comm="])
        .output()
        .expect("inspect per-thread CPU placement");
    assert!(output.status.success(), "ps CPU preflight failed");
    let mut loads = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let Some(cpu) = fields.next().and_then(|value| value.parse::<usize>().ok()) else {
            continue;
        };
        let Some(pct) = fields.next().and_then(|value| value.parse::<f64>().ok()) else {
            continue;
        };
        let Some(pid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        if pid != std::process::id() {
            *loads.entry(cpu).or_insert(0.0) += pct;
        }
    }
    loads
}

fn choose_and_pin_cores() -> (usize, usize) {
    let allowed = allowed_cpus();
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
    assert!(
        quiet.len() >= 2,
        "worker has fewer than two quiet allowed CPUs: allowed={allowed:?} loads={loads:?}"
    );

    let client_core = quiet[0];
    let client_siblings = sibling_group(client_core)
        .into_iter()
        .collect::<HashSet<_>>();
    let server_core = quiet
        .iter()
        .rev()
        .copied()
        .find(|cpu| !client_siblings.contains(cpu))
        .unwrap_or_else(|| {
            panic!(
                "worker has no quiet server CPU outside client SMT siblings: \
quiet={quiet:?} client_siblings={client_siblings:?}"
            )
        });
    let pin = Command::new("taskset")
        .args([
            "-apc",
            &client_core.to_string(),
            &std::process::id().to_string(),
        ])
        .output()
        .expect("pin benchmark client process");
    assert!(
        pin.status.success(),
        "client taskset failed: {}",
        String::from_utf8_lossy(&pin.stderr)
    );
    println!(
        "CPU_PREFLIGHT client={client_core} client_siblings={client_siblings:?} \
server={server_core} server_siblings={:?} allowed={allowed:?} loads={loads:?}",
        sibling_group(server_core)
    );
    (client_core, server_core)
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
    let ops_per_sample = parse_usize_env("FR_URING_AB_OPS_PER_SAMPLE", DEFAULT_OPS_PER_SAMPLE);
    let profile_seconds = parse_u64_env("FR_URING_AB_PROFILE_SECONDS", DEFAULT_PROFILE_SECONDS);
    let workloads = Workload::parse_list();
    let pipelines = parse_pipelines();
    assert!(!workloads.is_empty(), "at least one workload is required");
    assert!(!pipelines.is_empty(), "at least one pipeline is required");

    let perf_version = command_output("perf", &["--version"]);
    assert!(
        perf_version.status.success(),
        "worker must provide perf for profile attribution"
    );
    let (_client_core, server_core) = choose_and_pin_cores();
    let root = unique_root();
    println!("ARTIFACT_ROOT {}", root.display());

    let mut servers = [
        Server::spawn(&binary, &redis_binary, Arm::MioA, &root, server_core),
        Server::spawn(&binary, &redis_binary, Arm::MioB, &root, server_core),
        Server::spawn(&binary, &redis_binary, Arm::IoUring, &root, server_core),
        Server::spawn(&binary, &redis_binary, Arm::Redis, &root, server_core),
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
    servers[Arm::IoUring.index()].assert_flag_reached_process();
    profile_io_uring_path(&mut servers[Arm::IoUring.index()], &root, profile_seconds);

    let mut verdicts = Vec::new();
    for workload in workloads {
        for pipeline in &pipelines {
            let measured =
                measure_configuration(&mut servers, workload, *pipeline, samples, ops_per_sample);
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
