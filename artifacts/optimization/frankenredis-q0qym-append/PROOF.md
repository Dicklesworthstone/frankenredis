# frankenredis-q0qym APPEND Borrowed Fast Path

Status: kept.

## Target

Ready perf bead: `frankenredis-q0qym`.

Profile-backed route: borrowed dispatch/write fast paths are the next tier after
the read-command borrowed path and existing plain SET fast path. NobleRiver
provided a proven APPEND handoff after fr-runtime/fr-server reservations cleared;
OrangeMouse re-established the baseline and re-ran the keep gate before landing.

Alien/no-gaps primitive: zero-copy borrowed command dispatch for write commands.
This pass lands exactly one command-specific lever: `APPEND key value` skips
owned argv materialization only when the existing conservative borrowed write
predicate allows the fast path.

## Lever

One production lever was kept:

- Add `Runtime::execute_plain_append_borrowed`.
- Add `borrowed_plain_append_args` in the TCP server borrowed multibulk path.
- Preserve generic dispatch fallback for disabled states.

The fast path mirrors the generic APPEND handler:

- no-stat string length read, including WRONGTYPE behavior;
- `proto-max-bulk-len` string length guard;
- `store.append`;
- integer reply with the new length;
- write, command, slowlog, latency, errorstats, lazy-expiry propagation, and
  threat-event accounting.

## Behavior Proof

Golden transcript command mix:

- create, grow, and empty APPEND;
- GET verification;
- numeric string APPEND;
- wrong-type APPEND;
- binary key and value;
- create-empty APPEND;
- SELECT fallback;
- MULTI/EXEC fallback.

Golden SHA-256:

```text
oracle    sha256 = d38be4e317ec2f452852b91575d1a7fb906f7a4357af9a80d9ac1451595aeb71  (436 bytes)
candidate sha256 = d38be4e317ec2f452852b91575d1a7fb906f7a4357af9a80d9ac1451595aeb71
baseline  sha256 = d38be4e317ec2f452852b91575d1a7fb906f7a4357af9a80d9ac1451595aeb71
ISOMORPHISM (candidate==baseline): True
PARITY      (candidate==oracle):   True
```

Isomorphism notes:

- Ordering/tie-breaking: pipelined replies remain byte-identical and in the same
  order in the golden transcript.
- Floating point: N/A.
- RNG: N/A.
- Error precedence: wrong-type and disabled-state fallbacks match baseline and
  Redis oracle.
- Persistence/replication/keyspace notification state: fast path is disabled by
  the same borrowed write predicate family used by SET when AOF, replicas,
  tracking, or notification-sensitive modes are active.

Validation:

```text
cargo fmt -p fr-runtime -p fr-server --check
python3 -m py_compile scripts/append_fastpath_golden.py artifacts/optimization/frankenredis-q0qym-append/*.py
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-q0qym-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-q0qym-test-target cargo test -p fr-runtime plain_append_borrowed -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-q0qym-check-target cargo check -p fr-runtime -p fr-server --all-targets
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-q0qym-clippy-target cargo clippy -p fr-runtime -p fr-server --all-targets -- -D warnings
ubs crates/fr-runtime/src/lib.rs crates/fr-server/src/main.rs scripts/append_fastpath_golden.py
```

Focused runtime tests passed:

- `plain_append_borrowed_fast_path_matches_generic_create_grow_wrongtype`
- `plain_append_borrowed_fast_path_disabled_in_non_default_states`

UBS note: the pre-commit UBS run returned nonzero on file-wide legacy findings
in `fr-runtime` and `fr-server`; the embedded fmt, clippy, cargo check, and
test-build gates were clean, and no APPEND-hunk-specific UBS finding was kept.

## Benchmarks

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-q0qym-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-q0qym-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Pre-edit baseline calibration:

- APPEND P16/300k: `2.65244423678 s +/- 0.09823296596907849`

Paired APPEND P16/300k, same host, baseline first:

- baseline: `2.68091741942 s +/- 0.13635273769507414`
- candidate: `2.0451157464199996 s +/- 0.0930748284180931`
- candidate: `1.31x +/- 0.09` faster

Reversed APPEND P16/1M, same host, candidate first:

- candidate: `6.3760967195200005 s +/- 0.6691260648565939`
- baseline: `8.024819715320001 s +/- 0.21151754602614709`
- candidate: `1.26x +/- 0.14` faster

## Score

Score: `4.5 = Impact 3 x Confidence 3 / Effort 2`.

The lever clears the `>= 2.0` keep threshold.

## Next Route

Close this APPEND landing bead and re-profile. Remaining borrowed-write work
should continue as a new bead rather than expanding this commit:

- SETNX / GETSET / GETDEL;
- SETEX / PSETEX with TTL propagation proof;
- deeper zero-copy write dispatch that removes owned argv materialization as a
  command-family primitive rather than repeating small command stubs.
