//! Same-binary A/B for the small-PAYLOAD const-length store in `encode_bulk_string_slice`
//! (frankenredis-iqicb).
//!
//! Candidate emits payload+CRLF for `len <= 8` as one const-length array; reference is the
//! prior `extend_from_slice(bytes)` on a runtime-length slice, which lowers to a
//! `__memcpy_avx_unaligned_erms` CALL.
//!
//! WHY THIS BENCH EXISTS AT ALL. The same change was measured on a whole-server profile and
//! could NOT be resolved (ledger 7c6e0add0): its constructed nulls drifted +3 and +11 instr/op
//! against an effect of ~20, because the memcpy frame is shared libc and its per-op self cost
//! is not attributable to one call site — the same arm read 55.3 then 69.8 across two rounds.
//! Both arms live in ONE binary here, so build and layout differences cannot contribute, and
//! `perf stat -e instructions:u` counts instructions rather than time, so host load cannot.

use std::{env, hint::black_box, path::Path, process::Command};

use fr_protocol::bench_encode_bulk_payload_small;

const REPEATS: usize = 400_000;
const ROUNDS: usize = 9;
/// Sizes 1..=8 take the candidate path; 16 and 64 fall through it and are NULLS BY
/// CONSTRUCTION — if they move, the measurement is not isolating what it claims to.
const SIZES: &[usize] = &[1, 3, 8, 16, 64];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    Candidate,
    Reference,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::Candidate => "candidate",
            Arm::Reference => "reference",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "candidate" => Some(Arm::Candidate),
            "reference" => Some(Arm::Reference),
            _ => None,
        }
    }
}

fn workload(arm: Arm, size: usize, repeats: usize) {
    let value = vec![b'v'; size];
    let mut out = Vec::with_capacity(64);
    for _ in 0..repeats {
        out.clear();
        match arm {
            Arm::Candidate => {
                bench_encode_bulk_payload_small::<true>(Some(black_box(&value)), false, &mut out)
            }
            Arm::Reference => {
                bench_encode_bulk_payload_small::<false>(Some(black_box(&value)), false, &mut out)
            }
        }
        black_box(&out);
    }
}

fn perf_instructions(exe: &Path, arm: Arm, size: usize) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(exe)
        .args(["--child", arm.name(), &size.to_string(), &REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf stat: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "perf stat failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find(|line| line.contains("instructions"))
        .and_then(|line| line.split(',').next())
        .and_then(|field| field.trim().parse::<u64>().ok())
        .ok_or_else(|| "instructions:u missing from perf output".to_owned())
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|l, r| l.partial_cmp(r).expect("sample is not NaN"));
    samples[samples.len() / 2]
}

fn main() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("--child") {
        let arm = Arm::parse(&args[2]).ok_or("unknown arm")?;
        let size: usize = args[3].parse().map_err(|_| "bad size")?;
        let repeats: usize = args[4].parse().map_err(|_| "bad repeats")?;
        workload(arm, size, repeats);
        return Ok(());
    }

    let exe = env::current_exe().map_err(|error| format!("no current exe: {error}"))?;
    println!("encode_bulk_string_slice small-PAYLOAD store, same binary, instructions:u");
    println!("{REPEATS} encodes per sample, {ROUNDS} rounds, median reported\n");
    println!(
        "{:>5}  {:>14}  {:>14}  {:>10}  {}",
        "size", "reference", "candidate", "delta/enc", "role"
    );
    for &size in SIZES {
        let mut reference = Vec::new();
        let mut candidate = Vec::new();
        // Interleave the arms so any drift over the run lands on both.
        for _ in 0..ROUNDS {
            reference.push(perf_instructions(&exe, Arm::Reference, size)? as f64);
            candidate.push(perf_instructions(&exe, Arm::Candidate, size)? as f64);
        }
        let r = median(reference) / REPEATS as f64;
        let c = median(candidate) / REPEATS as f64;
        let role = if size <= 8 { "candidate path" } else { "NULL" };
        println!(
            "{size:>5}  {r:>14.2}  {c:>14.2}  {:>+10.2}  {role}",
            c - r
        );
    }
    Ok(())
}
