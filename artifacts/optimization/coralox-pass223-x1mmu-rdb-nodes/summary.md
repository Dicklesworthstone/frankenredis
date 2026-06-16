# frankenredis-x1mmu pass223: prebuilt quicklist2 nodes for RDB snapshots

Target: profile-backed follow-up from `frankenredis-99fwc`. DUMP now borrows
sealed listpack nodes, but RDB SAVE/BGSAVE/full-sync still converted lists to
`RdbValue::List(Vec<Vec<u8>>)` and re-synthesized quicklist2 nodes.

Lever: add an encode-path `RdbValue::ListQuicklist2Packed` variant carrying
already-shaped PACKED quicklist2 listpack blobs; expose owned packed-node blobs
from `ListValue`; use them in `store_to_rdb_entries` when a list already has
canonical packed node boundaries. Lists that do not qualify fall back to the
old raw-element path.

Focused repeated SAVE, 500k string list, local ts1-offline sequential run:

| binary | mean SAVE | min SAVE | max SAVE | RDB sha256 |
| --- | ---: | ---: | ---: | --- |
| baseline | 0.043460782s | 0.034829698s | 0.066673541s | 500fda8d3836623cc25ea1791a1f5499384a7266661740c4cada4dcee15ccb4d |
| candidate | 0.029793353s | 0.027915673s | 0.032984925s | 500fda8d3836623cc25ea1791a1f5499384a7266661740c4cada4dcee15ccb4d |

Focused speedup: 1.46x.

Focused repeated SAVE, 100k string list:

| binary | mean SAVE | RDB sha256 |
| --- | ---: | --- |
| baseline | 0.019662498s | 3f338187debd9766b76c2b01c9421b651357b58f7ea942b7574e7b09b179a761 |
| candidate | 0.017731617s | 3f338187debd9766b76c2b01c9421b651357b58f7ea942b7574e7b09b179a761 |

Hyperfine full command (server start + seed + three SAVE commands, 100k string
list):

- baseline: 1.816s +/- 0.016s
- candidate: 1.784s +/- 0.022s

Behavior proof:

- RDB bytes unchanged for both 100k and 500k string-list shapes.
- LRANGE RESP digest unchanged:
  - 100k: `31972b67be759f483c8e38cc25521afeec8af6c02a9c6293ed3f1dc6bef8a670`
  - 500k: `87d95cefe0e4452fba9cbb37b8c5ff87afe09f90500b50f9bb01fcb1c6317d6b`
- Redis `redis-check-rdb` accepts the candidate RDB.
- Vendored Redis loads the candidate RDB and reports `LLEN L == 100000` with
  the same 100k LRANGE RESP digest.
- Ordering is preserved by carrying existing node order; tie-breaking,
  floating point, and RNG are untouched.

Local gates:

- `cargo fmt -p fr-store -p fr-persist -p fr-runtime -- --check`
- `cargo check -j1 -p fr-store -p fr-persist -p fr-runtime --all-targets`
- `cargo clippy -j1 -p fr-store -p fr-persist -p fr-runtime --all-targets -- -D warnings`
- `cargo test -j1 -p fr-store --lib -- --nocapture`
- `cargo test -j1 -p fr-persist --lib -- --nocapture`
- `cargo test -j1 -p fr-runtime rdb -- --nocapture`
- `CARGO_TARGET_DIR=target/x1mmu-pass223-candidate-local cargo build -j1 -p fr-server --bin frankenredis --profile release-perf`

UBS:

- `ubs crates/fr-store/src/packed_set.rs crates/fr-store/src/lib.rs crates/fr-persist/src/lib.rs crates/fr-runtime/src/lib.rs`
- Exit: 1 due broad existing inventories in these large files.
- Embedded fmt/clippy/check/test sections were clean.

Score: Impact 2 x Confidence 4 / Effort 3 = 2.67, keep.
