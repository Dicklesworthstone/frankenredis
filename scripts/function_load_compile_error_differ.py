#!/usr/bin/env python3
"""Differential gate for FUNCTION LOAD error reporting, fr vs vendored redis 7.2.4.

redis FUNCTION LOAD compiles the library body as a Lua chunk; a syntax error
surfaces "Error compiling function: user_function:<line>: <msg>" BEFORE the
empty-body "No functions registered" check, but AFTER shebang-metadata validation
(missing/invalid metadata, missing name, unknown engine). fr previously text-
scanned for register_function and never compiled the body (frankenredis-mbyoe),
reporting "No functions registered" for a syntax-error body. Fixed in fr-command's
LOAD arm: on the no-functions path, compile-check the body (lua_execution_source
blanks the shebang, so the reported line is the true file line) and surface the
compile error. This gate HARD-checks the full surface byte-exact.

Usage: function_load_compile_error_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

GOOD = "#!lua name=goodlib\nredis.register_function('gf', function(k,a) return a[1] end)"

# (label, argv) — each byte-exact vs redis 7.2.4.
CASES = [
    # shebang-metadata errors fire FIRST (before compile) — order preserved
    ("missing_shebang", ("FUNCTION", "LOAD", "no shebang here")),
    ("invalid_metadata_no_nl", ("FUNCTION", "LOAD", "#!lua name=l1")),
    ("unknown_engine", ("FUNCTION", "LOAD", "#!badengine name=x\n)syntax")),
    ("missing_name", ("FUNCTION", "LOAD", "#!lua\n)syntax")),
    # empty / valid-no-register bodies -> "No functions registered" (compile ok)
    ("empty_body", ("FUNCTION", "LOAD", "#!lua name=e\n")),
    ("valid_no_register", ("FUNCTION", "LOAD", "#!lua name=nr\nlocal x=1+1")),
    ("comment_only", ("FUNCTION", "LOAD", "#!lua name=c\n-- just a comment")),
    # syntax-error bodies -> "Error compiling function: user_function:<line>"
    ("syntax_err_l2", ("FUNCTION", "LOAD", "#!lua name=bad\nsyntax error here")),
    ("syntax_err_l3", ("FUNCTION", "LOAD", "#!lua name=bad\nlocal x=1\n)bad")),
    ("syntax_err_l4", ("FUNCTION", "LOAD", "#!lua name=bad\nlocal a=1\nlocal b=2\nreturn return")),
    ("lex_unfinished_str", ("FUNCTION", "LOAD", "#!lua name=bad\nlocal s='nope")),
    ("lex_badnum", ("FUNCTION", "LOAD", "#!lua name=bad\nlocal n=0x")),
    # (frankenredis-sg7b4) register_function present BUT a syntax error
    # elsewhere -> compile error (non-REPLACE; lib must NOT register)
    ("reg_plus_syntax_l3",
     ("FUNCTION", "LOAD",
      "#!lua name=rps\nredis.register_function('f',function() return 1 end)\n)syntaxerr")),
    ("reg_plus_syntax_l4",
     ("FUNCTION", "LOAD",
      "#!lua name=rps2\nlocal q=1\nredis.register_function('f',function() return 1 end)\nbad bad")),
    ("list_after_failed_loads", ("FUNCTION", "LIST")),
    # valid library loads + is callable
    ("valid_lib", ("FUNCTION", "LOAD", GOOD)),
    ("valid_replace", ("FUNCTION", "LOAD", "REPLACE", GOOD)),
    ("fcall_valid", ("FCALL", "gf", "0", "hello")),
    # (frankenredis-sg7b4) REPLACE atomicity: a REPLACE whose new body fails to
    # compile must be rejected WITHOUT destroying the existing library (upstream
    # compiles before swapping). These run in order (no FLUSH between cases).
    ("repl_atom_setup",
     ("FUNCTION", "LOAD", "#!lua name=ratom\nredis.register_function('rf',function() return 1 end)")),
    ("repl_atom_bad",
     ("FUNCTION", "LOAD", "REPLACE",
      "#!lua name=ratom\nredis.register_function('rf',function() return 1 end)\n)syntaxerr")),
    ("repl_atom_old_preserved", ("FCALL", "rf", "0")),
]


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
    fails = []
    for label, argv in CASES:
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} FUNCTION LOAD divergence(s) vs redis 7.2.4:")
        for x in fails:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — FUNCTION LOAD error reporting byte-exact vs redis 7.2.4 "
        f"({len(CASES)} cases: metadata/empty/syntax/lexer/valid) [frankenredis-mbyoe fixed]"
    )


if __name__ == "__main__":
    main()
