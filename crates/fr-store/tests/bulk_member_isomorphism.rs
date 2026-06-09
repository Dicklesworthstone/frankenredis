//! Isomorphism + timing gate for the `BulkMember` generic on
//! `lpush`/`rpush`/`sadd` (frankenredis-11a0n).
//!
//! The store accepts members either as owned `Vec<u8>` (the generic argv path,
//! which clones each member into the container) or as borrowed `&[u8]` (the
//! server's borrowed fast path, which copies each member exactly once directly
//! into the container — eliminating the intermediate `Vec<Vec<u8>>` the old
//! code materialised first). Both must yield byte-identical containers.
//!
//! This is the golden/isomorphism proof for the lever: every assertion checks
//! that the owned and borrowed paths produce identical results. The `--nocapture`
//! timing print is informational (the win is per-member allocation/memcpy
//! elimination, profiled at 2.62% self + 1.56% __memmove in the borrowed push
//! path under LPUSH P16); it is intentionally not asserted on, to avoid CI flake.

use fr_store::Store;
use std::time::Instant;

fn batches() -> Vec<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    // Single-member, small.
    out.push(vec![b"v".to_vec()]);
    // Multi-member, varied lengths (amplifies the per-member copy).
    out.push((0..16).map(|i| vec![b'x'; 1 + (i % 7) * 13]).collect());
    // Large members.
    out.push((0..8).map(|i| vec![b'a' + i as u8; 4096]).collect());
    // Integer-looking members (intset path for sadd).
    out.push((0..32).map(|i| format!("{}", i * 7).into_bytes()).collect());
    // Empty member edge case.
    out.push(vec![Vec::new(), b"after-empty".to_vec()]);
    out
}

/// Run a batch through both the owned and borrowed paths and assert the
/// resulting containers are byte-identical.
fn assert_iso_list(rpush: bool) {
    for (bi, batch) in batches().into_iter().enumerate() {
        let key: Vec<u8> = format!("k{bi}").into_bytes();
        let borrowed: Vec<&[u8]> = batch.iter().map(|v| v.as_slice()).collect();

        let mut owned_store = Store::new();
        let mut borrowed_store = Store::new();
        if rpush {
            owned_store.rpush(&key, &batch, 0).unwrap();
            borrowed_store.rpush(&key, &borrowed, 0).unwrap();
        } else {
            owned_store.lpush(&key, &batch, 0).unwrap();
            borrowed_store.lpush(&key, &borrowed, 0).unwrap();
        }

        let a = owned_store.lrange(&key, 0, -1, 0).unwrap();
        let b = borrowed_store.lrange(&key, 0, -1, 0).unwrap();
        assert_eq!(a, b, "list batch {bi} (rpush={rpush}) diverged");
        // Encoding must also match (listpack vs quicklist decision is per-batch).
        assert_eq!(
            owned_store.object_encoding(&key, 0),
            borrowed_store.object_encoding(&key, 0),
            "list encoding batch {bi} (rpush={rpush}) diverged"
        );
    }
}

fn assert_iso_set() {
    for (bi, batch) in batches().into_iter().enumerate() {
        let key: Vec<u8> = format!("s{bi}").into_bytes();
        let borrowed: Vec<&[u8]> = batch.iter().map(|v| v.as_slice()).collect();

        let mut owned_store = Store::new();
        let mut borrowed_store = Store::new();
        let added_owned = owned_store.sadd(&key, &batch, 0).unwrap();
        let added_borrowed = borrowed_store.sadd(&key, &borrowed, 0).unwrap();
        assert_eq!(
            added_owned, added_borrowed,
            "sadd count batch {bi} diverged"
        );

        let mut a = owned_store.smembers(&key, 0).unwrap();
        let mut b = borrowed_store.smembers(&key, 0).unwrap();
        a.sort();
        b.sort();
        assert_eq!(a, b, "set batch {bi} diverged");
        assert_eq!(
            owned_store.object_encoding(&key, 0),
            borrowed_store.object_encoding(&key, 0),
            "set encoding batch {bi} diverged"
        );
    }
}

#[test]
fn bulk_member_owned_and_borrowed_are_isomorphic() {
    assert_iso_list(true);
    assert_iso_list(false);
    assert_iso_set();
}

#[test]
fn bulk_member_timing_report() {
    // Informational micro-timing of the actual server before/after. Both start
    // from the borrowed `&[&[u8]]` the parser hands us out of the read buffer.
    //
    //   BEFORE: collect an intermediate `Vec<Vec<u8>>` (N copies), then rpush
    //           clones each member into the container (N more copies) = 2N.
    //   AFTER:  rpush the borrowed slice directly, one copy per member = N.
    // Members large/numerous enough that the list promotes to a quicklist
    // (deque), where push_back *moves* the Vec into a retained node — so the
    // saved clone is a real retained allocation, not a cheap transient copied
    // into a listpack buffer and freed. We reuse one store and DEL between
    // iterations so per-command work (not Store::new) dominates the timing.
    const ITERS: usize = 30_000;
    let batch: Vec<Vec<u8>> = (0..64).map(|i| vec![b'a' + (i % 26) as u8; 256]).collect();
    let borrowed: Vec<&[u8]> = batch.iter().map(|v| v.as_slice()).collect();
    let key = b"bench".to_vec();
    let key_slice: &[Vec<u8>] = std::slice::from_ref(&key);

    // BEFORE: intermediate collect then store re-clones.
    let mut store = Store::new();
    let t0 = Instant::now();
    for _ in 0..ITERS {
        let owned: Vec<Vec<u8>> = borrowed.iter().map(|v| v.to_vec()).collect();
        store.rpush(&key, &owned, 0).unwrap();
        store.del(key_slice, 0);
    }
    let before_ns = t0.elapsed().as_nanos() as f64 / ITERS as f64;

    // AFTER: borrowed slice straight in, one copy per member.
    let mut store = Store::new();
    let t1 = Instant::now();
    for _ in 0..ITERS {
        store.rpush(&key, &borrowed, 0).unwrap();
        store.del(key_slice, 0);
    }
    let after_ns = t1.elapsed().as_nanos() as f64 / ITERS as f64;

    println!(
        "bulk_member rpush(64x256B->quicklist) borrowed-input: before(collect+clone)={before_ns:.0}ns/cmd  after(direct)={after_ns:.0}ns/cmd  speedup={:.3}x",
        before_ns / after_ns
    );
}
