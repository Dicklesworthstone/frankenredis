#![forbid(unsafe_code)]

use std::env;
use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

use fr_persist::lzf_compress;

const DEFAULT_REPS: usize = 20_000;
const DEFAULT_BYTES: usize = 96;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut mode = Mode::Bench;
    let mut reps = DEFAULT_REPS;
    let mut bytes = DEFAULT_BYTES;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bench" => mode = Mode::Bench,
            "--golden" => mode = Mode::Golden,
            "--reps" => reps = parse_usize(args.next(), "--reps")?,
            "--bytes" => bytes = parse_usize(args.next(), "--bytes")?,
            "--help" | "-h" => {
                println!("lzf-epoch-scratch-harness [--bench|--golden] [--reps N] [--bytes N]");
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    match mode {
        Mode::Bench => bench(reps, bytes),
        Mode::Golden => golden(),
    }
}

fn bench(reps: usize, bytes: usize) -> Result<(), String> {
    if bytes <= 20 {
        return Err(
            "--bytes must be > 20 so the RDB LZF compression policy would attempt it".into(),
        );
    }
    let payloads = build_payloads(bytes);
    let started = Instant::now();
    let mut checksum = 0usize;
    let mut compressed = 0usize;
    let mut total_out = 0usize;
    for i in 0..reps {
        let payload = black_box(&payloads[i % payloads.len()]);
        let budget = payload.len().saturating_sub(4);
        if let Some(out) = lzf_compress(payload, budget) {
            checksum ^= out.len();
            checksum = checksum.wrapping_add(usize::from(out[0]));
            total_out = total_out.wrapping_add(out.len());
            compressed += 1;
        } else {
            checksum ^= payload.len();
        }
    }
    let elapsed = started.elapsed();
    let seconds = elapsed.as_secs_f64();
    let table_bytes_per_call = 65_536usize * std::mem::size_of::<u32>();
    println!(
        "lzf_compress_many reps={reps} bytes={bytes} payloads={} seconds={seconds:.9} ops_per_sec={:.3} compressed={compressed} total_out={total_out} checksum={checksum} table_bytes_per_call={table_bytes_per_call} logical_table_bytes_touched={}",
        payloads.len(),
        reps as f64 / seconds,
        reps.saturating_mul(table_bytes_per_call),
    );
    Ok(())
}

fn golden() -> Result<(), String> {
    for (name, payload) in [
        ("xs30", vec![b'x'; 30]),
        ("mixed96", patterned_payload(96, 0x5a)),
        ("rle96", repeating_payload(96, b'A')),
        ("short21", repeating_payload(21, b'Q')),
        ("rawish96", rawish_payload(96)),
    ] {
        let budget = payload.len().saturating_sub(4);
        match lzf_compress(&payload, budget) {
            Some(out) => {
                println!("{name}:some:{}:{}", out.len(), hex_bytes(&out));
            }
            None => {
                println!("{name}:none");
            }
        }
    }
    Ok(())
}

fn build_payloads(bytes: usize) -> Vec<Vec<u8>> {
    vec![
        repeating_payload(bytes, b'A'),
        repeating_payload(bytes, b'0'),
        patterned_payload(bytes, 0x11),
        patterned_payload(bytes, 0x5a),
        mostly_repeating_payload(bytes, 0x31),
        mostly_repeating_payload(bytes, 0xa7),
        rawish_payload(bytes),
    ]
}

fn repeating_payload(bytes: usize, byte: u8) -> Vec<u8> {
    vec![byte; bytes]
}

fn patterned_payload(bytes: usize, seed: u8) -> Vec<u8> {
    (0..bytes)
        .map(|i| {
            let block = (i / 7) as u8;
            seed.wrapping_add(block % 5).wrapping_add((i % 3) as u8)
        })
        .collect()
}

fn mostly_repeating_payload(bytes: usize, seed: u8) -> Vec<u8> {
    (0..bytes)
        .map(|i| {
            if i % 17 == 0 {
                seed.wrapping_add(i as u8)
            } else {
                b'a'.wrapping_add((i % 4) as u8)
            }
        })
        .collect()
}

fn rawish_payload(bytes: usize) -> Vec<u8> {
    let mut x = 0x243f_6a88_85a3_08d3_u64;
    let mut out = Vec::with_capacity(bytes);
    for _ in 0..bytes {
        x ^= x << 7;
        x ^= x >> 9;
        x ^= x << 8;
        out.push(x as u8);
    }
    out
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn parse_usize(value: Option<String>, flag: &str) -> Result<usize, String> {
    let value = value.ok_or_else(|| format!("{flag} requires a value"))?;
    value
        .parse::<usize>()
        .map_err(|err| format!("invalid {flag} value {value:?}: {err}"))
}

enum Mode {
    Bench,
    Golden,
}
