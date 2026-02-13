# RaptorQ Durability Status

Current status: deferred for implementation, specified by contract.

Why deferred in this round:
- This round focused on landing a runnable core vertical slice and conformance harness.
- RaptorQ sidecar generation requires selecting/landing a concrete coding implementation and artifact pipeline integration.

What is already in place:
- Durable artifact paths identified (`baselines/*`, `golden_outputs/*`, conformance fixtures).
- Envelope schema and gate contract documented in `COMPREHENSIVE_SPEC_FOR_FRANKENREDIS_V1.md`.

Next implementation step:
1. Generate sidecars for `baselines/round1_conformance_baseline.json` and `golden_outputs/core_strings.json`.
2. Emit scrub report and decode-proof stub artifact.
