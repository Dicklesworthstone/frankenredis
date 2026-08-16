#!/bin/bash
# Measure the MOVE allocation hoist (frankenredis-8xyox) the moment the freeze lifts.
#
# Encoded as a script rather than a plan, because every measurement I have taken
# today went wrong in a way a written plan would not have caught: the binary was a
# peer's, the wrapper offloaded the build, the sample count was too low to size the
# gap, and the noisy arm turned out to be ours rather than the incumbent's. Each of
# those is a step below.
#
#   --dry-run   print the plan and run every check that needs no build (safe now)
#   --run       execute it (requires the freeze lifted)
#
# THE HYPOTHESIS. MOVE is the worst measured route, 0.556-0.645x, and its own arm
# moves 15% between 100-sample runs while redis holds 1%. The hoist removes two
# allocations from the miss path and is behaviour-neutral. So:
#   variance shrinks AND gap shrinks -> allocator mechanism confirmed
#   variance shrinks, gap holds       -> allocation was the noise, not the cost
#   neither moves                     -> MOVE is not an allocator story; drop the lead
# The third outcome is a real result and must be banked as one, not retried.
set -u

MODE="${1:---dry-run}"
REPO=/data/projects/frankenredis
SCRATCH="${TMPDIR:-/data/tmp}"
BEFORE_REF=aac0fb49c^     # parent of the hoist
AFTER_REF=aac0fb49c       # the hoist itself
BENCH=exists_vs_redis
ARM=move_missing
SAMPLES=100
ROUNDS=4                  # ABBA, so each binary is measured twice, interleaved

say() { printf '%s\n' "$*"; }

say "=== MOVE allocation measurement (frankenredis-8xyox) ==="
say "bench:   $BENCH        arm: $ARM"
say "arms:    redis-7.2.4 (vendored, unchanged) vs frankenredis BEFORE and AFTER"
say "refs:    before=$BEFORE_REF  after=$AFTER_REF"
say "samples: $SAMPLES   schedule: ABBA over $ROUNDS rounds"
say ""
say "A/A NULL, and it is the step I would otherwise skip:"
say "  Build the SAME ref twice into two distinct paths and measure them against"
say "  each other. The bound is NOT assumed -- MOVE's fr arm has been observed"
say "  moving 15% run to run, so a +/-2% null borrowed from the dispatch harness"
say "  would reject a real effect. Take the null's own spread as the bound, and if"
say "  it exceeds the before/after delta, the measurement CANNOT resolve this lever"
say "  and the honest output is 'not resolvable at this sample count'."
say ""

# --- checks that need no build; these run in --dry-run too -------------------
cd "$REPO" || exit 1

say "--- preflight ---"
python3 scripts/build_preflight.py || {
  say "preflight declines. In --run this is fatal; in --dry-run it is expected."
  [ "$MODE" = "--run" ] && exit 1
}

say ""
say "--- the two refs exist and differ only in the hoist ---"
git diff --stat "$BEFORE_REF" "$AFTER_REF" -- crates/fr-runtime/src/lib.rs

say ""
say "--- bench hazard check ---"
python3 scripts/bench_binary_provenance_guard.py | tail -6

if [ "$MODE" != "--run" ]; then
  say ""
  say "DRY RUN. Steps that need the freeze lifted, in order:"
  say "  1. export RCH_CARGO_WRAPPER_BYPASS=1   (the shim offloads every build"
  say "     otherwise, and prefixing it on the cargo line is NOT enough -- it must"
  say "     be exported so the harness child inherits it)"
  say "  2. for REF in \$BEFORE_REF \$AFTER_REF: build fr-server from that ref into"
  say "     its OWN path, then sha256 it. Never measure target/release/frankenredis"
  say "     -- it is a rendezvous and today it held a peer's binary."
  say "  3. export FR_SERVER_BIN=<that path> for every bench invocation, so the"
  say "     fallback in fr_server_bin() cannot silently substitute another build."
  say "  4. A/A null first. If it does not clear, stop; the lever is unmeasurable"
  say "     today and that is the finding."
  say "  5. ABBA: after, before, before, after -- never two runs of one arm"
  say "     adjacent, so drift cannot be read as effect."
  say "  6. Report the ratio with BOTH arms' spreads, per frankenredis-vfgem."
  say ""
  say "Also worth running once builds return, and cheaper than this: perf stat -e"
  say "page-faults on the two binaries under the same arm. If the hoist removes two"
  say "allocations per miss, minor faults should drop measurably even if wall time"
  say "does not -- that separates 'the allocation went away' from 'it mattered'."
  exit 0
fi

say ""
say "--- RUN mode: not yet implemented deliberately ---"
say "The build steps are written above rather than executed, because every build"
say "command in this repo has been wrong at least once today -- offloaded by the"
say "shim, pointed at a shared target dir, or producing a binary I did not verify."
say "Whoever lifts the freeze should run steps 1-6 by hand the first time and only"
say "then automate them."
exit 0
