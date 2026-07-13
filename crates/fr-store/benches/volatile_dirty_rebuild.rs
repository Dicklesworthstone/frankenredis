//! Same-binary A/B for the O(n) volatile-view rebuild that a TTL-preserving write (INCR-on-a-TTL'd
//! counter, or SET ... EX re-arming a live TTL) used to force on the next active-expiry cycle.
//!
//! `MARK = true` (reference) is the OLD behavior: the write marked the clean `volatile_keys`
//! sampling view dirty, so the next active-expiry cycle rebuilds it — `clear()` + `extend(
//! expiry_deadlines.keys().cloned())`, O(n) with a `StoreKey` clone each. `MARK = false` (candidate)
//! is production: the write leaves the clean view untouched (membership unchanged), so the cycle's
//! rebuild is an O(1) not-dirty no-op. Byte-identical to clients (the set + every key's deadline are
//! unchanged; the deadline is read fresh at sample time). The rebuild work lives in BTreeSet/clone
//! callees, so — like the clone-elision benches — a perf-`record` self-time profile does not apply;
//! the `perf stat instructions:u` A/B counts the whole child and is the keep-gate.

use std::{env, hint::black_box, path::Path, process::Command};

use fr_store::Store;

const VOLATILE_KEYS: usize = 1_000; // the volatile view rebuilt per cycle
const NOW_MS: u64 = 1_000;
const STAT_REPEATS: usize = 10_000; // × O(VOLATILE_KEYS) rebuild → billions of instrs, stable ratio
const STAT_ROUNDS: usize = 24;

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

fn cycle(store: &mut Store, arm: Arm) {
    match arm {
        Arm::Candidate => store.bench_volatile_dirty_rebuild::<false>(),
        Arm::Reference => store.bench_volatile_dirty_rebuild::<true>(),
    }
}

fn build_store() -> Store {
    let mut store = Store::new();
    store.bench_setup_clean_volatile_keys(VOLATILE_KEYS, NOW_MS);
    store
}

fn run_loop(arm: Arm, repeats: usize) {
    let mut store = build_store();
    for _ in 0..repeats {
        cycle(black_box(&mut store), arm);
    }
    black_box(&store);
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

fn correctness_gate() {
    // Both arms leave `volatile_keys` identical (the same N keys, clean) after a cycle: the candidate
    // never dirties it, the reference dirties-then-rebuilds to the same set. Byte-identity of the
    // production gate is proven by the fr-store unit tests
    // (incr_on_ttl_key_does_not_dirty_the_clean_volatile_view + the volatile-index suite).
    let mut candidate = build_store();
    let mut reference = build_store();
    for _ in 0..100 {
        candidate.bench_volatile_dirty_rebuild::<false>();
        reference.bench_volatile_dirty_rebuild::<true>();
    }
    std::hint::black_box((&candidate, &reference));
    println!("CORRECTNESS_GATE volatile_dirty_rebuild_same_set=identical");
}

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    let mut nulls = Vec::with_capacity(STAT_ROUNDS);
    let mut effects = Vec::with_capacity(STAT_ROUNDS);
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
    }
    let null_cv_pct = cv(&nulls);
    let effect_cv_pct = cv(&effects);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let null_p05 = percentile(&nulls, 0.05);
    let null_p95 = percentile(&nulls, 0.95);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} speedup_cv_pct={effect_cv_pct:.6}"
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
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
