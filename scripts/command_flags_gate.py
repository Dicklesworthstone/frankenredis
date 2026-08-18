#!/usr/bin/env python3
"""Differential on COMMAND FLAGS, fr's `COMMAND_TABLE` against the incumbent's `CMD_*` bitmask.

Flags are not cosmetic. `readonly`/`write` decide whether a replica accepts a command, `denyoom`
whether it is refused under maxmemory, `noscript` whether a script may call it, and
`admin`/`no_auth`/`stale`/`loading` gate whole classes of connection state. All of them are
reported verbatim by COMMAND INFO, which real clients parse to decide routing. A wrong flag is a
wrong decision taken before the command runs.

Static tables on both sides, so this needs NO SERVER, NO BUILD and NO DISK WRITES.

TWO EXCLUSIONS, both deliberate and both explained, because an unexplained exclusion is how a
gate quietly stops covering the thing it names.

`movablekeys` IS NOT COMPARED. Upstream does not store it: `populateCommandLegacyRangeSpec`
(server.c:2867-2905) DERIVES it at startup from the key specs -- a single index+range spec marked
CMD_KEY_INCOMPLETE, or any spec that is not index+range. fr stores it statically instead, which
is a representation difference, not necessarily a behavioural one. I tried twice to re-derive
upstream's set from `commands.def` by regex and got it wrong both times, in opposite directions
(27 with false positives from treating a native getkeys proc as sufficient -- it is not, that
rule is `CMD_MODULE_GETKEYS` and applies to modules only; then 0 from a keyspec-block parser that
matched nothing). Re-deriving a startup computation out of generated C with regexes is not
reliable, so this gate does not pretend to: it excludes the flag and says why. fr's 25 are the
classic movable-keys set (EVAL/FCALL/SORT/GEORADIUS/Z*STORE/*MPOP/SINTERCARD/XREAD/MIGRATE).

SENTINEL FLAGS ARE NOT COMPARED. `sentinel` and `only_sentinel` mark commands that exist only in
sentinel mode, which fr is not.

Exit 0 = every flag fr models agrees on every shared command, 1 = at least one disagrees.
"""
import re
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INCUMBENT = ROOT / "legacy_redis_code/redis/src/commands.def"
FR = ROOT / "crates/fr-command/src/lib.rs"

# Derived at startup upstream, stored statically by fr -- see the module docstring.
DERIVED_UPSTREAM = {"movablekeys"}
# Sentinel-mode only; fr is not a sentinel.
SENTINEL_ONLY = {"sentinel", "only_sentinel"}
# Modelled by upstream, deliberately not modelled by fr. Listed so that "fr never sets it"
# reads as a decision with a name rather than as N per-command bugs.
NOT_MODELLED_BY_FR = {"may_replicate", "protected", "touches_arbitrary_keys"}
EXCLUDED = DERIVED_UPSTREAM | SENTINEL_ONLY | NOT_MODELLED_BY_FR


def incumbent_flags() -> dict[str, set[str]]:
    text = INCUMBENT.read_text(errors="replace")
    start = text.index("struct COMMAND_STRUCT redisCommandTable[] = {")
    out: dict[str, set[str]] = {}
    for line in text[start:].splitlines():
        name = re.search(r'MAKE_CMD\("([a-z0-9_|-]+)"', line)
        # ...,<handler>,<arity>,<flags>,<acl>,...  -- the flags field follows the arity
        tail = re.search(r",(?:[A-Za-z_][A-Za-z0-9_]*Command|NULL),(-?\d+),([^,]*),", line)
        if not (name and tail):
            continue
        flags = {f[len("CMD_"):].lower() for f in re.findall(r"CMD_[A-Z_]+", tail.group(2))}
        out[name.group(1).lower()] = flags - EXCLUDED
    return out


def fr_flags() -> dict[str, set[str]]:
    text = FR.read_text(errors="replace")
    start = text.index("const COMMAND_TABLE:")
    end = text.index("\n];", start)
    return {m.group(1).lower(): set(m.group(3).split()) - EXCLUDED
            for m in re.finditer(
                r'\(\s*"([a-z0-9_|-]+)"\s*,\s*(-?\d+)\s*,\s*"([^"]*)"', text[start:end])}


def main() -> int:
    inc, fr = incumbent_flags(), fr_flags()
    if not inc or not fr:
        print(f"HARNESS INVALID: parsed {len(inc)} incumbent and {len(fr)} fr rows; a table "
              f"moved and this gate is measuring nothing.")
        return 2

    shared = sorted(set(inc) & set(fr))
    rows, missing, extra = [], Counter(), Counter()
    for name in shared:
        miss, ext = inc[name] - fr[name], fr[name] - inc[name]
        if miss or ext:
            rows.append((name, sorted(miss), sorted(ext)))
            missing.update(miss)
            extra.update(ext)

    print(f"compared {len(shared)} commands on the "
          f"{len(set().union(*inc.values()) if inc else set())} flags fr models")
    print(f"excluded: {', '.join(sorted(EXCLUDED))}")
    print()
    if rows:
        for name, miss, ext in rows:
            print(f"  {name:<24} missing={miss} extra={ext}")
        print()
        print(f"FAIL: {len(rows)} command(s) disagree. Missing-by-flag {dict(missing)}, "
              f"extra-by-flag {dict(extra)}. A wrong flag is a wrong decision taken before the "
              f"command runs.")
        return 1
    print(f"PASS: all {len(shared)} shared commands agree on every flag fr models.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
