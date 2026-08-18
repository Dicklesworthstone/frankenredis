#!/usr/bin/env python3
"""Which borrowed fast-path executors are NOT gated -- by FUNCTION, not by proximity.

The borrowed-fast-path vein (project_borrowed_fastpath_skips_generic_check_vein) has produced four
real bugs, one of them a security bypass, and its standing audit is: every `execute_plain_*_borrowed`
path must apply the gates the generic path applies. The two SHARED gates are already audited clean
(frankenredis-fastpath-gate-audit-0t4rp) -- they are self-disabling and bail whenever maxmemory,
keyspace notifications, MONITOR, replicas, AOF, MULTI, tracking or a non-permissive ACL is in play.

What is NOT audited is which executors consult a gate AT ALL. This answers that.

WHY NOT A PROXIMITY GREP: I tried one first and it claimed 348 of 596 call sites in main.rs had "no
gate within 1500 characters". That number is meaningless -- the gate is evaluated ONCE per read/write
pass and threaded down as `Option<bool>`, so a text window cannot see it. Same trap as
feedback_grep_call_sites_are_not_reach. This classifies each EXECUTOR by what it does, which is a
property of the function rather than of the text near a call.

CLASSES, in the order they are tested:
  SELF      the body evaluates a gate itself (`can_execute_plain_*` or `*_allows(`)
  PARAM     the signature takes a caller-supplied verdict (`default_read_allowed` /
            `default_write_allowed` / `*_gate`) -- converted, the caller holds the cache
  DELEGATE  the body calls another borrowed executor, which carries the gate
  UNGATED   none of the above -- the residual the audit cares about

UNGATED is a LEAD, not a bug: a helper that only ever runs after its caller gated is fine. Each one
needs its callers traced by hand. The point is to shrink 244 functions down to the few that need it.
"""
from __future__ import annotations

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
RUNTIME = ROOT / "crates" / "fr-runtime" / "src" / "lib.rs"

FN_RE = re.compile(r"^    (?:pub )?fn (execute_plain_[a-z_0-9]*borrowed[a-z_0-9]*)\s*\(", re.M)
GATE_SELF = re.compile(r"can_execute_plain_[a-z_0-9]*\(|_allows\(")
GATE_PARAM = re.compile(r"default_(?:read|write)_allowed|_gate\s*:")
# NOTE the trailing `[a-z_0-9]*`. Without it the match TRUNCATES at "borrowed", so a call to
# `execute_plain_set_borrowed_ok` from `execute_plain_set_borrowed` matched as the caller's
# own name and was discarded as a self-reference -- which reported six delegating wrappers as
# UNGATED on this script's first run. The suffix forms (`_ok`, `_into`,
# `_with_default_write_gate`) are the normal shape here, so the truncated pattern was wrong
# for the majority of delegations rather than an edge case.
DELEGATE = re.compile(r"execute_plain_[a-z_0-9]*borrowed[a-z_0-9]*")


def bodies(src: str) -> list[tuple[str, str, str]]:
    """(name, signature, body) for each borrowed executor, sliced to the next same-level fn."""
    out = []
    marks = [(m.group(1), m.start()) for m in FN_RE.finditer(src)]
    for i, (name, start) in enumerate(marks):
        end = marks[i + 1][1] if i + 1 < len(marks) else len(src)
        chunk = src[start:end]
        brace = chunk.find("{")
        sig, body = (chunk[:brace], chunk[brace:]) if brace != -1 else (chunk, "")
        out.append((name, sig, body))
    return out


def classify(name: str, sig: str, body: str) -> str:
    if GATE_SELF.search(body):
        return "SELF"
    if GATE_PARAM.search(sig):
        return "PARAM"
    # a delegation is only a delegation if it names a DIFFERENT executor
    for m in DELEGATE.finditer(body):
        if m.group(0) != name:
            return "DELEGATE"
    return "UNGATED"


SELF_TEST_CASES = [
    # (name, signature, body, expected class)
    ("execute_plain_x_borrowed", "(&mut self)",
     "{ self.execute_plain_x_borrowed_ok(k, v, now) }", "DELEGATE"),
    ("execute_plain_y_borrowed", "(&mut self)",
     "{ if !self.can_execute_plain_y_borrowed(k, now) { return None; } }", "SELF"),
    ("execute_plain_z_borrowed", "(&mut self, default_write_allowed: Option<bool>)",
     "{ self.store.set(k, v) }", "PARAM"),
    ("execute_plain_w_borrowed", "(&mut self)",
     "{ self.store.set(k, v) }", "UNGATED"),
    # the exact false positive: a self-CALL must not count as delegation
    ("execute_plain_v_borrowed", "(&mut self)",
     "{ self.execute_plain_v_borrowed(k) }", "UNGATED"),
]


def self_test() -> int:
    """The first version of this script mis-classified six delegating wrappers as UNGATED.

    Case 1 is that bug: a call to the `_ok` suffix form must read as DELEGATE. Case 5 is the
    property the truncated pattern was protecting -- a genuine self-call is not a delegation --
    so the fix must not trade one error for the other.
    """
    print("SELF-TEST: classifier against hand-checked shapes")
    bad = 0
    for name, sig, body, expected in SELF_TEST_CASES:
        got = classify(name, sig, body)
        ok = got == expected
        print("  %-26s expected %-9s got %-9s %s" % (name, expected, got, "ok" if ok else "WRONG"))
        bad += 0 if ok else 1
    print("SELF-TEST: %s" % ("PASS" if bad == 0 else f"FAIL ({bad})"))
    return 1 if bad else 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    src = RUNTIME.read_text()
    rows = [(n, classify(n, s, b)) for n, s, b in bodies(src)]
    counts: dict[str, int] = {}
    for _, cls in rows:
        counts[cls] = counts.get(cls, 0) + 1

    print("borrowed fast-path executors in %s" % RUNTIME.relative_to(ROOT))
    for cls in ("SELF", "PARAM", "DELEGATE", "UNGATED"):
        print("  %-9s %d" % (cls, counts.get(cls, 0)))
    print("  %-9s %d" % ("total", len(rows)))

    ungated = [n for n, cls in rows if cls == "UNGATED"]
    if ungated:
        print()
        print("UNGATED (%d) -- trace each one's callers; a helper that only runs post-gate is fine:"
              % len(ungated))
        for n in ungated:
            print("    %s" % n)
    print()
    print("A rising UNGATED count is the signal: a NEW executor that consults no gate is exactly")
    print("the shape that produced the PUBLISH ACL bypass. This script does not decide correctness.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
