//! Same-binary proof for repeated valid REPLCONF handshake transitions.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fr_repl::{HandshakeFsm, HandshakeState, HandshakeStep};

const PROFILE_REPEATS: usize = 5_000_000;
const STAT_REPEATS: usize = 1_000_000;
const STAT_ROUNDS: usize = 9;
const PERF_DELAY_MS: u64 = 1_000;

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
    const fn profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "<fr_repl::HandshakeFsm>::on_step",
            Self::Reference => "<fr_repl::HandshakeFsm>::bench_on_step_reference",
        }
    }
    const fn wrong_profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "bench_on_step_reference",
            Self::Reference => "fr_repl::HandshakeFsm::on_step",
        }
    }
}

fn step(arm: Arm, fsm: &mut HandshakeFsm, value: HandshakeStep) -> Result<(), fr_repl::ReplError> {
    match arm {
        Arm::Candidate => fsm.on_step(value),
        Arm::Reference => fsm.bench_on_step_reference(value),
    }
}

fn fresh_fsm(arm: Arm) -> HandshakeFsm {
    let mut fsm = HandshakeFsm::new(false);
    step(arm, &mut fsm, HandshakeStep::Ping).expect("ping transition");
    step(arm, &mut fsm, HandshakeStep::Replconf).expect("replconf transition");
    assert_eq!(fsm.state(), HandshakeState::ReplconfSeen);
    fsm
}

fn correctness_gate() {
    let sequences = [
        vec![
            HandshakeStep::Ping,
            HandshakeStep::Replconf,
            HandshakeStep::Replconf,
        ],
        vec![
            HandshakeStep::Ping,
            HandshakeStep::Replconf,
            HandshakeStep::Psync,
        ],
        vec![HandshakeStep::Ping, HandshakeStep::Auth],
        vec![HandshakeStep::Replconf],
    ];
    let mut cases = 0_usize;
    for auth_required in [false, true] {
        for sequence in &sequences {
            let mut candidate = HandshakeFsm::new(auth_required);
            let mut reference = HandshakeFsm::new(auth_required);
            for &value in sequence {
                assert_eq!(
                    step(Arm::Candidate, &mut candidate, value),
                    step(Arm::Reference, &mut reference, value)
                );
                assert_eq!(candidate.state(), reference.state());
                cases += 1;
            }
        }
    }
    println!(
        "CORRECTNESS_GATE result=identical cases={cases} auth_modes=2 invalid_and_repeated_replconf=covered"
    );
}

fn run_loop(arm: Arm, repeats: usize) {
    let mut fsm = fresh_fsm(arm);
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        let result = step(arm, &mut fsm, black_box(HandshakeStep::Replconf));
        checksum = checksum.wrapping_add(u64::from(result.is_ok()));
        black_box(&result);
    }
    black_box((checksum, fsm.state()));
}

fn child_args() -> Result<Option<(Arm, usize)>, String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) != Some("--child") {
        return Ok(None);
    }
    let arm = Arm::parse(args.get(2).ok_or("missing child arm")?)?;
    let repeats = args
        .get(3)
        .ok_or("missing repeat count")?
        .parse()
        .map_err(|e| format!("invalid repeats: {e}"))?;
    Ok(Some((arm, repeats)))
}

fn sha256(path: &Path) -> Result<String, String> {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "missing sha256".to_owned())
}

fn worker() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn profile(executable: &Path, arm: Arm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_repl_handshake_{}_{}_{}.data",
        process::id(),
        arm.name(),
        stamp
    ));
    let out = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "record",
            "-q",
            "--delay",
            &PERF_DELAY_MS.to_string(),
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
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let report = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["report", "-i"])
        .arg(data.to_str().ok_or("non-UTF8 perf path")?)
        .args([
            "--stdio",
            "--no-children",
            "-g",
            "none",
            "--percent-limit",
            "0.01",
        ])
        .output()
        .map_err(|e| e.to_string())?;
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
        .ok_or("missing lost samples")?
        .rsplit(':')
        .next()
        .ok_or("missing lost count")?
        .trim()
        .parse::<u64>()
        .map_err(|e| e.to_string())?;
    if lost != 0 {
        return Err(format!("lost {lost} samples"));
    }
    if stdout
        .lines()
        .any(|line| line.contains(arm.wrong_profile_symbol()))
    {
        return Err("wrong helper in profile".to_owned());
    }
    let line = stdout
        .lines()
        .find(|line| line.contains(arm.profile_symbol()))
        .ok_or_else(|| format!("missing {} profile frame", arm.profile_symbol()))?;
    let pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|e| e.to_string())?;
    if pct <= 0.0 {
        return Err("zero self time".to_owned());
    }
    Ok(pct)
}

fn instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let out = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "stat",
            "--delay",
            &PERF_DELAY_MS.to_string(),
            "--no-big-num",
            "-x,",
            "-e",
            "instructions:u",
            "--",
        ])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "perf stat failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .find_map(|line| {
            let fields: Vec<_> = line.split(',').collect();
            fields
                .iter()
                .any(|field| field.contains("instructions"))
                .then(|| fields[0].trim())
        })
        .ok_or_else(|| "missing instructions".to_owned())
        .and_then(|s| s.parse().map_err(|e| format!("invalid instructions: {e}")))
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("not NaN"));
    values[values.len() / 2]
}
fn cv(values: &[f64]) -> f64 {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
    100.0 * var.sqrt() / mean
}

fn ab(executable: &Path) -> Result<(), String> {
    let mut nulls = Vec::with_capacity(STAT_ROUNDS);
    let mut effects = Vec::with_capacity(STAT_ROUNDS);
    let mut candidates = Vec::with_capacity(STAT_ROUNDS);
    let mut references = Vec::with_capacity(STAT_ROUNDS);
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
            counts[slot] = instructions(executable, arm)?;
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
        candidates.push(counts[0] as f64);
        references.push(counts[2] as f64);
    }
    let null_cv = cv(&nulls);
    let effect_cv = cv(&effects);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let candidate_median = median(&mut candidates);
    let reference_median = median(&mut references);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} candidate_median={candidate_median:.0} reference_median={reference_median:.0} fewer_instructions_pct={:.6} null_median={null_median:.9} null_cv_pct={null_cv:.6} reference_over_candidate_median={effect_median:.9} effect_cv_pct={effect_cv:.6}",
        100.0 * (1.0 - candidate_median / reference_median)
    );
    if (null_median - 1.0).abs() >= 0.02 || effect_median <= 1.01 {
        return Err(format!(
            "keep gate failed effect={effect_median:.9} null={null_median:.9}"
        ));
    }
    println!("DECISION keep=true effect={effect_median:.9}");
    Ok(())
}

fn main() -> Result<(), String> {
    if let Some((arm, repeats)) = child_args()? {
        thread::sleep(Duration::from_millis(1_100));
        run_loop(arm, repeats);
        process::exit(0);
    }
    let executable = env::current_exe().map_err(|e| e.to_string())?;
    correctness_gate();
    println!("WORKER_ID {}", worker());
    println!("BINARY_SHA256 both_arms={}", sha256(&executable)?);
    profile(&executable, Arm::Candidate).map_err(|e| format!("PROFILE INVALID candidate: {e}"))?;
    profile(&executable, Arm::Reference).map_err(|e| format!("PROFILE INVALID reference: {e}"))?;
    ab(&executable).map_err(|e| format!("A/B INVALID: {e}"))
}
