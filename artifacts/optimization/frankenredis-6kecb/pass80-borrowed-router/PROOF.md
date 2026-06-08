# frankenredis-6kecb pass 80: borrowed fast-reply router rejection

## Target

- Bead: `frankenredis-6kecb`
- Profile-backed hotspot: SETEX/PSETEX borrowed RESP path, where profiled samples include `parse_command_args_borrowed_into`, `dispatch_with_client_context`, and `command_table_index`.
- Lever tested: replace the linear borrowed fast-path matcher chain in `fr-server` with a command length/first-byte router so unsupported commands like SETEX/PSETEX skip unrelated matcher probes before falling back to the generic path.

## Builds

- Baseline source: `8fafed0a9c3e599b436ae5512ab348398438bbb9`
- Baseline RCH worker: `vmi1152480`
- Baseline binary SHA256: `a8a0fe384b406ddb0a929b8abfe13c9baec3c782e36e165561d31238cd8e1258`
- Candidate source: live pass-80 patch on top of `8fafed0a9`
- Candidate RCH worker: `vmi1149989`
- Candidate binary SHA256: `ffccb319a77cafa3c18164169c2beec3d0fb612198f830ffbe85f3a01d689a90`

## Behavior proof

- Focused RCH check: `cargo check -p fr-server --all-targets` passed.
- Focused RCH test: `cargo test -p fr-server borrowed_fast_reply_router_covers_supported_commands_and_fallbacks -- --nocapture` passed.
- Golden comparator: `artifacts/optimization/frankenredis-svgvb/setex_golden_compare.py 27080 27081`.
- Baseline output bytes: `992`
- Candidate output bytes: `992`
- Shared golden SHA256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Comparator result: `equal=true`

Isomorphism notes: the router only chooses which existing borrowed matcher is attempted for a command length/first-byte bucket. It does not change command implementations, key ordering, tie-breaking, expiration timestamp inputs, output encoding, or RNG behavior. SETEX/PSETEX remain unsupported by the borrowed fast-reply table and fall back to the generic path.

## Performance result

Command:

```bash
hyperfine --warmup 1 --runs 7 --export-json artifacts/optimization/frankenredis-6kecb/pass80-borrowed-router/paired-setex-p16-1m-hyperfine.json --command-name baseline --command-name candidate ...
```

Results:

- Baseline: `4.554725265434286s +/- 0.05473310892464736`
- Candidate: `4.524410696434286s +/- 0.06671259906333712`
- Ratio: `1.01x +/- 0.02`

Decision: rejected. The measured delta is below the Score>=2.0 keep gate and within noise for this workload, so the source hunk was removed and only the rejection evidence is kept.

