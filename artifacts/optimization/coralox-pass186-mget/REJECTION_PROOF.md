# coralox pass186: MGET direct encoding rejected

## Target

- Bead: `frankenredis-ss0fz`
- Profile-backed residual: BlackThrush noted MGET still used borrowed args but returned an owned `RespFrame`, cloning values through `Store::mget`.
- Fresh confirmation: Redis `363141.16 req/s`, FrankenRedis baseline `340136.06 req/s`, `1.07x` Redis/FR on existing-key 10-key MGET.

## Candidate

One lever tested while applied:

- `Store::mget_borrow_scan`
- `Runtime::execute_plain_mget_borrowed_into`
- `fr-server` MGET route from `FastReply` to `FastEncodedReply`

The candidate encoded the MGET array and bulk elements directly into the client output buffer.

## Isomorphism

- Ordering preserved: yes. Output followed input key order, including duplicate keys.
- Tie-breaking unchanged: N/A. MGET has no ordering tie-break beyond input order.
- Floating-point unchanged: N/A.
- RNG unchanged: LFU/RNG was preserved by matching `Store::mget` sampling shape; fast-path gate keeps default hot read constraints.
- Null and wrongtype behavior unchanged: missing keys and wrongtype values both encoded as nil.
- CLIENT REPLY behavior unchanged: suppression path was covered in the raw transcript.
- RESP3 behavior unchanged: transcript covered `HELLO 3` followed by MGET null encoding.

Golden raw RESP input SHA256:

```text
c688d13485250fb1e9f34f8a832afd76123e2746e1075773dd02e626a5b40625
```

Baseline and candidate raw output SHA256:

```text
45b955a5cb7e0dfa350f9a478fa68a8805419550de38c4607d6e1237d077c3f3
```

## Validation While Applied

- `rch exec -- cargo check -p fr-server --all-targets`: passed on `vmi1227854`.
- `rch exec -- cargo test -p fr-store mget_borrow_scan_matches_owned_mget_mixed_values_and_stats -- --nocapture`: passed on `vmi1149989`.
- `rch exec -- cargo test -p fr-runtime plain_mget_borrowed_into_matches_generic_mixed_stats -- --nocapture`: passed on `vmi1149989`; surfaced a pre-existing test-only `unused_mut` warning outside this lever.
- `rch exec -- cargo clippy -p fr-store --lib -- -D warnings`: passed on `vmi1227854`.
- `rch exec -- cargo clippy -p fr-runtime --lib -- -D warnings`: passed on `vmi1227854`.
- `rch exec -- cargo clippy -p fr-server --bin frankenredis -- -D warnings`: passed on `vmi1227854`.
- `rch exec -- cargo build --release -p fr-server`: passed on `vmi1149989`.

## Benchmark

Paired hyperfine, existing-key 10-key MGET, `n=800000`, `P=8`, `c=50`, seven runs:

```text
baseline:  3.446s +/- 0.120s
candidate: 3.371s +/- 0.091s
ratio:     1.02x +/- 0.05 faster
```

Score: `0.4`, below the required `2.0`.

Decision: rejected. Production source hunk and candidate-only tests were removed before commit.

Next route: stop MGET reply-copy micro-work; attack a deeper zero-copy RESP frame/arena command-packet primitive or branchless command dispatch with fresh profile support.
