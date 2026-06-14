# Pass197 HSTRLEN Focus

## Decision

Evidence-only closeout. The extended-sweep HSTRLEN residual did not reproduce as a stable focused gap, so no source lever was attempted.

## Binary

- Current FrankenRedis SHA256: `b0e61847537860ea0a9c7b2a4d80d7039cbc7486b07152b1afab9a64b18b2b17`

## Focused Benchmark

HSTRLEN `bench:hash field`, P16/C50/n500k, fresh servers, hash field preloaded:

| order | Redis req/s | FrankenRedis req/s | redis/fr |
| --- | ---: | ---: | ---: |
| redis-first | 961,538.50 | 1,020,408.12 | 0.942x |
| fr-first | 996,016.00 | 980,392.19 | 1.016x |

## Result

No profile-backed source target. Do not tune HSTRLEN without new focused evidence.
