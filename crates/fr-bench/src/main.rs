#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fr_protocol::{RespFrame, parse_frame};
use hdrhistogram::Histogram;
use serde::Serialize;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 6379;
const DEFAULT_CLIENTS: usize = 50;
const DEFAULT_REQUESTS: usize = 100_000;
const DEFAULT_PIPELINE: usize = 1;
const DEFAULT_KEYSPACE: usize = 10_000;
const DEFAULT_DATASIZE: usize = 3;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_READ_PERCENT: u8 = 50;
const HISTOGRAM_MAX_US: u64 = 60_000_000;
const REPORT_SCHEMA_VERSION: &str = "fr_bench_report/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Workload {
    Set,
    Get,
    Incr,
    Lpush,
    Lpop,
    Hset,
    Hget,
    Mixed,
}

impl Workload {
    fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("set") {
            return Some(Self::Set);
        }
        if value.eq_ignore_ascii_case("get") {
            return Some(Self::Get);
        }
        if value.eq_ignore_ascii_case("incr") {
            return Some(Self::Incr);
        }
        if value.eq_ignore_ascii_case("lpush") {
            return Some(Self::Lpush);
        }
        if value.eq_ignore_ascii_case("lpop") {
            return Some(Self::Lpop);
        }
        if value.eq_ignore_ascii_case("hset") {
            return Some(Self::Hset);
        }
        if value.eq_ignore_ascii_case("hget") {
            return Some(Self::Hget);
        }
        if value.eq_ignore_ascii_case("mixed") {
            return Some(Self::Mixed);
        }
        None
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Get => "get",
            Self::Incr => "incr",
            Self::Lpush => "lpush",
            Self::Lpop => "lpop",
            Self::Hset => "hset",
            Self::Hget => "hget",
            Self::Mixed => "mixed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    Set,
    Get,
    Incr,
    Lpush,
    Lpop,
    Hset,
    Hget,
}

#[derive(Debug, Clone)]
struct BenchmarkConfig {
    host: String,
    port: u16,
    clients: usize,
    requests: usize,
    pipeline: usize,
    keyspace: usize,
    datasize: usize,
    workload: Workload,
    read_percent: u8,
    db: usize,
    username: Option<String>,
    password: Option<String>,
    timeout_ms: u64,
    json_out: Option<PathBuf>,
    key_prefix: String,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            clients: DEFAULT_CLIENTS,
            requests: DEFAULT_REQUESTS,
            pipeline: DEFAULT_PIPELINE,
            keyspace: DEFAULT_KEYSPACE,
            datasize: DEFAULT_DATASIZE,
            workload: Workload::Set,
            read_percent: DEFAULT_READ_PERCENT,
            db: 0,
            username: None,
            password: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            json_out: None,
            key_prefix: default_key_prefix(),
        }
    }
}

impl BenchmarkConfig {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut cfg = Self::default();
        let mut index = 1usize;
        while index < args.len() {
            let flag = args[index].as_str();
            match flag {
                "--host" => {
                    cfg.host = next_arg(args, &mut index, flag)?;
                }
                "--port" => {
                    cfg.port = parse_u16(&next_arg(args, &mut index, flag)?, flag)?;
                }
                "--clients" => {
                    cfg.clients = parse_usize(&next_arg(args, &mut index, flag)?, flag)?;
                }
                "--requests" => {
                    cfg.requests = parse_usize(&next_arg(args, &mut index, flag)?, flag)?;
                }
                "--pipeline" => {
                    cfg.pipeline = parse_usize(&next_arg(args, &mut index, flag)?, flag)?;
                }
                "--keyspace" => {
                    cfg.keyspace = parse_usize(&next_arg(args, &mut index, flag)?, flag)?;
                }
                "--datasize" => {
                    cfg.datasize = parse_usize(&next_arg(args, &mut index, flag)?, flag)?;
                }
                "--workload" => {
                    let value = next_arg(args, &mut index, flag)?;
                    cfg.workload = Workload::parse(&value)
                        .ok_or_else(|| format!("unsupported workload: {value}"))?;
                }
                "--read-percent" => {
                    cfg.read_percent = parse_u8(&next_arg(args, &mut index, flag)?, flag)?;
                }
                "--db" => {
                    cfg.db = parse_usize(&next_arg(args, &mut index, flag)?, flag)?;
                }
                "--username" => {
                    cfg.username = Some(next_arg(args, &mut index, flag)?);
                }
                "--password" => {
                    cfg.password = Some(next_arg(args, &mut index, flag)?);
                }
                "--timeout-ms" => {
                    cfg.timeout_ms = parse_u64(&next_arg(args, &mut index, flag)?, flag)?;
                }
                "--json-out" => {
                    cfg.json_out = Some(PathBuf::from(next_arg(args, &mut index, flag)?));
                }
                "--key-prefix" => {
                    cfg.key_prefix = next_arg(args, &mut index, flag)?;
                }
                "--help" | "-h" => {
                    return Err(help_text());
                }
                other => {
                    return Err(format!("unknown flag: {other}\n\n{}", help_text()));
                }
            }
            index += 1;
        }

        if cfg.clients == 0 {
            return Err("`--clients` must be greater than zero".to_string());
        }
        if cfg.requests == 0 {
            return Err("`--requests` must be greater than zero".to_string());
        }
        if cfg.pipeline == 0 {
            return Err("`--pipeline` must be greater than zero".to_string());
        }
        if cfg.keyspace == 0 {
            return Err("`--keyspace` must be greater than zero".to_string());
        }
        if cfg.read_percent > 100 {
            return Err("`--read-percent` must be between 0 and 100".to_string());
        }
        if cfg.username.is_some() && cfg.password.is_none() {
            return Err("`--username` requires `--password`".to_string());
        }

        Ok(cfg)
    }
}

#[derive(Debug, Clone, Serialize)]
struct BenchmarkReport {
    schema_version: &'static str,
    generated_at_ms: u64,
    host: String,
    port: u16,
    workload: String,
    clients: usize,
    requests: usize,
    pipeline: usize,
    keyspace: usize,
    datasize: usize,
    read_percent: u8,
    db: usize,
    key_prefix: String,
    total_time_ms: u64,
    ops_per_sec: f64,
    bytes_sent: u64,
    bytes_received: u64,
    latency_us: LatencySummary,
}

#[derive(Debug, Clone, Serialize)]
struct LatencySummary {
    min: u64,
    p50: u64,
    p95: u64,
    p99: u64,
    p999: u64,
    max: u64,
    mean: f64,
    samples: u64,
}

#[derive(Debug)]
struct WorkerResult {
    histogram: Histogram<u64>,
    bytes_sent: u64,
    bytes_received: u64,
}

#[derive(Debug, Clone, Copy)]
struct ResponseExpectation {
    kind: ExpectationKind,
}

#[derive(Debug, Clone, Copy)]
enum ExpectationKind {
    SimpleOk,
    Integer,
    NonError,
}

impl ResponseExpectation {
    const OK: Self = Self {
        kind: ExpectationKind::SimpleOk,
    };
    const INTEGER: Self = Self {
        kind: ExpectationKind::Integer,
    };
    const NON_ERROR: Self = Self {
        kind: ExpectationKind::NonError,
    };

    fn validate(self, frame: &RespFrame) -> Result<(), String> {
        match self.kind {
            ExpectationKind::SimpleOk => match frame {
                RespFrame::SimpleString(value) if value.eq_ignore_ascii_case("OK") => Ok(()),
                RespFrame::Error(value) => Err(format!("server returned error: {value}")),
                other => Err(format!("expected +OK response, got {other:?}")),
            },
            ExpectationKind::Integer => match frame {
                RespFrame::Integer(_) => Ok(()),
                RespFrame::Error(value) => Err(format!("server returned error: {value}")),
                other => Err(format!("expected integer response, got {other:?}")),
            },
            ExpectationKind::NonError => match frame {
                RespFrame::Error(value) => Err(format!("server returned error: {value}")),
                _ => Ok(()),
            },
        }
    }
}

#[derive(Debug)]
struct PreparedCommand {
    frame: RespFrame,
    expectation: ResponseExpectation,
}

#[derive(Debug)]
struct PendingCommand {
    sent_at: Instant,
    expectation: ResponseExpectation,
}

#[derive(Debug)]
struct BenchmarkClient {
    stream: TcpStream,
    read_buf: ReadBuffer,
    bytes_sent: u64,
    bytes_received: u64,
}

impl BenchmarkClient {
    fn connect(config: &BenchmarkConfig) -> Result<Self, String> {
        let address = format!("{}:{}", config.host, config.port);
        let stream = TcpStream::connect(&address)
            .map_err(|err| format!("failed to connect to {address}: {err}"))?;
        stream
            .set_nodelay(true)
            .map_err(|err| format!("failed to enable TCP_NODELAY: {err}"))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(config.timeout_ms)))
            .map_err(|err| format!("failed to set read timeout: {err}"))?;
        stream
            .set_write_timeout(Some(Duration::from_millis(config.timeout_ms)))
            .map_err(|err| format!("failed to set write timeout: {err}"))?;
        let mut client = Self {
            stream,
            read_buf: ReadBuffer::new(),
            bytes_sent: 0,
            bytes_received: 0,
        };

        if let Some(password) = &config.password {
            let mut auth_argv = vec![b"AUTH".to_vec()];
            if let Some(username) = &config.username {
                auth_argv.push(username.as_bytes().to_vec());
            }
            auth_argv.push(password.as_bytes().to_vec());
            let auth = PreparedCommand {
                frame: argv_frame(auth_argv),
                expectation: ResponseExpectation::OK,
            };
            client.run_batch(std::iter::once(&auth), None)?;
        }

        if config.db != 0 {
            let select = PreparedCommand {
                frame: argv_frame(vec![b"SELECT".to_vec(), config.db.to_string().into_bytes()]),
                expectation: ResponseExpectation::OK,
            };
            client.run_batch(std::iter::once(&select), None)?;
        }

        client.bytes_sent = 0;
        client.bytes_received = 0;

        Ok(client)
    }

    fn run_batch<'a, I>(
        &mut self,
        commands: I,
        mut histogram: Option<&mut Histogram<u64>>,
    ) -> Result<(), String>
    where
        I: IntoIterator<Item = &'a PreparedCommand>,
    {
        let commands: Vec<&PreparedCommand> = commands.into_iter().collect();
        if commands.is_empty() {
            return Ok(());
        }

        let mut encoded = Vec::new();
        let mut pending = VecDeque::with_capacity(commands.len());
        for command in commands {
            pending.push_back(PendingCommand {
                sent_at: Instant::now(),
                expectation: command.expectation,
            });
            encoded.extend_from_slice(&command.frame.to_bytes());
        }
        self.stream
            .write_all(&encoded)
            .map_err(|err| format!("failed to write request batch: {err}"))?;
        self.bytes_sent = self
            .bytes_sent
            .saturating_add(u64::try_from(encoded.len()).unwrap_or(u64::MAX));

        while let Some(pending_command) = pending.pop_front() {
            let frame = self.read_frame()?;
            pending_command.expectation.validate(&frame)?;
            if let Some(inner) = histogram.as_deref_mut() {
                let latency_us =
                    u64::try_from(pending_command.sent_at.elapsed().as_micros().max(1))
                        .unwrap_or(HISTOGRAM_MAX_US)
                        .min(HISTOGRAM_MAX_US);
                inner
                    .record(latency_us)
                    .map_err(|err| format!("failed to record latency {latency_us}us: {err}"))?;
            }
        }

        Ok(())
    }

    fn read_frame(&mut self) -> Result<RespFrame, String> {
        loop {
            if let Some(frame) = self.read_buf.try_parse_frame()? {
                return Ok(frame);
            }

            let mut chunk = [0u8; 8192];
            let read = self
                .stream
                .read(&mut chunk)
                .map_err(|err| format!("failed to read response bytes: {err}"))?;
            if read == 0 {
                return Err("connection closed while waiting for server response".to_string());
            }
            self.bytes_received = self
                .bytes_received
                .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            self.read_buf.push(&chunk[..read]);
        }
    }
}

#[derive(Debug, Default)]
struct ReadBuffer {
    data: Vec<u8>,
    start: usize,
}

impl ReadBuffer {
    fn new() -> Self {
        Self {
            data: Vec::with_capacity(8192),
            start: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
    }

    fn try_parse_frame(&mut self) -> Result<Option<RespFrame>, String> {
        if self.start >= self.data.len() {
            self.data.clear();
            self.start = 0;
            return Ok(None);
        }

        match parse_frame(&self.data[self.start..]) {
            Ok(parsed) => {
                self.start += parsed.consumed;
                let frame = parsed.frame;
                if self.start == self.data.len() {
                    self.data.clear();
                    self.start = 0;
                } else if self.start > 64 * 1024 {
                    self.data.drain(..self.start);
                    self.start = 0;
                }
                Ok(Some(frame))
            }
            Err(fr_protocol::RespParseError::Incomplete) => Ok(None),
            Err(err) => Err(format!("failed to parse server response: {err}")),
        }
    }
}

#[derive(Debug, Clone)]
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self { state: seed | 1 }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    fn pick_index(&mut self, upper: usize) -> usize {
        if upper <= 1 {
            return 0;
        }
        let upper_u64 = u64::try_from(upper).unwrap_or(u64::MAX);
        usize::try_from(self.next_u64() % upper_u64).unwrap_or(0)
    }

    fn pick_percent(&mut self) -> u8 {
        u8::try_from(self.next_u64() % 100).unwrap_or(0)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{}", help_text());
        return ExitCode::SUCCESS;
    }

    let config = match BenchmarkConfig::parse(&args) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };

    match run(&config) {
        Ok(report) => {
            print_summary(&report);
            if let Some(path) = &config.json_out
                && let Err(err) = write_report(path, &report)
            {
                eprintln!("{err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn run(config: &BenchmarkConfig) -> Result<BenchmarkReport, String> {
    prepare_workload(config)?;

    let start = Instant::now();
    let mut handles = Vec::with_capacity(config.clients);
    let per_client = config.requests / config.clients;
    let remainder = config.requests % config.clients;
    let template = make_value(config.datasize);

    for client_index in 0..config.clients {
        let cfg = config.clone();
        let value_template = template.clone();
        let assigned = per_client + usize::from(client_index < remainder);
        handles.push(thread::spawn(move || {
            run_worker(&cfg, client_index, assigned, value_template)
        }));
    }

    let mut aggregate = Histogram::<u64>::new_with_bounds(1, HISTOGRAM_MAX_US, 3)
        .map_err(|err| format!("failed to allocate latency histogram: {err}"))?;
    let mut bytes_sent = 0u64;
    let mut bytes_received = 0u64;

    for handle in handles {
        let worker = handle
            .join()
            .map_err(|_| "benchmark worker thread panicked".to_string())??;
        aggregate
            .add(&worker.histogram)
            .map_err(|err| format!("failed to merge histogram: {err}"))?;
        bytes_sent = bytes_sent.saturating_add(worker.bytes_sent);
        bytes_received = bytes_received.saturating_add(worker.bytes_received);
    }

    let total_time = start.elapsed();
    let total_time_ms = u64::try_from(total_time.as_millis()).unwrap_or(u64::MAX);
    let ops_per_sec = if total_time.as_secs_f64() == 0.0 {
        config.requests as f64
    } else {
        config.requests as f64 / total_time.as_secs_f64()
    };

    Ok(BenchmarkReport {
        schema_version: REPORT_SCHEMA_VERSION,
        generated_at_ms: unix_time_ms(),
        host: config.host.clone(),
        port: config.port,
        workload: config.workload.as_str().to_string(),
        clients: config.clients,
        requests: config.requests,
        pipeline: config.pipeline,
        keyspace: config.keyspace,
        datasize: config.datasize,
        read_percent: config.read_percent,
        db: config.db,
        key_prefix: config.key_prefix.clone(),
        total_time_ms,
        ops_per_sec,
        bytes_sent,
        bytes_received,
        latency_us: LatencySummary {
            min: aggregate.min(),
            p50: aggregate.value_at_quantile(0.50),
            p95: aggregate.value_at_quantile(0.95),
            p99: aggregate.value_at_quantile(0.99),
            p999: aggregate.value_at_quantile(0.999),
            max: aggregate.max(),
            mean: aggregate.mean(),
            samples: aggregate.len(),
        },
    })
}

fn prepare_workload(config: &BenchmarkConfig) -> Result<(), String> {
    let prep_count = match config.workload {
        Workload::Get | Workload::Mixed | Workload::Hget => config.keyspace,
        Workload::Lpop => list_prefill_per_key(config.requests, config.keyspace, config.pipeline)
            .saturating_mul(config.keyspace),
        Workload::Set | Workload::Incr | Workload::Lpush | Workload::Hset => 0,
    };

    if prep_count == 0 {
        return Ok(());
    }

    let mut client = BenchmarkClient::connect(config)?;
    let value = make_value(config.datasize);
    let mut batch = Vec::with_capacity(config.pipeline.min(256));

    match config.workload {
        Workload::Get | Workload::Mixed => {
            for key_index in 0..config.keyspace {
                batch.push(build_command(
                    config,
                    CommandKind::Set,
                    key_index,
                    &value,
                    None,
                ));
                flush_prepare_batch(&mut client, &mut batch)?;
            }
        }
        Workload::Hget => {
            for key_index in 0..config.keyspace {
                batch.push(build_command(
                    config,
                    CommandKind::Hset,
                    key_index,
                    &value,
                    None,
                ));
                flush_prepare_batch(&mut client, &mut batch)?;
            }
        }
        Workload::Lpop => {
            let per_key = list_prefill_per_key(config.requests, config.keyspace, config.pipeline);
            for key_index in 0..config.keyspace {
                for item_index in 0..per_key {
                    let list_value = value_for_list_item(&value, item_index);
                    batch.push(build_command(
                        config,
                        CommandKind::Lpush,
                        key_index,
                        &list_value,
                        None,
                    ));
                    flush_prepare_batch(&mut client, &mut batch)?;
                }
            }
        }
        Workload::Set | Workload::Incr | Workload::Lpush | Workload::Hset => {}
    }

    if !batch.is_empty() {
        client.run_batch(batch.iter(), None)?;
    }

    Ok(())
}

fn flush_prepare_batch(
    client: &mut BenchmarkClient,
    batch: &mut Vec<PreparedCommand>,
) -> Result<(), String> {
    if batch.len() < 256 {
        return Ok(());
    }
    client.run_batch(batch.iter(), None)?;
    batch.clear();
    Ok(())
}

fn run_worker(
    config: &BenchmarkConfig,
    client_index: usize,
    requests: usize,
    value_template: Vec<u8>,
) -> Result<WorkerResult, String> {
    let mut client = BenchmarkClient::connect(config)?;
    let mut histogram = Histogram::<u64>::new_with_bounds(1, HISTOGRAM_MAX_US, 3)
        .map_err(|err| format!("failed to allocate worker histogram: {err}"))?;
    let mut batch = Vec::with_capacity(config.pipeline);
    let mut rng = Lcg::new(unix_time_ms() ^ ((client_index as u64 + 1) * 0x9E37_79B9_7F4A_7C15));

    for request_index in 0..requests {
        let kind = select_command_kind(config.workload, config.read_percent, &mut rng);
        let key_index = rng.pick_index(config.keyspace);
        batch.push(build_command(
            config,
            kind,
            key_index,
            &value_template,
            Some(request_index),
        ));
        if batch.len() == config.pipeline {
            client.run_batch(batch.iter(), Some(&mut histogram))?;
            batch.clear();
        }
    }

    if !batch.is_empty() {
        client.run_batch(batch.iter(), Some(&mut histogram))?;
    }

    Ok(WorkerResult {
        histogram,
        bytes_sent: client.bytes_sent,
        bytes_received: client.bytes_received,
    })
}

fn select_command_kind(workload: Workload, read_percent: u8, rng: &mut Lcg) -> CommandKind {
    match workload {
        Workload::Set => CommandKind::Set,
        Workload::Get => CommandKind::Get,
        Workload::Incr => CommandKind::Incr,
        Workload::Lpush => CommandKind::Lpush,
        Workload::Lpop => CommandKind::Lpop,
        Workload::Hset => CommandKind::Hset,
        Workload::Hget => CommandKind::Hget,
        Workload::Mixed => {
            if rng.pick_percent() < read_percent {
                CommandKind::Get
            } else {
                CommandKind::Set
            }
        }
    }
}

fn build_command(
    config: &BenchmarkConfig,
    kind: CommandKind,
    key_index: usize,
    value_template: &[u8],
    request_index: Option<usize>,
) -> PreparedCommand {
    let expectation = match kind {
        CommandKind::Set => ResponseExpectation::OK,
        CommandKind::Get => ResponseExpectation::NON_ERROR,
        CommandKind::Incr => ResponseExpectation::INTEGER,
        CommandKind::Lpush => ResponseExpectation::INTEGER,
        CommandKind::Lpop => ResponseExpectation::NON_ERROR,
        CommandKind::Hset => ResponseExpectation::INTEGER,
        CommandKind::Hget => ResponseExpectation::NON_ERROR,
    };

    let key = benchmark_key(&config.key_prefix, kind, key_index);
    let argv = match kind {
        CommandKind::Set => vec![
            b"SET".to_vec(),
            key,
            request_index
                .map(|idx| value_for_request(value_template, idx))
                .unwrap_or_else(|| value_template.to_vec()),
        ],
        CommandKind::Get => vec![b"GET".to_vec(), key],
        CommandKind::Incr => vec![b"INCR".to_vec(), key],
        CommandKind::Lpush => vec![
            b"LPUSH".to_vec(),
            key,
            request_index
                .map(|idx| value_for_request(value_template, idx))
                .unwrap_or_else(|| value_template.to_vec()),
        ],
        CommandKind::Lpop => vec![b"LPOP".to_vec(), key],
        CommandKind::Hset => vec![
            b"HSET".to_vec(),
            key,
            b"field".to_vec(),
            request_index
                .map(|idx| value_for_request(value_template, idx))
                .unwrap_or_else(|| value_template.to_vec()),
        ],
        CommandKind::Hget => vec![b"HGET".to_vec(), key, b"field".to_vec()],
    };

    PreparedCommand {
        frame: argv_frame(argv),
        expectation,
    }
}

fn argv_frame(argv: Vec<Vec<u8>>) -> RespFrame {
    RespFrame::Array(Some(
        argv.into_iter()
            .map(|arg| RespFrame::BulkString(Some(arg)))
            .collect(),
    ))
}

fn benchmark_key(prefix: &str, kind: CommandKind, key_index: usize) -> Vec<u8> {
    let family = match kind {
        CommandKind::Set | CommandKind::Get => "string",
        CommandKind::Incr => "counter",
        CommandKind::Lpush | CommandKind::Lpop => "list",
        CommandKind::Hset | CommandKind::Hget => "hash",
    };
    format!("{prefix}:{family}:{key_index}").into_bytes()
}

fn make_value(datasize: usize) -> Vec<u8> {
    if datasize == 0 {
        return Vec::new();
    }
    vec![b'x'; datasize]
}

fn value_for_request(template: &[u8], request_index: usize) -> Vec<u8> {
    let suffix = request_index.to_string().into_bytes();
    if template.is_empty() || suffix.len() >= template.len() {
        return suffix;
    }
    let mut value = template.to_vec();
    for (slot, byte) in suffix.iter().rev().enumerate() {
        let index = value.len().saturating_sub(slot + 1);
        value[index] = *byte;
    }
    value
}

fn value_for_list_item(template: &[u8], item_index: usize) -> Vec<u8> {
    value_for_request(template, item_index)
}

fn list_prefill_per_key(requests: usize, keyspace: usize, pipeline: usize) -> usize {
    requests.div_ceil(keyspace).saturating_add(pipeline).max(1)
}

fn write_report(path: &Path, report: &BenchmarkReport) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create report directory {}: {err}",
                parent.display()
            )
        })?;
    }
    let json = serde_json::to_string_pretty(report)
        .map_err(|err| format!("failed to serialize benchmark report: {err}"))?;
    fs::write(path, json.as_bytes())
        .map_err(|err| format!("failed to write benchmark report {}: {err}", path.display()))?;
    Ok(())
}

fn print_summary(report: &BenchmarkReport) {
    println!("fr-bench summary");
    println!("target: {}:{}", report.host, report.port);
    println!("workload: {}", report.workload);
    println!(
        "clients: {}  requests: {}  pipeline: {}  keyspace: {}  datasize: {}",
        report.clients, report.requests, report.pipeline, report.keyspace, report.datasize
    );
    println!(
        "elapsed: {} ms  throughput: {:.2} ops/sec",
        report.total_time_ms, report.ops_per_sec
    );
    println!(
        "latency(us): min={} p50={} p95={} p99={} p999={} max={} mean={:.2}",
        report.latency_us.min,
        report.latency_us.p50,
        report.latency_us.p95,
        report.latency_us.p99,
        report.latency_us.p999,
        report.latency_us.max,
        report.latency_us.mean
    );
    println!(
        "bytes: sent={} received={} key_prefix={}",
        report.bytes_sent, report.bytes_received, report.key_prefix
    );
}

fn help_text() -> String {
    format!(
        "fr-bench - FrankenRedis TCP benchmark harness\n\n\
USAGE:\n\
  fr-bench [OPTIONS]\n\n\
OPTIONS:\n\
  --host <HOST>            Target host (default: {DEFAULT_HOST})\n\
  --port <PORT>            Target port (default: {DEFAULT_PORT})\n\
  --clients <N>            Concurrent TCP clients (default: {DEFAULT_CLIENTS})\n\
  --requests <N>           Total requests across all clients (default: {DEFAULT_REQUESTS})\n\
  --pipeline <N>           Pipeline depth per client (default: {DEFAULT_PIPELINE})\n\
  --keyspace <N>           Number of benchmark keys (default: {DEFAULT_KEYSPACE})\n\
  --datasize <BYTES>       Value size for write workloads (default: {DEFAULT_DATASIZE})\n\
  --workload <KIND>        set|get|incr|lpush|lpop|hset|hget|mixed (default: set)\n\
  --read-percent <N>       Read ratio for mixed workload, 0-100 (default: {DEFAULT_READ_PERCENT})\n\
  --db <N>                 Database number to select (default: 0)\n\
  --username <USER>        Optional ACL username for AUTH\n\
  --password <PASS>        Optional password for AUTH\n\
  --timeout-ms <MS>        Socket read/write timeout (default: {DEFAULT_TIMEOUT_MS})\n\
  --json-out <PATH>        Write the JSON report to this path\n\
  --key-prefix <PREFIX>    Prefix used for benchmark keys (default: auto-generated)\n\
  --help, -h               Show this help\n"
    )
}

fn next_arg(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("missing value for {flag}"))
}

fn parse_usize(value: &str, flag: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| format!("invalid value for {flag}: {value}"))
}

fn parse_u16(value: &str, flag: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| format!("invalid value for {flag}: {value}"))
}

fn parse_u64(value: &str, flag: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("invalid value for {flag}: {value}"))
}

fn parse_u8(value: &str, flag: &str) -> Result<u8, String> {
    value
        .parse::<u8>()
        .map_err(|_| format!("invalid value for {flag}: {value}"))
}

fn default_key_prefix() -> String {
    format!("fr:bench:{}", unix_time_ms())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        BenchmarkConfig, CommandKind, ExpectationKind, ResponseExpectation, Workload, argv_frame,
        benchmark_key, list_prefill_per_key, value_for_request,
    };
    use fr_protocol::RespFrame;

    #[test]
    fn workload_parser_accepts_expected_values() {
        assert_eq!(Workload::parse("set"), Some(Workload::Set));
        assert_eq!(Workload::parse("GET"), Some(Workload::Get));
        assert_eq!(Workload::parse("mixed"), Some(Workload::Mixed));
        assert_eq!(Workload::parse("nope"), None);
    }

    #[test]
    fn config_parser_accepts_core_flags() {
        let args = vec![
            "fr-bench".to_string(),
            "--host".to_string(),
            "cache.local".to_string(),
            "--port".to_string(),
            "6380".to_string(),
            "--workload".to_string(),
            "mixed".to_string(),
            "--requests".to_string(),
            "2000".to_string(),
            "--clients".to_string(),
            "8".to_string(),
            "--pipeline".to_string(),
            "4".to_string(),
            "--read-percent".to_string(),
            "75".to_string(),
            "--password".to_string(),
            "secret".to_string(),
        ];
        let parsed = BenchmarkConfig::parse(&args).expect("config parses");
        assert_eq!(parsed.host, "cache.local");
        assert_eq!(parsed.port, 6380);
        assert_eq!(parsed.workload, Workload::Mixed);
        assert_eq!(parsed.requests, 2000);
        assert_eq!(parsed.clients, 8);
        assert_eq!(parsed.pipeline, 4);
        assert_eq!(parsed.read_percent, 75);
        assert_eq!(parsed.password.as_deref(), Some("secret"));
    }

    #[test]
    fn list_prefill_keeps_batches_from_running_empty() {
        assert_eq!(list_prefill_per_key(100, 10, 1), 11);
        assert_eq!(list_prefill_per_key(1, 10, 16), 17);
    }

    #[test]
    fn request_suffix_rewrites_tail_bytes() {
        assert_eq!(value_for_request(b"xxxxx", 42), b"xxx42".to_vec());
        assert_eq!(value_for_request(b"", 42), b"42".to_vec());
    }

    #[test]
    fn benchmark_key_tracks_command_family() {
        assert_eq!(
            benchmark_key("fr:bench", CommandKind::Get, 9),
            b"fr:bench:string:9".to_vec()
        );
        assert_eq!(
            benchmark_key("fr:bench", CommandKind::Hset, 3),
            b"fr:bench:hash:3".to_vec()
        );
    }

    #[test]
    fn argv_frame_serializes_bulk_array() {
        assert_eq!(
            argv_frame(vec![b"PING".to_vec()]),
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"PING".to_vec()))]))
        );
    }

    #[test]
    fn response_expectations_reject_errors() {
        let error = RespFrame::Error("ERR nope".to_string());
        let ok = RespFrame::SimpleString("OK".to_string());
        let integer = RespFrame::Integer(1);

        assert!(ResponseExpectation::NON_ERROR.validate(&error).is_err());
        assert!(ResponseExpectation::OK.validate(&ok).is_ok());
        assert!(ResponseExpectation::INTEGER.validate(&integer).is_ok());
        assert!(matches!(
            ResponseExpectation::OK.kind,
            ExpectationKind::SimpleOk
        ));
    }
}
