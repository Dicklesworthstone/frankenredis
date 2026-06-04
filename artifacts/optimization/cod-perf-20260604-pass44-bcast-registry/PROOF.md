# Pass 44: BCAST Client Tracking Registry

Bead: `frankenredis-yaxr7.2`

Profile target:
- `Runtime::queue_client_tracking_invalidations` was a top self-time sample in
  the SET pipeline=16 profile because every write scanned all stored client
  sessions looking for BCAST tracking clients.

One lever:
- Added `ServerState::client_tracking_bcast_clients`, a deterministic set of
  client IDs that currently have `CLIENT TRACKING ... BCAST` enabled.
- Maintained membership on `CLIENT TRACKING`, `record_client_session`,
  `RESET`, `remove_client_session`, and disconnect cleanup.
- The default no-BCAST path now skips the all-session scan and avoids allocating
  an owner list when the registry is empty.

Behavior proof:
- Ordering: invalidation targets still drain through `BTreeMap<u64, Vec<_>>`,
  preserving deterministic target ordering; key order within a command is still
  the original command key order.
- Tie-breaking: duplicate key suppression still uses the existing
  `add_tracking_invalidation` helper.
- Floating point and RNG: no floating-point or random behavior touched.
- Dead sessions: stale IDs are filtered through
  `client_session_including_current` / `client_session_exists_including_current`
  and pruned.
- Golden raw RESP transcript matched baseline and candidate:
  `6741d9fc5d9e3567bd26019a00f69b411a7379d1db12111cc2d96bc6b4c46ef8`.
  Transcript includes `CLIENT TRACKING ON BCAST PREFIX foo`, a `SET foo:1`
  invalidation push, `CLIENT TRACKING OFF`, `SET foo:2`, and `RESET`.

Validation:
- `cargo fmt -p fr-runtime --check`
- `RCH_FORCE_REMOTE=true rch exec -- env CARGO_TARGET_DIR=target-cod-pass44-bcast-check-rch cargo check -p fr-runtime --all-targets`
- `RCH_FORCE_REMOTE=true rch exec -- env CARGO_TARGET_DIR=target-cod-pass44-bcast-clippy-nodeps-rch cargo clippy -p fr-runtime --all-targets --no-deps -- -D warnings`
- Dependency-inclusive clippy was blocked by unrelated `fr-store` `too_many_arguments`.
- `RCH_FORCE_REMOTE=true rch exec -- env CARGO_TARGET_DIR=target-cod-pass44-bcast-tests-rch cargo test -p fr-runtime bcast -- --nocapture`
- `ubs crates/fr-runtime/src/lib.rs` reported the existing file-wide inventory;
  no new hunk-specific issue was introduced.

Benchmarks:
- Initial per-run-server baseline, SET p16 500k:
  `1.596631677515s +/- 0.055019122127s`.
- Persistent-server baseline:
  `1.604357328895s +/- 0.121912420256s`.
- Persistent-server candidate:
  `1.515457758815s +/- 0.060012161013s`.
- Interleaved persistent comparison:
  baseline `1.714021356800s +/- 0.194592339843s`;
  candidate `1.577966083500s +/- 0.125789121848s`;
  hyperfine summary: candidate ran `1.09 +/- 0.15` times faster.
- Keep score: Impact 3.0 x Confidence 0.8 / Effort 1.0 = 2.4.

Artifact SHA256:
- `cd5fc1987a1efc9193553a285e4b51d92e98e0f78660673f3e5b8c5d48ffa7cb  baseline-set-p16-hyperfine.json`
- `6e5af773f6e2002097c4378cfba05ec636e61390cd2253c774b50339f410dcef  baseline-persistent-set-p16-hyperfine.json`
- `3772f6be9b596e5e33d95e702b13d0b3c1363420def945091c4c0128c3d4a34f  candidate2-persistent-set-p16-hyperfine.json`
- `d1af5184c0a430f2ce36cb3d4a259dededa100ca67ac09d52c71861bbbc2e19a  paired-persistent-set-p16-hyperfine.json`
- `6741d9fc5d9e3567bd26019a00f69b411a7379d1db12111cc2d96bc6b4c46ef8  golden-baseline-tracking-bcast.resp`
- `6741d9fc5d9e3567bd26019a00f69b411a7379d1db12111cc2d96bc6b4c46ef8  golden-candidate-tracking-bcast.resp`

Next profile direction:
- Re-profile after landing; remaining `frankenredis-yaxr7` surfaces are slowlog
  timing/fast-flag split, command metadata re-derivation, and
  `ClientSession::clone`.
