# Pass198 OBJECT ENCODING Focus

## Decision

Evidence-only closeout. OBJECT ENCODING did not reproduce as a stable focused gap after a longer confirmation run, so no source lever was attempted.

## Binary

- Current FrankenRedis SHA256: `b0e61847537860ea0a9c7b2a4d80d7039cbc7486b07152b1afab9a64b18b2b17`

## Focused Benchmark

OBJECT ENCODING `bench:string`, P16/C50/n500k:

| order | Redis req/s | FrankenRedis req/s | redis/fr |
| --- | ---: | ---: | ---: |
| redis-first | 950,570.31 | 941,619.56 | 1.010x |
| fr-first | 1,039,501.00 | 932,835.81 | 1.114x |

Longer confirmation, P16/C50/n1M:

| order | Redis req/s | FrankenRedis req/s | redis/fr |
| --- | ---: | ---: | ---: |
| redis-first-n1m | 976,562.44 | 969,932.06 | 1.007x |
| fr-first-n1m | 932,835.81 | 914,913.06 | 1.020x |

## Result

No profile-backed source target. Do not tune OBJECT ENCODING without new focused evidence.
