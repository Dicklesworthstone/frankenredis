#!/usr/bin/env python3
"""Say WHICH ARM is noisy, instead of assuming it is the incumbent.

Written because I got this exactly backwards. Measuring MOVE at criterion's default
10 samples, the redis-7.2.4 arm read 90.07, 64.85 and 93.04 us -- a 43% swing -- and
I concluded the harness could not size the gap. At 100 samples redis is steady to 1%
(66.221 then 65.589, criterion reporting no change at p=0.57) and FRANKENREDIS is the
mover: 102.73 -> 117.92, 15% apart, with 12% outliers against redis's 2%.

Both readings are "the ratio is unstable". Only the second tells you where to look,
and it points at our own execution path rather than at the bench.

This runs an ALREADY-BUILT criterion binary N times and reports, per arm, the spread
of its median across runs. It compiles nothing and needs no build slot -- which is
the point, since it exists for periods when builds are stopped.

  python3 scripts/bench_arm_stability.py <bench-binary> [--runs 3] [--sample-size 100]
                                         [--filter move_missing]
  python3 scripts/bench_arm_stability.py --self-test

Read it as: the arm with the LARGER spread is the one whose variance is limiting the
comparison. If that is ours, a tighter ratio needs the route stabilised, not more
samples.
"""

import argparse
import os
import re
import statistics
import subprocess
import sys

# criterion prints:  <group>/<arm>\n                        time:   [lo mid hi]
ARM = re.compile(r"^(\S+/\S+)\s*$")
TIME = re.compile(r"time:\s*\[\s*([\d.]+)\s*(\w+)\s+([\d.]+)\s*(\w+)\s+([\d.]+)\s*(\w+)")

UNIT = {"ns": 1e-3, "µs": 1.0, "us": 1.0, "ms": 1e3, "s": 1e6}


def parse(text):
    """{arm: median_in_us} from one criterion run."""
    out, pending = {}, None
    for line in text.splitlines():
        m = ARM.match(line.strip())
        if m and "/" in m.group(1):
            pending = m.group(1)
            continue
        t = TIME.search(line)
        if t and pending:
            mid, unit = float(t.group(3)), t.group(4)
            # Take the MEDIAN of criterion's [lo mid hi], not lo or hi: the point
            # here is run-to-run movement of the central estimate.
            out[pending] = mid * UNIT.get(unit, 1.0)
            pending = None
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--sample-size", type=int, default=100)
    ap.add_argument("--filter", default="")
    args = ap.parse_args(
        [a for a in sys.argv[1:] if a != "--self-test"])

    if not os.path.exists(args.binary):
        sys.exit("no such binary: %s (build it separately; this script never does)"
                 % args.binary)

    # WHOSE fr-server is under test? The vs_redis benches default FR_SERVER_BIN to
    # target/release/frankenredis, which in this shared checkout is a RENDEZVOUS: I
    # measured it three times believing it was my tree, and its sha turned out to
    # belong to a build I did not make. Worse, the bench will invoke cargo to CREATE
    # that binary if it is missing, so an innocent-looking bench run can start a
    # build during a freeze.
    fr_bin = os.environ.get("FR_SERVER_BIN")
    if not fr_bin:
        sys.exit("REFUSED: FR_SERVER_BIN is unset, so the bench would fall back to "
                 "target/release/frankenredis -- a shared path whose contents you "
                 "did not necessarily build, and which the bench will cargo-build "
                 "if absent. Point it at a binary you made and can sha.")
    if not os.path.exists(fr_bin):
        sys.exit("FR_SERVER_BIN=%s does not exist" % fr_bin)
    digest = subprocess.run(["sha256sum", fr_bin], capture_output=True, text=True,
                            check=False).stdout.split()[0][:24]
    print("fr-server under test: %s\n  sha256 %s" % (fr_bin, digest))

    runs = []
    for i in range(args.runs):
        cmd = [args.binary, "--bench", "--noplot",
               "--sample-size", str(args.sample_size)]
        if args.filter:
            cmd.append(args.filter)
        proc = subprocess.run(cmd, capture_output=True, text=True, check=False)
        got = parse(proc.stdout + proc.stderr)
        if not got:
            sys.exit("run %d produced no timings; is this a criterion binary?" % (i + 1))
        runs.append(got)
        print("run %d: %d arm(s)" % (i + 1, len(got)))

    arms = sorted(set().union(*[set(r) for r in runs]))
    print("\n%-52s %10s %10s %8s" % ("arm", "min us", "max us", "spread"))
    rows = []
    for arm in arms:
        vals = [r[arm] for r in runs if arm in r]
        if len(vals) < 2:
            continue
        lo, hi = min(vals), max(vals)
        spread = (hi - lo) / lo * 100.0
        rows.append((spread, arm, lo, hi))
        print("%-52s %10.2f %10.2f %7.1f%%" % (arm, lo, hi, spread))

    if rows:
        rows.sort(reverse=True)
        worst = rows[0]
        print("\nLEAST STABLE ARM: %s at %.1f%% spread across %d runs."
              % (worst[1], worst[0], args.runs))
        print("If that arm is ours, a tighter ratio needs the ROUTE stabilised; more")
        print("samples will not help. If it is the incumbent's, raise --sample-size.")
    return 0


SELF_TEST_OUTPUT = """
exists_vs_redis/move_missing/redis-7.2.4
                        time:   [64.874 µs 66.221 µs 67.690 µs]
exists_vs_redis/move_missing/frankenredis
                        time:   [101.03 µs 102.73 µs 104.42 µs]
set_algebra_vs_redis/SUNIONSTORE_SMALL/redis-7.2.4
                        time:   [11.858 ms 11.944 ms 12.127 ms]
"""


def self_test():
    """Hardcoded criterion text, NOT captured from a run of this parser.

    A corpus taken from the thing it validates proves nothing
    (frankenredis-feedback_test_oracle_derived_from_source_is_tautological), so
    these three lines are pasted from real output and the expected values written
    by hand.
    """
    got = parse(SELF_TEST_OUTPUT)
    want = {
        "exists_vs_redis/move_missing/redis-7.2.4": 66.221,
        "exists_vs_redis/move_missing/frankenredis": 102.73,
        # ms must be converted, or a millisecond arm reads as microseconds and
        # silently dominates every spread calculation.
        "set_algebra_vs_redis/SUNIONSTORE_SMALL/redis-7.2.4": 11944.0,
    }
    bad = []
    for arm, expect in want.items():
        actual = got.get(arm)
        if actual is None or abs(actual - expect) > 0.01:
            bad.append("%s parsed as %r, expected %r" % (arm, actual, expect))
    for line in bad:
        print("SELF-TEST FAIL: " + line)
    if bad:
        return 1

    # Mutation: drop the unit conversion, as a careless author would, and require
    # the millisecond case to go wrong. Without it the check is decoration.
    naive = {k: float(TIME.search(l).group(3))
             for k, l in [("ms", "time:   [11.858 ms 11.944 ms 12.127 ms]")]}
    if abs(naive["ms"] - 11944.0) < 0.01:
        print("VACUOUS: the millisecond case parses correctly even without unit "
              "conversion, so the conversion is untested")
        return 1
    print("self-test: 3/3 arms parsed, and dropping unit conversion IS caught "
          "(11.944 would be read as us, not %.0f us)" % want[
              "set_algebra_vs_redis/SUNIONSTORE_SMALL/redis-7.2.4"])
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else main())
