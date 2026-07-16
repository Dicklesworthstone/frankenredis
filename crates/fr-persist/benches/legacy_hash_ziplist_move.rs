//! Same-binary proof for legacy HASH_ZIPLIST field/value materialization.
//!
//! The production arm runs the full RDB-prefix decoder. The frozen reference keeps the original
//! deep clones after the ziplist decoder has already produced owned field/value byte vectors.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_persist::{bench_decode_rdb_prefix_reference, crc64_redis, decode_rdb_prefix};

const HASH_PAIRS: usize = 512;
const PROFILE_REPEATS: usize = 5_000;
const STAT_REPEATS: usize = 1_000;
const STAT_ROUNDS: usize = 9;
const RDB_TYPE_HASH_ZIPLIST: u8 = 13;
const RDB_OPCODE_EOF: u8 = 0xff;

#[derive(Clone, Copy)]
enum Arm {
    Candidate,
    Reference,
}

impl Arm {
    const fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Reference => "reference",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "reference" => Ok(Self::Reference),
            _ => Err(format!("unknown arm {value:?}")),
        }
    }
}

fn append_length(out: &mut Vec<u8>, len: usize) {
    if len < 64 {
        out.push(len as u8);
    } else if len < 16_384 {
        out.push(0x40 | ((len >> 8) as u8 & 0x3f));
        out.push(len as u8);
    } else {
        out.push(0x80);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
}

fn append_rdb_string(out: &mut Vec<u8>, bytes: &[u8]) {
    append_length(out, bytes.len());
    out.extend_from_slice(bytes);
}

fn encode_ziplist(entries: &[Vec<u8>], header_count: u16) -> Vec<u8> {
    let mut body = Vec::new();
    let mut previous_entry_len = 0usize;
    let mut tail_offset = 10usize;
    for entry in entries {
        tail_offset = 10 + body.len();
        let start = body.len();
        if previous_entry_len < 254 {
            body.push(previous_entry_len as u8);
        } else {
            body.push(0xfe);
            body.extend_from_slice(&(previous_entry_len as u32).to_le_bytes());
        }
        if entry.len() < 64 {
            body.push(entry.len() as u8);
        } else if entry.len() < 16_384 {
            body.push(0x40 | ((entry.len() >> 8) as u8 & 0x3f));
            body.push(entry.len() as u8);
        } else {
            body.push(0x80);
            body.extend_from_slice(&(entry.len() as u32).to_be_bytes());
        }
        body.extend_from_slice(entry);
        previous_entry_len = body.len() - start;
    }

    let total_len = 10 + body.len() + 1;
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&(total_len as u32).to_le_bytes());
    out.extend_from_slice(&(tail_offset as u32).to_le_bytes());
    out.extend_from_slice(&header_count.to_le_bytes());
    out.extend_from_slice(&body);
    out.push(0xff);
    out
}

fn rdb_with_hash_ziplist(entries: &[Vec<u8>], header_count: u16) -> Vec<u8> {
    let ziplist = encode_ziplist(entries, header_count);
    let mut rdb = b"REDIS0009".to_vec();
    rdb.push(RDB_TYPE_HASH_ZIPLIST);
    append_rdb_string(&mut rdb, b"legacy-hash");
    append_rdb_string(&mut rdb, &ziplist);
    rdb.push(RDB_OPCODE_EOF);
    let checksum = crc64_redis(&rdb);
    rdb.extend_from_slice(&checksum.to_le_bytes());
    rdb
}

fn generated_entries(pairs: usize) -> Vec<Vec<u8>> {
    let mut entries = Vec::with_capacity(pairs * 2);
    for index in 0..pairs {
        entries.push(format!("field:{index:04}").into_bytes());
        let mut value = format!("value:{index:04}:").into_bytes();
        value.resize(48, b'a' + (index % 26) as u8);
        entries.push(value);
    }
    entries
}

fn timing_fixture() -> Vec<u8> {
    let entries = generated_entries(HASH_PAIRS);
    rdb_with_hash_ziplist(&entries, entries.len() as u16)
}

fn decode(data: &[u8], arm: Arm) -> Result<fr_persist::RdbDecodeResult, fr_persist::PersistError> {
    match arm {
        Arm::Candidate => decode_rdb_prefix(black_box(data)),
        Arm::Reference => bench_decode_rdb_prefix_reference(black_box(data)),
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let fixture = timing_fixture();
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        let decoded = decode(black_box(&fixture), black_box(arm));
        match black_box(&decoded) {
            Ok(result) => {
                checksum = checksum
                    .wrapping_add(result.consumed as u64)
                    .wrapping_add(result.entries.len() as u64);
                if let Some(entry) = result.entries.first() {
                    checksum = checksum.wrapping_add(entry.key.len() as u64);
                    if let fr_persist::RdbValue::Hash(fields) = &entry.value {
                        checksum = checksum.wrapping_add(fields.len() as u64);
                        for (field, value) in fields {
                            checksum = checksum
                                .wrapping_add(field.len() as u64)
                                .wrapping_add(value.len() as u64)
                                .wrapping_add(u64::from(field.first().copied().unwrap_or(0)))
                                .wrapping_add(u64::from(value.last().copied().unwrap_or(0)));
                        }
                    }
                }
            }
            Err(error) => checksum = checksum.wrapping_add(format!("{error:?}").len() as u64),
        }
        let _ = black_box(decoded);
    }
    black_box(checksum);
}

fn unhex(input: &str) -> Vec<u8> {
    (0..input.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&input[index..index + 2], 16).expect("valid hex fixture"))
        .collect()
}

fn correctness_gate() {
    let real_redis_hash_ziplist = unhex(
        "524544495330303039fa0972656469732d7665720b3235352e3235352e323535fa0a72656469732d62697473c040fa056374696d65c2c85c9660fa08757365642d6d656dc290ad0c00fa0c616f662d707265616d626c65c000fe00fb01000d04686173681b1b00000016000000040000026631040276310402663204027632ffff4f9cd1fd16699883",
    );
    let small = generated_entries(3);
    let binary = vec![
        vec![0, 1, 2, 0xff],
        vec![0x80, 0, 0x7f],
        b"count".to_vec(),
        b"-9223372036854775808".to_vec(),
    ];
    let odd = vec![b"field".to_vec(), b"value".to_vec(), b"dangling".to_vec()];
    let mut bad_checksum = timing_fixture();
    let last = bad_checksum.len() - 1;
    bad_checksum[last] ^= 1;
    let cases = [
        real_redis_hash_ziplist,
        timing_fixture(),
        rdb_with_hash_ziplist(&small, small.len() as u16),
        rdb_with_hash_ziplist(&binary, binary.len() as u16),
        rdb_with_hash_ziplist(&small, u16::MAX),
        rdb_with_hash_ziplist(&small, small.len() as u16 + 1),
        rdb_with_hash_ziplist(&odd, odd.len() as u16),
        bad_checksum,
    ];
    for (index, data) in cases.iter().enumerate() {
        assert_eq!(
            decode(data, Arm::Candidate),
            decode(data, Arm::Reference),
            "legacy HASH_ZIPLIST decode differs for case {index}"
        );
    }
    println!(
        "CORRECTNESS_GATE exact_results=identical cases={} timing_pairs={HASH_PAIRS} timing_entries={}",
        cases.len(),
        HASH_PAIRS * 2
    );
}

fn child_args() -> Result<Option<(Arm, usize)>, String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) != Some("--child") {
        return Ok(None);
    }
    let arm = Arm::parse(args.get(2).ok_or("missing child arm")?)?;
    let repeats = args
        .get(3)
        .ok_or("missing child repeat count")?
        .parse()
        .map_err(|error| format!("invalid repeat count: {error}"))?;
    Ok(Some((arm, repeats)))
}

fn binary_sha256(executable: &Path) -> Result<String, String> {
    let output = Command::new("sha256sum")
        .arg(executable)
        .output()
        .map_err(|error| format!("could not launch sha256sum: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sha256sum failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "sha256sum emitted no digest".to_owned())
}

fn worker_id() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn measured_output(mut command: Command, label: &str) -> Result<std::process::Output, String> {
    let output = command
        .output()
        .map_err(|error| format!("could not launch {label}: {error}"))?;
    if output.status.code() == Some(124) {
        return Err(format!("{label} exceeded its measurement cap"));
    }
    Ok(output)
}

fn profile_trial(executable: &Path, arm: Arm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_persist_legacy_hash_ziplist_{}_{}_{}.data",
        process::id(),
        arm.name(),
        stamp
    ));
    if data.exists() {
        return Err(format!("refusing to overwrite {}", data.display()));
    }
    let mut record = Command::new("timeout");
    record
        .args([
            "--foreground",
            "45s",
            "perf",
            "record",
            "-q",
            "-F",
            "997",
            "-e",
            "instructions:u",
            "-g",
            "-o",
        ])
        .arg(&data)
        .arg("--")
        .arg(executable)
        .args(["--child", arm.name(), &PROFILE_REPEATS.to_string()]);
    let recorded = measured_output(record, "perf record")?;
    if !recorded.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }
    let mut report = Command::new("timeout");
    report.args([
        "--foreground",
        "15s",
        "perf",
        "report",
        "-i",
        data.to_str().ok_or("non-UTF-8 perf.data path")?,
        "--stdio",
        "--no-children",
        "-g",
        "none",
        "--percent-limit",
        "0.01",
    ]);
    let report = measured_output(report, "perf report")?;
    if !report.status.success() {
        return Err(format!(
            "perf report failed: {}",
            String::from_utf8_lossy(&report.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&report.stdout);
    println!(
        "PROFILE_TABLE_BEGIN arm={}\n{stdout}\nPROFILE_TABLE_END arm={}",
        arm.name(),
        arm.name()
    );
    let lost_samples = stdout
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .ok_or("perf report omitted Total Lost Samples; profile provenance INVALID")?
        .rsplit(':')
        .next()
        .ok_or("missing lost-sample count")?
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid lost-sample count: {error}"))?;
    if lost_samples != 0 {
        return Err(format!("profile lost {lost_samples} samples"));
    }
    let line = stdout
        .lines()
        .find(|line| line.contains("fr_persist::decode_rdb_prefix_impl"))
        .ok_or("profile has no exact decode_rdb_prefix_impl frame; workload INVALID")?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self-time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    if self_pct <= 0.0 {
        return Err("decode_rdb_prefix_impl has zero self-time".to_owned());
    }
    Ok(self_pct)
}

fn run_profile(executable: &Path, arms: &[Arm]) -> Result<(), String> {
    let fixture = timing_fixture();
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!(
        "TRIGGER format=RDB_TYPE_HASH_ZIPLIST redis_compat=le_6_2 pairs={HASH_PAIRS} entries={} bytes={}",
        HASH_PAIRS * 2,
        fixture.len()
    );
    for &arm in arms {
        let status = Command::new(executable)
            .args(["--child", arm.name(), "4"])
            .status()
            .map_err(|error| format!("could not launch warm-up: {error}"))?;
        if !status.success() {
            return Err(format!("{} warm-up failed", arm.name()));
        }
        let self_pct = profile_trial(executable, arm)?;
        println!("PROFILE_SELF arm={} self_pct={self_pct:.4}", arm.name());
    }
    Ok(())
}

fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let mut command = Command::new("timeout");
    command
        .args([
            "--foreground",
            "30s",
            "perf",
            "stat",
            "--no-big-num",
            "-x,",
            "-e",
            "instructions:u",
            "--",
        ])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()]);
    let output = measured_output(command, "perf stat")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat failed: {stderr}"));
    }
    stderr
        .lines()
        .find_map(|line| {
            let fields: Vec<_> = line.split(',').collect();
            fields
                .iter()
                .any(|field| field.trim().contains("instructions"))
                .then(|| fields[0].trim())
        })
        .ok_or_else(|| format!("instructions:u missing: {stderr}"))?
        .parse()
        .map_err(|error| format!("invalid instruction count: {error}"))
}

fn cv(samples: &[f64]) -> f64 {
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / samples.len() as f64;
    100.0 * variance.sqrt() / mean
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|left, right| left.partial_cmp(right).expect("sample is not NaN"));
    samples[samples.len() / 2]
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * percentile).round() as usize]
}

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    let mut nulls = Vec::with_capacity(STAT_ROUNDS);
    let mut effects = Vec::with_capacity(STAT_ROUNDS);
    let mut candidate_counts = Vec::with_capacity(STAT_ROUNDS);
    let mut reference_counts = Vec::with_capacity(STAT_ROUNDS);
    for round in 0..STAT_ROUNDS {
        let mut counts = [0_u64; 3];
        let mut order = [round % 3, (round + 1) % 3, (round + 2) % 3];
        if round % 2 == 1 {
            order.reverse();
        }
        for slot in order {
            let arm = if slot == 2 {
                Arm::Reference
            } else {
                Arm::Candidate
            };
            counts[slot] = perf_instructions(executable, arm)?;
        }
        let null = counts[0] as f64 / counts[1] as f64;
        let effect = counts[2] as f64 / counts[0] as f64;
        println!(
            "INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} reference={} null_ratio={null:.9} reference_over_candidate={effect:.9}",
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
        nulls.push(null);
        effects.push(effect);
        candidate_counts.push(counts[0] as f64);
        reference_counts.push(counts[2] as f64);
    }
    let null_cv_pct = cv(&nulls);
    let effect_cv_pct = cv(&effects);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let candidate_median = median(&mut candidate_counts);
    let reference_median = median(&mut reference_counts);
    let null_p05 = percentile(&nulls, 0.05);
    let null_p95 = percentile(&nulls, 0.95);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} candidate_median={candidate_median:.0} reference_median={reference_median:.0} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} effect_cv_pct={effect_cv_pct:.6}"
    );
    if (null_median - 1.0).abs() >= 0.02 {
        return Err(format!(
            "null median exposes harness bias: {null_median:.9}"
        ));
    }
    if effect_median <= null_p95 || effect_median <= 1.01 {
        return Err(format!(
            "candidate failed keep gate: effect={effect_median:.9}, null_p95={null_p95:.9}"
        ));
    }
    Ok(())
}

fn main() -> Result<(), String> {
    if let Some((arm, repeats)) = child_args()? {
        run_loop(arm, repeats);
        return Ok(());
    }
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    correctness_gate();
    let live_profile_only = env::args().any(|arg| arg == "--profile-live-only");
    if live_profile_only {
        return run_profile(&executable, &[Arm::Candidate])
            .map_err(|error| format!("PROFILE INVALID: {error}"));
    }
    run_profile(&executable, &[Arm::Candidate, Arm::Reference])
        .map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
