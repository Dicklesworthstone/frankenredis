# Isomorphism Proof - Round 2 Error/Protocol/Replay Expansion

## Change: Redis-style error normalization + protocol-negative + replay fixture paths

- Ordering preserved: yes, command execution order remains fixture-order deterministic.
- Tie-breaking unchanged: yes, no randomized branch selection added.
- Floating-point: N/A.
- RNG seeds: N/A.
- Golden outputs: `sha256sum -c golden_checksums.txt` passed for expanded fixture corpus.

## Behavioral notes

- Observable behavior intentionally changed where prior output diverged from Redis-style errors
  (`wrong arity` and unknown-command formatting).
- These changes are compatibility-aligning, not optimization-altering.
- Core command side effects and key mutation order remained unchanged.

## Verification

- `cargo test --workspace` passed.
- `cargo test -p fr-conformance -- --nocapture` passed.
- Protocol-negative suite passes with explicit fail-closed error strings.
- Replay suite passes with temporal assertions.
