# frankenredis-mn0qm — zset cold-rank gap: eager/self-warm treap both rejected (design tradeoff)

## Gap (real, vs redis)
Cold ZRANGE deep-index (no prior ZRANK) on a 100k zset, release-perf,
redis-benchmark -c1 best-of-3:
  baseline (lazy treap, O(start) linear skip):  2668 req/s
  eager treap (O(log n) select):               17986 req/s   = 6.7x
Redis's skiplist is ALWAYS rank-capable; fr's zset is BTreeMap (primary) + a
LAZY order-statistic treap built only on the first ZRANK. So ZRANGE-by-index /
ZRANDMEMBER / ZREMRANGEBYRANK pay an O(n) cold path until something warms the tree.

## Lever 1 — EAGER treap (build at FullSortedSet::with_capacity)  [REJECTED]
One line; the existing insert/remove already sync the tree when present.
  + cold ZRANGE deep-index: 2668 -> 17986 req/s (6.7x)
  - ZADD into a 100k Full zset (best-of-5): 18998 -> 16943 req/s (~0.89x, -11%)
The treap-maintenance cost lands on EVERY ZADD/ZREM, worsening fr's already-weak
collection-write path (already 1.6-2x slower than redis per project notes). Also
breaks the 3 zset A/B ratio tests (debug-skipped but release-fail) and the
documented invariant in zset_index_slice_treap_matches_linear_and_reports_ab_ratio
("zrange/zrevrange never build it; warm via ZRANK"). Net: trades a read gap for a
write gap — not a clean keep.

## Lever 2 — SELF-WARM in Store::zrange path  [REJECTED]
  + 0 write regression (pure-write workloads never read -> never build the tree)
  + repeated reads self-warm -> 6.7x
  - DIRECTLY contradicts the same tested invariant ("zrange never builds it")
  - one-shot deep ZRANGE on a cold large zset builds O(n log n) > O(start) skip
Not shippable without reversing + rewriting that intentional lazy-design test.

## Real fix (the actual lever — needs a focused session)
Replace BTreeMap-primary + lazy-treap with a SINGLE order-statistic structure
(the treap IS the sorted map). Then ZADD does ONE ordered insert instead of two
(potentially FASTER writes, not slower) and ALL rank/index/count ops are O(log n)
unconditionally — matching redis's single always-rank skiplist with NO tradeoff.
Refactor scope: the treap must grow range-iteration (ZRANGEBYSCORE/ZRANGEBYLEX),
lex scans, first/last, and range-removal — everything BTreeMap does today — and
the 3 A/B tests get rewritten (no cold path to compare). Target: 6.7x cold read
WITH neutral-or-better writes.
