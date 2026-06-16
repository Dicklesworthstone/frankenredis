# frankenredis-99fwc pass222: quicklist-boundary listpack sealing

Target: profile-backed list DUMP/RESTORE structural hotspot. Append-built
`ChunkedList` lists stored every element as an owned `Vec<u8>`, so DUMP had to
re-synthesize listpack nodes on every serialization.

Lever: split pushed deque chunks with the same Redis quicklist admission rule,
seal completed interior chunks to retained listpack blobs, and have DUMP/DEBUG
borrow sealed blobs while only encoding open owned chunks.

Focused 500x DUMP loop, 10k elements, local ts1-offline run:

| shape | baseline | final candidate | speedup | DUMP sha256 |
| --- | ---: | ---: | ---: | --- |
| int | 0.441884599s | 0.253909527s | 1.74x | e3cfb2db563e6a21db5858856eac1157e0c2956cce7155d5c6b9696f96f0e7dd |
| str | 0.507651047s | 0.237790137s | 2.13x | cfd44f5fb0fc3eb5940b348b8621bafe98f11cab135a95b56cbe811fefeda9a2 |
| wide | 1.489187456s | 0.781212485s | 1.91x | aab2ff70f31ff440fe9327478604aa550e2b6520239eed2e3832b2c149226c0e |

Earlier low-noise candidate artifact retained the same DUMP SHA values and
measured 2.79x-3.27x focused-loop wins; the final local rerun above was kept as
the conservative score input. Hyperfine end-to-end artifacts remain:
`hyperfine-dump-int.json`, `hyperfine-dump-str.json`, `hyperfine-dump-wide.json`
with 1.39x-1.44x server-start/seed/DUMP-loop wins.

Behavior proof:

- Ordering: list push order and LRANGE digest unchanged for all shapes.
- Tie-breaking/floating point/RNG: untouched; this lever only changes list
  chunk representation and DUMP node borrowing.
- Golden DUMP SHA: unchanged vs the pre-lever baseline for int/str/wide shapes.
- Redis oracle: `dump_restore_differ.py` passes DUMP/RESTORE both directions vs
  Redis 7.2.4 for 14 keys across encodings.
- DEBUG OBJECT: quicklist node metadata matches Redis-shaped node boundaries for
  the integer list shape (5 nodes, avg 2000.00, serializedlength 35821).

Local gates:

- `cargo fmt -p fr-store -p fr-persist -- --check`
- `cargo test -j1 -p fr-store --lib -- --nocapture`
- `cargo check -j1 -p fr-store -p fr-persist --all-targets`
- `cargo clippy -j1 -p fr-store -p fr-persist --all-targets -- -D warnings`
- `cargo test -j1 -p fr-persist --lib -- --nocapture`
- `CARGO_TARGET_DIR=target/99fwc-pass222-candidate-local cargo build -j1 -p fr-server --bin frankenredis --profile release-perf`
- `python3 scripts/dump_restore_differ.py --bin target/99fwc-pass222-candidate-local/release-perf/frankenredis --redis-bin legacy_redis_code/redis/src/redis-server`

UBS:

- `ubs crates/fr-store/src/packed_set.rs crates/fr-store/src/lib.rs crates/fr-persist/src/lib.rs`
- Exit: 1 due broad existing inventories in the three large files.
- Embedded fmt/clippy/check/test sections were clean; changed hot-path clone
  count was reduced before commit.

Score: Impact 3 x Confidence 4 / Effort 4 = 3.0, keep.
