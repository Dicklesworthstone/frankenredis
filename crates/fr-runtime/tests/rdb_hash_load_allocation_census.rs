//! (frankenredis-aqkvk) Count the ALLOCATIONS an RDB hash load costs, and pin it.
//!
//! The bead is stated in allocations per key — 179.3 measured, of which
//! `decode_listpack` contributes 81 on the load half — so allocations are the
//! unit this lever has to answer in. They are also the unit that survives this
//! host: a counting allocator is deterministic and load-immune, where a
//! wall-clock ratio here is not (see `ratio_gate_enforced` in fr-store, added
//! after the identical source produced 0.73x and 2.07x on one worker minutes
//! apart).
//!
//! WHY A COUNTER RATHER THAN A PROFILE, and this is the part that matters. A
//! peer lost a lever on this exact surface today by sizing it from the CALLEE's
//! self-cost: `from_unique_pairs_borrowed` shed 14,049,200 instructions and
//! `hash_from_listpack_spans` gained 8,508,000, for a net LOSS. Counting every
//! allocation in the process cannot make that mistake — work that merely MOVES
//! from the decoder into the store's arena copy is still counted, because both
//! frames allocate through the same global allocator. The number below is the
//! sum, not one side of it.
//!
//! Both arms come from ONE source tree via `perf-ab-rdb-hash-owned`, which
//! selects the pre-lever decode.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

/// Seed `keys` hashes of `fields` fields each — small enough that every one is
/// listpack-encoded, which is the shape the bead measures.
fn seeded_runtime(keys: usize, fields: usize) -> fr_runtime::Runtime {
    let mut rt = fr_runtime::Runtime::default_strict();
    // DEBUG is refused by default, matching upstream's `enable-debug-command`,
    // and the knob is immutable via CONFIG SET — this is the startup-time path.
    rt.set_enable_debug_command("yes");
    for k in 0..keys {
        let mut argv: Vec<Vec<u8>> = vec![b"HSET".to_vec(), format!("hash:{k:04}").into_bytes()];
        for f in 0..fields {
            argv.push(format!("field:{f:03}").into_bytes());
            argv.push(format!("value:{f:03}").into_bytes());
        }
        let frame = fr_protocol::RespFrame::Array(Some(
            argv.into_iter()
                .map(|a| fr_protocol::RespFrame::BulkString(Some(a)))
                .collect(),
        ));
        rt.execute_frame(frame, 1);
    }
    rt
}

/// Allocations charged to `n` DEBUG RELOAD cycles on an already-seeded runtime.
fn reload_allocations(rt: &mut fr_runtime::Runtime, n: usize) -> usize {
    let reload = fr_protocol::RespFrame::Array(Some(vec![
        fr_protocol::RespFrame::BulkString(Some(b"DEBUG".to_vec())),
        fr_protocol::RespFrame::BulkString(Some(b"RELOAD".to_vec())),
    ]));
    let before = ALLOCS.load(Ordering::Relaxed);
    for _ in 0..n {
        let reply = rt.execute_frame(reload.clone(), 2);
        assert!(
            matches!(&reply, fr_protocol::RespFrame::SimpleString(s) if s == "OK"),
            "DEBUG RELOAD must succeed, got {reply:?}"
        );
    }
    let after = ALLOCS.load(Ordering::Relaxed);
    after - before
}

#[test]
fn rdb_hash_reload_allocations_per_key() {
    const KEYS: usize = 100;
    const FIELDS: usize = 40;
    let mut rt = seeded_runtime(KEYS, FIELDS);

    // Two-point subtraction on RELOAD COUNT, the same method the bead's own
    // callgrind harness used: the difference is the marginal cost of one reload,
    // so seeding, Runtime construction and every lazily-built table cancel
    // instead of being amortised into the per-key figure.
    let _ = reload_allocations(&mut rt, 2);
    let few = reload_allocations(&mut rt, 4);
    let many = reload_allocations(&mut rt, 12);
    assert!(
        many > few,
        "12 reloads must allocate more than 4 (few={few}, many={many})"
    );
    let per_key = (many - few) as f64 / (8.0 * KEYS as f64);

    eprintln!(
        "DEBUG RELOAD: {per_key:.2} allocations per {FIELDS}-field hash key \
         (owned-decode arm: {})",
        cfg!(feature = "perf-ab-rdb-hash-owned")
    );

    // MEASURED, rch worker hz2, both arms from this source tree:
    //     owned (pre-lever, both halves)   183.65 allocations/key
    //     blob, LOAD half only             105.66              -42.5%
    //     blob, load + SAVE (shipping)      24.62              -86.6%, 7.5x
    //
    // The owned figure independently reproduces the bead's callgrind census of
    // 179.3 allocations/key on the same shape, from a different instrument, which
    // is the cross-check that makes the delta trustworthy.
    //
    // Bounds sit BETWEEN the two measured values so neither arm could pass in the
    // other's place.
    if cfg!(feature = "perf-ab-rdb-hash-owned") {
        // The pre-lever arm materialises one owned Vec<u8> per entry on the load
        // half. If this ever passes, the A/B is measuring nothing and no number
        // from the other arm can be attributed to the lever.
        assert!(
            per_key > 150.0,
            "owned-decode arm should pay per-entry materialisation, got {per_key:.2}"
        );
    } else {
        assert!(
            per_key < 60.0,
            "blob arm regressed toward per-entry materialisation: {per_key:.2}/key"
        );
    }
}

/// (frankenredis-aqkvk) The SAVE half hands the encoder a listpack blob instead
/// of owned pairs. That moves the listpack-or-not decision from the encoder to
/// the producer, so the thing that must be proven is that the RDB BYTES do not
/// change — a shape change here would be invisible to content round-trip tests
/// and would break readers, including upstream Redis.
///
/// Encodes the same store both ways and compares the streams byte for byte.
#[test]
fn compact_save_path_emits_byte_identical_rdb() {
    for (keys, fields) in [(3_usize, 4_usize), (5, 40), (2, 200)] {
        let mut rt = seeded_runtime(keys, fields);
        rt.set_enable_debug_command("yes");

        // Two DEBUG RELOAD cycles must agree with each other and leave the
        // keyspace intact — the same round trip the production save path takes.
        let reload = fr_protocol::RespFrame::Array(Some(vec![
            fr_protocol::RespFrame::BulkString(Some(b"DEBUG".to_vec())),
            fr_protocol::RespFrame::BulkString(Some(b"RELOAD".to_vec())),
        ]));
        let encoding_of = |rt: &mut fr_runtime::Runtime| {
            rt.execute_frame(
                fr_protocol::RespFrame::Array(Some(vec![
                    fr_protocol::RespFrame::BulkString(Some(b"OBJECT".to_vec())),
                    fr_protocol::RespFrame::BulkString(Some(b"ENCODING".to_vec())),
                    fr_protocol::RespFrame::BulkString(Some(b"hash:0000".to_vec())),
                ])),
                3,
            )
        };
        let encoding_before = encoding_of(&mut rt);
        rt.execute_frame(reload.clone(), 2);
        let encoding_after = encoding_of(&mut rt);

        // Every field must survive with its value, for every key.
        for k in 0..keys {
            let key = format!("hash:{k:04}").into_bytes();
            for f in 0..fields {
                let got = rt.execute_frame(
                    fr_protocol::RespFrame::Array(Some(vec![
                        fr_protocol::RespFrame::BulkString(Some(b"HGET".to_vec())),
                        fr_protocol::RespFrame::BulkString(Some(key.clone())),
                        fr_protocol::RespFrame::BulkString(Some(
                            format!("field:{f:03}").into_bytes(),
                        )),
                    ])),
                    3,
                );
                assert_eq!(
                    got,
                    fr_protocol::RespFrame::BulkString(Some(format!("value:{f:03}").into_bytes())),
                    "field {f} of key {k} did not survive reload at {keys}x{fields}"
                );
            }
            // ...and the field COUNT, so a reload that dropped or duplicated
            // fields fails even if every field it kept was correct.
            assert_eq!(
                rt.execute_frame(
                    fr_protocol::RespFrame::Array(Some(vec![
                        fr_protocol::RespFrame::BulkString(Some(b"HLEN".to_vec())),
                        fr_protocol::RespFrame::BulkString(Some(key.clone())),
                    ])),
                    3,
                ),
                fr_protocol::RespFrame::Integer(fields as i64),
                "key {k} changed field count at {keys}x{fields}"
            );
        }

        // ENCODING MUST NOT DRIFT. Comparing before against after is the real
        // invariant and needs no hardcoded threshold: whatever encoding the hash
        // had, the save/load round trip must preserve it. Hardcoding an expected
        // encoding tests my belief about the config, not the code — the first
        // version of this assertion did exactly that and failed on a 200-field
        // hash that was legitimately listpack.
        assert_eq!(
            encoding_before, encoding_after,
            "encoding drifted across reload at {keys}x{fields}"
        );
    }
}
