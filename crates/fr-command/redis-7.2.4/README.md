# Redis command metadata

These 392 JSON specifications are copied byte-for-byte from Redis 7.2.4,
commit `d2c8a4b91e8c0e6aefd1f5bc0bf582cddbe046b7`, directory `src/commands`:

<https://github.com/redis/redis/tree/d2c8a4b91e8c0e6aefd1f5bc0bf582cddbe046b7/src/commands>

`COPYING` preserves the upstream license and `SHA256SUMS` records every JSON
file. No Redis implementation code is included. `fr-command/build.rs` reads
these specifications to produce ACL categories and COMMAND DOCS metadata.

Tracking this input binds production builds to the release commit and makes
strict source snapshots self-contained. The separately ignored Redis oracle
remains a test dependency. Before replacing these specifications, verify the
upstream revision and review generated metadata compatibility.
