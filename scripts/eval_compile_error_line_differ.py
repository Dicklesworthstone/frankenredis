#!/usr/bin/env python3
"""Differential gate for Lua compile-error LINE numbers, fr vs vendored redis 7.2.4.

A Lua parse error reports `user_script:<line>:` with the actual error line. fr's
parser tracks per-token lines (Parser::with_lines) but every caller hardcodes
`:1:`, so multi-line scripts report the wrong line (frankenredis-5qhz7).

HARD checks: single-line compile errors (line == 1, already byte-exact).
DOCUMENTED divergence (NOTE, gate stays green; auto-promotes once 5qhz7 lands):
multi-line scripts whose syntax error is on line > 1.

Usage: eval_compile_error_line_differ.py <oracle_port> <fr_port>
       Exit 0 = parity (modulo documented 5qhz7 divergence), 1 = NEW divergence.
"""
import socket
import sys
import time
import re


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def evalerr(s, script):
    a = ("EVAL", script, "0")
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x.encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.03)
    return s.recv(1 << 20)


def line_of(reply):
    m = re.search(rb"user_script:(\d+):", reply)
    return int(m.group(1)) if m else None


# (label, script, kind) — "hard" must match byte-exact; "multiline" is 5qhz7.
CASES = [
    ("single_line", "syntax error here", "hard"),
    ("single_line_paren", ")bad", "hard"),
    ("multiline_l3", "local x=1\nlocal y=2\nsyntax error here", "multiline"),
    ("multiline_l4", "local a=1\nlocal b=2\nlocal c=3\n)bad", "multiline"),
    ("multiline_l2", "local ok=1\nreturn return", "multiline"),
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails, notes = [], []
    for label, script, kind in CASES:
        ro, rf = evalerr(od, script), evalerr(fr, script)
        if kind == "hard":
            if ro != rf:
                fails.append(f"{label}: redis={ro!r} fr={rf!r}")
        else:  # multiline
            ol, fl = line_of(ro), line_of(rf)
            if ol is None:
                fails.append(f"{label}: redis reply has no user_script:<line>: {ro!r}")
            elif ro == rf:
                notes.append(f"{label} now MATCHES (frankenredis-5qhz7 fixed?) — promote to HARD")
            elif fl == 1 and ol > 1:
                notes.append(
                    f"{label} KNOWN DIVERGENCE (frankenredis-5qhz7): redis line={ol} fr line={fl} "
                    f"(redis={ro!r} fr={rf!r})"
                )
            else:
                fails.append(f"{label} UNEXPECTED: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    for n in notes:
        print(f"NOTE  {n}")
    if fails:
        print(f"FAIL — {len(fails)} NEW compile-error-line divergence(s) vs redis 7.2.4:")
        for x in fails:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — Lua compile-error reporting matches redis 7.2.4 "
        f"(single-line hard; {len(notes)} documented 5qhz7 multi-line divergence(s))"
    )


if __name__ == "__main__":
    main()
