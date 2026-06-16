# frankenredis-peni2 independent residual-routing report

Target: zset actual RSS profile from `scripts/zset_memory_profile.py` on fresh Redis and FrankenRedis processes, 500 keys x 800 small members = 400000 members.

Baseline measured from pre-upstream-landing head `5970ec70bc9689ca077545aafce4805cddacc31c`:
- Redis 7.2.4: 37.79 MB, 99 B/member
- FrankenRedis: 59.46 MB, 156 B/member
- Ratio: 1.57x
- FrankenRedis binary sha256: `decde40073f2c6f852eff98ea9f738c6e847bcc57a04f0704564b3448741ab13`
- Redis binary sha256: `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`

Measured source variants from this worktree, all restored before this evidence commit:
- `Arc<[u8]>` shared member key across dict/ordered/treap: 58.29 MB, 153 B/member, ratio 1.55x.
- Inline/heap small-member enum key: 59.49 MB, 156 B/member, ratio 1.57x.
- `Box<[u8]>` compact owned key: 58.30 MB, 153 B/member, ratio 1.55x.

Decision for this commit: no runtime source hunk kept. Upstream commit `c4417d55e` separately landed the shared `Arc<[u8]>` implementation for `frankenredis-peni2` with its own proof bundle and kept tracker closeout. These independent measurements are retained only as residual-routing evidence: the best local delta was 59.46 MB -> 58.29 MB, a 1.02x memory win on the bead workload, which points deeper than member-byte duplication. The next target should replace the full zset structure with a structurally different safe-Rust primitive aimed at BTreeMap/IndexMap node and table overhead.

Behavior/isomorphism: no runtime source change is kept. `git diff --exit-code -- crates/fr-store/src/lib.rs` passed after restoring the rejected diff. The rejected patch is preserved in `rejected-zset-member-key-representation.diff` for audit only.
