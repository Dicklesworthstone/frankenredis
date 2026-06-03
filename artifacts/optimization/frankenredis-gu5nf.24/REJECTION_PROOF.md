# frankenredis-gu5nf.24 Rejection Proof

## Target

Profile-backed pass 21 candidate: remove the per-command
`server.client_sessions.insert(self.session.client_id, self.session.clone())`
inside `Runtime::execute_frame_with_optional_unix_time_us`, relying on the
server-side post-readable-batch `record_client_session` publication plus the
current-session overlay used by CLIENT LIST/INFO/KILL/tracking paths.

## Profile Basis

- Post-`frankenredis-ptqye` direct SET p16 baseline:
  `228872.31 ops/sec`, p99 `5719 us`.
- Post-`frankenredis-ptqye` hyperfine SET p16 baseline:
  `349.8 ms +/- 16.0 ms`.
- Server strace on 500k SET p16:
  `sendto 51.26%`, `recvfrom 20.76%`, `epoll_ctl 16.59%`.
- Corrected GDB server samples repeatedly hit
  `execute_frame_with_optional_unix_time_us`; one sample still hit
  `refresh_current_dispatch_client_context` clone work.
- `perf` sampling was blocked by `perf_event_paranoid=4`; see
  `artifacts/optimization/icywolf-perf-20260603-pass21-current-profile/perf-server.log`.

## Candidate Measurement

- Candidate direct SET p16:
  `231724.80 ops/sec`, p99 `6075 us`.
- Candidate hyperfine SET p16:
  `391.7 ms +/- 117.2 ms`.

## Verdict

Rejected. Direct throughput improved only `1.25%`, but p99 regressed from
`5719 us` to `6075 us`. Hyperfine regressed from `349.8 ms` to `391.7 ms`
with high variance. This fails the keep gate because the tail regressed and
the wall-clock signal is negative.

No source change is retained.

## Isomorphism

The candidate passed a focused current-session visibility regression before it
was rejected, covering CLIENT LIST ID, CLIENT TRACKING REDIRECT, and CLIENT
KILL current-client visibility without per-command snapshot publication.
Because the benchmark gate failed, the source and test changes were removed;
the final tree preserves ordering, tie-breaking, client visibility, floating
point behavior, and RNG behavior exactly as `HEAD`.

## Artifact Verification

`sha256sum -c artifacts/optimization/frankenredis-gu5nf.24/rejected-artifacts.sha256`
passed for the baseline profile artifacts and candidate benchmark JSON files.

## Next Primitive

Do not repeat this publication micro-lever. The next attack is a structurally
different dispatch-context dirty-epoch primitive: avoid per-command clone/format
work in `refresh_current_dispatch_client_context` by publishing only fields
whose source session/runtime state changed, with explicit epoch invalidation for
auth, RESP mode, client flags, buffer metrics, pubsub/tracking, and DB changes.
