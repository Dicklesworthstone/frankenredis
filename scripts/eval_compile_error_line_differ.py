#!/usr/bin/env python3
"""Differential gate for Lua compile-error LINE numbers, fr vs vendored redis 7.2.4.

A Lua parse/lex error reports `user_script:<line>:` with the actual error line.
fr previously hardcoded `:1:` at every caller (frankenredis-5qhz7); the fix
threads the parser's per-token line map (and the lexer's line) through a located
compile path so EVAL/EVALSHA/SCRIPT LOAD render the true line. This gate now HARD-
checks every case byte-exact: single-line (line 1), multi-line parser errors, the
`<eof>` case, multi-line LEXER errors (malformed number / unfinished string /
unexpected symbol / unterminated block comment), and a valid script.

Usage: eval_compile_error_line_differ.py <oracle_port> <fr_port>
       Exit 0 = every case byte-exact, 1 = divergence.
"""
import socket
import sys
import time


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x.encode() if isinstance(x, str) else x
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.03)
    return s.recv(1 << 20)


# (label, argv) — each must be byte-exact vs redis 7.2.4.
CASES = [
    ("eval_single_line", ("EVAL", "syntax error here", "0")),
    ("eval_paren_l1", ("EVAL", ")bad", "0")),
    ("eval_parse_err_l3", ("EVAL", "local x=1\nlocal y=2\nsyntax error here", "0")),
    ("eval_parse_err_l4", ("EVAL", "local a=1\nlocal b=2\nlocal c=3\n)bad", "0")),
    ("eval_parse_err_l2", ("EVAL", "local ok=1\nreturn return", "0")),
    ("eval_eof_err_l2", ("EVAL", "local x=1\nreturn 1 +", "0")),
    ("eval_eof_err_l1", ("EVAL", "return 1 +", "0")),
    ("eval_lex_badnum_l2", ("EVAL", "local x=1\nlocal y=0x", "0")),
    ("eval_lex_unfinished_str_l2", ("EVAL", "local a=1\nlocal s='unterminated", "0")),
    ("eval_lex_unfinished_str_l4",
     ("EVAL", "local a=1\nlocal b=2\nlocal c=3\nlocal s='x", "0")),
    ("eval_lex_unterm_comment_l2", ("EVAL", "local x=1\n--[[ unterminated", "0")),
    ("scriptload_single_line", ("SCRIPT", "LOAD", "@@@")),
    ("scriptload_lex_err_l3", ("SCRIPT", "LOAD", "local x=1\nlocal y=2\n!!bad")),
    ("eval_valid", ("EVAL", "return 1+1", "0")),
    # loadstring / load(func) compile errors also carry the true line + label
    ("loadstring_ml2",
     ("EVAL", "local f,e=loadstring('local x=1\\nbad syntax here') return tostring(e)", "0")),
    ("loadstring_ml3",
     ("EVAL", "local f,e=loadstring('local a=1\\nlocal b=2\\n)bad') return tostring(e)", "0")),
    ("loadstring_chunkname_ml",
     ("EVAL", "local f,e=loadstring('local x=1\\n)bad','mychunk') return tostring(e)", "0")),
    ("loadstring_lex_ml",
     ("EVAL", "local f,e=loadstring('local x=1\\nlocal n=0x') return tostring(e)", "0")),
    ("load_func_ml_label_and_line",
     ("EVAL", "local c=0 local f,e=load(function() c=c+1 if c==1 then "
      "return 'local x=1\\nlocal y=2\\n)bad' end end) return tostring(e)", "0")),
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []
    for label, argv in CASES:
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} compile-error-line divergence(s) vs redis 7.2.4:")
        for x in fails:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — Lua compile-error line reporting byte-exact vs redis 7.2.4 "
        f"({len(CASES)} cases: parser/lexer/eof, single+multi-line) [frankenredis-5qhz7 fixed]"
    )


if __name__ == "__main__":
    main()
