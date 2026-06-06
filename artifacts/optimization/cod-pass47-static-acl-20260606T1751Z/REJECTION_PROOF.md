# Pass47 rejection proof: static SET ACL selector packet

Target: `frankenredis-ohsk5`, SET P16/300k command metadata and ACL selector cost.

Fresh profile target from `cod-pass47-reprofile-20260606T1745Z`:
- `RandomState::hash_one::<&[u8]>`: 7.14%
- `Runtime::execute_dispatch`: 5.00%
- `Runtime::execute_frame_internal`: 3.17%
- `Runtime::dispatch_with_client_context`: 2.68%
- `AclUser::acl_permission_error_for_argv`: 1.40%
- `canonical_command_fullname`: 1.17%
- `command_table_index`: 0.92%
- `acl_command_selectors_for_argv`: 0.72%
- `classify_command`: 0.60%

Lever tested:
- Add a static ACL selector for top-level `SET` and make `AclUser::is_command_allowed_for_argv` consume it before building `Vec<String>` selectors.
- The candidate preserved the same deny-first, allow-second, category, and `all_commands` checks.
- The source hunk was removed after benchmark rejection.

Behavior proof:
- Ordering/tie-breaking: unchanged; command dispatch, store mutation, ACL deny ordering, and key checks were not reordered.
- Floating point/RNG: not touched.
- ACL semantics: static selector was exactly `"set"`, matching `acl_command_selectors_for_argv([SET,...])`.
- Golden TCP RESP covered default SET, `ACL SETUSER`, `AUTH`, allowed GET, and denied SET:
  - input sha256: `1eece928f4a5ec738b73db3047e60dafe0ae8c618ca695f3bbde4e61a0a861ab`
  - baseline output sha256: `8bd8baffef27e6b2e883c3477006530e1c6e0426e75a791764c3a04b0d8685ae`
  - candidate output sha256: `8bd8baffef27e6b2e883c3477006530e1c6e0426e75a791764c3a04b0d8685ae`
  - output length: 94 bytes baseline, 94 bytes candidate

Validation:
- `cargo fmt -p fr-command -- --check`
- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass47-static-acl-check2 cargo check -p fr-command -p fr-runtime --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass47-static-acl-tests2 cargo test -p fr-command acl_command_selectors_prefer_known_subcommands -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass47-static-acl-runtime-tests cargo test -p fr-runtime acl_dispatch_permission_snapshot_cache_invalidates_on_acl_generation_and_user_switch_ptqye -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass47-static-acl-candidate cargo build --release -p fr-server -p fr-bench`
- Broad `cargo fmt -p fr-runtime -- --check` remains red on pre-existing whole-file formatting drift; no runtime formatting churn was kept.

Baseline:
- Clean-head SET P16/300k standalone hyperfine: `1.175s +/- 0.135s`.
- Clean-head SET P16/1M profile: `352082.47283496766 ops/sec`, p99 `4111us`, total `2840ms`, zero lost samples.

Benchmark after lever:
- Paired SET P16/300k, 20 runs:
  - baseline: `0.93249730439s +/- 0.02472774030s`
  - candidate: `0.94919881529s +/- 0.03323821269s`
  - baseline faster by `1.02x +/- 0.04`

Decision:
- Reject. The static ACL selector packet regressed the same-machine paired benchmark, so Score < 2.0.
- Candidate source hunk removed before commit.

Next structural route:
- Stop command/ACL metadata micro-packets. The next pass should attack a broader dataflow primitive: borrowed/arena command execution that removes `Vec<Vec<u8>>` argv copies as a class, or store key-hash/key-layout work if the next profile puts RandomState/memcmp/store probes first.
