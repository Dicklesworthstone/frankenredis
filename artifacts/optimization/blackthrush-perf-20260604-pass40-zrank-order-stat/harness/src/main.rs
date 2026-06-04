use fr_store::Store;
use std::env;
use std::hint::black_box;
use std::time::Instant;

const KEY: &[u8] = b"z";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Rank,
    RevRank,
    Paired,
    Golden,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = parse_mode(&args);
    let n = parse_usize_arg(&args, "--n").unwrap_or(20_000);
    let queries = parse_usize_arg(&args, "--queries").unwrap_or(20_000);

    let mut store = Store::new();
    let mut pairs = Vec::with_capacity(n);
    for i in 0..n {
        let member = format!("member-{i:08}").into_bytes();
        pairs.push((i as f64, member));
    }
    store.zadd(KEY, &pairs, 0).expect("zadd setup");
    let members: Vec<Vec<u8>> = pairs.into_iter().map(|(_, member)| member).collect();

    let started = Instant::now();
    let mut rank_sum = 0_u128;
    let mut revrank_sum = 0_u128;
    for query in 0..queries {
        let idx = deterministic_index(query, n);
        let member = black_box(members[idx].as_slice());
        match mode {
            Mode::Rank => {
                rank_sum += store
                    .zrank(KEY, member, query as u64)
                    .expect("zrank")
                    .expect("rank present") as u128;
            }
            Mode::RevRank => {
                revrank_sum += store
                    .zrevrank(KEY, member, query as u64)
                    .expect("zrevrank")
                    .expect("rank present") as u128;
            }
            Mode::Paired | Mode::Golden => {
                rank_sum += store
                    .zrank(KEY, member, query as u64)
                    .expect("zrank")
                    .expect("rank present") as u128;
                revrank_sum += store
                    .zrevrank(KEY, member, query as u64)
                    .expect("zrevrank")
                    .expect("rank present") as u128;
            }
        }
    }
    let elapsed = started.elapsed();

    let first = store
        .zrange(KEY, 0, 2, queries as u64 + 1)
        .expect("zrange first");
    let last = store
        .zrevrange(KEY, 0, 2, queries as u64 + 2)
        .expect("zrevrange last");

    println!("mode={mode:?}");
    println!("n={n}");
    println!("queries={queries}");
    println!("elapsed_ns={}", elapsed.as_nanos());
    println!("rank_sum={rank_sum}");
    println!("revrank_sum={revrank_sum}");
    println!("first={}", join_members(&first));
    println!("last={}", join_members(&last));
}

fn parse_mode(args: &[String]) -> Mode {
    match parse_string_arg(args, "--mode").as_deref() {
        Some("rank") => Mode::Rank,
        Some("revrank") => Mode::RevRank,
        Some("paired") | None => Mode::Paired,
        Some("golden") => Mode::Golden,
        Some(other) => panic!("unknown --mode {other}"),
    }
}

fn parse_usize_arg(args: &[String], name: &str) -> Option<usize> {
    parse_string_arg(args, name).map(|value| value.parse().expect("usize arg"))
}

fn parse_string_arg(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find_map(|pair| (pair[0] == name).then(|| pair[1].clone()))
}

fn deterministic_index(query: usize, n: usize) -> usize {
    let mixed = (query as u64)
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    (mixed % n as u64) as usize
}

fn join_members(members: &[Vec<u8>]) -> String {
    members
        .iter()
        .map(|member| String::from_utf8_lossy(member))
        .collect::<Vec<_>>()
        .join(",")
}
