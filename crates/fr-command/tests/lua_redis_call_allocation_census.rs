//! (frankenredis-w08xv) Count the ALLOCATIONS a `redis.call` costs, and pin the
//! result.
//!
//! The bead's own instruction is to hold this path to an allocation COUNT and
//! not to `instructions:u` — the campaign's history on the Lua path is that
//! instruction shavings sat in the shadow of stalls and did not translate, while
//! the allocator is 22% of an EVAL op and 34% of its D1 misses. Callgrind's
//! `calls=` records answered that question for the server binary; this test
//! answers it deterministically, in-process, on every run.
//!
//! Method is the same two-point subtraction the census used: run the identical
//! script at N and at 2N `redis.call`s and difference the totals, so chunk
//! compilation, `LuaState` construction, KEYS/ARGV setup and the reply frame all
//! cancel exactly and what remains is the marginal cost of one `redis.call`.
//!
//! This test binary installs its own counting `#[global_allocator]`; it does not
//! affect any other target.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use fr_command::lua_eval::eval_script;
use fr_protocol::RespFrame;
use fr_store::Store;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct CountingAlloc;

// SAFETY-equivalent note: this file contains no `unsafe` of its own beyond the
// two forwarding calls the trait requires; every allocation is served by
// `System` unchanged and only a counter is added.
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

/// The SHA-256 of the image that is actually executing this measurement, read
/// from `/proc/self/exe` — the ledger's provenance contract is a hash the
/// benchmarked process reports from INSIDE itself, not one taken of a path that
/// a peer's build may have replaced between the hash and the run.
fn self_image_sha256() -> String {
    use sha2::{Digest, Sha256};
    match std::fs::read("/proc/self/exe") {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }
        Err(e) => format!("<unavailable: {e}>"),
    }
}

/// Allocations charged to one run of `script`.
fn allocations_for(script: &[u8], store: &mut Store) -> usize {
    let before = ALLOCS.load(Ordering::Relaxed);
    let frame = eval_script(script, &[b"census-key".to_vec()], &[], store, 0)
        .expect("census script must succeed");
    let after = ALLOCS.load(Ordering::Relaxed);
    assert_eq!(frame, RespFrame::Integer(1), "census script must return 1");
    after - before
}

/// Marginal allocations per `redis.call`, from the N vs 2N difference.
fn allocations_per_redis_call(body: &str, n: usize) -> f64 {
    let mut store = Store::new();
    // Seed the key the loop reads, then warm BOTH scripts. The chunk cache is
    // keyed on source text, so the N and 2N bodies are separate entries: warming
    // only one of them would charge the other's compilation to the difference
    // and fake a per-call cost that is really a one-off parse.
    eval_script(
        b"redis.call('SET', KEYS[1], 'census-value')",
        &[b"census-key".to_vec()],
        &[],
        &mut store,
        0,
    )
    .expect("seed must succeed");

    let single = format!("for i=1,{n} do {body} end return 1");
    let double = format!("for i=1,{} do {body} end return 1", n * 2);
    for _ in 0..3 {
        let _ = allocations_for(single.as_bytes(), &mut store);
        let _ = allocations_for(double.as_bytes(), &mut store);
    }

    let a = allocations_for(single.as_bytes(), &mut store);
    let b = allocations_for(double.as_bytes(), &mut store);
    assert!(
        b > a,
        "2N must allocate more than N (N={a}, 2N={b}); the census is broken"
    );
    (b - a) as f64 / n as f64
}

/// The two shapes the census is taken on: one string literal (the command name)
/// and three (command name plus two literal arguments).
const ONE_LITERAL: &str = "redis.call('GET', KEYS[1])";
const THREE_LITERALS: &str = "redis.call('SETRANGE', KEYS[1], '0', 'abc')";

#[test]
fn a_string_literal_redis_call_argument_costs_no_allocation() {
    // Measured on thinkstation1, build worker vmi1227854, both arms from this
    // same source tree via `--features perf-ab-lua-argv-direct-bypass`:
    //
    //   shape                                    pre-lever   post-lever
    //   redis.call('GET', KEYS[1])                    3.00         2.00
    //   redis.call('SETRANGE', KEYS[1], '0', 'abc')  14.00        11.00
    //
    // Exactly one allocation per string-literal argument, which is what the
    // builder set out to remove: the evaluator used to copy `Expr::Str`'s
    // `Rc<[u8]>` into a fresh `LuaValue::Str`, move that buffer into argv, and
    // free it one dispatch later. It now copies the bytes into a RECYCLED
    // buffer, so the count no longer scales with the literal count at all.
    //
    // The bounds sit between the two measured values rather than on either one,
    // so allocator or std churn of a whole allocation per call does not fail the
    // gate while a revert of the lever still does.
    let one_literal = allocations_per_redis_call(ONE_LITERAL, 200);
    let three_literals = allocations_per_redis_call(THREE_LITERALS, 200);

    eprintln!(
        "census image sha256 {} (bypass feature: {})",
        self_image_sha256(),
        cfg!(feature = "perf-ab-lua-argv-direct-bypass")
    );
    eprintln!(
        "allocations per redis.call: 1-literal shape {one_literal:.4}, \
         3-literal shape {three_literals:.4}"
    );

    if cfg!(feature = "perf-ab-lua-argv-direct-bypass") {
        // The bypass arm must be MEASURABLY worse, or the A/B is measuring
        // nothing and the numbers above cannot be attributed to the builder.
        assert!(
            one_literal > 2.5,
            "bypass arm should pay for its one literal, got {one_literal:.2}/call"
        );
        assert!(
            three_literals > 12.5,
            "bypass arm should pay for its three literals, got {three_literals:.2}/call"
        );
    } else {
        assert!(
            one_literal < 2.5,
            "redis.call with one string literal now costs {one_literal:.2} allocations per call"
        );
        assert!(
            three_literals < 12.5,
            "redis.call with three string literals now costs {three_literals:.2} \
             allocations per call; two extra literals must cost nothing"
        );
    }
}
