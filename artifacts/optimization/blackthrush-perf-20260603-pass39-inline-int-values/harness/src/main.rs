use std::env;
use std::process::ExitCode;
use std::time::Instant;

use fr_store::Store;

fn parse_usize_arg(args: &[String], name: &str, default: usize) -> usize {
    args.windows(2)
        .find_map(|pair| (pair[0] == name).then(|| pair[1].parse().ok()).flatten())
        .unwrap_or(default)
}

fn bench(args: &[String]) {
    let reps = parse_usize_arg(args, "--reps", 1_000_000);
    let delta = args
        .windows(2)
        .find_map(|pair| {
            (pair[0] == "--delta")
                .then(|| pair[1].parse::<i64>().ok())
                .flatten()
        })
        .unwrap_or(1);

    let mut store = Store::new();
    store.set(b"n".to_vec(), b"0".to_vec(), None, 0);

    let started = Instant::now();
    let mut checksum = 0_i64;
    for i in 0..reps {
        let value = store
            .incrby(
                std::hint::black_box(b"n"),
                std::hint::black_box(delta),
                i as u64 + 1,
            )
            .expect("incrby");
        checksum ^= value.rotate_left((i & 31) as u32);
    }
    let elapsed = started.elapsed();
    let final_get = store
        .get(b"n", reps as u64 + 2)
        .expect("get")
        .expect("value");
    let final_text = String::from_utf8(final_get).expect("integer utf8");
    let encoding = store
        .object_encoding(b"n", reps as u64 + 3)
        .unwrap_or("missing");
    let memory = store
        .memory_usage_for_key(b"n", reps as u64 + 4)
        .unwrap_or(0);
    let seconds = elapsed.as_secs_f64();
    let ops_per_sec = reps as f64 / seconds.max(f64::MIN_POSITIVE);

    println!(
        "inline_int_incrby reps={reps} delta={delta} seconds={seconds:.9} ops_per_sec={ops_per_sec:.3} checksum={checksum} final={final_text} encoding={encoding} memory={memory}"
    );
}

fn golden() {
    let mut store = Store::new();
    store.set(b"n".to_vec(), b"0".to_vec(), None, 100);
    println!("seed_get={:?}", store.get(b"n", 101).unwrap());
    println!("seed_encoding={:?}", store.object_encoding(b"n", 102));
    println!("seed_memory={:?}", store.memory_usage_for_key(b"n", 103));
    println!("incr={:?}", store.incr(b"n", 104));
    println!("after_incr_get={:?}", store.get(b"n", 105).unwrap());
    println!("after_incr_encoding={:?}", store.object_encoding(b"n", 106));
    println!("incrby_neg={:?}", store.incrby(b"n", -3, 107));
    println!("after_neg_get={:?}", store.get(b"n", 108).unwrap());
    println!("after_neg_encoding={:?}", store.object_encoding(b"n", 109));
    println!("strlen={:?}", store.strlen(b"n", 110));
    println!("memory={:?}", store.memory_usage_for_key(b"n", 111));
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|arg| arg == "--golden") {
        golden();
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|arg| arg == "--bench") {
        bench(&args);
        return ExitCode::SUCCESS;
    }
    eprintln!("usage: inline-int-values-harness --bench [--reps N] [--delta N] | --golden");
    ExitCode::from(2)
}
