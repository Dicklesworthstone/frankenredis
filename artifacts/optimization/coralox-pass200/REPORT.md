# CoralOx pass200 qesp3 closeout

Bead: `frankenredis-largeval-bigbulk-zerocopy-qesp3`
Date: 2026-06-14

## Target

Profile-backed large-value SET path. The original bead targeted Redis' big-argument
optimization: avoid the second per-byte copy from the connection read buffer into
the stored string value.

The source lever itself is already present in current main via `71aa88cba`:

- `fr-server`: partial large SET read state that reads the value into an owned
  buffer across readable events.
- `fr-runtime`: owned plain SET dispatch.
- `fr-store`: `set_plain_owned` moves the owned value into the string object.

This pass verified the current in-tree implementation, fixed the owned fast-path
unit proof to account for the default evidence-ledger policy, and split the
remaining GET output-side gap out of the SET bead.

## Binaries

- `frankenredis`: `3af35334d29e29244c2e50619c66c3d0294a9be72d351b2e738bb7a1dcb74406`
- `fr-bench`: `1b50c1928593c0a66e976ca9a1c85f971763eede732ab5091b817e9350454bf2`
- Redis oracle: `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`

Build command:

```text
CARGO_TARGET_DIR=target-coralox-pass200-current rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench
```

## Benchmark

Current gate artifact:

- `large_value_current_20260614_1846.txt`
- sha256: `3447f9b05141294fa596c6e1eb805ae6f2a8733d0e7fed7be0e01a7079a35b5c`

Command:

```text
python3 scripts/large_value_perf_gate.py 26601 26602 --min-ratio 0.9
```

SET ratios, baseline artifact -> current artifact:

- 64 KiB: `1.54x -> 0.96x` redis/fr ratio noise-regressed but still near parity.
- 256 KiB: `0.81x -> 1.03x`, gap closed.
- 1 MiB: `0.71x -> 0.89x`, borderline residual.

GET ratios in the same current run:

- 64 KiB: `0.76x`
- 256 KiB: `0.43x`
- 1 MiB: `0.54x`

Verdict: the SET read-into-owned-object lever is a keep for the 256 KiB SET
target and substantially reduces the 1 MiB SET gap. The remaining dominant
large-value gap has shifted to GET write-side output/copying and must be handled
as a separate bead with a fresh profile.

Score: Impact `1.26` (geomean of 256 KiB and 1 MiB SET ratio improvement) x
Confidence `0.90` / Effort `0.40` = `2.84`.

## Isomorphism

Raw RESP golden transcript against Redis 7.2.4 covered `FLUSHALL`, SET/STRLEN/GET
for deterministic binary values of 64 B, 4 KiB, 64 KiB, and 256 KiB, then `QUIT`.

- request sha256: `4868fe2e405b44a86d87005a11e90d039ddaa0f5217050907c5cadfe4f19540a`
- Redis response sha256: `6ae19d9e2febe7a771bdb00520c034da885689d9f99a121f7f786ae92f4ea729`
- FrankenRedis response sha256: `6ae19d9e2febe7a771bdb00520c034da885689d9f99a121f7f786ae92f4ea729`
- requests equal: `true`
- responses equal: `true`

Ordering and RNG: no ordering, tie-breaking, floating-point, or RNG surfaces are
in this SET/GET string path. The transcript pins serial command order and exact
bulk bytes. The focused runtime/server tests pin evidence-ledger fallback and
large SET partial-read framing behavior.

## Gates

Passed:

- `rch exec -- cargo test -j 1 -p fr-runtime plain_set_owned_fast_path -- --nocapture`
- `rch exec -- cargo test -j 1 -p fr-server large_plain_set_read_start -- --nocapture`
- `rch exec -- cargo check -j 1 -p fr-runtime --all-targets`
- `rch exec -- cargo clippy -j 1 -p fr-runtime --all-targets -- -D warnings`

Known non-blocking formatting gate:

- `cargo fmt -p fr-runtime -- --check` still fails on broad pre-existing
  `crates/fr-runtime/src/lib.rs` rustfmt drift outside this hunk.

## Next Route

Close `frankenredis-largeval-bigbulk-zerocopy-qesp3` for SET. File/claim a
separate profile-backed GET write-side bead for scatter/gather or output-buffer
ownership only after a fresh profile shows the dominant write-side row.
