# frankenredis-x5i4w Rejection Proof

## Lever

Output-side zero-copy for large-value GET replies.

Target: avoid cloning stored bytes into a `RespFrame` and avoid a second payload
copy into the socket write buffer for GET/MGET/LRANGE-class replies.

## Profile-backed target

Prior profiling and bead evidence identified large-value GET reply construction
as the candidate read-path bottleneck after keyspace lookups had been reduced to
foldhash probes. The target workload was pipelined 64 KiB GET.

## Gates

### Direct GET wire/output fast path

Command:

```text
hyperfine paired 64KiB GET p16, 50 clients, 50k requests
```

Baseline:

```text
2.6894852571800003 s +/- 0.05924456883294379
```

Candidate:

```text
2.74820721718 s +/- 0.27125024290910166
```

Verdict: rejected. The candidate regressed the measured gate.

### Segmented/vectored bulk reply queue

Command:

```text
hyperfine paired short 64KiB GET p16, 50 clients, 20k requests
```

Baseline:

```text
1.7064432705800001 s +/- 0.05642971711594915
```

Candidate:

```text
1.57452241638 s +/- 0.06418255349378861
```

Ratio: 1.08x. Verdict: rejected. The result is below the Score>=2.0 keep gate
and below the >=2x bead target, and it did not survive the longer direct-output
gate.

## Isomorphism Proof

- Ordering preserved: yes. No source hunk is retained.
- Tie-breaking unchanged: yes. No source hunk is retained.
- Floating-point: N/A.
- RNG seeds: unchanged/N/A.
- Golden outputs: no retained behavior change. Local evidence hashes:

```text
74d7854a7e437a3eab7ec4e479f53e4c3b1a157101da3cdcbed0769458f07094  artifacts/optimization/cod-perf-20260604-x5i4w-output-zero-copy/paired-baseline-get64k-p16-last.json
ab5f2cf94a5044c48a9c0cdce465fc7ba35cc77fc9b45f76ad07b2ad2d8c4078  artifacts/optimization/cod-perf-20260604-x5i4w-output-zero-copy/paired-candidate-get64k-p16-last.json
b4734be58b0b92e3afef6e3394170802f6ba6ee5f0e35cf7fb6f23f1759d17e2  artifacts/optimization/cod-perf-20260604-x5i4w-vectored-bulk/paired-short-baseline-get64k-p16-last.json
fa4c88fdae7166b18ec4c702b52fbf3132334332120144fc2c0d9f466aa307f0  artifacts/optimization/cod-perf-20260604-x5i4w-vectored-bulk/paired-short-candidate-get64k-p16-last.json
```

## Decision

No production source is retained for `frankenredis-x5i4w`.

Next primitive: switch away from this narrow reply-copy lever and attack a
different profile-backed axis. The live ready queue has
`frankenredis-w2t01`, a memory/cache-layout primitive for boxed large
`Value` variants plus small-string storage.
