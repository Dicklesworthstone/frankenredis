#!/usr/bin/env python3
"""Commands served by a SHARED borrowed executor, which per-command name scans cannot see.

(frankenredis-ozrro) This exists because I made the mistake it prevents. `corpus_coverage.py`
decides whether a command has borrowed machinery by matching `fn execute_plain_<cmd>_borrowed`
— one executor per command. fr also dispatches through SHARED executors that take a
discriminant, e.g.

    execute_plain_keymeta_borrowed(PlainKeyMetaCmd::Pexpiretime, key, now_ms)

A command served that way matches NOTHING in a per-command scan, so the report calls it
[C] "no borrowed machinery, one must be written first". Acting on that, I wrote a duplicate
PEXPIRETIME executor that nothing calls (ledger `b2df577ba`). There are EIGHT such enums,
so this is not a one-command wrinkle.

The error is ONE-DIRECTIONAL and that matters for how much to trust the existing report:
the per-command scan UNDERSTATES machinery, so it cannot invent a false [A] — [A] requires
finding a real per-command executor. What it produces is false [C] and false [B]. Every
lever the report has pointed at so far was real; its [C] count is an UPPER BOUND on work.

Wire the output of this into `corpus_coverage.py`'s executor set and [C] becomes honest.

Usage:
    shared_executor_map.py             # report
    shared_executor_map.py --self-test
"""

from __future__ import annotations

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RUNTIME = os.path.join(ROOT, "crates", "fr-runtime", "src", "lib.rs")

# Variants that name a DISPATCH MODE rather than a command. Listed explicitly, because
# silently dropping them would understate coverage in the same way the original bug did.
NOT_COMMANDS = {"none", "default", "plain", "ro", "withscores"}


def shared_enums(text: str) -> dict[str, list[str]]:
    """Every `pub enum Plain*Cmd` and its variants, in declaration order."""
    out: dict[str, list[str]] = {}
    for m in re.finditer(r"^pub enum (Plain[A-Za-z]*Cmd)\s*\{", text, re.M):
        name = m.group(1)
        body = text[m.end() : text.index("\n}", m.end())]
        variants = re.findall(r"^\s*([A-Z][A-Za-z0-9]*)\s*,", body, re.M)
        out[name] = variants
    return out


def covered_commands(text: str) -> set[str]:
    """Lowercased command names reachable through any shared dispatch enum."""
    names: set[str] = set()
    for variants in shared_enums(text).values():
        for v in variants:
            low = v.lower()
            if low not in NOT_COMMANDS:
                names.add(low)
    return names


def report() -> int:
    text = open(RUNTIME).read()
    enums = shared_enums(text)
    print(f"{len(enums)} shared dispatch enums in fr-runtime:\n")
    for name, variants in sorted(enums.items()):
        print(f"  {name:24} {', '.join(v.lower() for v in variants)}")
    cov = sorted(covered_commands(text))
    print(f"\n{len(cov)} commands reachable through a SHARED executor:")
    for i in range(0, len(cov), 7):
        print("   " + "  ".join(f"{c:<14}" for c in cov[i : i + 7]))
    print(
        "\nA per-command scan (`fn execute_plain_<cmd>_borrowed`) sees NONE of these.\n"
        "Any of them reported as [C] 'needs an executor written' is a FALSE POSITIVE —\n"
        "that is how a duplicate PEXPIRETIME executor got written (ledger b2df577ba)."
    )
    return 0


def _self_test() -> int:
    src = "\n".join([
        "pub enum PlainKeyMetaCmd {",
        "    Ttl,",
        "    Pttl,",
        "    Pexpiretime,",
        "}",
        "",
        "pub enum PlainRankCmd {",
        "    Zrank,",
        "    Zrevrank,",
        "}",
        "",
        "// a non-matching enum must be ignored entirely",
        "pub enum SomethingElse {",
        "    Nope,",
        "}",
    ])
    enums = shared_enums(src)
    assert set(enums) == {"PlainKeyMetaCmd", "PlainRankCmd"}, enums
    assert enums["PlainKeyMetaCmd"] == ["Ttl", "Pttl", "Pexpiretime"], enums
    cov = covered_commands(src)
    assert cov == {"ttl", "pttl", "pexpiretime", "zrank", "zrevrank"}, cov
    # `SomethingElse` is not a Plain*Cmd and must contribute nothing — a looser enum regex
    # would pull unrelated variants in and OVERSTATE coverage, turning a real [C] into a
    # missed lever, which is the opposite failure and just as bad.
    assert "nope" not in cov, cov

    # Dispatch-mode variants must not be reported as commands.
    modes = "\n".join(["pub enum PlainBitfieldGetCmd {", "    Get,", "    Ro,", "}"])
    assert covered_commands(modes) == {"get"}, covered_commands(modes)

    print("self-test ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(_self_test() if "--self-test" in sys.argv else report())
