#!/usr/bin/env python3
"""Is this measurement binary as new as the code it claims to measure?

WHY THIS EXISTS, measured rather than imagined. On 2026-08-16 I checked my own
scratchpad fr binary against HEAD after franken_networkx reported that its INSTALLED
package had drifted 2,751 lines and twelve days behind its repo, and that the drift
INVERTED a ratio by 5.4x. Mine had drifted too -- two commits, one of them
`perf(loop): stop busy-spinning the event loop ... sinter_big -75.6 pct`. A probe I
had run one turn earlier used that binary, so it was measuring an engine that was
missing a 75.6 pct change on the very shape under discussion.

Nothing in this repo's instruments caught it. They all take an arbitrary path:

    restore_instr_per_op.py <fr_bin> ...
    restore_profile_frames.py <fr_bin> ...
    restore_cert_gate.sh <fr_bin> ...
    hash_restore_read_premise_run.sh <fr_bin> ...

and a stale binary produces a clean, reproducible, wrong number -- the worst kind,
because nothing looks broken.

THE CHECK is deliberately the cheap conservative one: a binary built BEFORE a commit
cannot contain that commit. So compare the binary's mtime against the newest commit
touching a compiled crate. This can only ever be right about staleness; it says
nothing about a binary NEWER than HEAD (a working-tree build with uncommitted
changes), which is normal and is not flagged.

It WARNS rather than refuses, and the distinction is deliberate: deliberately
measuring an older arm is a legitimate A/B (scripts/paired_ab_build.sh builds exactly
such a pair). Set FR_ALLOW_STALE=1 to silence it when that is what you are doing.

    assert_fresh_build.py <binary>        exit 0 fresh (or allowed), 1 stale
"""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# Only crates/ -- a commit touching scripts/ or docs/ cannot change the binary.
COMPILED = "crates/"


def newest_compiled_commit():
    """(unix_ts, sha, subject) of the newest commit touching a compiled crate."""
    out = subprocess.run(
        ["git", "-C", str(ROOT), "log", "-1", "--format=%ct%x00%h%x00%s", "--", COMPILED],
        capture_output=True, text=True, check=False).stdout.strip()
    if not out:
        return None
    ts, sha, subject = out.split("\0", 2)
    return int(ts), sha, subject


def stale_commits(binary_mtime):
    """Commits touching compiled crates that are NEWER than the binary."""
    out = subprocess.run(
        ["git", "-C", str(ROOT), "log", "--format=%ct%x00%h%x00%s",
         f"--since=@{int(binary_mtime)}", "--", COMPILED],
        capture_output=True, text=True, check=False).stdout.strip()
    if not out:
        return []
    rows = []
    for line in out.splitlines():
        ts, sha, subject = line.split("\0", 2)
        if int(ts) > binary_mtime:
            rows.append((sha, subject))
    return rows


def check(path):
    binary = Path(path)
    if not binary.exists():
        print(f"assert_fresh_build: {path} does not exist", file=sys.stderr)
        return 1
    mtime = binary.stat().st_mtime
    missing = stale_commits(mtime)
    if not missing:
        newest = newest_compiled_commit()
        tip = f" (newest compiled commit {newest[1]})" if newest else ""
        print(f"fresh: {binary.name} is not behind any commit touching {COMPILED}{tip}")
        return 0

    print(f"STALE BINARY: {binary.name} predates {len(missing)} commit(s) to {COMPILED}",
          file=sys.stderr)
    for sha, subject in missing[:8]:
        print(f"    {sha}  {subject[:96]}", file=sys.stderr)
    if len(missing) > 8:
        print(f"    ... and {len(missing) - 8} more", file=sys.stderr)
    print("  A binary built before a commit cannot contain it, so this arm is measuring",
          file=sys.stderr)
    print("  an engine that is missing the above. Rebuild, or set FR_ALLOW_STALE=1 if you",
          file=sys.stderr)
    print("  are deliberately measuring an older arm (a paired A/B, for instance).",
          file=sys.stderr)
    return 1


def main():
    if len(sys.argv) != 2:
        print("usage: assert_fresh_build.py <binary>", file=sys.stderr)
        return 2
    rc = check(sys.argv[1])
    if rc and os.environ.get("FR_ALLOW_STALE") == "1":
        print("  FR_ALLOW_STALE=1 set -- proceeding with the stale arm.", file=sys.stderr)
        return 0
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
