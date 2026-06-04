# Pass 40: Lazy Order-Stat ZSet Rank Cache

Bead: `frankenredis-gu5nf.28`

## Target

`Store::zrank` and `Store::zrevrank` walked the sorted-set `BTreeMap` from one end and counted entries until the target member. The pass-40 harness isolates a 20k-member sorted set and 20k paired `ZRANK`/`ZREVRANK` lookups.

Profile evidence:

- Baseline paired loop direct timing: `1.321919946s`.
- Rank-only loop direct timing: `0.584010236s`.
- Reverse-rank-only loop direct timing: `0.886264458s`.
- Long run with 50k members and 200k paired lookups: `32.594240761s`.
- Strace was startup-only: baseline `116` total syscalls, `0.001455s` syscall time.
- `perf` was blocked by `perf_event_paranoid=4`; gdb attach was blocked by host ptrace policy. Both blocked artifacts are recorded.

## Lever

Add behavior-invisible lazy `member -> rank` caches inside `SortedSet`:

- Build ascending rank cache on first `ZRANK` after mutation.
- Build descending rank cache on first `ZREVRANK` after mutation.
- Invalidate both caches on zset insert, score update, remove, pop-min, and pop-max.
- Keep `SortedSet::PartialEq` defined only over logical `dict` and ordered index so caches remain invisible to behavior and tests.

## Baseline

- rch-built release harness: `target-blackthrush-pass40-zrank-baseline-rch`.
- Hyperfine: `1.321s +/- 0.018s`.
- Direct paired loop: `1.321919946s`.
- Rank-only direct loop: `0.584010236s`.
- Reverse-rank-only direct loop: `0.886264458s`.

## Candidate

- rch-built release harness: `target-blackthrush-pass40-zrank-candidate-rch`.
- Hyperfine: `26.9ms +/- 5.0ms`.
- Direct paired loop: `0.014327482s`.
- Rank-only direct loop: `0.012574381s`.
- Reverse-rank-only direct loop: `0.007292364s`.
- Strace remains startup-only.

## Delta

- Hyperfine speedup: `49.1x` (`1.321s / 0.0269s`).
- Direct paired-loop speedup: `92.3x` (`1.321919946s / 0.014327482s`).

## Isomorphism Proof

- Ordering preserved: yes. Cache entries are enumerated from the existing `ScoreMember` order, which already canonicalizes zero scores and breaks ties lexicographically by member.
- Tie-breaking unchanged: yes. Equal-score ranks still derive from `ScoreMember` order; the cache stores only the resulting rank.
- Floating-point: identical. No score arithmetic or comparison semantics changed; cache construction consumes existing ordered entries.
- RNG: unchanged. LFU random sampling still occurs before rank lookup exactly as before.
- Missing members: unchanged. `rank`/`rev_rank` check `dict` before building or reading a cache and still return `None`.
- Mutation behavior: unchanged. Caches are invalidated on every logical zset ordering mutation and ignored by `PartialEq`.
- Golden behavior sha256: `206a7f277dfaa54f66e12fd9263d9f1fd4af1225f11bd9bf12121711fac8c156`; baseline and candidate normalized behavior goldens match byte-for-byte.

## Score

Impact `5.0` x Confidence `0.95` / Effort `1.0` = `4.75`.

Verdict: KEEP.

## Validation

- `cargo fmt -p fr-store --check`
- `rch exec -- cargo test -p fr-store zrank_cache_invalidates_after_sorted_set_mutations -- --nocapture`
- `rch exec -- cargo check -p fr-store --all-targets`
- `rch exec -- cargo clippy -p fr-store --all-targets -- -D warnings`
- `ubs crates/fr-store/src/lib.rs` completed; it exited nonzero on the existing broad `fr-store` inventory. New cache clone warnings are the intentional one-time cache construction cost, and fmt/check/clippy were clean.
