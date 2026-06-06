# Pass46 rejection proof: SET direct dispatch arm

Target: `frankenredis-ohsk5`, SET P16/300k dispatch hot path.

Profile-backed hotspot from `cod-pass46-reprofile-20260606T1727Z`:
- `RandomState::hash_one::<&[u8]>`: 5.90%
- `Runtime::execute_dispatch`: 5.03%
- `dispatch_with_client_context`: 3.35%
- `classify_command`: 0.93%
- `command_table_index`: 0.59%

Lever tested:
- Add a direct `raw_cmd.eq_ignore_ascii_case(b"SET")` dispatch arm in `fr-command::dispatch_argv` after existing client-reply/script/read-only/ACL gates and before the generic `classify_command` match.
- This is one source lever only. It was reverted after failing the same-machine benchmark gate.

Behavior proof:
- Ordering and tie-breaking: unchanged; SET reaches the same `set(argv, store, now_ms)` handler after the same generic gates.
- Floating point/RNG: not touched.
- ACL/auth/read-only/script semantics: unchanged; the direct arm was below the existing gates and above only the generic classifier.
- Golden RESP fixture bytes:
  - input sha256: `09000457d9d24c2632b10129f2bdcd7f442ccf16e8f9ce82221e2d34090135b6`
  - baseline output sha256: `dfb555a4a8a67b66ee78296c231deb8ef024426befb1132411582fd2c6e6bb3f`
  - candidate output sha256: `dfb555a4a8a67b66ee78296c231deb8ef024426befb1132411582fd2c6e6bb3f`
  - output length: 54 bytes baseline, 54 bytes candidate

Validation:
- `cargo fmt -p fr-command -- --check`
- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass46-set-direct-check cargo check -p fr-command --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass46-set-direct-tests cargo test -p fr-command set -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass46-set-direct-candidate cargo build --release -p fr-server -p fr-bench`

Baseline before lever:
- Current-source SET P16/300k baseline from `cod-pass46-reprofile-20260606T1727Z/baseline-set-p16-300k-hyperfine.json`: mean `1.0008576713s`, stddev `0.0333255169s`.

Benchmark after lever:
- Paired 10-run, baseline first:
  - baseline: `1.0659632133s +/- 0.0777426209s`
  - candidate: `1.0078717403s +/- 0.0316988735s`
  - apparent candidate speedup: `1.06x +/- 0.08`
- Paired 20-run, baseline first:
  - baseline: `1.1869005318s +/- 0.1551700286s`
  - candidate: `1.0782965671s +/- 0.0819187240s`
  - apparent candidate speedup: `1.10x +/- 0.17`
- Reversed-order 12-run:
  - candidate: `1.0645356719s +/- 0.0901212085s`
  - baseline: `1.0315139338s +/- 0.0742298909s`
  - baseline faster by `1.03x +/- 0.11`

Decision:
- Reject. The win did not survive reversed-order confirmation, so confidence is too low for Score >= 2.0.
- Candidate source hunk removed before commit.

Next structural route:
- The fresh profile still points at dispatch/metadata/key-hash cost. The next pass should avoid another command-classifier micro-lever and instead attack a deeper primitive: borrowed/arena-backed command execution for simple write commands or a compiled ACL/command-selector packet that removes repeated selector construction and key hashing as a class.
