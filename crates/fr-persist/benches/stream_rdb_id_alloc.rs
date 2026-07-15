//! Same-binary proof for stream-RDB ID materialization.
//!
//! The trigger includes both macro-node keys and a dense consumer-group PEL. The current encoder
//! writes each 16-byte ID from stack bytes; the reference constructs a temporary `Vec`, copies it
//! into the output, then drops it.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_persist::{
    EncodableStreamEntry, RdbStreamConsumer, RdbStreamConsumerGroup, RdbStreamPendingEntry,
    bench_encode_upstream_stream_listpacks3_payload,
};

const ENTRIES: usize = 512;
const CONSUMERS: usize = 8;
const PENDING: usize = 256;
const PROFILE_REPEATS: usize = 8_000;
const STAT_REPEATS: usize = 400;
const STAT_ROUNDS: usize = 9;

type Entry = EncodableStreamEntry<Vec<u8>, Vec<u8>>;

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

fn fixture() -> (Vec<Entry>, Vec<RdbStreamConsumerGroup>) {
    let entries = (0..ENTRIES)
        .map(|index| {
            (
                1_700_000_000_000 + index as u64,
                index as u64 % 7,
                vec![(b"field".to_vec(), format!("value:{index:04}").into_bytes())],
            )
        })
        .collect();
    let consumers: Vec<_> = (0..CONSUMERS)
        .map(|index| RdbStreamConsumer {
            name: format!("consumer:{index}").into_bytes(),
            seen_time_ms: 1_700_000_100_000 + index as u64,
            active_time_ms: Some(1_700_000_200_000 + index as u64),
        })
        .collect();
    let pending = (0..PENDING)
        .map(|index| RdbStreamPendingEntry {
            entry_id_ms: 1_700_000_000_000 + index as u64,
            entry_id_seq: index as u64 % 7,
            consumer: consumers[index % CONSUMERS].name.clone(),
            deliveries: 1 + index as u64 % 5,
            last_delivered_ms: 1_700_000_300_000 + index as u64,
        })
        .collect();
    let groups = vec![RdbStreamConsumerGroup {
        name: b"workers".to_vec(),
        last_delivered_id_ms: 1_700_000_000_000 + (ENTRIES - 1) as u64,
        last_delivered_id_seq: (ENTRIES - 1) as u64 % 7,
        entries_read: Some(ENTRIES as u64),
        consumers,
        pending,
    }];
    (entries, groups)
}

fn encode(entries: &[Entry], groups: &[RdbStreamConsumerGroup], arm: Arm) -> Vec<u8> {
    let watermark = Some((
        1_700_000_000_000 + (ENTRIES - 1) as u64,
        (ENTRIES - 1) as u64 % 7,
    ));
    match arm {
        Arm::Candidate => bench_encode_upstream_stream_listpacks3_payload::<true, _, _>(
            black_box(entries),
            watermark,
            black_box(groups),
            Some(ENTRIES as u64),
            None,
        ),
        Arm::Reference => bench_encode_upstream_stream_listpacks3_payload::<false, _, _>(
            black_box(entries),
            watermark,
            black_box(groups),
            Some(ENTRIES as u64),
            None,
        ),
    }
    .expect("stream fixture must encode")
}

fn run_loop(arm: Arm, repeats: usize) {
    let (entries, groups) = fixture();
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        let encoded = encode(black_box(&entries), black_box(&groups), arm);
        checksum = checksum
            .wrapping_add(encoded.len() as u64)
            .wrapping_add(u64::from(encoded.first().copied().unwrap_or(0)))
            .wrapping_add(u64::from(encoded.last().copied().unwrap_or(0)));
        black_box(encoded);
    }
    black_box(checksum);
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

fn profile_trial(executable: &Path, arm: Arm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_persist_stream_rdb_id_{}_{}_{}.data",
        process::id(),
        arm.name(),
        stamp
    ));
    if data.exists() {
        return Err(format!("refusing to overwrite {}", data.display()));
    }
    let recorded = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
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
        .args(["--child", arm.name(), &PROFILE_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf record: {error}"))?;
    if !recorded.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }
    let report = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "report",
            "-i",
            data.to_str().ok_or("non-UTF-8 perf.data path")?,
            "--stdio",
            "--no-children",
            "-g",
            "none",
            "--percent-limit",
            "0.01",
        ])
        .output()
        .map_err(|error| format!("could not launch perf report: {error}"))?;
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
    let lost = stdout
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .ok_or("perf report omitted Total Lost Samples")?
        .rsplit(':')
        .next()
        .ok_or("missing lost-sample count")?
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid lost-sample count: {error}"))?;
    if lost != 0 {
        return Err(format!("profile lost {lost} samples"));
    }
    let line = stdout
        .lines()
        .find(|line| {
            line.contains("fr_persist::rdb_stream::encode_upstream_stream_listpacks3_impl")
                && !line.contains("closure#")
        })
        .ok_or("profile has no exact stream encoder frame; workload INVALID")?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self-time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    if self_pct <= 0.0 {
        return Err("stream encoder has zero self-time".to_owned());
    }
    Ok(self_pct)
}

fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf stat: {error}"))?;
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
        nulls.push(counts[0] as f64 / counts[1] as f64);
        effects.push(counts[2] as f64 / counts[0] as f64);
        candidate_counts.push(counts[0] as f64);
        reference_counts.push(counts[2] as f64);
        println!(
            "ROUND index={} candidate_a={} candidate_b={} reference={}",
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
    }
    let null_cv = cv(&nulls);
    let effect_cv = cv(&effects);
    let null_median = median(&mut nulls);
    let null_p05 = percentile(&nulls, 0.05);
    let null_p95 = percentile(&nulls, 0.95);
    let effect_median = median(&mut effects);
    let candidate_median = median(&mut candidate_counts);
    let reference_median = median(&mut reference_counts);
    let reduction_pct = 100.0 * (1.0 - 1.0 / effect_median);
    println!(
        "A_A_NULL median={null_median:.9} p05={null_p05:.9} p95={null_p95:.9} cv_pct={null_cv:.6}"
    );
    println!(
        "A_B_EFFECT reference_over_candidate_median={effect_median:.9} candidate_instructions_median={candidate_median:.0} reference_instructions_median={reference_median:.0} reduction_pct={reduction_pct:.6} cv_pct={effect_cv:.6}"
    );
    if effect_median <= null_p95 || effect_median <= 1.0 {
        return Err(format!(
            "candidate did not clear null band: effect={effect_median:.9}, null_p95={null_p95:.9}"
        ));
    }
    println!("VERDICT KEEP effect_outside_null=true");
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("--child") {
        let arm = Arm::parse(args.get(2).expect("missing child arm")).expect("invalid child arm");
        let repeats = args
            .get(3)
            .expect("missing child repeats")
            .parse()
            .expect("invalid child repeats");
        run_loop(arm, repeats);
        return;
    }

    let (entries, groups) = fixture();
    let candidate = encode(&entries, &groups, Arm::Candidate);
    let reference = encode(&entries, &groups, Arm::Reference);
    assert_eq!(candidate, reference, "stream ID arms differ");
    println!(
        "CORRECTNESS_GATE byte_identical=true output_bytes={} entries={ENTRIES} pending={PENDING} consumers={CONSUMERS}",
        candidate.len()
    );
    let executable = env::current_exe().expect("current executable");
    println!("WORKER_ID {}", worker_id());
    println!(
        "BINARY_SHA256 both_arms={}",
        binary_sha256(&executable).expect("binary sha256")
    );
    println!(
        "TRIGGER entries={ENTRIES} pending={PENDING} consumers={CONSUMERS} id_materializations_at_least={}",
        PENDING * 2
    );
    if args.get(1).map(String::as_str) == Some("--measure") {
        for arm in [Arm::Candidate, Arm::Reference] {
            Command::new(&executable)
                .args(["--child", arm.name(), "10"])
                .status()
                .expect("profile warm-up child");
            let self_pct = profile_trial(&executable, arm).expect("arm profile");
            println!(
                "PROFILE_SELF arm={} function=encode_upstream_stream_listpacks3_impl self_pct={self_pct:.4}",
                arm.name()
            );
        }
        Command::new(&executable)
            .args(["--child", Arm::Candidate.name(), "10"])
            .status()
            .expect("instruction warm-up child");
        run_instruction_ab(&executable).expect("instruction A/B");
        return;
    }
    panic!("expected --measure");
}
