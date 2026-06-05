use fr_store::{ScoreBound, Store};
use std::env;
use std::hint::black_box;
use std::time::Instant;

const NOW_MS: u64 = 1_777_000_000_000;

fn mix(mut acc: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        acc ^= u64::from(b);
        acc = acc.wrapping_mul(0x100_0000_01b3);
        acc = acc.rotate_left(5);
    }
    acc
}

fn mix_u64(acc: u64, value: u64) -> u64 {
    mix(acc, &value.to_le_bytes())
}

fn mix_score(acc: u64, score: f64) -> u64 {
    mix_u64(acc, score.to_bits())
}

fn key(i: usize) -> Vec<u8> {
    format!("zset:{i:06}").into_bytes()
}

fn member(i: usize, j: usize) -> Vec<u8> {
    format!("m:{i:06}:{j:03}").into_bytes()
}

fn score(i: usize, j: usize) -> f64 {
    let raw = ((i.wrapping_mul(37) + j.wrapping_mul(17)) % 257) as f64;
    if (i + j) % 31 == 0 {
        -0.0
    } else {
        raw / 8.0
    }
}

fn populate(store: &mut Store, keys: usize, members: usize) -> (u128, u64) {
    let start = Instant::now();
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    for i in 0..keys {
        let mut pairs = Vec::with_capacity(members);
        for j in 0..members {
            pairs.push((score(i, j), member(i, j)));
        }
        let added = store.zadd(&key(i), &pairs, NOW_MS).expect("zadd");
        checksum = mix_u64(checksum, added as u64);
        if i % 17 == 0 {
            let update = [(score(i, members / 2) + 0.125, member(i, members / 2))];
            let changed = store
                .zadd_with_options(&key(i), &update, Default::default(), NOW_MS)
                .expect("zadd update");
            checksum = mix_u64(checksum, changed.0 as u64);
            checksum = mix_u64(checksum, changed.1 as u64);
        }
    }
    (start.elapsed().as_nanos(), checksum)
}

fn read_workload(store: &mut Store, keys: usize, members: usize, checksum: u64) -> (u128, u64) {
    let start = Instant::now();
    let mut checksum = checksum;
    for i in 0..keys {
        let k = key(i);
        for &j in &[0usize, members / 3, members / 2, members - 1] {
            let m = member(i, j);
            if let Some(s) = store.zscore(&k, &m, NOW_MS).expect("zscore") {
                checksum = mix_score(checksum, s);
            }
            if let Some(rank) = store.zrank(&k, &m, NOW_MS).expect("zrank") {
                checksum = mix_u64(checksum, rank as u64);
            }
            if let Some(rank) = store.zrevrank(&k, &m, NOW_MS).expect("zrevrank") {
                checksum = mix_u64(checksum, rank as u64);
            }
        }

        let asc = store
            .zrange_withscores(&k, 0, 11, NOW_MS)
            .expect("zrange withscores");
        checksum = fold_pairs(checksum, &asc);

        let desc = store
            .zrevrange_withscores(&k, 0, 11, NOW_MS)
            .expect("zrevrange withscores");
        checksum = fold_pairs(checksum, &desc);

        let by_score = store
            .zrangebyscore_withscores_limited(
                &k,
                ScoreBound::Inclusive(4.0),
                ScoreBound::Inclusive(28.0),
                false,
                2,
                Some(9),
                NOW_MS,
            )
            .expect("zrangebyscore limited");
        checksum = fold_pairs(checksum, &by_score);

        let by_score_rev = store
            .zrangebyscore_withscores_limited(
                &k,
                ScoreBound::Inclusive(4.0),
                ScoreBound::Inclusive(28.0),
                true,
                2,
                Some(9),
                NOW_MS,
            )
            .expect("zrevrangebyscore limited");
        checksum = fold_pairs(checksum, &by_score_rev);

        let lex = store
            .zrangebylex(&k, b"[m:000000:000", b"+", NOW_MS)
            .expect("zrangebylex");
        for member in lex.iter().take(7) {
            checksum = mix(checksum, member);
        }

        if i % 13 == 0 {
            let blob = store.dump_key(&k, NOW_MS).expect("dump");
            checksum = mix_u64(checksum, blob.len() as u64);
            checksum = mix(checksum, &blob[..blob.len().min(32)]);
        }
    }
    (start.elapsed().as_nanos(), checksum)
}

fn pop_workload(store: &mut Store, keys: usize, checksum: u64) -> (u128, u64) {
    let start = Instant::now();
    let mut checksum = checksum;
    for i in (0..keys).step_by(5) {
        let k = key(i);
        let mins = store.zpopmin_count(&k, 3, NOW_MS).expect("zpopmin count");
        checksum = fold_pairs(checksum, &mins);
        let maxes = store.zpopmax_count(&k, 2, NOW_MS).expect("zpopmax count");
        checksum = fold_pairs(checksum, &maxes);
    }
    (start.elapsed().as_nanos(), checksum)
}

fn fold_pairs(mut checksum: u64, pairs: &[(Vec<u8>, f64)]) -> u64 {
    checksum = mix_u64(checksum, pairs.len() as u64);
    for (member, score) in pairs {
        checksum = mix(checksum, member);
        checksum = mix_score(checksum, *score);
    }
    checksum
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let keys = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(8_192);
    let members = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(48);

    assert!(members > 3, "members must exceed 3");
    let mut store = Store::new();

    let (insert_ns, c0) = populate(&mut store, keys, members);
    let (read_ns, c1) = read_workload(&mut store, keys, members, c0);
    let memory_before_pop = store.estimate_memory_usage_bytes();
    let (pop_ns, c2) = pop_workload(&mut store, keys, c1);
    let digest = store.state_digest();
    let memory_after_pop = store.estimate_memory_usage_bytes();

    black_box(&store);
    println!("keys={keys}");
    println!("members={members}");
    println!("insert_ns={insert_ns}");
    println!("read_ns={read_ns}");
    println!("pop_ns={pop_ns}");
    println!("memory_before_pop={memory_before_pop}");
    println!("memory_after_pop={memory_after_pop}");
    println!("state_digest={digest}");
    println!("checksum={c2}");
}
