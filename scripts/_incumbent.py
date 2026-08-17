"""Shared incumbent-arm provenance check for the measurement harnesses.

(cross-project check) Every fr/redis ratio in this repo divides by a CHECKED-IN binary at
`legacy_redis_code/redis/src/redis-server`, which nothing in the build rebuilds. If the
vendored source is updated without rebuilding it, every ratio silently acquires a stale
DENOMINATOR -- the half nobody re-derives. franken_networkx measured through an artifact
2,751 lines and twelve days behind its repo and it INVERTED a ratio by 5.4x.

This lives in its own module because THREE harnesses divide by that binary and a guard that
only one of them calls is not a guard.
"""

import os
import re
import subprocess


def incumbent_provenance(version_line, head_sha):
    """Does the vendored redis BINARY match the vendored redis SOURCE?

    redis stamps `sha=<short>:<dirty>` into `--version`, where dirty=0 means the tree was
    clean at build time. Same commit AND clean means the binary IS the source.

    Returns (ok, message). Refuses a DIRTY build too: `sha=...:1` is a binary containing
    changes that are in no commit, so "which source is this" has no answer.
    """
    m = re.search(r"sha=([0-9a-f]+):(\d+)", version_line or "")
    if not m:
        return False, ("redis --version has no sha= stamp; cannot establish which source "
                       "the incumbent binary was built from")
    bin_sha, dirty = m.group(1), m.group(2)
    if dirty != "0":
        return False, ("incumbent binary was built from a DIRTY tree (sha=%s:%s); its "
                       "source is not any commit" % (bin_sha, dirty))
    if not head_sha:
        return False, ("cannot read the vendored source tree HEAD; incumbent provenance "
                       "is unverifiable")
    if not head_sha.startswith(bin_sha):
        return False, ("INCUMBENT DRIFT: binary built from %s but vendored source HEAD is "
                       "%s -- rebuild redis-server or every ratio has a stale denominator"
                       % (bin_sha, head_sha[:len(bin_sha) + 4]))
    return True, ("incumbent verified: redis-server sha=%s == vendored source HEAD, clean"
                  % bin_sha)


def check_incumbent_provenance(redis_bin, vendored_tree):
    """Run `incumbent_provenance` against a live binary and its vendored tree."""
    try:
        ver = subprocess.run([redis_bin, "--version"], capture_output=True, text=True,
                             timeout=30).stdout
    except (OSError, subprocess.SubprocessError) as exc:
        return False, "could not run the incumbent binary: %s" % exc
    try:
        head = subprocess.run(["git", "-C", vendored_tree, "rev-parse", "HEAD"],
                              capture_output=True, text=True, timeout=30).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        head = None
    return incumbent_provenance(ver, head or None)


def require_incumbent(redis_bin, vendored_tree, printer=print):
    """Print the provenance line and RAISE if the incumbent cannot be verified."""
    ok, msg = check_incumbent_provenance(redis_bin, vendored_tree)
    printer("  %s" % msg)
    if not ok:
        raise SystemExit(
            "REFUSED: %s\nEvery ratio this harness prints divides by that binary; a stale "
            "or unidentifiable denominator is worse than no measurement." % msg)
    return msg
