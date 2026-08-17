//! Same-binary A/B for the LZF match-tail SIMD routing (frankenredis-g9h0v follow-up).
//!
//! `lzf_compress`'s match-extension inner loop calls `common_prefix_len` on the
//! tail of every match that already matched its first 18 bytes (the SWAR fast
//! path). ORIG always runs the local scalar word loop. CAND (`lzf_match_tail_len`)
//! keeps that inlined word loop for short tails (`< 128 B`) but routes long tails
//! (`>= 128 B`, i.e. highly repetitive runs) through `fr_simd::common_prefix_len`,
//! whose AVX2 arm is BIT-identical to the word loop but 1.8–2.9x faster from 128 B.
//! Gating in fr-persist keeps the common short match on the zero-overhead inline
//! path, so the routing is Pareto-safe: never a regression, a win only where LZF
//! actually feeds long match tails (large repeated values in a DUMP payload).
//!
//! ORIG = `bench_lzf_compress::<false>`  (always-local, = production).
//! CAND = `bench_lzf_compress::<true>`   (>= 128 B tails via fr_simd AVX2).
//! Expectation: WIN on long-run payloads, INDISTINGUISHABLE on short-match /
//! text / structured payloads (the guards — must never regress). Both arms emit
//! BYTE-IDENTICAL compressed bytes (asserted before timing).
//!
//! Substrate = ONE self-identifying binary and ONE invocation, position-balanced
//! A/A+A/B pairs, black_box on both input and result, and reps calibrated per
//! input. Decisions use the larger of twice the bootstrap 95% null-median CI
//! radius and an absolute 1.01 floor. CV is provenance only.

use std::env;
use std::hint::black_box;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use fr_persist::bench_lzf_compress;

const ROUNDS: usize = 81;
const TARGET_SEGMENT_SECS: f64 = 0.020;
const BOOTSTRAP_RESAMPLES: usize = 20_000;

/// Long repeated-run payload: `copies` back-to-back copies of a `unit`-byte pseudo
/// -random block. Each copy matches the previous one for the full `unit` bytes, so
/// LZF's match tails routinely exceed 128 B — the AVX2 win regime. Realistic of a
/// list/hash DUMP whose elements are large and near-identical.
fn repeated_runs(unit: usize, copies: usize) -> Vec<u8> {
    let mut block = Vec::with_capacity(unit);
    let mut s: u32 = 0x1234_5678;
    for _ in 0..unit {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        block.push((s >> 24) as u8);
    }
    let mut out = Vec::with_capacity(unit * copies);
    for _ in 0..copies {
        out.extend_from_slice(&block);
    }
    out
}

/// A single big run of one byte — the pure AVX2-heavy extreme (every match extends
/// to MAX_REF, tails ~245 B).
fn single_byte_run(n: usize) -> Vec<u8> {
    vec![b'x'; n]
}

/// Structured members with SHORT common prefixes (`member:00001:...`). Matches are
/// dominated by the ~14 B shared prefix, well under 128 B — a guard that the gate
/// never regresses the common short-match case.
fn structured(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n * 28);
    for i in 0..n {
        out.extend_from_slice(format!("member:{i:05}:payload-{:03}\n", (i * 7) % 999).as_bytes());
    }
    out
}

/// Repetitive-but-not-long English-ish text: moderate matches, typically < 128 B —
/// the realistic-corpus guard.
fn textish(target: usize) -> Vec<u8> {
    const WORDS: &[&str] = &[
        "the",
        "quick",
        "brown",
        "fox",
        "jumps",
        "over",
        "lazy",
        "dog",
        "redis",
        "listpack",
        "quicklist",
        "compress",
        "value",
        "member",
        "score",
        "field",
    ];
    let mut out = Vec::with_capacity(target + 16);
    let mut s: u32 = 0x9e37_79b9;
    while out.len() < target {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.extend_from_slice(WORDS[(s >> 28) as usize % WORDS.len()].as_bytes());
        out.push(b' ');
    }
    out
}

fn compress_orig(p: &[u8]) -> usize {
    bench_lzf_compress::<false>(p, p.len()).map_or(0, |c| c.len())
}
fn compress_cand(p: &[u8]) -> usize {
    bench_lzf_compress::<true>(p, p.len()).map_or(0, |c| c.len())
}

fn median(r: &mut [f64]) -> f64 {
    r.sort_by(f64::total_cmp);
    let middle = r.len() / 2;
    if r.len().is_multiple_of(2) {
        (r[middle - 1] + r[middle]) / 2.0
    } else {
        r[middle]
    }
}

fn cv(r: &[f64]) -> f64 {
    let m = r.iter().sum::<f64>() / r.len() as f64;
    100.0 * (r.iter().map(|x| (x - m).powi(2)).sum::<f64>() / r.len() as f64).sqrt() / m
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * percentile).round() as usize]
}

fn bootstrap_median_ci(samples: &[f64]) -> (f64, f64) {
    let mut state = 0x5eed_f00d_cafe_babe_u64;
    for sample in samples {
        state ^= sample.to_bits().wrapping_mul(0x9e37_79b9_7f4a_7c15);
        state = state.rotate_left(17);
    }
    let mut scratch = vec![0.0; samples.len()];
    let mut medians = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    for _ in 0..BOOTSTRAP_RESAMPLES {
        for value in &mut scratch {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let draw = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            *value = samples[(draw as usize) % samples.len()];
        }
        medians.push(median(&mut scratch));
    }
    medians.sort_by(f64::total_cmp);
    (percentile(&medians, 0.025), percentile(&medians, 0.975))
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
    let digest = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .ok_or_else(|| "sha256sum emitted no digest".to_owned())?
        .to_owned();
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("sha256sum emitted invalid digest {digest:?}"));
    }
    Ok(digest)
}

fn main() -> Result<(), String> {
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    let elf_sha256 = binary_sha256(&executable)?;
    println!(
        "bench_elf_sha256={elf_sha256} executable={}",
        executable.display()
    );

    let cases: &[(&str, Vec<u8>)] = &[
        ("runs_256x24", repeated_runs(256, 24)), // long tails -> AVX2 (expect WIN)
        ("runs_512x12", repeated_runs(512, 12)), // long tails -> AVX2 (expect WIN)
        ("onebyte_8k", single_byte_run(8192)),   // AVX2 extreme (expect WIN)
        ("structured_512", structured(512)),     // short prefixes (guard: no regression)
        ("textish_8k", textish(8192)),           // moderate matches (guard: no regression)
    ];

    // Correctness gate: ORIG and CAND compress to BYTE-IDENTICAL bytes on every shape.
    for (label, p) in cases {
        let a = bench_lzf_compress::<false>(p, p.len());
        let b = bench_lzf_compress::<true>(p, p.len());
        assert_eq!(a, b, "{label}: SIMD routing changed the compressed bytes");
    }
    println!("CORRECTNESS_GATE lzf_compressed_output=byte_identical cases=5");

    println!(
        "\n{:<16} {:>7} {:>9} {:>24} {:>8} {:>11} {:>12} {:>8}",
        "workload",
        "reps",
        "NULL med",
        "null bootstrap CI95",
        "null cv%",
        "orig/cand",
        "threshold",
        "verdict"
    );

    let mut long_tail_keeps = 0_usize;
    let mut guard_rejects = 0_usize;
    for (label, p) in cases {
        let time = |f: &dyn Fn(&[u8]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(p.as_slice())));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };
        let orig = |p: &[u8]| compress_orig(p);
        let cand = |p: &[u8]| compress_cand(p);

        let mut reps = 1usize;
        loop {
            let e = time(&orig, reps);
            if e >= TARGET_SEGMENT_SECS || reps > 1 << 18 {
                reps =
                    ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
                break;
            }
            reps *= 4;
        }

        let mut nulls = Vec::with_capacity(ROUNDS);
        // (frankenredis-ucye4) The SECOND null arm. The only null used to be
        // orig-vs-orig, which certifies timing stability for the SCALAR arm while the
        // candidate is the AVX2 path -- the worst arm to be blind to, since heavy AVX2
        // can shift core frequency and thermal behaviour, so cand's run-to-run variance
        // may legitimately differ from orig's. An orig-only null cannot see that, and a
        // threshold derived from it is then too small, giving a false KEEP. Both arms are
        // now nulled and BOTH must bracket 1.0, exactly as scripts/balanced_square_ab.py
        // already does with null_redis and null_fr.
        let mut nulls_cand = Vec::with_capacity(ROUNDS);
        let mut speeds = Vec::with_capacity(ROUNDS);
        // (frankenredis-qj6jn) Tag each null with its configuration so a refused row
        // can be split by (orientation, null-position). See the LZF_DUMP_NULLS block.
        let mut null_tags: Vec<(bool, bool)> = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap_within_pair = round % 2 == 1;
            let pair = |bf: &dyn Fn(&[u8]) -> usize, cf: &dyn Fn(&[u8]) -> usize| {
                if swap_within_pair {
                    let c = time(cf, reps);
                    time(bf, reps) / c
                } else {
                    let b = time(bf, reps);
                    b / time(cf, reps)
                }
            };
            let (nn, nc, sp) = if round % 4 < 2 {
                (pair(&orig, &orig), pair(&cand, &cand), pair(&orig, &cand))
            } else {
                let effect = pair(&orig, &cand);
                (pair(&orig, &orig), pair(&cand, &cand), effect)
            };
            if round == 0 {
                continue;
            }
            nulls.push(nn);
            nulls_cand.push(nc);
            null_tags.push((swap_within_pair, round % 4 < 2));
            speeds.push(sp);
        }

        if std::env::var("LZF_DUMP_NULLS").is_ok() {
            let mut sorted = nulls.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let above = nulls.iter().filter(|v| **v > 1.0).count();
            let below = nulls.iter().filter(|v| **v < 1.0).count();
            // Split by configuration. A within-pair POSITION effect must land above
            // 1.0 in one orientation and below in the other, because the swap puts
            // t1/t2 in one and t2/t1 in the other. If BOTH orientations sit on the
            // same side of 1.0, position is refuted and the cause is elsewhere.
            for (want_swap, want_first) in
                [(false, true), (false, false), (true, true), (true, false)]
            {
                let mut bucket: Vec<f64> = nulls
                    .iter()
                    .zip(null_tags.iter())
                    .filter(|(_, (sw, nf))| *sw == want_swap && *nf == want_first)
                    .map(|(v, _)| *v)
                    .collect();
                if bucket.is_empty() {
                    continue;
                }
                bucket.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let above = bucket.iter().filter(|v| **v > 1.0).count();
                eprintln!(
                    "NULLSPLIT {label}: swap={want_swap} null_first={want_first} n={} above1={above} med={:.6}",
                    bucket.len(),
                    bucket[bucket.len() / 2]
                );
            }
            eprintln!(
                "NULLDUMP {label}: n={} above1={above} below1={below} min={:.6} p25={:.6} med={:.6} p75={:.6} max={:.6}",
                sorted.len(),
                sorted[0],
                sorted[sorted.len() / 4],
                sorted[sorted.len() / 2],
                sorted[sorted.len() * 3 / 4],
                sorted[sorted.len() - 1]
            );
        }
        let null_cv_pct = cv(&nulls);
        let effect_cv_pct = cv(&speeds);
        let (null_ci95_low, null_ci95_high) = bootstrap_median_ci(&nulls);
        let (cand_ci95_low, cand_ci95_high) = bootstrap_median_ci(&nulls_cand);
        let null_med = median(&mut nulls);
        let cand_null_med = median(&mut nulls_cand);
        let speedup = median(&mut speeds);
        // (frankenredis-ucye4) BOTH arms must be nulled. Reporting which arm failed is
        // the point: "the null failed" sent me through four blind re-runs, and an
        // orig-only null could not have implicated the AVX2 arm at all.
        if null_ci95_low > 1.0 || null_ci95_high < 1.0 {
            return Err(format!(
                "{label}: ORIG A/A null CI does not bracket 1.0: \
                 [{null_ci95_low:.9}, {null_ci95_high:.9}] (cand null med {cand_null_med:.6})"
            ));
        }
        if cand_ci95_low > 1.0 || cand_ci95_high < 1.0 {
            return Err(format!(
                "{label}: CAND A/A null CI does not bracket 1.0: \
                 [{cand_ci95_low:.9}, {cand_ci95_high:.9}] -- the AVX2 arm is the one \
                 an orig-only null could never have caught (orig null med {null_med:.6})"
            ));
        }
        let null_radius = (null_ci95_low - 1.0)
            .abs()
            .max((null_ci95_high - 1.0).abs())
            .max((cand_ci95_low - 1.0).abs())
            .max((cand_ci95_high - 1.0).abs());
        // (frankenredis-ucye4) The threshold must be able to EXCEED the constant floor,
        // or the adaptive term is dead code. Measured: null_radius is ~0.002, so
        // `(1.0 + 2.0*null_radius).max(1.01)` always resolved to exactly 1.01 -- while
        // this harness's own null MEDIAN moved 0.98844 -> 1.00257 between runs of the
        // SAME binary, i.e. ~1.4% of drift under a 1% gate. An effect may not be claimed
        // below the bias the instrument just demonstrated, which is the rule landed for
        // scripts/balanced_square_ab.py in frankenredis-enrhw; apply it here too, taking
        // the worse of the two arms' observed bias.
        let observed_null_bias = (null_med - 1.0).abs().max((cand_null_med - 1.0).abs());
        let adaptive = (2.0 * null_radius).max(observed_null_bias);
        let decisive_threshold = (1.0 + adaptive).max(1.01);
        let binding = if adaptive >= 0.01 {
            if observed_null_bias > 2.0 * null_radius {
                "observed_null_bias"
            } else {
                "2x_null_ci_radius"
            }
        } else {
            "absolute_floor_1.01"
        };
        let verdict = if speedup >= decisive_threshold {
            "KEEP"
        } else if speedup.recip() >= decisive_threshold {
            "REJECT"
        } else {
            "NULL"
        };
        if matches!(*label, "runs_256x24" | "runs_512x12" | "onebyte_8k") && verdict == "KEEP" {
            long_tail_keeps += 1;
        }
        if matches!(*label, "structured_512" | "textish_8k") && verdict == "REJECT" {
            guard_rejects += 1;
        }
        println!(
            "{:<16} {:>7} {:>9.4} {:>24} {:>8.2} {:>10.3}x {:>12.3} {:>8}",
            label,
            reps,
            null_med,
            format!("[{null_ci95_low:.4}, {null_ci95_high:.4}]"),
            null_cv_pct,
            speedup,
            decisive_threshold,
            verdict
        );
        println!(
            "MEDIAN_CI_EVIDENCE workload={label} rounds={ROUNDS} \
             null_median={null_med:.9} null_ci95_low={null_ci95_low:.9} \
             null_ci95_high={null_ci95_high:.9} \
             reference_over_candidate_median={speedup:.9} \
             decisive_threshold={decisive_threshold:.9} \
             bootstrap_resamples={BOOTSTRAP_RESAMPLES} null_cv_pct={null_cv_pct:.6} \
             effect_cv_pct={effect_cv_pct:.6} verdict={verdict} \
             same_invocation_aa=true position_balanced=true \
             absolute_floor=1.01 cv_used_as_provenance_only=true \
             cand_null_med={cand_null_med:.9} \
             cand_null_ci95=[{cand_ci95_low:.9},{cand_ci95_high:.9}] \
             observed_null_bias={observed_null_bias:.9} binding={binding}"
        );
    }

    if long_tail_keeps == 0 {
        return Err("no long-tail workload cleared the median-CI KEEP gate".to_owned());
    }
    if guard_rejects != 0 {
        return Err(format!(
            "{guard_rejects} short-tail guard workload(s) cleared the REJECT gate"
        ));
    }
    println!(
        "DECISION verdict=KEEP long_tail_keeps={long_tail_keeps} guard_rejects={guard_rejects} \
         gate=bootstrap_median_ci_95 two_x_margin=true absolute_floor=1.01 \
         cv_used_as_provenance_only=true"
    );
    Ok(())
}
