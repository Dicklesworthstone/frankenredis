# frankenredis-3uwif Pass 169 Report

## Scope

- Bead: `frankenredis-3uwif`
- Target: restore userspace attribution for LPUSH command-packet residual.
- Decision: profile-enabled; no production source change kept.

## Method

The host blocks `perf`, `samply`, and ptrace attach, but GDB can trace a process it starts. The server was run as a GDB-owned child using the `release-perf` binary, then `fr-bench` drove LPUSH P16/C50 workloads on port `24195`.

Useful command shape:

```text
gdb -q /data/tmp/frankenredis-152va-pass168-perf-target/release-perf/frankenredis
(gdb) run --port 24195
(gdb) set logging file artifacts/optimization/frankenredis-3uwif/gdb_lpush_samples.txt
(gdb) set logging overwrite on
(gdb) set logging enabled on
```

The workload was:

```text
fr-bench --host 127.0.0.1 --port 24195 --workload lpush --clients 50 --pipeline 16 --requests 5000000 --keyspace 100000
```

Repeated `Ctrl-C`, `thread apply all bt 18`, and `continue` produced the logged samples.

After the pass168 closeout was rebased onto the newer `origin/main`, current HEAD was rebuilt with:

```text
rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-3uwif-release-perf-target cargo build --profile release-perf -p fr-server -p fr-bench
```

That RCH build completed on worker `vmi1227854` with the pre-existing `fr-command` unused-assignment warnings and `fr-runtime` unused `is_zpop` warning. A current-head GDB child run confirmed the same syscall-floor shape (`recv`/`epoll_wait` samples); optimized line breakpoints did not reliably fire for the inlined LPUSH store frame.

## Attribution

The active main-thread sample from the symbolized GDB child run landed in:

```text
core::slice::cmp::eq
hashbrown lookup
fr_store::Store::drop_if_expired at crates/fr-store/src/lib.rs:15952
fr_store::Store::lpush<&[u8]> at crates/fr-store/src/lib.rs:9382
fr_runtime::Runtime::execute_plain_keyed_values_write_borrowed at crates/fr-runtime/src/lib.rs:7191
parse_borrowed_multibulk_action at crates/fr-server/src/main.rs:2423
process_buffered_frames
handle_readable
```

The other captured main-thread samples landed in `epoll_wait`, matching the pass168 syscall-floor profile. The current-head rerun also sampled `recv`/`epoll_wait`, reinforcing that syscall pacing remains material even after the upstream SCAN/SWAPDB commits.

## Interpretation

This restores the missing userspace direction: LPUSH is paying at least one store hash lookup through `drop_if_expired` before the actual `get_mut`/mutation path. The next source pass should baseline current main and test exactly one lazy-expiry/hash-probe avoidance lever for list pushes when no expiries exist.

Do not switch to parser scratch reuse on this evidence. The code inspection found a fresh `Vec<&[u8]>` in `parse_borrowed_multibulk_action`, but the GDB sample did not attribute the active cost there.

## Next Acceptance

- Baseline current main with RCH-built `fr-server`/`fr-bench`.
- Preserve Redis-observable reply ordering, list order, client reply suppression, lazy-expiry propagation, RNG/tie-breaking, and floating-point behavior.
- Prove golden input/output SHA unchanged.
- Re-benchmark LPUSH P16/C50 and keep only if Score >= 2.0.
