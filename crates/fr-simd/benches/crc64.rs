//! Same-binary A/B for CRC-64/Jones: PCLMULQDQ fold vs a slice-by-8 table (the shipped baseline's
//! class), null-gated on the median.
//!
//! Substrate identical to the other `fr-simd` benches: ONE binary / ONE invocation, adjacent-pair
//! interleaving (order swapped on odd rounds), `black_box` on inputs, reps calibrated per size,
//! median of paired per-round ratios, gated on the candidate median lying outside the null
//! control's p5..p95 spread (`cv` reported, never gated).
//!
//! ORIG = a slice-by-8 table CRC (representative of `fr-persist`'s slice-by-16 table baseline; the
//! bench builds the tables at startup so it needs no cross-crate access).
//! CAND = `fr_simd::crc64` (PCLMULQDQ where available).
//! Both are asserted byte-identical before timing.

use std::hint::black_box;
use std::time::Instant;

use fr_simd::crc64;

const CRC64_POLY_REFLECTED: u64 = 0x95AC_9329_AC4B_C9B5; // reflect(0xAD93D23594C935A9)
const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const SIZES: [usize; 6] = [256, 512, 1024, 2048, 4096, 8192];
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn build_tables() -> [[u64; 256]; 8] {
    let mut t = [[0u64; 256]; 8];
    for n in 0..256usize {
        let mut crc = n as u64;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (CRC64_POLY_REFLECTED & 0u64.wrapping_sub(crc & 1));
        }
        t[0][n] = crc;
    }
    for n in 0..256usize {
        for k in 1..8usize {
            t[k][n] = (t[k - 1][n] >> 8) ^ t[0][(t[k - 1][n] & 0xff) as usize];
        }
    }
    t
}

fn table_crc(t: &[[u64; 256]; 8], data: &[u8]) -> u64 {
    let mut crc = 0u64;
    let (chunks, rem) = data.as_chunks::<8>();
    for c in chunks {
        let v = u64::from_le_bytes(*c) ^ crc;
        crc = t[7][(v & 0xff) as usize]
            ^ t[6][((v >> 8) & 0xff) as usize]
            ^ t[5][((v >> 16) & 0xff) as usize]
            ^ t[4][((v >> 24) & 0xff) as usize]
            ^ t[3][((v >> 32) & 0xff) as usize]
            ^ t[2][((v >> 40) & 0xff) as usize]
            ^ t[1][((v >> 48) & 0xff) as usize]
            ^ t[0][((v >> 56) & 0xff) as usize];
    }
    for &b in rem {
        crc = (crc >> 8) ^ t[0][((crc as u8) ^ b) as usize];
    }
    crc
}

fn median(r: &mut [f64]) -> f64 {
    r.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    r[r.len() / 2]
}
fn cv(r: &[f64]) -> f64 {
    let m = r.iter().sum::<f64>() / r.len() as f64;
    100.0 * (r.iter().map(|x| (x - m).powi(2)).sum::<f64>() / r.len() as f64).sqrt() / m
}
fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let tables = build_tables();
    println!("pclmulqdq_detected={}", std::arch::is_x86_feature_detected!("pclmulqdq"));
    println!(
        "\n{:<10} {:>9} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "size", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );

    for size in SIZES {
        let mut buf = vec![0u8; size];
        let mut s = 0xa5a5_5a5a_c3c3_3c3cu64;
        for b in buf.iter_mut() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b = (s >> 33) as u8;
        }
        assert_eq!(crc64(&buf), table_crc(&tables, &buf), "CAND != table baseline");

        let base = |d: &[u8]| table_crc(&tables, d);
        let cand = |d: &[u8]| crc64(d);
        let time = |f: &dyn Fn(&[u8]) -> u64, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0u64;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(&buf)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };
        let mut reps = 1usize;
        loop {
            let e = time(&base, reps);
            if e >= TARGET_SEGMENT_SECS || reps > 1 << 24 {
                reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
                break;
            }
            reps *= 4;
        }

        let mut nulls = Vec::with_capacity(ROUNDS);
        let mut speeds = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |bf: &dyn Fn(&[u8]) -> u64, cf: &dyn Fn(&[u8]) -> u64| {
                if swap {
                    let c = time(cf, reps);
                    time(bf, reps) / c
                } else {
                    let b = time(bf, reps);
                    b / time(cf, reps)
                }
            };
            let n = pair(&base, &base);
            let sp = pair(&base, &cand);
            if round == 0 {
                continue;
            }
            nulls.push(n);
            speeds.push(sp);
        }

        let null_med = median(&mut nulls);
        let speedup = median(&mut speeds);
        let lo = pct(&nulls, NULL_LO);
        let hi = pct(&nulls, NULL_HI);
        let verdict = if speedup > 1.0 && speedup > hi {
            "WIN"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        let label = if size >= 1024 * 1024 {
            format!("{} MiB", size / (1024 * 1024))
        } else if size >= 1024 {
            format!("{} KiB", size / 1024)
        } else {
            format!("{size} B")
        };
        println!(
            "{:<10} {:>9} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
            label,
            reps,
            null_med,
            format!("[{lo:.3}, {hi:.3}]"),
            cv(&nulls),
            speedup,
            verdict
        );
    }
}
