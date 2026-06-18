#!/usr/bin/env python3
"""Differential gate: inline (non-RESP) command parser (frankenredis-cey73).

Besides RESP arrays, redis accepts INLINE commands — a bare `CMD arg arg\\r\\n` line
parsed by the sdssplitargs tokenizer: double quotes (with \\xHH / \\t / \\n / \\r / \\a
/ \\b escapes), single quotes (only \\' escapes), unbalanced quotes -> error, a
closing quote must be followed by space-or-end -> error otherwise, leading/trailing
whitespace + tabs as separators, empty lines = no-op, bare LF accepted. Real clients
send RESP arrays, so this tokenizer is rarely exercised and a prime spot for a latent
edge bug. This gate sends raw inline bytes (fresh connection per case, since a parse
error closes the connection) and compares the reply byte-exact vs redis 7.2.4.

Usage: inline_command_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time

# (label, raw bytes incl. terminators)
CASES = [
    ("ping", b"PING\r\n"),
    ("ping_arg", b"PING hello\r\n"),
    ("set_get", b"SET ik vv\r\nGET ik\r\n"),
    ("extra_spaces", b"SET   sp    val\r\n"),
    ("leading_space", b"   PING\r\n"),
    ("tab_separated", b"SET\ttk\ttv\r\n"),
    ("dquote", b'SET dq "hello world"\r\nGET dq\r\n'),
    ("hex_escape", b'SET hx "a\\x41b"\r\nGET hx\r\n'),          # \x41=A -> aAb
    ("hex_nul_ff", b'SET hx2 "\\x00\\xff"\r\nSTRLEN hx2\r\n'),
    ("escapes_tnr", b'SET es "a\\tb\\nc"\r\nSTRLEN es\r\n'),
    ("squote", b"SET sq 'single quoted'\r\nGET sq\r\n"),
    ("squote_escaped_quote", b"SET sq2 'a\\'b'\r\nGET sq2\r\n"),
    ("empty_dquote", b'SET ed ""\r\nSTRLEN ed\r\n'),
    ("unbalanced_dquote", b'SET ub "no close\r\n'),            # err unbalanced
    ("unbalanced_squote", b"SET ub2 'no close\r\n"),
    ("quote_no_space_after", b'SET q "a"b\r\n'),               # err closing-quote rule
    ("empty_line", b"\r\n"),                                   # no-op
    ("only_spaces", b"   \r\n"),
    ("bare_lf", b"PING\n"),                                    # bare LF accepted
    ("unknown_cmd", b"NOSUCHCMD a b\r\n"),
    ("wrong_arity", b"GET\r\n"),
    ("hex_lower_J", b'SET hl "\\x4a"\r\nGET hl\r\n'),
    ("mixed_quote_append", b'APPEND mq "x"\r\nAPPEND mq abc\r\nGET mq\r\n'),
    ("dquote_then_bare", b'MSET "a b" 1 c 2\r\nGET "a b"\r\n'),  # quoted key with space
]


def conn(p):
    s = socket.create_connection(("127.0.0.1", p), timeout=5)
    s.settimeout(1.5)
    return s


def raw(s, b):
    s.sendall(b)
    time.sleep(0.05)
    try:
        return s.recv(1 << 20)
    except Exception:
        return b"(timeout)"


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    # reset both keyspaces
    for p in (op, fp):
        c = conn(p)
        raw(c, b"FLUSHALL\r\n")
        c.close()
    fails = []
    for label, payload in CASES:
        # fresh connection per case: an inline parse error closes the connection
        o, f = conn(op), conn(fp)
        ro, rf = raw(o, payload), raw(f, payload)
        o.close()
        f.close()
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} inline-parser divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — inline command parser byte-exact vs redis 7.2.4 "
        f"({len(CASES)} cases: quotes/escapes/unbalanced/whitespace/bare-LF/arity)"
    )


if __name__ == "__main__":
    main()
