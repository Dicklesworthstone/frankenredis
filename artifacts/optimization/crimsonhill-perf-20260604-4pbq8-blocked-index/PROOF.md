frankenredis-4pbq8 blocked wake index proof

Target
- Profile-backed bead: frankenredis-4pbq8.
- Hotspot: every blocked-client tick walked all blocked tokens and rechecked all watched keys even when only one key was ready or only a timeout was due.
- Lever: one safe-Rust advisory sidecar, BlockedWakeIndex, indexed by ready key plus timeout heap plus WAIT/WAITAOF tick queue.

Before/after evidence
- Baseline rch evidence: baseline-rch-test.txt.
- Baseline prior hot loop remained O(blocked): keys-clone 101 ms -> borrow 60 ms on the pre-lever 2000-client scan guard.
- Candidate rch focused index test: candidate-index-tests-2.txt.
- Candidate focused A/B: 10000 blocked clients x 3 keys, one ready key: scan 147 ms -> index 38 us = 3849.78x.
- Candidate full fr-server package test: candidate-fr-server-tests.txt.
- Candidate full package A/B replay: scan 147 ms -> index 31 us = 4721.72x.

Behavior isomorphism
- blocked_tokens and conn.blocked remain authoritative. The index only chooses candidate tokens; every candidate is revalidated against the current connection state and current BlockingOp before timeout or fulfillment.
- Key wake semantics are preserved: the old path tested blocked.op.any_key_ready(ready_keys), while the new index returns only tokens registered under those same keys. try_fulfill_blocked still performs the final command-specific key order and response construction, preserving key priority and tie-breaking.
- Per-key FIFO is preserved by appending BlockedWakeRef entries in insertion order; stale refs are skipped by sequence number.
- Timeout ordering is preserved by BinaryHeap<Reverse<(deadline_ms, seq, token)>>. Equal-deadline deterministic tie order is stable and behavior-invisible because each blocked client still receives the same timeout response.
- WAIT and WAITAOF keep their per-tick recheck behavior by living in a separate waiter queue.
- CLIENT UNBLOCK, timeout unblocks, successful key unblocks, disconnections, and missing connection cleanup remove the live registration. Stale queue entries are advisory and ignored.
- RESP output ordering/tie-breaking for all command replies remains driven by the existing runtime paths. Floating-point and RNG behavior are not involved.

Validation
- rustfmt --edition 2024 --check crates/fr-server/src/main.rs: passed.
- git diff --check -- crates/fr-server/src/main.rs: passed.
- rch exec -- cargo test -p fr-server blocked_wake_index --release -- --nocapture: passed.
- rch exec -- cargo test -p fr-server --release -- --nocapture: passed, including TCP legacy-reference tests.
- rch exec -- cargo check -p fr-server --all-targets: passed.
- rch exec -- cargo clippy -p fr-server --all-targets --no-deps -- -D warnings: passed for fr-server.
- rch exec -- cargo clippy -p fr-server --all-targets -- -D warnings: blocked by pre-existing fr-store clippy lints in a surface reserved by another agent; see candidate-fr-server-clippy.txt.
- ubs crates/fr-server/src/main.rs: exited 0; scanner still reports the historical fr-server warning inventory, while its fmt, clippy, cargo check, and test-build sections are clean. See ubs-final.txt.

Score
- Impact 5.0 x Confidence 0.95 / Effort 1.0 = 4.75.
- Verdict: keep and close frankenredis-4pbq8.
