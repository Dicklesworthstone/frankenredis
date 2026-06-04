#![forbid(unsafe_code)]

use std::env;
use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

use fr_store::Store;

const DEFAULT_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_REPS: usize = 128;

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
    let mut bytes = DEFAULT_BYTES;
    let mut reps = DEFAULT_REPS;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--golden" => {
                print_golden()?;
                return Ok(());
            }
            "--bytes" => {
                bytes = parse_usize(args.next(), "--bytes")?;
            }
            "--reps" => {
                reps = parse_usize(args.next(), "--reps")?;
            }
            "--help" | "-h" => {
                println!("bitfield-inplace-harness [--bytes N] [--reps N] [--golden]");
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    let key = b"bf";
    let mut store = Store::new();
    store.set(key.to_vec(), vec![0; bytes], None, 0);

    let started = Instant::now();
    let mut checksum = 0_i64;
    for i in 0..reps {
        let value = i64::try_from(i & 1).expect("small toggle value");
        let old = store
            .bitfield_set(black_box(key), 0, 8, value, i as u64 + 1)
            .map_err(|err| format!("bitfield_set failed: {err:?}"))?;
        checksum ^= old;
    }
    let elapsed = started.elapsed();

    let final_value = store
        .bitfield_get(key, 0, 8, false, reps as u64 + 2)
        .map_err(|err| format!("bitfield_get failed: {err:?}"))?;
    let len = store
        .get(key, reps as u64 + 3)
        .map_err(|err| format!("get failed: {err:?}"))?
        .ok_or("missing benchmark key after writes")?
        .len();
    if len != bytes {
        return Err(format!("length changed: expected {bytes}, got {len}"));
    }

    let seconds = elapsed.as_secs_f64();
    println!(
        "bitfield_set_large_string bytes={bytes} reps={reps} seconds={seconds:.9} ops_per_sec={:.3} checksum={checksum} final={final_value}",
        reps as f64 / seconds
    );
    Ok(())
}

fn print_golden() -> Result<(), String> {
    let mut store = Store::new();
    store.set(b"bf".to_vec(), vec![0x12, 0x34, 0x56, 0x78], None, 10);
    let old_a = store
        .bitfield_set(b"bf", 0, 8, 0xAB, 11)
        .map_err(|err| format!("set a failed: {err:?}"))?;
    let get_a = store
        .bitfield_get(b"bf", 0, 8, false, 12)
        .map_err(|err| format!("get a failed: {err:?}"))?;
    let old_b = store
        .bitfield_set(b"bf", 5, 13, 0x1555, 13)
        .map_err(|err| format!("set b failed: {err:?}"))?;
    let get_b = store
        .bitfield_get(b"bf", 5, 13, false, 14)
        .map_err(|err| format!("get b failed: {err:?}"))?;
    let old_missing = store
        .bitfield_set(b"missing", 16, 8, 7, 15)
        .map_err(|err| format!("set missing failed: {err:?}"))?;
    let missing_get = store
        .bitfield_get(b"missing", 16, 8, false, 16)
        .map_err(|err| format!("get missing failed: {err:?}"))?;
    let bf = store
        .get(b"bf", 17)
        .map_err(|err| format!("get bf failed: {err:?}"))?
        .ok_or("missing bf key")?;
    let missing = store
        .get(b"missing", 18)
        .map_err(|err| format!("get missing string failed: {err:?}"))?
        .ok_or("missing generated key")?;

    println!("old_a={old_a}");
    println!("get_a={get_a}");
    println!("old_b={old_b}");
    println!("get_b={get_b}");
    println!("old_missing={old_missing}");
    println!("missing_get={missing_get}");
    println!("bf={}", hex_bytes(&bf));
    println!("missing={}", hex_bytes(&missing));
    Ok(())
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
