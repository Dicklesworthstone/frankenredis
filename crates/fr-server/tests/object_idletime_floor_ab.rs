#![forbid(unsafe_code)]
// The fail-closed stub remains runnable without the measurement feature, while
// the worker-only harness helpers are intentionally dormant in that build.
#![cfg_attr(
    not(any(
        feature = "perf-ab-object-idletime-floor",
        feature = "perf-ab-lpos-floor"
    )),
    allow(dead_code)
)]

//! One-binary, one-invocation, interleaved P16/C50 instruction A/B for exact
//! dispatch-floor routes. Each new target calibrates a same-function A/A null
//! before trusting its A/B median.
//!
//! Run only through strict remote RCH:
//! `RCH_REQUIRE_REMOTE=1 env -u CARGO_TARGET_DIR rch exec -- cargo test --profile
//! release-perf -p fr-server --features perf-ab-lpos-floor --test
//! object_idletime_floor_ab -- --ignored --nocapture
//! lpos_floor_same_binary_null_then_interleaved_instruction_ab`

use std::fs;
use std::hint::black_box;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLIENTS: usize = 50;
const PIPELINE: usize = 16;
#[cfg(feature = "perf-ab-object-idletime-floor")]
const GUARD_SAMPLES: usize = 4;
#[cfg(feature = "perf-ab-object-idletime-floor")]
const STAT_SAMPLES: usize = 8;
#[cfg(feature = "perf-ab-lpos-floor")]
const LPOS_NULL_SAMPLES: usize = 10;
#[cfg(feature = "perf-ab-lpos-floor")]
const LPOS_STAT_SAMPLES: usize = 10;
const STAT_ROUNDS: usize = 160;
const PROFILE_ROUNDS: usize = 1_000;
#[cfg(feature = "perf-ab-object-idletime-floor")]
const MAX_CV_PCT: f64 = 5.0;
const KEEP_GATE_RATIO: f64 = 0.99;
#[cfg(feature = "perf-ab-object-idletime-floor")]
const GUARD_RATIO_TOLERANCE: f64 = 0.01;
#[cfg(feature = "perf-ab-object-idletime-floor")]
const REQUEST: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nIDLETIME\r\n$1\r\nk\r\n";
#[cfg(feature = "perf-ab-object-idletime-floor")]
const GUARD_REQUEST: &[u8] = b"*3\r\n$6\r\nGETBIT\r\n$1\r\nk\r\n$1\r\n0\r\n";
#[cfg(feature = "perf-ab-lpos-floor")]
const LPOS_REQUEST: &[u8] = b"*3\r\n$4\r\nLPOS\r\n$1\r\nl\r\n$1\r\na\r\n";
#[cfg(feature = "perf-ab-lpos-floor")]
const LPOS_SETUP: &[u8] = b"*3\r\n$5\r\nRPUSH\r\n$1\r\nl\r\n$1\r\na\r\n";
#[cfg(feature = "perf-ab-lpos-floor")]
const LPOS_SETUP_REPLY: &[u8] = b":1\r\n";
const SETUP: &[u8] = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n";
const SHUTDOWN: &[u8] = b"*2\r\n$8\r\nSHUTDOWN\r\n$6\r\nNOSAVE\r\n";

#[derive(Clone, Copy, Debug)]
enum Arm {
    Orig,
    Candidate,
}

impl Arm {
    const fn name(self) -> &'static str {
        match self {
            Self::Orig => "orig",
            Self::Candidate => "candidate",
        }
    }
}

struct Server {
    child: Child,
    port: u16,
    clients: Vec<TcpStream>,
}

impl Server {
    fn spawn(binary: &Path, arm: Arm, runtime_dir: &Path, server_core: usize) -> Self {
        fs::create_dir_all(runtime_dir).expect("create unique server runtime directory");
        let port = free_port();
        let mut command = Command::new("taskset");
        command
            .args(["-c", &server_core.to_string()])
            .arg(binary)
            .args(["--bind", "127.0.0.1", "--port", &port.to_string()])
            .env(
                "FR_PERF_AB_OBJECT_IDLETIME_FLOOR_ORIG",
                if matches!(arm, Arm::Orig) { "1" } else { "0" },
            )
            .env(
                "FR_PERF_AB_LPOS_FLOOR_ORIG",
                if matches!(arm, Arm::Orig) { "1" } else { "0" },
            )
            .current_dir(runtime_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().expect("spawn same-binary server arm");
        let mut server = Self {
            child,
            port,
            clients: Vec::new(),
        };
        wait_until_ready(port);
        exchange_one(port, SETUP, b"+OK\r\n");
        server.clients = (0..CLIENTS).map(|_| connect(port)).collect::<Vec<_>>();
        server
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn shutdown(mut self) {
        self.clients.clear();
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", self.port)) {
            let _ = stream.write_all(SHUTDOWN);
        }
        let status = self.child.wait().expect("wait for server shutdown");
        assert!(status.success(), "server exited with {status}");
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
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .expect("set write timeout");
    stream
}

fn wait_until_ready(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("server on port {port} did not become ready");
}

fn exchange_one(port: u16, request: &[u8], expected: &[u8]) {
    let mut stream = connect(port);
    stream.write_all(request).expect("write setup request");
    let mut got = vec![0; expected.len()];
    stream.read_exact(&mut got).expect("read setup response");
    assert_eq!(got, expected);
}

fn pipeline_packet(request: &[u8]) -> Vec<u8> {
    request.repeat(PIPELINE)
}

fn exchange_group(clients: &mut [TcpStream], packet: &[u8]) {
    let input = black_box(packet);
    for client in clients.iter_mut() {
        client.write_all(input).expect("write P16 request batch");
    }
    for client in clients.iter_mut() {
        let mut reply = Vec::with_capacity(PIPELINE * 8);
        let mut newlines = 0usize;
        while newlines < PIPELINE {
            let mut chunk = [0u8; 512];
            let read = client.read(&mut chunk).expect("read P16 replies");
            assert_ne!(read, 0, "server closed during measured routine");
            newlines += chunk[..read].iter().filter(|&&byte| byte == b'\n').count();
            reply.extend_from_slice(&chunk[..read]);
        }
        assert_eq!(newlines, PIPELINE, "unexpected reply frame count");
        assert!(
            reply
                .split_inclusive(|&byte| byte == b'\n')
                .all(|line| line.starts_with(b":") && line.ends_with(b"\r\n")),
            "dispatch-floor target returned a non-integer reply: {reply:?}"
        );
        black_box(reply);
    }
}

fn run_interleaved(
    orig: &mut Server,
    candidate: &mut Server,
    reverse: bool,
    rounds: usize,
    request: &[u8],
) {
    let packet = pipeline_packet(request);
    for _ in 0..rounds {
        if reverse {
            exchange_group(&mut candidate.clients, &packet);
            exchange_group(&mut orig.clients, &packet);
            exchange_group(&mut orig.clients, &packet);
            exchange_group(&mut candidate.clients, &packet);
        } else {
            exchange_group(&mut orig.clients, &packet);
            exchange_group(&mut candidate.clients, &packet);
            exchange_group(&mut candidate.clients, &packet);
            exchange_group(&mut orig.clients, &packet);
        }
    }
}

fn run_single(server: &mut Server, rounds: usize, request: &[u8]) {
    let packet = pipeline_packet(request);
    for _ in 0..rounds {
        exchange_group(&mut server.clients, &packet);
    }
}

fn perf_stat(pid: u32) -> Child {
    Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "stat",
            "--no-big-num",
            "-x,",
            "-e",
            "instructions:u",
            "-p",
            &pid.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn perf stat")
}

fn parse_instructions(output: Output) -> u64 {
    assert!(output.status.success(), "perf stat failed: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("perf stat output is UTF-8");
    for line in stderr.lines() {
        let mut fields = line.split(',');
        let raw = fields.next().unwrap_or_default().trim();
        if fields.any(|field| field.contains("instructions")) {
            assert!(!raw.starts_with('<'), "unavailable perf counter: {line}");
            return raw.parse().expect("parse instruction count");
        }
    }
    panic!("instructions:u missing from perf output: {stderr}");
}

fn mean_cv(samples: &[f64]) -> (f64, f64) {
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / (samples.len() - 1) as f64;
    (mean, variance.sqrt() / mean * 100.0)
}

#[cfg(feature = "perf-ab-lpos-floor")]
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

#[cfg(feature = "perf-ab-lpos-floor")]
fn median(samples: &[f64]) -> f64 {
    quantile(samples, 0.5)
}

fn pin_client_and_select_server_core() -> usize {
    let status = fs::read_to_string("/proc/self/status").expect("read process CPU allowance");
    let allowed = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .map(str::trim)
        .expect("Cpus_allowed_list is present");
    let mut cpus = Vec::new();
    for range in allowed.split(',') {
        if let Some((start, end)) = range.split_once('-') {
            let start = start.parse::<usize>().expect("parse CPU range start");
            let end = end.parse::<usize>().expect("parse CPU range end");
            cpus.extend(start..=end);
        } else {
            cpus.push(range.parse::<usize>().expect("parse allowed CPU"));
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    assert!(cpus.len() >= 2, "A/B needs distinct client and server CPUs");
    let client_core = cpus[0];
    let server_core = *cpus.last().expect("at least two allowed CPUs");
    let pin = Command::new("taskset")
        .args([
            "-pc",
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
    println!("CPU_PIN client={client_core} server={server_core} allowed={allowed}");
    server_core
}

fn unique_root() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "fr_object_idletime_floor_ab_{}_{stamp}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("create unique A/B root");
    root
}

fn profile_arm(
    binary: &Path,
    arm: Arm,
    root: &Path,
    server_core: usize,
    label: &str,
    request: &[u8],
    extra_setup: Option<(&[u8], &[u8])>,
) -> String {
    let mut server = Server::spawn(
        binary,
        arm,
        &root.join(format!("profile_{label}_{}", arm.name())),
        server_core,
    );
    if let Some((setup, expected)) = extra_setup {
        exchange_one(server.port, setup, expected);
    }
    thread::sleep(Duration::from_secs(3));
    let data = root.join(format!("profile_{label}_{}.data", arm.name()));
    assert!(!data.exists(), "refusing to overwrite {}", data.display());
    let mut perf = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "record",
            "-q",
            "-F",
            "997",
            "-e",
            "instructions:u",
            "-g",
            "-m",
            "4",
            "-p",
            &server.pid().to_string(),
            "-o",
        ])
        .arg(&data)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn perf record");
    thread::sleep(Duration::from_millis(750));
    assert!(
        perf.try_wait().expect("poll perf record").is_none(),
        "perf record exited before workload"
    );
    run_single(&mut server, PROFILE_ROUNDS, request);
    server.shutdown();
    let perf_output = perf.wait_with_output().expect("wait for perf record");
    assert!(
        perf_output.status.success(),
        "perf record for {} failed: {}",
        arm.name(),
        String::from_utf8_lossy(&perf_output.stderr)
    );
    let report = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "report",
            "--stdio",
            "--no-children",
            "--percent-limit",
            "0.1",
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
        "perf report for {} failed: {}",
        arm.name(),
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
    report
}

fn self_pct(report: &str, symbol: &str) -> f64 {
    let line = report
        .lines()
        .find(|line| line.contains("[.]") && line.contains(symbol))
        .unwrap_or_else(|| panic!("profile has no {symbol} frame"));
    line.split_whitespace()
        .next()
        .expect("profile row has percentage")
        .trim_end_matches('%')
        .parse()
        .expect("parse profile self percentage")
}

struct InstructionSamples {
    orig: Vec<f64>,
    candidate: Vec<f64>,
    ratio: Vec<f64>,
}

struct InterleavedMeasurement<'a> {
    label: &'a str,
    request: &'a [u8],
    samples: usize,
    left_mode: Arm,
    right_mode: Arm,
    extra_setup: Option<(&'a [u8], &'a [u8])>,
}

fn measure_interleaved(
    binary: &Path,
    root: &Path,
    server_core: usize,
    measurement: InterleavedMeasurement<'_>,
) -> InstructionSamples {
    let InterleavedMeasurement {
        label,
        request,
        samples,
        left_mode,
        right_mode,
        extra_setup,
    } = measurement;
    let mut orig_samples = Vec::with_capacity(samples);
    let mut candidate_samples = Vec::with_capacity(samples);
    let mut ratio_samples = Vec::with_capacity(samples);
    for sample in 0..samples {
        let reverse = sample % 2 == 1;
        let orig_dir = root.join(format!("{label}_{sample}_left"));
        let candidate_dir = root.join(format!("{label}_{sample}_right"));
        let (mut orig, mut candidate) = if reverse {
            let candidate = Server::spawn(binary, right_mode, &candidate_dir, server_core);
            let orig = Server::spawn(binary, left_mode, &orig_dir, server_core);
            (orig, candidate)
        } else {
            let orig = Server::spawn(binary, left_mode, &orig_dir, server_core);
            let candidate = Server::spawn(binary, right_mode, &candidate_dir, server_core);
            (orig, candidate)
        };
        if let Some((setup, expected)) = extra_setup {
            exchange_one(orig.port, setup, expected);
            exchange_one(candidate.port, setup, expected);
        }
        thread::sleep(Duration::from_secs(3));
        let (orig_perf, candidate_perf) = if reverse {
            let candidate_perf = perf_stat(candidate.pid());
            let orig_perf = perf_stat(orig.pid());
            (orig_perf, candidate_perf)
        } else {
            let orig_perf = perf_stat(orig.pid());
            let candidate_perf = perf_stat(candidate.pid());
            (orig_perf, candidate_perf)
        };
        thread::sleep(Duration::from_millis(750));
        run_interleaved(&mut orig, &mut candidate, reverse, STAT_ROUNDS, request);
        if reverse {
            candidate.shutdown();
            orig.shutdown();
        } else {
            orig.shutdown();
            candidate.shutdown();
        }
        let orig_count = parse_instructions(orig_perf.wait_with_output().expect("wait ORIG perf"));
        let candidate_count = parse_instructions(
            candidate_perf
                .wait_with_output()
                .expect("wait candidate perf"),
        );
        let ratio = candidate_count as f64 / orig_count as f64;
        println!(
            "INSTRUCTIONS label={label} sample={} order={} left_mode={} right_mode={} left={} \
right={} right_over_left={ratio:.9}",
            sample + 1,
            if reverse { "COOC" } else { "OCCO" },
            left_mode.name(),
            right_mode.name(),
            orig_count,
            candidate_count,
        );
        orig_samples.push(orig_count as f64);
        candidate_samples.push(candidate_count as f64);
        ratio_samples.push(ratio);
    }
    InstructionSamples {
        orig: orig_samples,
        candidate: candidate_samples,
        ratio: ratio_samples,
    }
}

#[cfg(not(feature = "perf-ab-object-idletime-floor"))]
#[test]
#[ignore = "requires --features perf-ab-object-idletime-floor"]
fn object_idletime_floor_same_binary_interleaved_instruction_ab() {
    panic!("A/B requires the same-binary control feature");
}

#[cfg(feature = "perf-ab-object-idletime-floor")]
#[test]
#[ignore = "strict-remote perf gate; run explicitly with the measurement feature"]
fn object_idletime_floor_same_binary_interleaved_instruction_ab() {
    let perf_version = Command::new("perf")
        .arg("--version")
        .output()
        .expect("worker must provide perf");
    assert!(
        perf_version.status.success(),
        "worker perf preflight failed"
    );

    let binary = PathBuf::from(env!("CARGO_BIN_EXE_frankenredis"));
    let hash = Command::new("sha256sum")
        .arg(&binary)
        .output()
        .expect("hash same-binary server");
    assert!(hash.status.success(), "sha256sum failed");
    let hash_output = String::from_utf8(hash.stdout).expect("sha256sum output is UTF-8");
    let binary_sha256 = hash_output
        .split_whitespace()
        .next()
        .expect("sha256sum emitted a digest");
    println!("BINARY_SHA256 arms=orig,candidate sha256={binary_sha256}");
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("WORKER_ID {hostname}");

    let root = unique_root();
    let server_core = pin_client_and_select_server_core();

    let orig_profile = profile_arm(
        &binary,
        Arm::Orig,
        &root,
        server_core,
        "object_idletime",
        REQUEST,
        None,
    );
    let candidate_profile = profile_arm(
        &binary,
        Arm::Candidate,
        &root,
        server_core,
        "object_idletime",
        REQUEST,
        None,
    );
    println!("ORIG_PROFILE_TABLE_BEGIN\n{orig_profile}\nORIG_PROFILE_TABLE_END");
    println!("CANDIDATE_PROFILE_TABLE_BEGIN\n{candidate_profile}\nCANDIDATE_PROFILE_TABLE_END");
    let orig_process_self = self_pct(&orig_profile, "frankenredis::process_buffered_frames");
    let candidate_floor_self = self_pct(
        &candidate_profile,
        "frankenredis::dispatch_floor_fast_object_idletime",
    );
    assert!(orig_process_self > 0.0);
    assert!(candidate_floor_self > 0.0);
    println!(
        "PROFILE_REACHABILITY orig_process_buffered_frames_self_pct={orig_process_self:.4} \
candidate_object_idletime_floor_self_pct={candidate_floor_self:.4}"
    );

    let guard = measure_interleaved(
        &binary,
        &root,
        server_core,
        InterleavedMeasurement {
            label: "guard_getbit",
            request: GUARD_REQUEST,
            samples: GUARD_SAMPLES,
            left_mode: Arm::Orig,
            right_mode: Arm::Candidate,
            extra_setup: None,
        },
    );
    let (guard_orig_mean, guard_orig_cv_pct) = mean_cv(&guard.orig);
    let (guard_candidate_mean, guard_candidate_cv_pct) = mean_cv(&guard.candidate);
    let (guard_ratio_mean, guard_ratio_cv_pct) = mean_cv(&guard.ratio);
    println!(
        "INSTRUCTIONS_SUMMARY label=guard_getbit orig_mean={guard_orig_mean:.3} \
orig_cv_pct={guard_orig_cv_pct:.6} candidate_mean={guard_candidate_mean:.3} \
candidate_cv_pct={guard_candidate_cv_pct:.6} candidate_over_orig={guard_ratio_mean:.9} \
ratio_cv_pct={guard_ratio_cv_pct:.6}"
    );
    assert!(guard_orig_cv_pct < MAX_CV_PCT, "guard ORIG CV gate failed");
    assert!(
        guard_candidate_cv_pct < MAX_CV_PCT,
        "guard candidate CV gate failed"
    );
    assert!(
        guard_ratio_cv_pct < MAX_CV_PCT,
        "guard paired-ratio CV gate failed"
    );
    assert!(
        (guard_ratio_mean - 1.0).abs() < GUARD_RATIO_TOLERANCE,
        "shared-path guard is not neutral: {guard_ratio_mean:.9}"
    );

    let target = measure_interleaved(
        &binary,
        &root,
        server_core,
        InterleavedMeasurement {
            label: "object_idletime",
            request: REQUEST,
            samples: STAT_SAMPLES,
            left_mode: Arm::Orig,
            right_mode: Arm::Candidate,
            extra_setup: None,
        },
    );
    let (orig_mean, orig_cv_pct) = mean_cv(&target.orig);
    let (candidate_mean, candidate_cv_pct) = mean_cv(&target.candidate);
    let (ratio_mean, ratio_cv_pct) = mean_cv(&target.ratio);
    println!(
        "INSTRUCTIONS_SUMMARY label=object_idletime orig_mean={orig_mean:.3} \
orig_cv_pct={orig_cv_pct:.6} candidate_mean={candidate_mean:.3} \
candidate_cv_pct={candidate_cv_pct:.6} candidate_over_orig={ratio_mean:.9} \
ratio_cv_pct={ratio_cv_pct:.6}"
    );
    assert!(orig_cv_pct < MAX_CV_PCT, "ORIG CV gate failed");
    assert!(candidate_cv_pct < MAX_CV_PCT, "candidate CV gate failed");
    assert!(ratio_cv_pct < MAX_CV_PCT, "paired-ratio CV gate failed");
    assert!(
        ratio_mean < KEEP_GATE_RATIO,
        "1% instruction keep gate failed: {ratio_mean:.9}"
    );
}

#[cfg(not(feature = "perf-ab-lpos-floor"))]
#[test]
#[ignore = "requires --features perf-ab-lpos-floor"]
fn lpos_floor_same_binary_null_then_interleaved_instruction_ab() {
    panic!("A/B requires the same-binary LPOS control feature");
}

#[cfg(feature = "perf-ab-lpos-floor")]
#[test]
#[ignore = "strict-remote perf gate; run explicitly with the LPOS measurement feature"]
fn lpos_floor_same_binary_null_then_interleaved_instruction_ab() {
    let perf_version = Command::new("perf")
        .arg("--version")
        .output()
        .expect("worker must provide perf");
    assert!(
        perf_version.status.success(),
        "worker perf preflight failed"
    );

    let binary = PathBuf::from(env!("CARGO_BIN_EXE_frankenredis"));
    let hash = Command::new("sha256sum")
        .arg(&binary)
        .output()
        .expect("hash same-binary server");
    assert!(hash.status.success(), "sha256sum failed");
    let hash_output = String::from_utf8(hash.stdout).expect("sha256sum output is UTF-8");
    let binary_sha256 = hash_output
        .split_whitespace()
        .next()
        .expect("sha256sum emitted a digest");
    println!("BINARY_SHA256 arms=null_a,null_b,orig,candidate sha256={binary_sha256}");
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("WORKER_ID {hostname}");

    let root = unique_root();
    let server_core = pin_client_and_select_server_core();
    let setup = Some((LPOS_SETUP, LPOS_SETUP_REPLY));

    let orig_profile = profile_arm(
        &binary,
        Arm::Orig,
        &root,
        server_core,
        "lpos",
        LPOS_REQUEST,
        setup,
    );
    let candidate_profile = profile_arm(
        &binary,
        Arm::Candidate,
        &root,
        server_core,
        "lpos",
        LPOS_REQUEST,
        setup,
    );
    println!("ORIG_PROFILE_TABLE_BEGIN\n{orig_profile}\nORIG_PROFILE_TABLE_END");
    println!("CANDIDATE_PROFILE_TABLE_BEGIN\n{candidate_profile}\nCANDIDATE_PROFILE_TABLE_END");
    let orig_process_self = self_pct(&orig_profile, "frankenredis::process_buffered_frames");
    let candidate_floor_self =
        self_pct(&candidate_profile, "frankenredis::dispatch_floor_fast_lpos");
    assert!(orig_process_self > 0.0);
    assert!(candidate_floor_self > 0.0);
    println!(
        "PROFILE_REACHABILITY orig_process_buffered_frames_self_pct={orig_process_self:.4} \
candidate_lpos_floor_self_pct={candidate_floor_self:.4}"
    );

    // Per-function null comes first: both servers select the exact pre-LPOS-floor monomorph.
    let null = measure_interleaved(
        &binary,
        &root,
        server_core,
        InterleavedMeasurement {
            label: "lpos_null",
            request: LPOS_REQUEST,
            samples: LPOS_NULL_SAMPLES,
            left_mode: Arm::Orig,
            right_mode: Arm::Orig,
            extra_setup: setup,
        },
    );
    let null_median = median(&null.ratio);
    let null_p05 = quantile(&null.ratio, 0.05);
    let null_p95 = quantile(&null.ratio, 0.95);
    let (_, null_left_cv_pct) = mean_cv(&null.orig);
    let (_, null_right_cv_pct) = mean_cv(&null.candidate);
    let (_, null_ratio_cv_pct) = mean_cv(&null.ratio);
    println!(
        "NULL_SUMMARY label=lpos median={null_median:.9} p05={null_p05:.9} \
p95={null_p95:.9} left_cv_pct={null_left_cv_pct:.6} \
right_cv_pct={null_right_cv_pct:.6} ratio_cv_pct={null_ratio_cv_pct:.6}"
    );
    assert!(
        (null_median - 1.0).abs() < 0.02,
        "LPOS null median exposes a harness bias: {null_median:.9}"
    );

    let target = measure_interleaved(
        &binary,
        &root,
        server_core,
        InterleavedMeasurement {
            label: "lpos_target",
            request: LPOS_REQUEST,
            samples: LPOS_STAT_SAMPLES,
            left_mode: Arm::Orig,
            right_mode: Arm::Candidate,
            extra_setup: setup,
        },
    );
    let candidate_median = median(&target.ratio);
    let candidate_p05 = quantile(&target.ratio, 0.05);
    let candidate_p95 = quantile(&target.ratio, 0.95);
    let (orig_mean, orig_cv_pct) = mean_cv(&target.orig);
    let (candidate_mean, candidate_cv_pct) = mean_cv(&target.candidate);
    let (ratio_mean, ratio_cv_pct) = mean_cv(&target.ratio);
    println!(
        "INSTRUCTIONS_SUMMARY label=lpos orig_mean={orig_mean:.3} \
orig_cv_pct={orig_cv_pct:.6} candidate_mean={candidate_mean:.3} \
candidate_cv_pct={candidate_cv_pct:.6} ratio_mean={ratio_mean:.9} \
ratio_cv_pct={ratio_cv_pct:.6} candidate_median={candidate_median:.9} \
candidate_p05={candidate_p05:.9} candidate_p95={candidate_p95:.9} \
null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9}"
    );
    assert!(
        candidate_median < null_p05,
        "candidate median {candidate_median:.9} does not clear null floor {null_p05:.9}"
    );
    assert!(
        candidate_median < KEEP_GATE_RATIO,
        "1% instruction keep gate failed: {candidate_median:.9}"
    );
}
