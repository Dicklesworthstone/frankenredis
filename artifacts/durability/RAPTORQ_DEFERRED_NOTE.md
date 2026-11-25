# RaptorQ Durability Status

Current status: **sidecar encoding, scrubbing, and artifact verification are implemented in `crates/fr-fec`**; automatic generation-safe database recovery remains disabled and tracked separately.

What is implemented:
- `crates/fr-fec` provides the RFC 6330 RaptorQ codec (`raptorq`), SHA-256 `source_hash` binding, per-symbol digests, scrub reports, and decode proofs.
- Optional persistence sidecars (`.envelope.json` + `.symbols`) are published safely without mutating committed primary files.
- 137 checked-in artifact sidecar pairs exist across `baselines/`, `golden_outputs/`, and conformance fixtures.

What remains open:
- Generation-safe automatic RDB/AOF database repair (binding sidecars to committed persistence generations with crash-safe recovery) and upgrading the underlying `raptorq` crate / aligning `raptorq_artifact_gate` are tracked in GitHub [issue #1](https://github.com/Dicklesworthstone/frankenredis/issues/1) and bead `frankenredis-z6nnf`.
