#!/usr/bin/env python3
"""Fail if a patch held in scripts/ no longer applies.

Two levers are currently parked as patches rather than edits, because /data has been
below the build floor for four turns and an uncompiled edit to main.rs reddens every
pane's suite (frankenredis-50ntn, frankenredis-ex3il). That is the right trade while
there is no slot, but it has a failure mode of its own: main.rs moves hourly in this
checkout, so a held patch rots SILENTLY. The first sign would be `git apply` failing
for whoever finally has the headroom to land it -- at exactly the moment they are
trying to spend a scarce build.

This turns that into a fast local check. It does not compile anything and does not
need a build slot.

Exit 0 = every held patch still applies. Exit 1 = at least one has rotted, and the
rotted ones are named.

Usage:  python3 scripts/held_patches_check.py [--self-test]
"""

import glob
import os
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PATCH_DIR = os.path.join(REPO, "scripts")


def held_patches():
    return sorted(glob.glob(os.path.join(PATCH_DIR, "*.patch")))


def applies(path):
    """Does `git apply --check` accept this patch against the working tree?"""
    proc = subprocess.run(
        ["git", "apply", "--check", path],
        cwd=REPO, capture_output=True, text=True, check=False)
    return proc.returncode == 0, proc.stderr.strip()


def already_landed(path):
    """Is this patch's content ALREADY in the tree?

    A patch that has been applied and committed no longer applies -- and reporting
    that as rot is a false alarm, which is how a guard gets ignored. `git apply
    --reverse --check` succeeds exactly when the change is already present, so it
    separates "landed" from "rotted" without guessing.
    """
    proc = subprocess.run(
        ["git", "apply", "--check", "--reverse", path],
        cwd=REPO, capture_output=True, text=True, check=False)
    return proc.returncode == 0


def main():
    patches = held_patches()
    if not patches:
        print("no held patches in scripts/ -- nothing to check")
        return 0

    rotted = []
    for path in patches:
        ok, err = applies(path)
        name = os.path.relpath(path, REPO)
        if ok:
            print("  OK      %s" % name)
        elif already_landed(path):
            print("  LANDED  %s  (content already in the tree; not rot)" % name)
        else:
            rotted.append((name, err))
            print("  ROTTED  %s" % name)
            for line in err.splitlines()[:3]:
                print("            %s" % line)

    if rotted:
        print("\n%d held patch(es) no longer apply. Re-generate against current main "
              "BEFORE spending a build slot on them: the point of holding a patch is "
              "that it is ready the moment a slot appears." % len(rotted))
        return 1
    print("\nall %d held patch(es) still apply" % len(patches))
    return 0


def self_test():
    """A check that cannot fail is not a check.

    Corrupt a real patch in memory and require `git apply --check` to reject it.
    Written against a COPY under the scratch dir, never against the held patch
    itself -- this repo's standing rule is to delete and overwrite nothing.
    """
    patches = held_patches()
    if not patches:
        print("SELF-TEST SKIPPED: no held patches to corrupt")
        return 0

    ok, _ = applies(patches[0])
    if not ok:
        print("SELF-TEST FAIL: %s does not apply even before corruption, so the "
              "mutation below would prove nothing" % os.path.basename(patches[0]))
        return 1

    scratch = os.environ.get("TMPDIR") or "/data/tmp"
    mutant = os.path.join(scratch, "held_patch_mutant.patch")
    body = open(patches[0], encoding="utf-8", errors="replace").read()
    # Change a context line so the hunk cannot match. Corrupting the HEADER would
    # also fail, but for a parse reason rather than a rot reason, and rot is what
    # this guard is for.
    lines = body.splitlines(keepends=True)
    # Only INSIDE a hunk. These patches carry a prose header whose lines are also
    # indented, and corrupting one of those changes nothing git looks at -- the
    # first version of this self-test did exactly that and correctly reported
    # itself VACUOUS rather than passing.
    in_hunk = False
    for i, line in enumerate(lines):
        if line.startswith("@@"):
            in_hunk = True
            continue
        if in_hunk and line.startswith(" ") and len(line.strip()) > 12:
            lines[i] = " ZZ_ROTTED_CONTEXT_LINE_THAT_CANNOT_MATCH\n"
            break
    else:
        print("SELF-TEST FAIL: no in-hunk context line found to corrupt")
        return 1
    open(mutant, "w").write("".join(lines))

    proc = subprocess.run(["git", "apply", "--check", mutant],
                          cwd=REPO, capture_output=True, text=True, check=False)
    if proc.returncode == 0:
        print("VACUOUS: a patch with a corrupted context line still passes "
              "`git apply --check`, so this guard proves nothing")
        return 1
    print("self-test: corrupting a context line is CAUGHT (%s)"
          % proc.stderr.strip().splitlines()[0][:70])
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else main())
