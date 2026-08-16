#!/usr/bin/env python3
"""Answer 'may I start a build right now?' identically from every pane.

The budget (orders file section 7) is: check free space before ANY build, skip
below 59G, one build per pane and two per project. Holding that needs every pane
to COUNT THE SAME WAY, and the obvious count is wrong in both directions:

  THRESHOLD: the floor was 60G and is now 59G. /data idles at ~60G once builds
  drain, so a 60G floor sat exactly ON the working set -- every preflight
  declined, and the project went to ZERO concurrent jobs while every pane
  reported itself compliant. A threshold set at the resting value is
  indistinguishable from one set at infinity, and it fails silently in the
  direction that looks like discipline.

  UNDERCOUNT: frankenredis's crates are named fr-*, not frankenredis. A
  `pgrep | grep frankenredis` sees `cargo build --bin frankenredis` but is blind
  to `cargo test -p fr-command`, `-p fr-runtime`, `-p fr-store` and the other
  thirteen. Measured live at 2 real jobs against 1 reported -- half. Every pane
  can self-report compliant while the project sits at twice its budget, which is
  how prose caps fail without anyone defecting.

  OVERCOUNT: the near-miss crates on this host are fp-conformance and
  fnp-conformance (other projects) against our fr-conformance. Any loosened
  pattern like `f.-conformance` sweeps them in and blocks a build that was fine.

  SELF-MATCH: a pgrep whose pattern names what it searches for matches the shell
  running it, because the wrapper carries the whole pipeline as its argument. No
  choice of pattern fixes that (frankenredis-gs40t); the searcher must be
  excluded. Every self-match is a `zsh -c ... shell-snapshots ...` line.

So membership is decided by the crate list read from crates/ at runtime rather
than by a project-name substring, and wrappers are dropped explicitly.

Exit 0 = clear to build. Exit 1 = do not build; the reason is printed.
Usage:  python3 scripts/build_preflight.py [--budget N] [--floor-gb N]
"""

import argparse
import os
import re
import shutil
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# A wrapper shell, not a compiler. These carry the pattern we search for.
WRAPPER_MARKERS = ("zsh -c", "bash -c", "shell-snapshots")


def free_gb(path="/data"):
    """Free space in GB, from the filesystem rather than by parsing df output."""
    return shutil.disk_usage(path).free / (1024 ** 3)


def df_h_avail(path):
    """What `df -h` PRINTS for available space, in GB, or None if unreadable.

    Not a model of df's rounding -- it rounds up, and guessing the rule is how the
    first version of this warning failed to fire at 58.3G while df showed 59G. Ask
    df what it says and compare.
    """
    try:
        out = subprocess.run(["df", "-h", path], capture_output=True, text=True,
                             check=False).stdout.splitlines()
    except FileNotFoundError:
        return None
    if len(out) < 2:
        return None
    fields = out[-1].split()
    if len(fields) < 4:
        return None
    avail = fields[3]
    try:
        if avail.endswith("G"):
            return float(avail[:-1])
        if avail.endswith("T"):
            return float(avail[:-1]) * 1024
    except ValueError:
        return None
    return None


def our_crates():
    """Crate names owned by THIS repo, read from disk so it cannot drift."""
    crates_dir = os.path.join(REPO, "crates")
    return sorted(
        name for name in os.listdir(crates_dir)
        if os.path.isdir(os.path.join(crates_dir, name))
    )


def running_jobs():
    """Live cargo invocations, wrappers excluded."""
    try:
        out = subprocess.run(
            ["pgrep", "-af", r"cargo (build|test|run)"],
            capture_output=True, text=True, check=False).stdout
    except FileNotFoundError:
        sys.exit("pgrep not available; cannot count builds, so cannot clear a build")
    jobs = []
    for line in out.splitlines():
        if not line.strip():
            continue
        if any(marker in line for marker in WRAPPER_MARKERS):
            continue
        pid, _, cmd = line.partition(" ")
        jobs.append((pid, cmd))
    return jobs


def owns(cmd, crates):
    """Does this cargo invocation belong to this repo?

    Exact `-p <crate>` matches only, so fp-conformance and fnp-conformance do not
    masquerade as fr-conformance.
    """
    for crate in crates:
        if re.search(r"(?:^|\s)-p\s+%s(?:\s|$)" % re.escape(crate), cmd):
            return True
    if "--bin frankenredis" in cmd or "legacy_redis_code" in cmd:
        return True
    return False


# Hardcoded oracle for --self-test. Written from the fleet's actual job lines,
# NOT derived from crates/ -- a corpus taken from the thing it validates proves
# nothing (frankenredis-feedback_test_oracle_derived_from_source_is_tautological).
SELF_TEST_CASES = [
    ("cargo test -p fr-command --lib", True),
    ("cargo build --release -p fr-server --bin frankenredis", True),
    ("cargo test -p fr-runtime --lib", True),
    ("cargo test -p fr-conformance", True),
    ("cargo build --overlay-path legacy_redis_code/redis/src/commands", True),
    ("cargo test -p fp-conformance --release --lib", False),
    ("cargo test -j2 -p fnp-conformance --test ledger_hygiene", False),
    ("cargo test -j 1 -p fm-layout --lib", False),
    ("cargo test -j2 -p frankenlibc-abi --test conformance_diff_strcoll_l", False),
    ("cargo build --release -p frankenmermaid-cli", False),
    ("cargo test -j 1 -p fsci-stats --lib brunnermunzel", False),
    # The case that makes the exact `-p` match load-bearing rather than
    # decorative: a crate of ours named as a FEATURE of someone else's build.
    ("cargo test -p other --features fr-server", False),
]


def self_test():
    crates = our_crates()
    bad = [(cmd, want, owns(cmd, crates))
           for cmd, want in SELF_TEST_CASES if owns(cmd, crates) != want]
    for cmd, want, got in bad:
        print("MISMATCH expected=%s got=%s :: %s" % (want, got, cmd))
    print("%d/%d correct" % (len(SELF_TEST_CASES) - len(bad), len(SELF_TEST_CASES)))
    if bad:
        return 1

    # Mutation: replace the exact `-p <crate>` match with the bare substring
    # test a careless author would write, and require the corpus to go red. If
    # this passes, the exactness is decoration and the guard is vacuous.
    loose = lambda cmd, cr: (any(c in cmd for c in cr)
                             or "--bin frankenredis" in cmd
                             or "legacy_redis_code" in cmd)
    caught = [cmd for cmd, want in SELF_TEST_CASES if loose(cmd, crates) != want]
    if not caught:
        print("VACUOUS: substring matching passes the whole corpus, so the exact "
              "-p match is untested. Add a case that distinguishes them.")
        return 1
    print("mutation (substring instead of exact -p): CAUGHT on %d case(s)"
          % len(caught))
    return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--self-test", action="store_true",
                    help="check the ownership discriminator against a hardcoded "
                         "corpus, then mutate it and require the corpus to fail")
    ap.add_argument("--budget", type=int, default=2,
                    help="max concurrent jobs for this project (default 2)")
    # 59, not 60. A 60G floor sat exactly ON the working set: /data idles at ~60G
    # with builds drained, so every preflight declined and the fleet throttled
    # itself to almost zero while reporting compliance. A threshold set at the
    # resting value is indistinguishable from a threshold set at infinity.
    ap.add_argument("--floor-gb", type=float, default=59.0,
                    help="do not build below this many GB free (default 59)")
    ap.add_argument("--path", default="/data")
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    crates = our_crates()
    jobs = running_jobs()
    ours = [(pid, cmd) for pid, cmd in jobs if owns(cmd, crates)]

    gb = free_gb(args.path)
    print("%s free: %.1fG (floor %.0fG)" % (args.path, gb, args.floor_gb))
    print("this project: %d job(s) against a budget of %d" % (len(ours), args.budget))
    for pid, cmd in ours:
        print("  %s  %s" % (pid, cmd[:140]))

    # Report the naive count too, so a pane that has been using it can see the
    # gap rather than be told about it.
    naive = sum(1 for _, cmd in jobs if "frankenredis" in cmd)
    if naive != len(ours):
        print("  NOTE: a bare 'frankenredis' grep would report %d, not %d "
              "-- fr-* crates carry no project name" % (naive, len(ours)))

    # `df -h` ROUNDS. At 58.3G free it prints "59G", which reads as exactly at the
    # threshold and invites a build the budget forbids. Every stale go-ahead this
    # session has come in at "59G" while statvfs said 58.x. Say so explicitly
    # rather than let the two numbers quietly disagree.
    shown = df_h_avail(args.path)
    if shown is not None and gb < args.floor_gb <= shown:
        print("  NOTE: `df -h` PRINTS %.0fG here, which reads as AT the %.0fG floor. "
              "statvfs says %.1fG, which is BELOW it. df -h rounds UP; every stale "
              "go-ahead this session arrived as \"59G\"." % (shown, args.floor_gb, gb))

    blocked = []
    if gb < args.floor_gb:
        blocked.append("only %.1fG free, below the %.0fG floor" % (gb, args.floor_gb))
    if len(ours) >= args.budget:
        blocked.append("%d job(s) already in flight, budget is %d"
                       % (len(ours), args.budget))

    if blocked:
        print("DO NOT BUILD: " + "; ".join(blocked))
        return 1
    print("CLEAR TO BUILD (claim the slot in Agent Mail before starting)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
