//! (frankenredis-iqicb) Count the ALLOCATIONS an INCRBYFLOAT costs, and pin the exact
//! fast path with the only instrument that can see it.
//!
//! WHY A COUNTER AND NOT A DIFFERENTIAL. The exact-decimal fast path in fr-store is
//! BIT-IDENTICAL to the bignum path by construction — that is its correctness argument,
//! and it is asserted three ways in fr-store's own tests. The consequence is that the
//! path has NO behavioural signature: a version that silently declined every input
//! would return identical replies, pass every differential and reply-shape test, and be
//! a perfect no-op. Nothing functional can tell whether it fired.
//!
//! WHY ALLOCATIONS ARE THE RIGHT UNIT. `BigNat` is `Vec<u32>`-backed and
//! `from_decimal_digits` grows it one `mul_small`/`add_small` per digit, so a bignum
//! parse costs a SEQUENCE OF REALLOCS rather than a single allocation, and `add_abs`
//! allocates a fresh `Vec` per add. The fast path does `sig * 5^exp` in u64 and
//! constructs no `BigNat` at all. Note the counter below hooks `realloc` as well as
//! `alloc` — without that, most of the growth would be invisible.
//!
//! THIS COUNTS CALLS, NOT COST. It links `CountingAlloc`, not the shipping mimalloc, so
//! it says nothing about how expensive those allocations are. The census pins the
//! INVARIANT; a callgrind row SIZES it. Keeping the two apart matters: a modest cost
//! figure must not be read as "the invariant does not matter".
//!
//! THE CONTROL ARM IS LOAD-BEARING. A bound on the fast arm alone cannot distinguish
//! "the fast path eliminated the bignum" from "this census cannot see bignum allocation
//! at all". The non-dyadic arm below MUST decline the fast path, so it pins that the
//! instrument has resolution before the fast arm's bound is read.

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
    // Counted deliberately: BigNat's per-digit growth appears here, not in `alloc`.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn command(args: &[&[u8]]) -> fr_protocol::RespFrame {
    fr_protocol::RespFrame::Array(Some(
        args.iter()
            .map(|a| fr_protocol::RespFrame::BulkString(Some(a.to_vec())))
            .collect(),
    ))
}

/// Allocations attributable to one INCRBYFLOAT on `key`, by two-point subtraction so
/// per-process construction and lazily-built tables cancel exactly.
fn allocs_per_op(rt: &mut fr_runtime::Runtime, key: &[u8], incr: &[u8], ts0: u64) -> f64 {
    const WARM: usize = 64;
    const LOW: usize = 128;
    const HIGH: usize = 384;

    let mut run = |n: usize, base: u64| -> usize {
        let before = ALLOCS.load(Ordering::Relaxed);
        for i in 0..n {
            let reply = rt.execute_frame(command(&[b"INCRBYFLOAT", key, incr]), base + i as u64);
            // ASSERTED INSIDE THE COUNTING LOOP. If the op errored or declined, the
            // census would count almost nothing and PASS — vacuous-and-green. A bulk
            // reply is the proof the work actually happened.
            assert!(
                matches!(reply, fr_protocol::RespFrame::BulkString(Some(_))),
                "INCRBYFLOAT {key:?} {incr:?} did not return a value; the census would \
                 otherwise be counting an error path"
            );
        }
        ALLOCS.load(Ordering::Relaxed) - before
    };

    let _ = run(WARM, ts0);
    let low = run(LOW, ts0 + 1_000);
    let high = run(HIGH, ts0 + 10_000);
    (high as f64 - low as f64) / (HIGH - LOW) as f64
}

/// (frankenredis-iqicb) The exact fast path must eliminate the bignum on dyadic input,
/// and the control must prove the census can see a bignum when one is built.
///
/// 1.5 is 3/2 — a dyadic rational, so `5^1` divides its significand and the value has an
/// exact binary representation. 0.3333333333333333 is 3333333333333333 * 10^-16 and
/// 5^16 does not divide it, so the fast path MUST decline and the bignum MUST run.
#[test]
fn exact_window_incrbyfloat_allocates_less_than_the_bignum_control() {
    let mut rt = fr_runtime::Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"fast", b"1.5"]), 1);
    rt.execute_frame(command(&[b"SET", b"slow", b"0.3333333333333333"]), 1);

    // BOTH ARMS INCREMENT BY ZERO so the stored value never changes and every op is
    // identical. An earlier version used 0.25 on the fast arm: INCRBYFLOAT MUTATES, so
    // the value walked 1.5 -> 1.75 -> 2.0 -> ... across 576 ops, growing its digit count
    // while the control's value stayed fixed. That made the arms incomparable and the
    // fast arm allocate MORE (25.00 vs 23.00/op) for a reason that had nothing to do
    // with the fast path. Zero is itself in the exact window (sig == 0), so the fast
    // path is still exercised on both operands.
    let fast = allocs_per_op(&mut rt, b"fast", b"0", 100_000);
    let slow = allocs_per_op(&mut rt, b"slow", b"0", 200_000);

    // A census should report its numbers on SUCCESS, not only when an assertion fires.
    // The figures are the output; the bounds are only the gate.
    println!(
        "INCRBYFLOAT allocations/op: exact-window(1.5)={fast:.2}  bignum-control(1/3)={slow:.2}"
    );

    // CONTROL FIRST. If the bignum arm is near zero the instrument is blind and the
    // fast arm's figure below means nothing at all.
    assert!(
        slow >= 1.0,
        "control arm allocated {slow:.2}/op. The bignum path builds BigNat values whose \
         limbs grow by realloc per digit, so it must allocate. Near-zero here means the \
         census cannot see bignum allocation and NO conclusion can be drawn from the \
         fast arm."
    );

    // THE INVARIANT. The fast path constructs no BigNat, so it must allocate strictly
    // less than the arm that does. Stated as a comparison rather than an absolute
    // bound: `parse_long_double` allocates a digits Vec on BOTH paths (fr-store, the
    // `Vec::with_capacity(significand.len())` before the fast path is reached), so the
    // floor is not zero and an absolute bound would encode that incidental allocation.
    assert!(
        fast < slow,
        "exact-window INCRBYFLOAT allocated {fast:.2}/op against the bignum control's \
         {slow:.2}/op. The fast path does sig * 5^exp in u64 and constructs no BigNat, \
         so it must allocate less — this is either a regression to the bignum path or a \
         fast path that silently declined, which are the same defect wearing different \
         hats."
    );
}
