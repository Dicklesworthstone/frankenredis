# Pass196 SCARD Focus

## Decision

Evidence-only closeout. The extended-sweep SCARD residual did not reproduce in a focused fresh-server run, so no source lever was attempted.

## Binary

- Current FrankenRedis SHA256: `b0e61847537860ea0a9c7b2a4d80d7039cbc7486b07152b1afab9a64b18b2b17`

## Focused Benchmark

SCARD `bench:set`, P16/C50/n500k, fresh servers, 5-member set preloaded:

| order | Redis req/s | FrankenRedis req/s | redis/fr |
| --- | ---: | ---: | ---: |
| redis-first | 970,873.81 | 963,391.12 | 1.008x |
| fr-first | 972,762.62 | 963,391.12 | 1.010x |

## Result

No profile-backed source target. Do not tune SCARD without new evidence. Confirm another residual, such as HSTRLEN or OBJECT ENCODING, before editing.
