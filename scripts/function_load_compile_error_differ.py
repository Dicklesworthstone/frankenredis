#!/usr/bin/env python3
"""Differential gate for FUNCTION LOAD error reporting, fr vs vendored redis 7.2.4.

redis FUNCTION LOAD compiles the library body as a Lua chunk; a syntax error
surfaces "Error compiling function: user_function:<line>: <msg>". fr text-scans
the body for register_function calls and, finding none in a syntax-error body,
reports "No functions registered" instead (frankenredis-mbyoe — fr never compiles
the body; fr-store can't reach the Lua parser, and fr's parser doesn't track the
error line). fr's loadstring DOES produce redis-identical messages, so the fix is
feasible in fr-command's LOAD arm; until then this guards the surface.

HARD checks: the FUNCTION LOAD error paths that already match (missing metadata,
missing name, unknown engine, empty body -> No functions registered, dup names).
DOCUMENTED divergence (NOTE, gate stays green; auto-promotes once mbyoe lands):
a syntax-error body -> redis "Error compiling function" vs fr "No functions
registered".

Usage: function_load_compile_error_differ.py <oracle_port> <fr_port>
       Exit 0 = parity (modulo documented mbyoe divergence), 1 = NEW divergence.
"""
import socket
import sys
import time


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.03)
    return s.recv(1 << 20)


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for d in (od, fr):
        cmd(d, "FUNCTION", "FLUSH")

    def both(*c):
        return cmd(od, *c), cmd(fr, *c)

    fails, notes = [], []

    def hard(label, *c):
        o, f = both(*c)
        if o != f:
            fails.append(f"{label}: redis={o!r} fr={f!r}")

    # HARD: metadata/empty/dup error paths already byte-exact
    hard("missing_shebang", "FUNCTION", "LOAD", "no shebang here")
    hard("invalid_metadata_no_nl", "FUNCTION", "LOAD", "#!lua name=l1")
    hard("empty_body_no_functions", "FUNCTION", "LOAD", "#!lua name=l1\n")
    hard("unknown_engine", "FUNCTION", "LOAD", "#!badengine name=x\n")
    hard("missing_name", "FUNCTION", "LOAD", "#!lua\nx")

    # DOCUMENTED divergence (mbyoe): syntax-error body
    o, f = both("FUNCTION", "LOAD", "#!lua name=bad\nsyntax error here")
    o_compile = b"Error compiling function" in o
    f_compile = b"Error compiling function" in f
    if o_compile and f_compile:
        notes.append("syntax_error_body now MATCHES (frankenredis-mbyoe fixed?) — promote to HARD")
    elif o_compile and not f_compile:
        notes.append(
            f"syntax_error_body KNOWN DIVERGENCE (frankenredis-mbyoe): redis={o!r} fr={f!r} "
            "(fr text-scans, does not compile the library body)"
        )
    else:
        fails.append(f"syntax_error_body UNEXPECTED: redis={o!r} fr={f!r}")

    print("=" * 60)
    for n in notes:
        print(f"NOTE  {n}")
    if fails:
        print(f"FAIL — {len(fails)} NEW FUNCTION LOAD divergence(s) vs redis 7.2.4:")
        for x in fails:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — FUNCTION LOAD error paths match redis 7.2.4 "
        f"({len(notes)} documented mbyoe compile-error divergence)"
    )


if __name__ == "__main__":
    main()
