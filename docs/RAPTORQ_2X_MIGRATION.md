# RaptorQ 1.8.1 → 2.x Persisted Sidecar Migration Plan

Tracking: GitHub Issue #1 / `frankenredis-z6nnf`.

## Why `raptorq` Remains Pinned at `1.8.1`

Upstream `raptorq` `2.0.0` changed RFC 6330 `PayloadId` encoding-symbol-ID assignments
(<https://github.com/cberner/raptorq/releases/tag/v2.0.0>). Repair packets encoded by
`raptorq 1.8.1` cannot be decoded by `raptorq 2.x` without re-encoding or maintaining a
dual-codec decoder.

Because FrankenRedis persists RaptorQ sidecars on disk (for checked-in conformance/baseline
evidence bundles and opt-in RDB/AOF durability sidecars), upgrading the crate in place would
silently break recovery of historical `.symbols` files unless the wire format discriminates
codec generations.

## Codec Discrimination Now Enforced in `fr-fec`

1. Every newly written `frankenredis_fec_envelope/v1` and `frankenredis_fec_envelope/v2`
   envelope stamps `"codec_id": "raptorq-1.8.1"` (`CODEC_ID_RAPTORQ_1_8_1`), and every
   authenticated `<artifact>.generation.json` binds `"codec_id"` under its HMAC-SHA256 tag.
2. Legacy `v1` envelopes omitting `codec_id` are treated as implicit `"raptorq-1.8.1"`.
3. `fr-fec` fails closed on any unknown or mismatched `codec_id` during scrub, gate
   verification, export recovery, and in-place recovery — preventing silent misdecoding across
   a `1.8.1` / `2.x` boundary.

## Staged Upgrade Procedure (When Moving to `raptorq 2.x`)

### Phase 1 — Dual-Codec Read Compatibility (Non-Breaking)
1. Vendor or rename-depend on `raptorq_v1` (`package = "raptorq", version = "=1.8.1"`)
   alongside `raptorq = "2"` in `crates/fr-fec/Cargo.toml`.
2. Define `CODEC_ID_RAPTORQ_2_RFC6330 = "raptorq-2.x-rfc6330"`.
3. Dispatch decode/scrub by `envelope.codec_id`:
   - `None | Some("raptorq-1.8.1")` → `raptorq_v1::Decoder`
   - `Some("raptorq-2.x-rfc6330")` → `raptorq::Decoder` (2.x)
4. Keep default encoding on `"raptorq-1.8.1"` until dual-decode has shipped in one release
   so rollbacks remain readable.

### Phase 2 — Switch Encoder Default & Offline Re-Encoding
1. Switch `encode_artifact` and chunked `v2` encoders to emit `codec_id = "raptorq-2.x-rfc6330"`.
2. Re-encode all checked-in repository sidecars (`artifacts/`, `baselines/`,
   `crates/fr-persist/tests/golden/`) via `fr-fec sidecar` and verify with
   `fr-fec verify-gate` and `raptorq_artifact_gate`.
3. For operator databases, run an offline re-encode pass (`fr-fec sidecar --generation-bound`)
   after verifying existing `1.8.1` sidecars with `fr-fec verify-gate --generation-bound`.
   Because `.generation.json` commits each generation atomically in two phases, an interrupted
   re-encode never activates a half-migrated sidecar pair.

### Phase 3 — Retire `raptorq_v1`
1. Only after zero `"raptorq-1.8.1"` sidecars remain in tracked artifacts and supported
   upgrade paths, remove the `raptorq_v1` dependency; any leftover `1.8.1` sidecar will
   continue to fail closed on `codec_id` mismatch with an actionable operator error.

## Verification Checklist Before Flipping the Dependency

- [ ] Cross-version fixture tests: encode with `1.8.1`, verify decode succeeds under dual-codec
      build and fails closed (never silent corruption) under `2.x`-only build.
- [ ] Both `v1` single-object and `v2` chunked multi-block layouts tested under both codecs.
- [ ] `fr-fec` generation-binding HMAC tests pass across `1.8.1` → `2.x` generation rollover.
- [ ] `raptorq_artifact_gate` passes with corruption simulation on all discovered targets.
