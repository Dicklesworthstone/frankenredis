#!/usr/bin/env python3
"""Pin the stack cost of fr's recursive walkers over user-supplied structure.

WHY THIS EXISTS (frankenredis-thread-stack-size-1tlyh)
------------------------------------------------------
fr sets no `stack_size` anywhere, so every spawned thread -- the reactor
workers that actually execute commands -- gets Rust's default 2 MiB, not the
main thread's 8 MiB. A depth limit is only a defence if the engine survives
everything it ADMITS: `cjson.encode` is bounded at upstream's 1000, so 1000
frames of the encoder must fit in a worker's stack with room to spare.

The bead measured a real stack overflow at depth 1000 -- but under `cargo test
--lib`, which uses the DEV profile (this workspace defines no [profile.test] or
[profile.dev] override, so opt-level is 0 and nothing inlines). Dev frames are
several times larger than shipping frames, so that overflow does not by itself
say anything about production.

THE INSTRUMENT, and it is the point of this file: a recursion's per-level stack
cost is a COMPILE-TIME CONSTANT, readable straight out of the shipping ELF's
function prologue:

    per-level bytes = 8 (return address)
                    + 8 * (leading register pushes)
                    + the immediate of `sub $N,%rsp`

That needs no build, no run, and no recursing-to-depth harness -- which matters
because the alternative the bead proposed (one process per depth, since a stack
overflow ABORTS rather than unwinding) cannot run under a cargo throttle at all.

WHAT IT REFUSES TO DO SILENTLY
------------------------------
A gate that passes because it could not find its subject is worse than no gate.
So: a missing ELF FAILS unless --allow-missing-elf is passed, and a symbol that
has vanished (inlined away, or renamed) FAILS rather than being skipped. If a
cycle's self-call count is zero the recursion runs through some other function
and the printed figure is a FLOOR, not the cycle cost -- that is reported as
UNKNOWN, never as a pass.

DELETION CONDITION: none. Frame sizes move whenever the encoder changes, which
is exactly what this is here to notice.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import subprocess
import sys
import time

# The stack a WORKER thread actually gets. This was Rust's 2 MiB default until
# c63b56ef1 sized the three spawn sites explicitly at
# WORKER_THREAD_STACK_SIZE = 8 MiB (fr-server/src/main.rs:79), matching the OS
# default the main thread gets and upstream relies on. The default here tracks
# that constant: a gate defaulting to 2 MiB would judge every walker against a
# stack no fr thread has had since, which fails in the SAFE direction but
# reports a budget nobody is spending.
DEFAULT_THREAD_STACK = 8 * 1024 * 1024

# Fraction of a worker stack the recursion at its own limit may consume. The
# remainder has to hold everything BELOW the walker: the reactor loop, dispatch,
# and the Lua interpreter's own frames. 50 pct is a deliberately loose bound --
# it is a tripwire for a frame that has grown by multiples, not a budget.
MAX_STACK_FRACTION = 0.50

# symbol substring -> (human name, depth limit enforced in code, why that limit)
WALKERS = [
    (
        "lua_value_to_json_at_depth",
        "cjson.encode",
        1000,
        "upstream's CJSON_MAX_DEPTH, pinned by frankenredis-cjson-encode-depth-zo5ac",
    ),
    (
        "JsonParser11parse_value",
        "cjson.decode",
        1000,
        "same limit on the decode side; json_to_lua_value delegates into this",
    ),
    (
        "cmsgpack_pack_value",
        "cmsgpack.pack",
        1000,
        "cmsgpack_pack_table is inlined into this symbol, so the cycle is value->value",
    ),
]

# A recursion whose cycle spans SEVERAL functions. `WALKERS` above proves a cycle
# is direct by counting a symbol's own self-calls; that test reports UNKNOWN for
# the interpreter, whose per-level cost is the SUM of the frames it passes
# through. One Lua call runs call_function, exec_block, exec_stmts, exec_stmt and
# eval_expr before reaching call_function again, so no single prologue is the
# answer and the gate said so rather than guessing.
#
# EVAL_EXPR_PER_LEVEL is why this is a range and not a number: a nested
# expression pushes another eval_expr without another Lua call, so the cycle
# carries it more than once. 3 is the figure that reconciles the summed
# prologues with the ~6.2 KB/call that frankenredis-lua-call-depth-ug22x
# measured by removing the bound and finding where the server actually died.
#
# (frankenredis-lua-call-depth-ug22x, and the residual named in
# frankenredis-thread-stack-size-1tlyh: "the interpreter's own call depth,
# bounded in FRAMES by MAX_CALL_DEPTH rather than in BYTES, is still unpriced".)
EVAL_EXPR_PER_LEVEL = 3

CYCLES = [
    (
        "lua call",
        [
            ("::call_function", 1),
            ("::exec_block", 1),
            ("::exec_stmts", 1),
            ("::exec_stmt", 1),
            ("::eval_expr", EVAL_EXPR_PER_LEVEL),
        ],
        512,
        "MAX_CALL_DEPTH in lua_eval.rs; upstream reaches ~16000, which fr cannot "
        "afford at this frame cost -- the residual is recorded on ug22x",
    ),
]

PROLOGUE_WINDOW = 40  # instructions to scan before giving up on finding `sub`
INSN_RE = re.compile(r"^\s+[0-9a-f]+:\s")
PUSH_RE = re.compile(r"\bpush\s+%")
SUB_RSP_RE = re.compile(r"\bsub\s+\$(0x[0-9a-f]+),%rsp")


def sh(cmd: list[str]) -> str:
    return subprocess.run(cmd, capture_output=True, text=True).stdout


def elf_provenance(path: str) -> str:
    """COMPUTED, not asserted -- a banner is not provenance."""
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    mtime = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(os.path.getmtime(path)))
    return f"{path}\n  sha256 {h.hexdigest()}\n  built  {mtime}  ({os.path.getsize(path)} bytes)"


def symbol_span(elf: str, pattern: str):
    """Return (start, end) addresses of the first symbol matching `pattern`."""
    table = [ln.split() for ln in sh(["nm", "-n", elf]).splitlines()]
    table = [row for row in table if len(row) >= 3]
    for i, row in enumerate(table):
        if pattern in row[2]:
            start = row[0]
            end = table[i + 1][0] if i + 1 < len(table) else None
            return start, end, row[2]
    return None, None, None


MIN_PLAUSIBLE_FRAME = 32  # below this it is a thunk or a stub, not a real frame


def symbol_span_exact(elf: str, suffix: str):
    """Span of the symbol whose DEMANGLED name ends with `suffix`.

    `symbol_span` matches a substring and returns the first hit in address
    order. For a cycle member that is not good enough: `eval_expr` also names
    closures and thunks, and picking one of those under-reports the frame
    without any visible error. Requiring an exact demangled suffix, and
    refusing when more than one symbol matches, makes that failure loud.
    """
    rows = []
    for ln in sh(["nm", "-nC", elf]).splitlines():
        parts = ln.split(" ", 2)
        if len(parts) < 3:
            continue
        rows.append((parts[0], parts[2].strip()))
    hits = [i for i, (_a, n) in enumerate(rows) if n.endswith(suffix)]
    if len(hits) != 1:
        return None, None, None, len(hits)
    i = hits[0]
    start = rows[i][0]
    end = rows[i + 1][0] if i + 1 < len(rows) else None
    return start, end, rows[i][1], 1


def frame_bytes_exact(elf: str, suffix: str):
    """Per-frame stack bytes for an exactly-named symbol."""
    start, end, name, nhits = symbol_span_exact(elf, suffix)
    if start is None:
        return None, nhits, None
    cmd = ["objdump", "-d", f"--start-address=0x{start}"]
    if end:
        cmd.append(f"--stop-address=0x{end}")
    cmd.append(elf)
    insns = [ln for ln in sh(cmd).splitlines() if INSN_RE.match(ln)]
    pushes, sub = 0, 0
    for ln in insns[:PROLOGUE_WINDOW]:
        m = SUB_RSP_RE.search(ln)
        if m:
            sub = int(m.group(1), 16)
            break
        if PUSH_RE.search(ln):
            pushes += 1
    return 8 + 8 * pushes + sub, 1, name


def frame_bytes(elf: str, pattern: str):
    """Per-recursion-level stack bytes, read from the prologue.

    Returns (per_level, pushes, sub, self_calls, mangled) or None if absent.
    """
    start, end, mangled = symbol_span(elf, pattern)
    if start is None:
        return None
    cmd = ["objdump", "-d", f"--start-address=0x{start}"]
    if end:
        cmd.append(f"--stop-address=0x{end}")
    cmd.append(elf)
    insns = [ln for ln in sh(cmd).splitlines() if INSN_RE.match(ln)]

    pushes, sub = 0, 0
    for ln in insns[:PROLOGUE_WINDOW]:
        m = SUB_RSP_RE.search(ln)
        if m:
            sub = int(m.group(1), 16)
            break
        if PUSH_RE.search(ln):
            pushes += 1

    self_calls = sum(1 for ln in insns if "call" in ln and pattern in ln)
    return 8 + 8 * pushes + sub, pushes, sub, self_calls, mangled


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--elf", default="target/release/frankenredis")
    ap.add_argument("--stack", type=int, default=DEFAULT_THREAD_STACK,
                    help="worker thread stack in bytes (default: Rust's 2 MiB)")
    ap.add_argument("--allow-missing-elf", action="store_true",
                    help="downgrade a missing binary to SKIP; without this it FAILS, "
                         "because a gate that passes for lack of a subject is a false pass")
    args = ap.parse_args()

    if not os.path.exists(args.elf):
        if args.allow_missing_elf:
            print(f"SKIP: no ELF at {args.elf} (--allow-missing-elf given). "
                  f"NOTHING WAS CHECKED.")
            return 0
        print(f"FAIL: no ELF at {args.elf}. Build it, or pass --allow-missing-elf "
              f"to acknowledge that this gate checked nothing.", file=sys.stderr)
        return 1

    print("recursion stack budget, read from the SHIPPING binary's prologues")
    print(f"ELF: {elf_provenance(args.elf)}")
    print(f"worker stack budget: {args.stack} bytes "
          f"({args.stack / 1024 / 1024:.1f} MiB), ceiling {MAX_STACK_FRACTION:.0%}\n")

    ceiling = args.stack * MAX_STACK_FRACTION
    rows, failures, unknowns = [], [], []

    for pattern, name, limit, note in WALKERS:
        got = frame_bytes(args.elf, pattern)
        if got is None:
            failures.append(f"{name}: symbol matching {pattern!r} NOT FOUND "
                            f"(inlined away or renamed) -- cannot vouch for its stack cost")
            continue
        per_level, pushes, sub, self_calls, _mangled = got
        at_limit = per_level * limit
        pct = at_limit / args.stack * 100
        fits = args.stack // per_level

        if self_calls == 0:
            unknowns.append(f"{name}: 0 self-calls, so the recursion runs through another "
                            f"function and {per_level} B is a FLOOR for the cycle, not its cost")
            verdict = "UNKNOWN"
        elif at_limit > ceiling:
            failures.append(f"{name}: {per_level} B/level x {limit} = {at_limit} B "
                            f"= {pct:.1f} pct of the worker stack, over the "
                            f"{MAX_STACK_FRACTION:.0%} ceiling")
            verdict = "OVER"
        else:
            verdict = "ok"

        rows.append((name, per_level, pushes, sub, self_calls, limit, at_limit, pct, fits, verdict, note))

    hdr = f"{'walker':<16}{'B/level':>8}{'push':>5}{'sub':>6}{'self':>5}{'limit':>7}{'B@limit':>10}{'%stack':>8}{'fits':>8}  verdict"
    print(hdr)
    print("-" * len(hdr))
    for (name, per, pushes, sub, self_calls, limit, at, pct, fits, verdict, _n) in rows:
        print(f"{name:<16}{per:>8}{pushes:>5}{sub:>6}{self_calls:>5}{limit:>7}"
              f"{at:>10}{pct:>7.1f}%{fits:>8}  {verdict}")
    print()
    for (name, per, *_rest, note) in rows:
        print(f"  {name}: {note}")
    print()

    # ---- multi-function cycles -------------------------------------------
    # These do NOT get the self-call test: the whole point is that the cycle
    # leaves each function, so a self-call count of 0 is expected rather than
    # suspicious. What is checked instead is that every named frame still
    # EXISTS -- if one is inlined away the sum silently under-reports, which is
    # the same false pass the WALKERS table refuses.
    for cname, members, limit, note in CYCLES:
        per_level = 0
        missing = []
        parts = []
        for suffix, mult in members:
            human = suffix.lstrip(":")
            bytes_, nhits, resolved = frame_bytes_exact(args.elf, suffix)
            if bytes_ is None:
                missing.append(f"{human} ({nhits} symbols matched, need exactly 1)")
                continue
            if bytes_ < MIN_PLAUSIBLE_FRAME:
                missing.append(f"{human} resolved to {resolved} with only {bytes_} B -- "
                               f"that is a thunk, not the frame")
                continue
            per_level += bytes_ * mult
            parts.append(f"{human} {bytes_}" + (f" x{mult}" if mult != 1 else ""))
        if missing:
            failures.append(
                f"{cname}: frame(s) {', '.join(missing)} NOT FOUND -- inlined away or "
                f"renamed, so the summed per-level cost would UNDER-report")
            continue
        at_limit = per_level * limit
        pct = at_limit / args.stack * 100
        fits = args.stack // per_level
        verdict = "ok"
        if at_limit > ceiling:
            failures.append(
                f"{cname}: {per_level} B/level x {limit} = {at_limit} B = {pct:.1f} pct "
                f"of the worker stack, over the {MAX_STACK_FRACTION:.0%} ceiling")
            verdict = "OVER"
        print(f"cycle {cname}: {per_level} B/level ({' + '.join(parts)})")
        print(f"  at limit {limit}: {at_limit} B = {pct:.1f} pct of stack; "
              f"survives ~{fits} levels  {verdict}")
        print(f"  {note}")
    print()

    for u in unknowns:
        print(f"UNKNOWN: {u}")
    for f in failures:
        print(f"FAIL: {f}", file=sys.stderr)

    if failures:
        print(f"\nFAIL: {len(failures)} walker(s) outside budget or unaccounted for.",
              file=sys.stderr)
        return 1
    if unknowns:
        print(f"UNKNOWN: {len(unknowns)} cycle(s) span more than one function; their "
              f"figures are floors. Not a pass.", file=sys.stderr)
        return 2

    narrowest = min(rows, key=lambda r: r[8])
    print(f"PASS: every recursive walker fits its own limit inside a "
          f"{args.stack / 1024 / 1024:.0f} MiB worker stack. "
          f"Narrowest margin: {narrowest[0]} at {narrowest[8]} levels "
          f"({narrowest[8] / narrowest[5]:.1f}x its limit of {narrowest[5]}).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
