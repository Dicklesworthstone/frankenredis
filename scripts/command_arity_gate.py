#!/usr/bin/env python3
"""Differential on COMMAND ARITY, fr's `COMMAND_TABLE` against the incumbent's table.

Arity is the first thing `processCommand` checks, so a wrong value is a wrong reply to a wrong
argument count -- fr either accepts a call Redis refuses or refuses one Redis accepts, before any
of the command's own logic runs. Both sides encode it as a static table, so this needs NO SERVER,
NO BUILD and NO DISK WRITES: it is usable during a build freeze, which is when it was written.

The convention is upstream's and fr copies it: a POSITIVE arity is exact ("this many argv
entries, including the command name"), a NEGATIVE arity is a minimum ("at least this many").

Upstream's table is `redisCommandTable[]` at the end of `commands.def`. Only its TOP-LEVEL rows
are compared: container subcommands (`CONFIG GET`, `SLOWLOG GET`) share bare names like "get"
with real commands and live in separate per-container tables, so pulling every `MAKE_CMD` would
compare `GET` against `CONFIG GET`. fr checks subcommand arity in a separate function
(`check_command_arity_with_subcommand`) which this gate does not cover.

Exit 0 = every command fr implements agrees with the incumbent, 1 = at least one disagrees.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INCUMBENT = ROOT / "legacy_redis_code/redis/src/commands.def"
FR = ROOT / "crates/fr-command/src/lib.rs"


def incumbent_arities() -> dict[str, int]:
    """name -> arity, from the top-level `redisCommandTable[]` only."""
    text = INCUMBENT.read_text(errors="replace")
    start = text.index("struct COMMAND_STRUCT redisCommandTable[] = {")
    table = text[start:]
    out: dict[str, int] = {}
    for line in table.splitlines():
        m = re.search(r'MAKE_CMD\("([a-z0-9_|-]+)"', line)
        if not m:
            continue
        # ...,<handler>Command,<arity>,CMD_... -- the arity is the int between the handler
        # symbol and the flag list, which is the only place an optionally-negative integer
        # sits directly after an identifier ending in "Command".
        a = re.search(r",([A-Za-z_][A-Za-z0-9_]*Command|NULL),(-?\d+),", line)
        if not a:
            continue
        out[m.group(1).lower()] = int(a.group(2))
    return out


def fr_arities() -> dict[str, int]:
    """name -> arity, from `COMMAND_TABLE`."""
    text = FR.read_text(errors="replace")
    start = text.index("const COMMAND_TABLE:")
    end = text.index("\n];", start)
    out: dict[str, int] = {}
    for m in re.finditer(r'\(\s*"([a-z0-9_|-]+)"\s*,\s*(-?\d+)\s*,', text[start:end]):
        out[m.group(1).lower()] = int(m.group(2))
    return out


def main() -> int:
    inc = incumbent_arities()
    fr = fr_arities()
    if not inc or not fr:
        print(f"HARNESS INVALID: parsed {len(inc)} incumbent and {len(fr)} fr entries; "
              f"a table moved and this gate is measuring nothing.")
        return 2

    shared = sorted(set(inc) & set(fr))
    mismatches = [(n, fr[n], inc[n]) for n in shared if fr[n] != inc[n]]

    print(f"incumbent top-level commands : {len(inc)}")
    print(f"fr COMMAND_TABLE entries     : {len(fr)}")
    print(f"compared (in both)           : {len(shared)}")
    print()
    if mismatches:
        print(f"{'command':<24} {'fr':>6} {'redis 7.2.4':>12}   meaning")
        for name, f, i in mismatches:
            def describe(a: int) -> str:
                return f"exactly {a}" if a > 0 else f"at least {-a}"
            print(f"{name:<24} {f:>6} {i:>12}   fr {describe(f)}, redis {describe(i)}")
        print()
        print(f"FAIL: {len(mismatches)} arity mismatch(es). Each is a wrong reply to a wrong "
              f"argument count, decided before the command runs.")
        return 1

    only_fr = sorted(set(fr) - set(inc))
    if only_fr:
        print(f"note: {len(only_fr)} fr entries absent from the incumbent's top-level table "
              f"(subcommands or fr-only): {', '.join(only_fr[:12])}")
    print(f"PASS: all {len(shared)} shared commands agree on arity.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
