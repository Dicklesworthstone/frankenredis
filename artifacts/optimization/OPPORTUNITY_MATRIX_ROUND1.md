# Round 1 Opportunity Matrix

| Hotspot / Concept | Impact (1-5) | Confidence (1-5) | Effort (1-5) | Score | Decision |
|---|---:|---:|---:|---:|---|
| Protocol parse allocations (`fr-protocol`) | 4 | 4 | 2 | 8.0 | pursue in next optimization-only change |
| Command dispatch branch ladder (`fr-command`) | 3 | 4 | 2 | 6.0 | pursue after parser optimization |
| Keyspace map structure (`fr-store`) | 5 | 3 | 3 | 5.0 | evaluate ART/SwissTable tradeoff post-baseline |
| TTL scan strategy | 4 | 3 | 2 | 6.0 | evaluate once active expiry loop lands |
| Replication lag accounting (`fr-repl`) | 3 | 3 | 2 | 4.5 | defer until live replication path exists |

Selection rule: apply one lever per change, score >= 2.0.
