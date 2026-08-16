#!/usr/bin/env python3
"""Flag benches that measure an unprovenanced binary, or that can start a build.

Two hazards, found the hard way. I banked three sets of measurements today believing
they described my tree; they described `target/release/frankenredis`, sha 51708552,
which I did not build. In this shared checkout that path is a RENDEZVOUS -- whoever
built last owns it.

  HAZARD A, all six vs_redis benches: `fr_server_bin()` falls back to
  target/release/frankenredis when FR_SERVER_BIN is unset. Nothing warns you. The
  numbers are real measurements of A frankenredis, just not necessarily yours, so
  they look completely normal and cannot be attributed to a commit.

  HAZARD B, three of them: `ensure_default_fr_server_bin()` invokes cargo to CREATE
  that binary when it is missing. So `cargo bench` -- or running the bench binary
  directly -- can start a BUILD. During a disk freeze that is the difference between
  a permitted action and a forbidden one, and nothing about the command says so.

This does not edit the benches: it names them, so a reviewer sees the hazard and a
freeze-time operator knows which are safe to run. Fixing hazard B properly means
making the fallback fail loudly instead of building, which is a Rust change and wants
a compiler.

Exit 0 = the hazard set is unchanged. Exit 1 = it grew, which should be a decision.

  python3 scripts/bench_binary_provenance_guard.py [--self-test]
"""

import glob
import os
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

FALLBACK = 'target_dir.join("release/frankenredis")'
BUILDER = "ensure_default_fr_server_bin"

# Observed 2026-08-16. Exact, not a range: this file exists to make a CHANGE visible.
FALLBACK_BASELINE = 6
BUILDER_BASELINE = 3


def scan():
    fallback, builder = [], []
    for path in sorted(glob.glob(os.path.join(REPO, "crates/*/benches/*.rs"))):
        text = open(path, encoding="utf-8", errors="replace").read()
        name = os.path.relpath(path, REPO)
        if FALLBACK in text:
            fallback.append(name)
        if BUILDER in text:
            builder.append(name)
    return fallback, builder


def main():
    fallback, builder = scan()
    print("HAZARD A -- silently measure target/release/frankenredis when "
          "FR_SERVER_BIN is unset (%d):" % len(fallback))
    for n in fallback:
        print("   %s%s" % (n, "   [also builds]" if n in builder else ""))
    print("\nHAZARD B -- can invoke cargo to BUILD that binary when missing (%d).\n"
          "These are NOT safe to run during a build freeze:" % len(builder))
    for n in builder:
        print("   %s" % n)
    print("\nAlways set FR_SERVER_BIN to a binary you built and can sha.")

    bad = []
    if len(fallback) > FALLBACK_BASELINE:
        bad.append("fallback benches %d, baseline %d -- a new bench inherited the "
                   "unprovenanced default" % (len(fallback), FALLBACK_BASELINE))
    if len(builder) > BUILDER_BASELINE:
        bad.append("build-capable benches %d, baseline %d -- another bench can now "
                   "start a build, which changes what is safe under a freeze"
                   % (len(builder), BUILDER_BASELINE))
    for line in bad:
        print("\nFAIL: " + line)
    return 1 if bad else 0


def self_test():
    """Pin both counts and one membership fact, hardcoded rather than derived."""
    fallback, builder = scan()
    bad = []
    if len(fallback) != 6:
        bad.append("expected 6 benches with the fallback, got %d: %s"
                   % (len(fallback), fallback))
    if len(builder) != 3:
        bad.append("expected 3 build-capable benches, got %d: %s"
                   % (len(builder), builder))
    # set_algebra has the fallback but NOT the builder -- it will fail rather than
    # build if the binary is missing. That asymmetry is the whole point of listing
    # the two hazards separately, so pin it.
    sa = [n for n in fallback if "set_algebra" in n]
    if not sa:
        bad.append("set_algebra_vs_redis should carry the fallback")
    if any("set_algebra" in n for n in builder):
        bad.append("set_algebra_vs_redis should NOT be build-capable; if it now is, "
                   "the freeze-safe list is wrong")
    for line in bad:
        print("SELF-TEST FAIL: " + line)
    if bad:
        return 1
    print("self-test: 6 fallback, 3 build-capable, and set_algebra is fallback-only")
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else main())
