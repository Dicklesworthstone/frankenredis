//! (frankenredis-z2ce3) Count the allocations a BORROWED read costs, and pin it.
//!
//! THE LEVER. Eight borrowed fast paths took an unconditional `to_vec()` whose only
//! reader was the LAZY metrics closure — which runs via `argv.get_or_insert_with`
//! only when slowlog or latency sampling fires, both off by default. So the default
//! path allocated a `Vec` plus one `Vec<u8>` per key to build an argv that was then
//! never built. TOUCH and EXISTS are fixed here; both complete their work entirely
//! on borrows, so nothing else ever wanted owned keys.
//!
//! WHY A COUNT AND NOT A CALL-STYLE ASSERTION. An earlier draft asserted that the
//! store fn accepts both owned and borrowed keys. That pins a PROXY: a generic
//! signature does not prevent an internal copy, so someone can keep the signature,
//! copy inside, and the assertion stays green while every allocation returns. This
//! pins the property directly and reddens wherever the copy is reintroduced.
//!
//! WHAT THIS TEST IS NOT. It links `CountingAlloc`, not the shipping mimalloc, so it
//! counts allocation CALLS and says nothing about their cost. THE CENSUS PINS THE
//! INVARIANT; A CALLGRIND ROW SIZES IT. Two small mimalloc allocations may be only a
//! few percent of `touch_missing`, and a small cost number must not be read as "the
//! invariant does not matter".
//!
//! `#[global_allocator]` is binary-wide, which is why this lives under `tests/`.
//!
//! NOTE the counter hooks `realloc` as well as `alloc`: growth-by-realloc is
//! invisible to an alloc-only counter, and a sibling census on the bignum path
//! depends on that. Do not "simplify" it away.

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

/// Two-point subtraction on CALL COUNT: the difference is the marginal cost of one
/// op, so construction and any lazily-built table cancel rather than being
/// amortised into the per-op figure.
fn per_op<F: FnMut(&mut fr_runtime::Runtime)>(rt: &mut fr_runtime::Runtime, mut op: F) -> f64 {
    let mut probe = |rt: &mut fr_runtime::Runtime, n: usize| -> usize {
        let before = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..n {
            op(rt);
        }
        ALLOCS.load(Ordering::Relaxed) - before
    };
    let _ = probe(rt, 64);
    let few = probe(rt, 128);
    let many = probe(rt, 384);
    many.saturating_sub(few) as f64 / 256.0
}

#[test]
fn borrowed_touch_and_exists_allocate_nothing_per_key_by_default() {
    let mut rt = fr_runtime::Runtime::default_strict();
    let keys: [&[u8]; 2] = [b"nosuch:a", b"nosuch:b"];

    let touch = per_op(&mut rt, |rt| {
        // A None means the borrowed route DECLINED its gate and the census would be
        // counting nothing while passing — the vacuous-and-green failure mode.
        assert_eq!(
            rt.execute_plain_touch_borrowed(&keys, 1),
            Some(fr_protocol::RespFrame::Integer(0)),
            "borrowed TOUCH must serve missing keys and report 0"
        );
    });
    let exists = per_op(&mut rt, |rt| {
        assert_eq!(
            rt.execute_plain_exists_multi_borrowed(&keys, 1),
            Some(fr_protocol::RespFrame::Integer(0)),
            "borrowed EXISTS must serve missing keys and report 0"
        );
    });

    // THE CONTROL ARM, measured in the SAME test rather than a sibling one.
    //
    // `ALLOCS` is a process-global counter and cargo runs tests in a binary
    // CONCURRENTLY, so a sibling test doing its own allocating corrupts these
    // figures nondeterministically. That is not hypothetical: the control started
    // life as its own #[test], passed twice in isolation, and then failed under the
    // full suite. Anything that counts a global must be measured in one test.
    //
    // Its purpose is unchanged: a bound on the fast path alone cannot distinguish
    // "the lever eliminated the allocations" from "this census cannot see
    // allocations at all". A generic owned-argv command MUST read well above the
    // fast path's bound, or every figure here is meaningless however green.
    let control = per_op(&mut rt, |rt| {
        rt.execute_frame(
            fr_protocol::RespFrame::Array(Some(vec![
                fr_protocol::RespFrame::BulkString(Some(b"TOUCH".to_vec())),
                fr_protocol::RespFrame::BulkString(Some(b"nosuch:a".to_vec())),
            ])),
            1,
        );
    });

    eprintln!("borrowed TOUCH  (2 missing keys): {touch:.3} allocations/op");
    eprintln!("borrowed EXISTS (2 missing keys): {exists:.3} allocations/op");
    eprintln!("generic owned-argv control:       {control:.3} allocations/op");

    assert!(
        control >= 2.0,
        "control arm allocated only {control:.3}/op — the census cannot see \
         allocations, so the borrowed figures below prove nothing"
    );

    // Before the lever this was >= 3 per op — one Vec plus one to_vec() per key,
    // two keys. After it is 0: the routes hold only borrows and the metrics closure
    // is lazy. The bound sits BETWEEN the two states so neither can pass in the
    // other's place.
    assert!(
        touch < 1.0,
        "borrowed TOUCH still allocates per call: {touch:.3}/op — the metrics \
         closure is lazy, so the default path should allocate nothing here"
    );
    assert!(
        exists < 1.0,
        "borrowed EXISTS still allocates per call: {exists:.3}/op"
    );
}
