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
    //     owned-decode (pre-lever)   183.63 allocations/key
    //     blob (shipping)            105.66 allocations/key   -42.5%
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
            per_key < 140.0,
            "blob arm regressed toward per-entry materialisation: {per_key:.2}/key"
        );
    }
}
