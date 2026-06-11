use fr_store::{Store, StreamGroupReadCursor, StreamGroupReadOptions, StreamPendingSummary};
use std::time::Instant;

const KEY: &[u8] = b"st";
const GROUP: &[u8] = b"g";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Summary,
    Golden,
    Xinfo,
    XinfoGolden,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    mode: Mode,
    pending: usize,
    consumers: usize,
    iters: usize,
}

fn main() {
    let config = parse_args();
    let mut store = seed_store(config.pending, config.consumers);
    match config.mode {
        Mode::Summary => run_summary(&mut store, config),
        Mode::Golden => run_golden(&mut store, config),
        Mode::Xinfo => run_xinfo(&mut store, config),
        Mode::XinfoGolden => run_xinfo_golden(&mut store, config),
    }
}

fn parse_args() -> Config {
    let mut config = Config {
        mode: Mode::Summary,
        pending: 50_000,
        consumers: 1_000,
        iters: 1_000,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let Some(value) = args.next() else {
            panic!("missing value for {arg}");
        };
        match arg.as_str() {
            "--mode" => {
                config.mode = match value.as_str() {
                    "summary" => Mode::Summary,
                    "golden" => Mode::Golden,
                    "xinfo" => Mode::Xinfo,
                    "xinfo-golden" => Mode::XinfoGolden,
                    _ => panic!("unknown mode {value}"),
                };
            }
            "--pending" => config.pending = value.parse().expect("pending must be usize"),
            "--consumers" => config.consumers = value.parse().expect("consumers must be usize"),
            "--iters" => config.iters = value.parse().expect("iters must be usize"),
            _ => panic!("unknown argument {arg}"),
        }
    }
    assert!(config.consumers > 0, "consumers must be nonzero");
    assert!(
        config.pending >= config.consumers,
        "pending must cover consumers"
    );
    config
}

fn seed_store(pending: usize, consumers: usize) -> Store {
    let mut store = Store::new();
    let field = [(b"f".to_vec(), b"v".to_vec())];
    for i in 0..pending {
        store
            .xadd(KEY, ((i + 1) as u64, 0), &field, 0)
            .expect("xadd seed");
    }
    assert!(
        store
            .xgroup_create(KEY, GROUP, (0, 0), false, 0)
            .expect("xgroup create"),
        "group must be created"
    );

    let base = pending / consumers;
    let rem = pending % consumers;
    for idx in 0..consumers {
        let count = base + usize::from(idx < rem);
        let consumer = format!("c{idx:06}");
        let rows = store
            .xreadgroup(
                KEY,
                GROUP,
                consumer.as_bytes(),
                StreamGroupReadOptions {
                    cursor: StreamGroupReadCursor::NewEntries,
                    noack: false,
                    count: Some(count),
                },
                10 + idx as u64,
            )
            .expect("xreadgroup seed")
            .expect("xreadgroup rows");
        assert_eq!(rows.len(), count, "seeded pending count must match");
    }
    store
}

fn run_summary(store: &mut Store, config: Config) {
    let started = Instant::now();
    let mut checksum = 0_u128;
    for _ in 0..config.iters {
        let summary = store
            .xpending_summary(KEY, GROUP, 100)
            .expect("summary ok")
            .expect("summary exists");
        checksum = checksum.wrapping_add(summary_checksum(&summary));
    }
    let elapsed = started.elapsed();
    println!("mode=Summary");
    println!("pending={}", config.pending);
    println!("consumers={}", config.consumers);
    println!("iters={}", config.iters);
    println!("elapsed_ns={}", elapsed.as_nanos());
    println!("checksum={checksum}");
}

fn run_golden(store: &mut Store, config: Config) {
    let summary = store
        .xpending_summary(KEY, GROUP, 100)
        .expect("summary ok")
        .expect("summary exists");
    let first = summary.1.expect("first pending");
    let last = summary.2.expect("last pending");
    let consumers = &summary.3;
    let mid = consumers.len() / 2;
    println!("mode=Golden");
    println!("pending={}", config.pending);
    println!("consumers={}", config.consumers);
    println!("total={}", summary.0);
    println!("first={}-{}", first.0, first.1);
    println!("last={}-{}", last.0, last.1);
    println!("consumer_first={}", format_consumer(&consumers[0]));
    println!("consumer_mid={}", format_consumer(&consumers[mid]));
    println!(
        "consumer_last={}",
        format_consumer(&consumers[consumers.len() - 1])
    );
    println!("summary_checksum={}", summary_checksum(&summary));
}

fn run_xinfo(store: &mut Store, config: Config) {
    let started = Instant::now();
    let mut checksum = 0_u128;
    for _ in 0..config.iters {
        let rows = store
            .xinfo_consumers(KEY, GROUP, 100)
            .expect("xinfo ok")
            .expect("xinfo exists");
        checksum = checksum.wrapping_add(xinfo_checksum(&rows));
    }
    let elapsed = started.elapsed();
    println!("mode=Xinfo");
    println!("pending={}", config.pending);
    println!("consumers={}", config.consumers);
    println!("iters={}", config.iters);
    println!("elapsed_ns={}", elapsed.as_nanos());
    println!("checksum={checksum}");
}

fn run_xinfo_golden(store: &mut Store, config: Config) {
    let rows = store
        .xinfo_consumers(KEY, GROUP, 100)
        .expect("xinfo ok")
        .expect("xinfo exists");
    let mid = rows.len() / 2;
    println!("mode=XinfoGolden");
    println!("pending={}", config.pending);
    println!("consumers={}", config.consumers);
    println!("rows={}", rows.len());
    println!("consumer_first={}", format_xinfo_consumer(&rows[0]));
    println!("consumer_mid={}", format_xinfo_consumer(&rows[mid]));
    println!(
        "consumer_last={}",
        format_xinfo_consumer(&rows[rows.len() - 1])
    );
    println!("xinfo_checksum={}", xinfo_checksum(&rows));
}

fn format_consumer((name, count): &(Vec<u8>, usize)) -> String {
    format!("{}:{count}", String::from_utf8_lossy(name))
}

fn format_xinfo_consumer((name, pending, idle, inactive): &(Vec<u8>, usize, u64, i64)) -> String {
    format!(
        "{}:{pending}:{idle}:{inactive}",
        String::from_utf8_lossy(name)
    )
}

fn summary_checksum(summary: &StreamPendingSummary) -> u128 {
    let mut acc = summary.0 as u128;
    for id in [summary.1, summary.2].into_iter().flatten() {
        acc = acc.wrapping_mul(1_000_003).wrapping_add(id.0 as u128);
        acc = acc.wrapping_mul(1_000_003).wrapping_add(id.1 as u128);
    }
    for (name, count) in &summary.3 {
        for &byte in name {
            acc = acc.wrapping_mul(257).wrapping_add(byte as u128);
        }
        acc = acc.wrapping_mul(1_000_003).wrapping_add(*count as u128);
    }
    acc
}

fn xinfo_checksum(rows: &[(Vec<u8>, usize, u64, i64)]) -> u128 {
    let mut acc = rows.len() as u128;
    for (idx, (name, pending, idle, inactive)) in rows.iter().enumerate() {
        for &byte in name {
            acc = acc
                .wrapping_mul(257)
                .wrapping_add(byte as u128)
                .wrapping_add(idx as u128);
        }
        acc = acc.wrapping_mul(1_000_003).wrapping_add(*pending as u128);
        acc = acc.wrapping_mul(1_000_003).wrapping_add(*idle as u128);
        acc = acc
            .wrapping_mul(1_000_003)
            .wrapping_add((*inactive as i128) as u128);
    }
    acc
}
