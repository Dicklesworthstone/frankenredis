# frankenredis-4e51w pass64 proof

Status: rejected, source hunk removed.

## Profile-backed target

Current-main profile from `b5ab1a76c61f17ce53ab8048a4ca519c16bd7cdd` on GET/P16/1M showed:

- `__memmove_avx_unaligned_erms`: 9.56% flat
- `<fr_store::Value>::string_owned`: 8.83% flat
- `frankenredis::process_buffered_frames`: 1.90% flat
- `fr_protocol::parse_command_args_borrowed_into`: 1.62% flat

Lever tested: replace `ClientConnection::try_flush` front-drain with a logical output-buffer cursor so partial writes avoid memmoving queued output. Candidate patch is preserved in `candidate-output-cursor.patch`.

## Behavior proof

Raw RESP transcript:

1. `SET k v`
2. `GET k`
3. `GET missing`
4. `PING`

Baseline and candidate both emitted 24 bytes:

```text
+OK\r\n$1\r\nv\r\n$-1\r\n+PONG\r\n
```

Golden SHA-256:

```text
2612d02989f4a06e17bf0f2f06c69dfe9bc475051f1674481e85347a3f44e688
```

Isomorphism notes: no command ordering, tie-breaking, floating-point, or RNG behavior is touched. The candidate only changes internal pending-output bookkeeping during socket flush; RESP bytes, command execution, keyspace stats, and reply order remain byte-identical on the golden transcript.

## Validation

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass64-outputcursor-check cargo check -p fr-server --all-targets` passed from the clean scratch candidate worktree.
- Candidate source was validated in `/data/projects/.scratch/frankenredis-pass64-outputcursor-20260607192309`, with the vendored Redis oracle linked from the main checkout for build-script metadata.

## Benchmarks

GET P16, 300k requests, paired:

- Baseline: `462.5 ms +/- 26.5`
- Candidate: `421.4 ms +/- 14.8`

GET P16, 1M requests, reversed:

- Candidate: `1.441 s +/- 0.030`
- Baseline: `1.438 s +/- 0.088`

GET P16, 300k requests, reversed confirmation:

- Candidate: `422.2 ms +/- 27.7`
- Baseline: `419.4 ms +/- 9.5`

Decision: reject. The initial 300k win did not survive reversed 1M or reversed 300k confirmation. Score = `Impact 1.0 * Confidence 0.6 / Effort 1.0 = 0.6`, below the required `2.0`.

## Next route

Do not retry output-cursor or read-buffer cursor variants. The next profile-backed structural primitive should attack cloned/owned GET payloads and reply slabs as a class, or an epoch-validated hot-key read certificate if a key-locality profile confirms hash/drop-expire reuse dominates.
