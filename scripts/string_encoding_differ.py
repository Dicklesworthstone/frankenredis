#!/usr/bin/env python3
"""Differential gate: string-encoding decision (int / embstr / raw) (frankenredis-n1i7i).

A string value is `int`-encoded only if it is the CANONICAL decimal form of an i64
(no leading zero, no '+', no spaces, within i64 range); otherwise `embstr` (<= 44
bytes) or `raw` (> 44 bytes). Some mutations force `raw` regardless (APPEND,
SETRANGE, SETBIT), APPEND that creates a key is raw (never int), INCR yields/keeps
int, and a fresh SET re-derives the encoding. The existing encoding gates probe
collection thresholds but NOT these string int-canonicalization edges or the exact
embstr/raw boundary — this gate pins them byte-exact vs redis 7.2.4.

Usage: string_encoding_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
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
    time.sleep(0.02)
    return s.recv(1 << 20)


# (label, value) -> SET then OBJECT ENCODING must match redis
SET_ENC = [
    ("int_small", "123"),
    ("int_neg", "-123"),
    ("int_zero", "0"),
    ("int_imax", "9223372036854775807"),
    ("int_imin", "-9223372036854775808"),
    ("over_i64_embstr", "9223372036854775808"),     # > i64 -> embstr
    ("twenty_digit_embstr", "12345678901234567890"),
    ("leading_zero_embstr", "0123"),                # not canonical -> embstr
    ("plus_prefix_embstr", "+123"),
    ("leading_space_embstr", " 123"),
    ("trailing_space_embstr", "123 "),
    ("embstr_43", "x" * 43),
    ("embstr_44", "x" * 44),                         # boundary: embstr
    ("raw_45", "x" * 45),                            # boundary: raw
    ("empty_embstr", ""),
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
    fails = []

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    def each(*c):
        for s in (od, fr):
            cmd(s, *c)

    for label, val in SET_ENC:
        each("DEL", "k")
        each("SET", "k", val)
        chk(f"set_{label}", "OBJECT", "ENCODING", "k")

    # transitions force raw
    each("DEL", "t")
    each("SET", "t", "short")
    chk("append_reply", "APPEND", "t", "x")
    chk("append_forces_raw", "OBJECT", "ENCODING", "t")
    each("DEL", "t2")
    each("SET", "t2", "123")               # int
    chk("setrange_reply", "SETRANGE", "t2", "0", "9")
    chk("setrange_forces_raw", "OBJECT", "ENCODING", "t2")  # raw even though "923" looks int
    each("DEL", "t3")
    each("SET", "t3", "5")
    chk("setbit_reply", "SETBIT", "t3", "0", "1")
    chk("setbit_forces_raw", "OBJECT", "ENCODING", "t3")
    # APPEND creating a key -> raw (not int) even if the appended text is numeric
    each("DEL", "ap")
    chk("append_new_reply", "APPEND", "ap", "123")
    chk("append_new_raw", "OBJECT", "ENCODING", "ap")
    # INCR yields/keeps int
    each("DEL", "c")
    each("SET", "c", "10")
    chk("incr_reply", "INCR", "c")
    chk("incr_int", "OBJECT", "ENCODING", "c")
    each("DEL", "c2")
    chk("incr_new_reply", "INCR", "c2")
    chk("incr_new_int", "OBJECT", "ENCODING", "c2")
    # fresh SET re-derives encoding (raw -> int)
    each("DEL", "g")
    each("APPEND", "g", "x" * 50)           # raw
    chk("overwrite_reply", "SET", "g", "5")
    chk("overwrite_to_int", "OBJECT", "ENCODING", "g")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} string-encoding divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — string-encoding decision byte-exact vs redis 7.2.4 "
        f"({len(SET_ENC)} SET-encs + int-canonicalization + embstr/raw boundary + force-raw transitions)"
    )


if __name__ == "__main__":
    main()
