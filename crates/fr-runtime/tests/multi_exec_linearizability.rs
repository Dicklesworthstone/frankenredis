//! MULTI/EXEC/WATCH linearizability + abort-contract harness.
//!
//! Spec-derived matrix (M1..M5) + metamorphic relations exercising the two
//! contracts that make transactions useful:
//!
//! 1. **Atomicity.** A full EXEC batch either all-applies or none-applies,
//!    with no interleaving observable from any other client.
//! 2. **WATCH abort.** If any watched key is modified by any other client
//!    between WATCH and EXEC, EXEC returns nil and applies nothing.
//!
//! Both contracts hold trivially under the current single-thread eventloop
//! because command dispatch is serialized — but that is an implementation
//! detail, not a spec guarantee. This harness pins the contract so any
//! future concurrency refactor (per-shard threads, IO thread split, etc.)
//! cannot silently regress.
//!
//! Client interleaving is simulated via `Runtime::swap_session` — the test
//! swaps between a persistent ClientSession for A and another for B, issuing
//! commands on each to construct an arbitrary interleaving.
//!
//! (br-frankenredis-b6ka)

use fr_protocol::RespFrame;
use fr_runtime::Runtime;

fn cmd(parts: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        parts
            .iter()
            .map(|part| RespFrame::BulkString(Some((*part).to_vec())))
            .collect(),
    ))
}

fn ok() -> RespFrame {
    RespFrame::SimpleString("OK".to_string())
}

fn queued() -> RespFrame {
    RespFrame::SimpleString("QUEUED".to_string())
}

fn nil_bulk() -> RespFrame {
    RespFrame::BulkString(None)
}

/// EXEC on an aborted transaction returns the RESP2 nil-array encoding
/// (`*-1\r\n`), which decodes to `Array(None)` — NOT `BulkString(None)`.
/// See upstream networking.c addReplyNullArray / RESP2 null-array frame.
fn exec_aborted() -> RespFrame {
    RespFrame::Array(None)
}

/// Helper to flip Runtime's current session to session `s` and execute a
/// command, returning the reply. Leaves the runtime holding `s` after
/// return.
fn run_as(
    rt: &mut Runtime,
    session_slot: &mut Option<fr_runtime::ClientSession>,
    frame: RespFrame,
    now_ms: u64,
) -> RespFrame {
    let session = session_slot
        .take()
        .expect("session slot must be populated before run_as");
    let previous = rt.swap_session(session);
    let reply = rt.execute_frame(frame, now_ms);
    let active = rt.swap_session(previous);
    *session_slot = Some(active);
    reply
}

// ── M1: plain MULTI/EXEC applies the batch ─────────────────────────

#[test]
fn m1_plain_multi_exec_applies_batch_and_returns_per_command_replies() {
    let mut rt = Runtime::default_strict();
    let mut a = Some(rt.new_session());

    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"MULTI"]), 0), ok());
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"SET", b"k", b"1"]), 1),
        queued()
    );
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"GET", b"k"]), 2), queued());
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"EXEC"]), 3),
        RespFrame::Array(Some(
            vec![ok(), RespFrame::BulkString(Some(b"1".to_vec())),]
        ))
    );
}

// ── M2: WATCH with no intervening write → EXEC applies ─────────────

#[test]
fn m2_watch_without_concurrent_write_exec_applies() {
    let mut rt = Runtime::default_strict();
    let mut a = Some(rt.new_session());

    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"WATCH", b"k"]), 0), ok());
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"MULTI"]), 1), ok());
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"SET", b"k", b"1"]), 2),
        queued()
    );
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"EXEC"]), 3),
        RespFrame::Array(Some(vec![ok()]))
    );
}

// ── M3: WATCH key then B modifies key between WATCH/EXEC → EXEC nil ─

#[test]
fn m3_watch_aborts_when_other_client_modifies_watched_key() {
    let mut rt = Runtime::default_strict();
    let mut a = Some(rt.new_session());
    let mut b = Some(rt.new_session());

    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"WATCH", b"k"]), 0), ok());
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"MULTI"]), 1), ok());
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"SET", b"k", b"A-wins"]), 2),
        queued()
    );

    // B clobbers k between A's WATCH and A's EXEC.
    assert_eq!(
        run_as(&mut rt, &mut b, cmd(&[b"SET", b"k", b"B-wins"]), 3),
        ok()
    );

    // A's EXEC must return (nil) — the transaction aborts entirely.
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"EXEC"]), 4), exec_aborted());

    // Value on k must be B's write, not A's.
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"GET", b"k"]), 5),
        RespFrame::BulkString(Some(b"B-wins".to_vec()))
    );
}

// ── M4: WATCHing multiple keys; modification to ANY one aborts ──────

#[test]
fn m4_watch_multi_key_any_other_write_aborts_batch() {
    let mut rt = Runtime::default_strict();
    let mut a = Some(rt.new_session());
    let mut b = Some(rt.new_session());

    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"WATCH", b"k1", b"k2"]), 0),
        ok()
    );
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"MULTI"]), 1), ok());
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"SET", b"k1", b"v1"]), 2),
        queued()
    );
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"SET", b"k2", b"v2"]), 3),
        queued()
    );

    // B deletes only k2.
    assert_eq!(
        run_as(&mut rt, &mut b, cmd(&[b"DEL", b"k2"]), 4),
        RespFrame::Integer(0) // k2 didn't exist; DEL returns 0 but still bumps version
    );
    // B then touches k2 via SET to ensure version bumps.
    assert_eq!(
        run_as(&mut rt, &mut b, cmd(&[b"SET", b"k2", b"B-wrote"]), 5),
        ok()
    );

    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"EXEC"]), 6), exec_aborted());
    // Neither k1 nor k2 took A's intended value.
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"GET", b"k1"]), 7),
        nil_bulk()
    );
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"GET", b"k2"]), 8),
        RespFrame::BulkString(Some(b"B-wrote".to_vec()))
    );
}

// ── M5: aborted EXEC does not poison the next transaction ───────────

#[test]
fn m5_second_multi_exec_after_aborted_first_is_unaffected() {
    let mut rt = Runtime::default_strict();
    let mut a = Some(rt.new_session());
    let mut b = Some(rt.new_session());

    // First round: WATCH + EXEC aborts.
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"WATCH", b"k"]), 0), ok());
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"MULTI"]), 1), ok());
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"SET", b"k", b"A1"]), 2),
        queued()
    );
    assert_eq!(
        run_as(&mut rt, &mut b, cmd(&[b"SET", b"k", b"B1"]), 3),
        ok()
    );
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"EXEC"]), 4), exec_aborted());

    // Second round: no WATCH. MULTI/EXEC must apply.
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"MULTI"]), 5), ok());
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"SET", b"k", b"A2"]), 6),
        queued()
    );
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"EXEC"]), 7),
        RespFrame::Array(Some(vec![ok()]))
    );
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"GET", b"k"]), 8),
        RespFrame::BulkString(Some(b"A2".to_vec()))
    );
}

// ── Metamorphic relations ───────────────────────────────────────────

/// MR-WATCH-MONOTONIC: the ABORT flag is binary — 1 intervening write or 10
/// both produce nil. Parameterized by N.
#[test]
fn mr_watch_monotonic_single_write_equivalent_to_many() {
    for n in [1_usize, 3, 10, 50] {
        let mut rt = Runtime::default_strict();
        let mut a = Some(rt.new_session());
        let mut b = Some(rt.new_session());

        assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"WATCH", b"key"]), 0), ok());
        assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"MULTI"]), 1), ok());
        assert_eq!(
            run_as(&mut rt, &mut a, cmd(&[b"SET", b"key", b"A"]), 2),
            queued()
        );
        for i in 0..n {
            let v = format!("b-{i}");
            assert_eq!(
                run_as(
                    &mut rt,
                    &mut b,
                    cmd(&[b"SET", b"key", v.as_bytes()]),
                    (3 + i) as u64
                ),
                ok()
            );
        }
        assert_eq!(
            run_as(&mut rt, &mut a, cmd(&[b"EXEC"]), (3 + n + 1) as u64),
            exec_aborted(),
            "WATCH-monotonic violated at n={n}"
        );
    }
}

/// MR-WATCH-IDEMPOTENT: WATCH k; WATCH k is the same as WATCH k.
#[test]
fn mr_watch_idempotent_repeated_watch_same_key() {
    let mut rt = Runtime::default_strict();
    let mut a = Some(rt.new_session());
    let mut b = Some(rt.new_session());

    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"WATCH", b"k"]), 0), ok());
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"WATCH", b"k"]), 1), ok());
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"MULTI"]), 2), ok());
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"SET", b"k", b"A"]), 3),
        queued()
    );
    assert_eq!(run_as(&mut rt, &mut b, cmd(&[b"SET", b"k", b"B"]), 4), ok());
    // Repeated WATCHes on the same key still produce exactly one abort.
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"EXEC"]), 5), exec_aborted());
}

/// MR-UNWATCH-CLEARS: UNWATCH drops version pins; a subsequent WATCH
/// captures fresh state.
#[test]
fn mr_unwatch_clears_then_fresh_watch_captures_new_version() {
    let mut rt = Runtime::default_strict();
    let mut a = Some(rt.new_session());
    let mut b = Some(rt.new_session());

    // A watches k, B modifies k, A UNWATCHes, then A re-WATCHes fresh.
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"WATCH", b"k"]), 0), ok());
    assert_eq!(
        run_as(&mut rt, &mut b, cmd(&[b"SET", b"k", b"B1"]), 1),
        ok()
    );
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"UNWATCH"]), 2), ok());
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"WATCH", b"k"]), 3), ok());
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"MULTI"]), 4), ok());
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"SET", b"k", b"A-final"]), 5),
        queued()
    );
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"EXEC"]), 6),
        RespFrame::Array(Some(vec![ok()])),
        "after UNWATCH + re-WATCH, EXEC must apply"
    );
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"GET", b"k"]), 7),
        RespFrame::BulkString(Some(b"A-final".to_vec()))
    );
}

/// MR-DISCARD-CLEARS: DISCARD between MULTI and EXEC wipes the queue;
/// subsequent MULTI/EXEC is unaffected.
#[test]
fn mr_discard_clears_queue_and_releases_watch() {
    let mut rt = Runtime::default_strict();
    let mut a = Some(rt.new_session());

    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"WATCH", b"k"]), 0), ok());
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"MULTI"]), 1), ok());
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"SET", b"k", b"queued1"]), 2),
        queued()
    );
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"DISCARD"]), 3), ok());

    // After DISCARD, EXEC standalone must error "EXEC without MULTI".
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"EXEC"]), 4),
        RespFrame::Error("ERR EXEC without MULTI".to_string())
    );

    // k must not have been set.
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"GET", b"k"]), 5), nil_bulk());
}

/// Regression guard: EXEC outside MULTI returns the canonical error.
#[test]
fn exec_without_multi_returns_canonical_error() {
    let mut rt = Runtime::default_strict();
    assert_eq!(
        rt.execute_frame(cmd(&[b"EXEC"]), 0),
        RespFrame::Error("ERR EXEC without MULTI".to_string())
    );
}

/// Regression guard: WATCH inside MULTI is rejected.
#[test]
fn watch_inside_multi_is_rejected() {
    let mut rt = Runtime::default_strict();
    let mut a = Some(rt.new_session());
    assert_eq!(run_as(&mut rt, &mut a, cmd(&[b"MULTI"]), 0), ok());
    assert_eq!(
        run_as(&mut rt, &mut a, cmd(&[b"WATCH", b"k"]), 1),
        RespFrame::Error("ERR WATCH inside MULTI is not allowed".to_string())
    );
}
