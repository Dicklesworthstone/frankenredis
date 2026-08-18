#!/usr/bin/env python3
"""Pin the four source facts that make frankenredis-9hori's option (a) impossible.

9hori has to choose between two fixes for the FCALL KEYS/ARGV leak. Option (a) -- "capture and nil
them on the blanked shebang line" -- was recorded as UNDECIDABLE without a build. It is decidable,
and it is dead, for two independent reasons that are both plain in the source:

  1. The assignment is REFUSED. `LuaState::execute_compiled` locks the globals before user code
     runs, and both global-assignment arms return "Attempt to modify a readonly table" once
     locked. So `KEYS = nil` would raise on EVERY FCALL, not occasionally.
  2. Even if permitted it would still diverge. `LuaGlobals::insert` STORES the value rather than
     removing the key, and the read path raises the nonexistent-global error only on `None`. So a
     global set to nil reads back as nil WITHOUT raising, where upstream -- which never defines
     KEYS/ARGV in FUNCTION mode -- raises "Script attempted to access nonexistent global
     variable" (script_lua.c:1269).

This gate fails if any of those facts stops being true. That is the point: each one is load-bearing
for the conclusion, and a refactor that changes one silently turns a settled design decision back
into an open question without anyone noticing. A failure here is not "the code is wrong" -- it is
"go re-derive 9hori's option (a) before trusting the note that killed it".

DELETE THIS GATE when 9hori lands option (b). Once FCALL stops consulting source text, the wrapper
path that leaks the globals is gone and there is nothing left for option (a) to have been an
alternative to.

Source-shape gate: greps, no build, no server. Exit 0 = all four hold.
"""
from __future__ import annotations

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
LUA_EVAL = ROOT / "crates" / "fr-command" / "src" / "lua_eval.rs"


def check(src: str) -> list[str]:
    """Returns a list of failure descriptions; empty means every invariant holds."""
    failures: list[str] = []

    # 1. the lock is armed before user code runs
    if "self.globals_locked = true;" not in src:
        failures.append(
            "globals_locked is never set to true -- reason 1 (the assignment is refused) no "
            "longer holds, so option (a) may have become possible"
        )

    # 2. EVERY bare-global write is guarded by the lock. Counting refusals globally was the first
    #    version of this check and the self-test proved it blind: the statement appears three
    #    times, so deleting one still cleared a ">= 2" threshold. What matters is not how many
    #    refusals exist but that no WRITE path lacks one -- `Expr::Name` and `Expr::LocalName` are
    #    separate arms, and a guard removed from either leaves `KEYS = nil` reachable.
    writes = [m.start() for m in re.finditer(
        r"self\.globals\.insert\(name\.to_string\(\), value\);", src)]
    if not writes:
        failures.append(
            "no bare-global write found -- the assignment path option (a) needs has moved, so "
            "reason 1 cannot be checked in its current form"
        )
    for pos in writes:
        window = src[max(0, pos - 320):pos]
        guarded = "if self.globals_locked {" in window and (
            "Attempt to modify a readonly table" in window)
        if not guarded:
            line = src.count("\n", 0, pos) + 1
            failures.append(
                f"the bare-global write at line {line} is NOT preceded by the locked refusal -- "
                f"`KEYS = nil` is reachable through that arm, which is exactly what makes "
                f"option (a) impossible today"
            )

    # 3. insert STORES rather than removing, so a nil global stays present
    insert_body = re.search(
        r"fn insert\(&mut self, key: String, value: LuaValue\) -> Option<LuaValue> \{(.*?)\n    \}",
        src,
        re.S,
    )
    if insert_body is None:
        failures.append("LuaGlobals::insert not found -- reason 2 cannot be checked")
    elif "remove" in insert_body.group(1):
        failures.append(
            "LuaGlobals::insert now removes -- if a nil assignment deletes the key, a nil'd "
            "global would read as ABSENT and option (a) could match upstream after all"
        )

    # 4. the nonexistent-global error fires on absence only
    if "Script attempted to access nonexistent global variable" not in src:
        failures.append(
            "the nonexistent-global error is gone -- the behaviour option (a) was trying to "
            "reproduce no longer exists to be reproduced"
        )

    return failures


SELF_TEST_MUTATIONS = [
    # (invariant, mutation applied to the real source, substring the failure must mention)
    ("lock armed",
     lambda s: s.replace("self.globals_locked = true;", "self.globals_locked = false;", 1),
     "globals_locked is never set to true"),
    # Removes the guard from the FIRST bare-global write only. The earlier version of this
    # mutation deleted one refusal statement anywhere and the check did not notice, which is how
    # the ">= 2 refusals" threshold was found to be blind.
    ("a bare-global write loses its guard",
     lambda s: s.replace(
         '''                    if self.globals_locked {
                        return Err("user_script:1: Attempt to modify a readonly table".to_string());
                    }
                    self.globals.insert(name.to_string(), value);''',
         "                    self.globals.insert(name.to_string(), value);", 1),
     "NOT preceded by the locked refusal"),
    ("insert stores rather than removes",
     lambda s: re.sub(
         r"(fn insert\(&mut self, key: String, value: LuaValue\) -> Option<LuaValue> \{)",
         r"\1\n        self.overlay.remove(&key);", s, count=1),
     "insert now removes"),
    ("nonexistent-global error exists",
     lambda s: s.replace("Script attempted to access nonexistent global variable", "REMOVED"),
     "nonexistent-global error is gone"),
]


def self_test(src: str) -> int:
    """Break each invariant in memory and require the matching check to fire.

    A gate nobody has seen fail is a gate nobody has tested; this one would otherwise be four
    greps that have only ever returned green. Mutations are applied to an in-memory COPY -- never
    to the checkout, which is shared with other agents.
    """
    print("SELF-TEST: each invariant broken in memory, the matching check must fire")
    if check(src):
        print("FAIL: the unmutated source does not pass; fix that before trusting the mutations")
        return 1
    bad = 0
    for name, mutate, expected in SELF_TEST_MUTATIONS:
        failures = check(mutate(src))
        hit = any(expected in f for f in failures)
        print("  %-34s %s" % (name, "caught" if hit else "NOT CAUGHT"))
        if not hit:
            bad += 1
            print("      mutation produced: %s" % (failures or "no failures at all"))
    print("SELF-TEST: %s" % ("PASS" if bad == 0 else f"FAIL ({bad} blind check(s))"))
    return 1 if bad else 0


def main() -> int:
    src = LUA_EVAL.read_text()
    if "--self-test" in sys.argv:
        return self_test(src)
    failures = check(src)
    print("9hori option-(a) invariants, checked against %s" % LUA_EVAL.relative_to(ROOT))
    if not failures:
        print("PASS: all four hold -- option (a) is still impossible, for both recorded reasons.")
        return 0
    for f in failures:
        print("FAIL: %s" % f)
    print()
    print("One or more invariants moved. Re-derive 9hori's option (a) from source before")
    print("trusting the note that eliminated it; do not simply re-run this gate until green.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
