use fr_store::Store;
use std::env;
use std::hint::black_box;
use std::time::Instant;

const KEY: &[u8] = b"z";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    ColdRange,
    OneShot,
    Zadd,
    Golden,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = parse_mode(&args);
    let n = parse_usize_arg(&args, "--n").unwrap_or(100_000);
    let queries = parse_usize_arg(&args, "--queries").unwrap_or(5_000);
    let start = parse_i64_arg(&args, "--start").unwrap_or((n as i64) * 9 / 10);
    let stop = parse_i64_arg(&args, "--stop").unwrap_or(start);

    match mode {
        Mode::ColdRange => run_cold_range(n, queries, start, stop),
        Mode::OneShot => run_cold_range(n, 1, start, stop),
        Mode::Zadd => run_zadd(n),
        Mode::Golden => run_golden(n, start, stop),
    }
}

fn run_cold_range(n: usize, queries: usize, start: i64, stop: i64) {
    let mut store = seeded_store(n);
    let started = Instant::now();
    let mut checksum = 0_u128;
    for query in 0..queries {
        let members = store
            .zrange(black_box(KEY), black_box(start), black_box(stop), query as u64)
            .expect("zrange");
        checksum = checksum.wrapping_add(hash_members(&members));
    }
    let elapsed = started.elapsed();
    println!("mode=ColdRange");
    println!("n={n}");
    println!("queries={queries}");
    println!("start={start}");
    println!("stop={stop}");
    println!("elapsed_ns={}", elapsed.as_nanos());
    println!("checksum={checksum}");
}

fn run_zadd(n: usize) {
    let mut store = Store::new();
    let started = Instant::now();
    for i in 0..n {
        let member = format!("member-{i:08}").into_bytes();
        store
            .zadd(black_box(KEY), &[(i as f64, member)], i as u64)
            .expect("zadd");
    }
    let elapsed = started.elapsed();
    let first = store.zrange(KEY, 0, 2, n as u64 + 1).expect("first");
    let last = store
        .zrevrange(KEY, 0, 2, n as u64 + 2)
        .expect("last");
    println!("mode=Zadd");
    println!("n={n}");
    println!("elapsed_ns={}", elapsed.as_nanos());
    println!("first={}", join_members(&first));
    println!("last={}", join_members(&last));
}

fn run_golden(n: usize, start: i64, stop: i64) {
    let mut store = seeded_store(n);
    let first = store.zrange(KEY, 0, 2, 1).expect("first");
    let deep = store.zrange(KEY, start, stop, 2).expect("deep");
    let rev = store.zrevrange(KEY, 0, 2, 3).expect("rev");
    let scored = store
        .zrange_withscores(KEY, start, stop, 4)
        .expect("deep scored");
    let rank = deep
        .first()
        .and_then(|member| store.zrank(KEY, member, 5).expect("zrank"));
    println!("mode=Golden");
    println!("n={n}");
    println!("start={start}");
    println!("stop={stop}");
    println!("first={}", join_members(&first));
    println!("deep={}", join_members(&deep));
    println!("rev={}", join_members(&rev));
    println!("scored={}", join_scored(&scored));
    println!("rank={rank:?}");
}

fn seeded_store(n: usize) -> Store {
    let mut store = Store::new();
    let mut pairs = Vec::with_capacity(n);
    for i in 0..n {
        let member = format!("member-{i:08}").into_bytes();
        pairs.push((i as f64, member));
    }
    store.zadd(KEY, &pairs, 0).expect("zadd setup");
    store
}

fn parse_mode(args: &[String]) -> Mode {
    match parse_string_arg(args, "--mode").as_deref() {
        Some("cold-range") | None => Mode::ColdRange,
        Some("one-shot") => Mode::OneShot,
        Some("zadd") => Mode::Zadd,
        Some("golden") => Mode::Golden,
        Some(other) => panic!("unknown --mode {other}"),
    }
}

fn parse_usize_arg(args: &[String], name: &str) -> Option<usize> {
    parse_string_arg(args, name).map(|value| value.parse().expect("usize arg"))
}

fn parse_i64_arg(args: &[String], name: &str) -> Option<i64> {
    parse_string_arg(args, name).map(|value| value.parse().expect("i64 arg"))
}

fn parse_string_arg(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find_map(|pair| (pair[0] == name).then(|| pair[1].clone()))
}

fn hash_members(members: &[Vec<u8>]) -> u128 {
    let mut h = 0xcbf2_9ce4_8422_2325_u128;
    for member in members {
        for &byte in member {
            h ^= byte as u128;
            h = h.wrapping_mul(0x0000_0001_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0001_0000_01b3);
    }
    h
}

fn join_members(members: &[Vec<u8>]) -> String {
    members
        .iter()
        .map(|member| String::from_utf8_lossy(member))
        .collect::<Vec<_>>()
        .join(",")
}

fn join_scored(scored: &[(Vec<u8>, f64)]) -> String {
    scored
        .iter()
        .map(|(member, score)| format!("{}:{score}", String::from_utf8_lossy(member)))
        .collect::<Vec<_>>()
        .join(",")
}
