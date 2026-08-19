#!/usr/bin/env python3
"""Byte-exhaustive differential sweep of Lua string functions, fr against the incumbent.

WHY THIS EXISTS
---------------
`frankenredis-zxtuk` was found by hand-probing one pattern and then generalised by
sweeping. The generalisation is what made it actionable, and it is what a hand probe
cannot give:

    single-byte patterns          1 of 256 diverge  -- looks like a curiosity about ')'
    two-byte patterns chr(X)..')' 246 of 256 diverge -- and the 10 that AGREE are
                                                        exactly upstream's SPECIALS set

The second sweep did not just raise the severity. It validated the MECHANISM
byte-exhaustively: the divergence set is precisely the complement of
SPECIALS = "^$*+?.([%-", which is upstream's own plain-search predicate
(deps/lua/src/lstrlib.c:502). Reading the C and trusting the reading would have given
the same hypothesis with none of the confidence.

WHAT IT IS NOT
--------------
Not a timing instrument. It compares REPLIES, so it is valid on a loaded or
IO-saturated host where no ratio may be taken. It starts both servers with `--save ''`
and does no disk work of its own.

The whole sweep runs as ONE EVAL per engine -- the loop lives in Lua, not in Python --
so a 256-case sweep costs two round trips rather than 512.

USAGE
    scripts/lua_string_differential_sweep.py <fr_bin> [--case NAME] [--list]
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import time

REDIS_SERVER = "legacy_redis_code/redis/src/redis-server"
REDIS_CLI = "legacy_redis_code/redis/src/redis-cli"

# Upstream's plain-search predicate (lstrlib.c:183). Kept here so a sweep can say
# WHICH bytes are expected to agree rather than only that some do.
SPECIALS = set(b"^$*+?.([%-")

# Cases whose index IS the pattern byte, so a SPECIALS breakdown is meaningful.
# The others are keyed by case number and must not print one.
BYTE_INDEXED = {"byte", "byte_close_paren", "byte_close_paren_match", "byte_gsub"}

# name -> Lua body producing "idx:ok:result" comma-joined over 0..255.
CASES = {
    # the single-byte surface
    "byte": (
        'local out={} for i=0,255 do local c=string.char(i) '
        'local ok,r=pcall(string.find,"a"..c.."b",c) '
        'out[#out+1]=i..":"..tostring(ok)..":"..tostring(r) end '
        'return table.concat(out,",")'
    ),
    # chr(X) .. ')' -- the form that exposed the SPECIALS rule
    "byte_close_paren": (
        'local out={} for i=0,255 do local p=string.char(i)..")" '
        'local ok,r=pcall(string.find,"z"..p.."z",p) '
        'out[#out+1]=i..":"..tostring(ok)..":"..tostring(r) end '
        'return table.concat(out,",")'
    ),
    # the same shape through match, which upstream NEVER plain-searches
    # (str_find_aux(L,0)). Divergence here means a fix widened past `find &&`.
    "byte_close_paren_match": (
        'local out={} for i=0,255 do local p=string.char(i)..")" '
        'local ok,r=pcall(string.match,"z"..p.."z",p) '
        'out[#out+1]=i..":"..tostring(ok)..":"..tostring(r) end '
        'return table.concat(out,",")'
    ),
    # Upstream raises "unfinished capture" only when a capture is READ, not when the
    # pattern is compiled: add_s calls push_onecapture solely for a %N in the
    # replacement. So the SAME pattern succeeds with replacement "X" and raises with
    # "%1". This case varies ONLY the replacement, which is what isolates the timing
    # from the pattern. fr validates eagerly and raises for both.
    "capture_read_timing": (
        'local pats={"(","(a","a("} local reps={"X","%1"} local out={} local n=0 '
        'for i,p in ipairs(pats) do for j,r in ipairs(reps) do n=n+1 '
        'local ok,res=pcall(string.gsub,"zazbz",p,r) '
        'out[#out+1]=n..":"..tostring(ok)..":"..tostring(res) end end '
        'return table.concat(out,",")'
    ),
    # A back-reference to a capture that does not exist. Upstream raises "invalid
    # capture index"; fr silently fails to match -- the OPPOSITE direction to the
    # case above, so the two together show fr's validator is not simply stricter.
    "backref_no_capture": (
        'local out={} local pats={"%1","%2","a%1"} '
        'for i,p in ipairs(pats) do local ok,r=pcall(string.gsub,"zazbz",p,"X") '
        'out[#out+1]=i..":"..tostring(ok)..":"..tostring(r) end '
        'return table.concat(out,",")'
    ),
    # string.format specifier parity. Results are comma-sanitised in Lua because the
    # transport joins on commas and a formatted value may contain one.
    "format": (
        'local cs={{"%d",3.7},{"%d","5"},{"%i",42},{"%5.2f",3.14159},{"%x",255},'
        '{"%X",255},{"%o",8},{"%c",65},{"%e",1234.5},{"%g",0.0001},{"%s",true},'
        '{"%q","a b"},{"%%",1},{"%10s","hi"},{"%.3s","abcdef"},{"%d",2^53},'
        '{"%s",-0.0},{"%d",-2^53},{"%5d",7},{"%-5d|",7},{"%+d",7},{"%.0f",2.5}} '
        'local out={} for i,c in ipairs(cs) do '
        'local ok,r=pcall(string.format,c[1],c[2]) '
        'r=tostring(r) r=string.gsub(r,",","<c>") '
        'out[#out+1]=i..":"..tostring(ok)..":"..r end '
        'return table.concat(out,",")'
    ),
    # (frankenredis-fcoxw) Negative zero across every numeric specifier plus the
    # routes that are already correct. IEEE754 makes -0.0 == 0.0 true, so any
    # equality-to-zero shortcut drops the sign; %f has no such shortcut and is the
    # control proving the general float path is fine. tostring/concat are included
    # because a fix must not regress them -- they already emit -0.
    "negative_zero": (
        'local z = 0.0*-1 local out={} local cs={'
        'function() return string.format("%s",z) end,'
        'function() return string.format("%g",z) end,'
        'function() return string.format("%e",z) end,'
        'function() return string.format("%f",z) end,'
        'function() return string.format("%.2f",z) end,'
        'function() return string.format("%d",z) end,'
        'function() return tostring(z) end,'
        'function() return ""..z end,'
        'function() return tostring(1/z) end} '
        'for i,f in ipairs(cs) do local ok,r=pcall(f) '
        'r=tostring(r) r=string.gsub(r,",","<c>") '
        'out[#out+1]=i..":"..tostring(ok)..":"..r end '
        'return table.concat(out,",")'
    ),
    "byte_gsub": (
        'local out={} for i=0,255 do local c=string.char(i) '
        'local ok,r=pcall(string.gsub,"a"..c.."b",c,"X") '
        'out[#out+1]=i..":"..tostring(ok)..":"..tostring(r) end '
        'return table.concat(out,",")'
    ),
}


def wait_ready(port: int, deadline_s: float = 20.0) -> bool:
    end = time.monotonic() + deadline_s
    while time.monotonic() < end:
        r = subprocess.run([REDIS_CLI, "-p", str(port), "ping"],
                           capture_output=True, text=True)
        if r.stdout.strip() == "PONG":
            return True
        time.sleep(0.3)
    return False


def run_case(port: int, body: str) -> dict[int, str]:
    r = subprocess.run([REDIS_CLI, "-p", str(port), "EVAL", body, "0"],
                       capture_output=True, text=True)
    out: dict[int, str] = {}
    for item in r.stdout.strip().strip('"').split(","):
        parts = item.split(":")
        if len(parts) >= 3 and parts[0].isdigit():
            out[int(parts[0])] = ":".join(parts[1:])
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("fr_bin", nargs="?")
    ap.add_argument("--case", default=None, help="one case name; default runs all")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--fr-port", type=int, default=7891)
    ap.add_argument("--redis-port", type=int, default=7892)
    args = ap.parse_args()

    if args.list:
        for k in CASES:
            print(k)
        return 0
    if not args.fr_bin:
        print("fr_bin is required (or pass --list)", file=sys.stderr)
        return 2
    for p in (args.fr_bin, REDIS_SERVER, REDIS_CLI):
        if not os.path.exists(p):
            print(f"missing: {p}", file=sys.stderr)
            return 2

    procs = []
    devnull = subprocess.DEVNULL
    procs.append(subprocess.Popen([args.fr_bin, "--port", str(args.fr_port), "--save", ""],
                                  stdout=devnull, stderr=devnull))
    procs.append(subprocess.Popen([REDIS_SERVER, "--port", str(args.redis_port), "--save", ""],
                                  stdout=devnull, stderr=devnull))
    try:
        for port, name in ((args.fr_port, "fr"), (args.redis_port, "redis")):
            if not wait_ready(port):
                print(f"{name} did not become ready on {port}", file=sys.stderr)
                return 1

        names = [args.case] if args.case else list(CASES)
        total_div = 0
        for name in names:
            body = CASES.get(name)
            if body is None:
                print(f"unknown case {name!r}", file=sys.stderr)
                return 2
            F = run_case(args.fr_port, body)
            R = run_case(args.redis_port, body)
            common = sorted(set(F) & set(R))
            if not common:
                print(f"{name}: NO COMPARABLE OUTPUT — both arms returned nothing parseable")
                return 1
            div = [b for b in common if F[b] != R[b]]
            total_div += len(div)
            byte_indexed = name in BYTE_INDEXED
            unit = "bytes" if byte_indexed else "cases"
            print(f"\n{name}: {len(div)} of {len(common)} {unit} diverge")
            if byte_indexed:
                # Only meaningful when the index IS the pattern byte. Printing a
                # SPECIALS breakdown for an index-keyed case would be a number that
                # looks like a finding and means nothing.
                agree_nonspecial = [b for b in common if b not in SPECIALS and b not in div]
                div_special = [b for b in div if b in SPECIALS]
                print(f"  divergent AND in SPECIALS      : {len(div_special)}")
                print(f"  agreeing AND not in SPECIALS   : {len(agree_nonspecial)}")
            for b in div[:8]:
                if byte_indexed:
                    ch = chr(b) if 32 <= b < 127 else "."
                    label = f"byte {b:>3} {ch!r:<4}"
                else:
                    label = f"case {b:>3}      "
                print(f"    {label} fr={F[b][:30]:<30} redis={R[b][:30]}")
            if len(div) > 8:
                print(f"    ... and {len(div) - 8} more")
        print(f"\nTOTAL DIVERGENT ROWS: {total_div}")
        return 1 if total_div else 0
    finally:
        for port in (args.fr_port, args.redis_port):
            subprocess.run([REDIS_CLI, "-p", str(port), "shutdown", "nosave"],
                           capture_output=True, text=True)
        for p in procs:
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()


if __name__ == "__main__":
    sys.exit(main())
