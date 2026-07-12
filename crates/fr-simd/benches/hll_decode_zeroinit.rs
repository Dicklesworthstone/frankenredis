//! Same-binary A/B for HLL dense-register decode alloc: `vec![0u8; N]` (alloc_zeroed = 16 KiB
//! memset) + overwrite-every-byte loop, vs `with_capacity` + `extend` (no memset). Each timed
//! iteration allocates a fresh result `Vec` and drops it, matching the real PFMERGE / multi-key
//! PFCOUNT hot path (mimalloc recycles the 16 KiB block dirty, so the zeroed path re-memsets it
//! each call). Both produce byte-identical registers (asserted).
//!
//! ORIG = `decode_zeroed` (the pre-change path).  CAND = `decode_uninit` (with_capacity+extend).

use std::hint::black_box;
use std::time::Instant;

const HLL_REGISTERS: usize = 16384;
const DENSE_BYTES: usize = HLL_REGISTERS / 4 * 3; // 12288

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.006;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn decode_zeroed(payload: &[u8]) -> Vec<u8> {
    let mut registers = vec![0u8; HLL_REGISTERS];
    let (register_chunks, _) = registers.as_chunks_mut::<4>();
    let (payload_chunks, _) = payload.as_chunks::<3>();
    for (regs, bytes) in register_chunks.iter_mut().zip(payload_chunks.iter()) {
        let w = u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16);
        regs[0] = (w & 0x3f) as u8;
        regs[1] = ((w >> 6) & 0x3f) as u8;
        regs[2] = ((w >> 12) & 0x3f) as u8;
        regs[3] = ((w >> 18) & 0x3f) as u8;
    }
    registers
}

fn decode_uninit(payload: &[u8]) -> Vec<u8> {
    let (payload_chunks, _) = payload.as_chunks::<3>();
    let mut registers = Vec::with_capacity(HLL_REGISTERS);
    registers.extend(payload_chunks.iter().flat_map(|bytes| {
        let w = u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16);
        [
            (w & 0x3f) as u8,
            ((w >> 6) & 0x3f) as u8,
            ((w >> 12) & 0x3f) as u8,
            ((w >> 18) & 0x3f) as u8,
        ]
    }));
    registers
}

fn fill(buf: &mut [u8], mut seed: u64) {
    for b in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (seed >> 33) as u8;
    }
}

fn time(reps: usize, payload: &[u8], f: fn(&[u8]) -> Vec<u8>) -> f64 {
    let start = Instant::now();
    let mut acc = 0u8;
    for _ in 0..reps {
        let r = f(black_box(payload));
        acc = acc.wrapping_add(r[r.len() - 1]).wrapping_add(r[0]);
        drop(black_box(r));
    }
    black_box(acc);
    start.elapsed().as_secs_f64()
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
    let mut payload = vec![0u8; DENSE_BYTES];
    fill(&mut payload, 0x48_4c_4c_00);
    assert_eq!(decode_zeroed(&payload), decode_uninit(&payload), "decode variants diverged");

    let mut reps = 1usize;
    loop {
        let e = time(reps, &payload, decode_zeroed);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 20 {
            reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
            break;
        }
        reps *= 4;
    }

    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speeds = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        let pair = |bf: fn(&[u8]) -> Vec<u8>, cf: fn(&[u8]) -> Vec<u8>| {
            if swap {
                let c = time(reps, &payload, cf);
                time(reps, &payload, bf) / c
            } else {
                let b = time(reps, &payload, bf);
                b / time(reps, &payload, cf)
            }
        };
        let nn = pair(decode_zeroed, decode_zeroed);
        let sp = pair(decode_zeroed, decode_uninit);
        if round == 0 {
            continue;
        }
        nulls.push(nn);
        speeds.push(sp);
    }

    let null_med = median(&mut nulls);
    let speedup = median(&mut speeds);
    let lo = pct(&nulls, NULL_LO);
    let hi = pct(&nulls, NULL_HI);
    let verdict = if speedup > 1.0 && speedup > hi {
        "WIN(no-memset)"
    } else if speedup < 1.0 && speedup < lo {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    println!("\n{:>10} {:>9} {:>16} {:>8} {:>11} {:>16}", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict");
    println!(
        "{:>10} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>16}",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}
