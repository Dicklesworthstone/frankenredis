# Pass195 OBJECT REFCOUNT Borrowed Fast Path

## Decision

Kept `frankenredis-tgi3w`: add an exact multibulk `OBJECT REFCOUNT key` borrowed fast path in `fr-server`/`fr-runtime`, reusing `Store::object_refcount` for Redis-visible semantics.

Score: `1.65 * 0.95 / 0.60 = 2.61`.

## Binaries

- Baseline `frankenredis`: `e48fb3500664c60e9a14a38be8e251111de1eedf6da8fa203455a611a8271253`
- Candidate `frankenredis`: `b0e61847537860ea0a9c7b2a4d80d7039cbc7486b07152b1afab9a64b18b2b17`
- Candidate `fr-bench`: `c032488a1771e30b0a73cd0b5ee72cd41061d6ae2540b890cc296fd87ebd88e3`

## Behavior Proof

- Raw RESP request SHA256: `e06b91f0a8223f0a4b6479a1e42cc76e0e1db46013aae641614f2bd43f9862de`
- Redis response SHA256: `23bd56a51526f15747ee16f39c974aca88ae10977b064c7f466b6d1e588fd8ba`
- FrankenRedis response SHA256: `23bd56a51526f15747ee16f39c974aca88ae10977b064c7f466b6d1e588fd8ba`
- Byte compare: match

Replay covered missing keys, shared integers, private strings, list keys, COPY, lowercase OBJECT/REFCOUNT, wrong-arity fallback, and QUIT. Ordering/tie-breaking is unchanged because the fast path calls the same store method as generic dispatch. Floating point and RNG are not involved.

## Benchmarks

Target, P16/C50/n500k:

| order | baseline req/s | candidate req/s | candidate/base |
| --- | ---: | ---: | ---: |
| baseline-first | 569,476.06 | 938,086.31 | 1.647x |
| candidate-first | 565,610.88 | 939,849.62 | 1.662x |

Adjacent checks, P16/C50/n300k:

| command | order | baseline req/s | candidate req/s | candidate/base |
| --- | --- | ---: | ---: | ---: |
| OBJECT ENCODING | baseline-first | 890,207.69 | 898,203.62 | 1.009x |
| OBJECT ENCODING | candidate-first | 911,854.12 | 923,076.94 | 1.012x |
| MEMORY USAGE | baseline-first | 1,006,711.38 | 1,003,344.50 | 0.997x |
| MEMORY USAGE | candidate-first | 958,466.50 | 1,016,949.19 | 1.061x |

## Gates

- `cargo fmt -p fr-server -- --check`
- `cargo fmt -p fr-runtime -- --check` still fails on broad pre-existing rustfmt drift.
- RCH `cargo check -j 1 -p fr-runtime -p fr-server --all-targets`
- RCH exact runtime test `tests::plain_object_refcount_borrowed_matches_generic`
- RCH exact server test `tests::borrowed_plain_object_refcount_args_match_exact_shape_only`
- RCH `cargo clippy -j 1 -p fr-runtime -p fr-server --all-targets -- -D warnings`
