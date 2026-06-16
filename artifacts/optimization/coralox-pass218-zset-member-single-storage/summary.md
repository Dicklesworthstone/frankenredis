# Pass 218 - zset member single-storage

Bead: `frankenredis-peni2`

Lever kept: `FullSortedSet` stores each full zset member once as `Arc<[u8]>`, shared by the member dict, ordered score/member tree, and rank treap keys. Public command/API output still clones to `Vec<u8>` at the same boundaries.

Baseline:
- Head: `5970ec70bc9689ca077545aafce4805cddacc31c`
- `frankenredis` sha256: `23440062d4e8e2fd5567df59bbecbb0a7b5860c627db4faa91dc6e8f08bdf6f0`
- Fresh-process RSS: `59.38 MB`, `156 B/member`, Redis ratio `1.57x`

Candidate:
- `frankenredis` sha256: `c21830d578aea7114b5c98820aee9883f0c0e65d4d7699b65ee9895ed47c2f41`
- Fresh-process RSS: `58.23 MB`, `153 B/member`, Redis ratio `1.54x`
- ZADD guardrail: `68446.27 -> 68212.83 req/s`, p50 `0.359 ms -> 0.359 ms`

Golden proof:
- Baseline transcript sha256: `f137ac4efd7f901a866cda94094477d42f1f8460724966bfb4ac52727870bded`
- Candidate transcript sha256: `f137ac4efd7f901a866cda94094477d42f1f8460724966bfb4ac52727870bded`
- Byte-identical: true

Isomorphism:
- Ordering and equal-score tie-breaking still use `ScoreMember::Ord`: canonicalized score total order, then member byte order.
- Floating-point behavior is unchanged: score canonicalization and `total_cmp` paths are untouched.
- RNG semantics are unchanged: random sampling still picks from the same `IndexMap` member set; only key storage changes.
- Deterministic zset replies for ZADD, range, lex, rank, pop, increment, union, and intersection commands matched the baseline transcript byte-for-byte.

Validation:
- RCH `cargo check -j 1 -p fr-store --all-targets`
- RCH `cargo test -j 1 -p fr-store zset -- --nocapture`
- RCH `cargo test -j 1 -p fr-store --test metamorphic_zset -- --nocapture`
- RCH `cargo test -j 1 -p fr-store --test metamorphic_zset_advanced --test metamorphic_zset_geo -- --nocapture`
- RCH `cargo clippy -j 1 -p fr-store --all-targets -- -D warnings`
- RCH `cargo build --profile release-perf -p fr-server -p fr-bench`

Formatting:
- `cargo fmt -p fr-store -- --check` remains blocked by pre-existing rustfmt drift in unrelated sections of `crates/fr-store/src/lib.rs`.

Score:
- Impact 2 * Confidence 4 / Effort 3 = 2.67

Next route:
- Re-profile. If zset actual RSS remains the active gap, attack a fundamentally different member/index layout primitive rather than more Arc micro-tuning.
