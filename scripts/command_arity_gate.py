#!/usr/bin/env python3
"""Differential on COMMAND ARITY, fr's `COMMAND_TABLE` against the incumbent's table.

Arity is the first thing `processCommand` checks, so a wrong value is a wrong reply to a wrong
argument count -- fr either accepts a call Redis refuses or refuses one Redis accepts, before any
of the command's own logic runs. Both sides encode it as a static table, so this needs NO SERVER,
NO BUILD and NO DISK WRITES: it is usable during a build freeze, which is when it was written.

The convention is upstream's and fr copies it: a POSITIVE arity is exact ("this many argv
entries, including the command name"), a NEGATIVE arity is a minimum ("at least this many").

BOTH LEVELS ARE COMPARED, and they must be read from different places. Top-level commands come
from `redisCommandTable[]` at the end of `commands.def`; container subcommands live in separate
`<PARENT>_Subcommands[]` blocks and are keyed `parent|sub`, because a bare `MAKE_CMD("get")`
appears three times upstream -- as GET, CONFIG GET and SLOWLOG GET -- and flattening them would
compare a command against a subcommand of the same name. fr keys its `SUBCOMMAND_TABLE` the same
way, and `check_full_command_arity` enforces both levels at once, matching upstream's
`processCommand`, which resolves the subcommand and checks its arity at the same point as the
parent's.

SENTINEL SUBCOMMANDS ARE EXCLUDED. fr is not a sentinel, so upstream's 21 `sentinel|*` rows are
absent from fr by design, not by omission; counting them as gaps would put a permanent 21 in the
output and train the reader to ignore it.

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


def incumbent_subcommand_arities() -> dict[str, int]:
    """`parent|sub` -> arity, from every `<PARENT>_Subcommands[]` block."""
    text = INCUMBENT.read_text(errors="replace")
    out: dict[str, int] = {}
    for block in re.finditer(
            r"struct COMMAND_STRUCT ([A-Z0-9_]+)_Subcommands\[\] = \{(.*?)\n\};",
            text, re.S):
        parent = block.group(1).lower()
        for line in block.group(2).splitlines():
            m = re.search(r'MAKE_CMD\("([a-z0-9_|-]+)"', line)
            a = re.search(r",([A-Za-z_][A-Za-z0-9_]*Command|NULL),(-?\d+),", line)
            if m and a:
                out[f"{parent}|{m.group(1).lower()}"] = int(a.group(2))
    return out


def fr_subcommand_arities() -> dict[str, int]:
    """`parent|sub` -> arity, from `SUBCOMMAND_TABLE`."""
    text = FR.read_text(errors="replace")
    start = text.index("const SUBCOMMAND_TABLE:")
    end = text.index("\n];", start)
    return {m.group(1).lower(): int(m.group(2))
            for m in re.finditer(r'\(\s*"([a-z0-9_|-]+\|[a-z0-9_-]+)"\s*,\s*(-?\d+)\s*,',
                                 text[start:end])}


# fr is not a sentinel; these exist upstream only in sentinel mode.
SENTINEL_PREFIX = "sentinel|"


def compare(label: str, inc: dict[str, int], fr: dict[str, int]) -> int:
    """Print a comparison and return the mismatch count."""
    shared = sorted(set(inc) & set(fr))
    mismatches = [(n, fr[n], inc[n]) for n in shared if fr[n] != inc[n]]
    print(f"{label}: incumbent {len(inc)}, fr {len(fr)}, compared {len(shared)}")
    if mismatches:
        print(f"  {'command':<28} {'fr':>6} {'redis 7.2.4':>12}   meaning")
        for name, f, i in mismatches:
            def describe(a: int) -> str:
                return f"exactly {a}" if a > 0 else f"at least {-a}"
            print(f"  {name:<28} {f:>6} {i:>12}   fr {describe(f)}, redis {describe(i)}")
    missing = sorted(n for n in set(inc) - set(fr) if not n.startswith(SENTINEL_PREFIX))
    if missing:
        print(f"  {len(missing)} in the incumbent and NOT in fr's table: "
              f"{', '.join(missing[:10])}")
    extra = sorted(set(fr) - set(inc))
    if extra:
        print(f"  {len(extra)} in fr and NOT upstream: {', '.join(extra[:10])}")
    return len(mismatches)


def main() -> int:
    inc, fr = incumbent_arities(), fr_arities()
    inc_sub, fr_sub = incumbent_subcommand_arities(), fr_subcommand_arities()
    if not inc or not fr or not inc_sub or not fr_sub:
        print(f"HARNESS INVALID: parsed {len(inc)}/{len(fr)} commands and "
              f"{len(inc_sub)}/{len(fr_sub)} subcommands; a table moved and this gate is "
              f"measuring nothing.")
        return 2

    bad = compare("top-level", inc, fr)
    print()
    bad += compare("subcommands", inc_sub, fr_sub)
    print()
    if bad:
        print(f"FAIL: {bad} arity mismatch(es). Each is a wrong reply to a wrong argument "
              f"count, decided before the command runs.")
        return 1
    print("PASS: every shared command and subcommand agrees on arity.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
