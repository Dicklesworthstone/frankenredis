#!/usr/bin/env python3
"""Differential gate: DUMP byte-equality over LZF-compressible values (frankenredis-m5ofz).

fr-persist's lzf_compress is a byte-exact port of vendored liblzf (HLOG=16,
VERY_FAST), so a DUMP of a compressible string embeds an LZF payload that must be
byte-identical to redis 7.2.4's. The match-length scan was rewritten for speed
(g9h0v: SWAR 16-byte unrolled fast path + common_prefix_len tail scan); that path
has a subtle "overshoot" — when maxlen > 16 the unrolled compare runs to offset 18
regardless of maxlen, so the produced match length (hence the wire bytes) is
sensitive to inputs whose match ends right around offsets 16-20. This gate pins
the wire output across that boundary, plus longer/mixed payloads, so any future
lzf refactor that diverges from liblzf is caught at the DUMP byte level. It also
round-trips every value through RESTORE on fr to confirm self-consistency.

Usage: lzf_dump_byte_equality_differ.py <oracle_port> <fr_port>
       Exit 0 = every DUMP byte-exact + RESTORE round-trips, 1 = divergence.
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


def values():
    out = []
    # all-same bytes 3..40 — exercises the match-length scan right across the
    # 16-byte unrolled fast-path / overshoot boundary one length at a time.
    for n in range(3, 41):
        out.append((f"same_{n}", b"a" * n))
    # repeating 4-gram at and around the overshoot boundary + longer.
    for n in [16, 17, 18, 19, 20, 21, 24, 32, 64, 100, 263, 264, 265, 300, 1000, 4096]:
        out.append((f"gram4_{n}", (b"abcd" * (n // 4 + 1))[:n]))
    # repeating 3-gram (trigram == lzf hash window) and a longer-period pattern.
    out.append(("gram3", b"xyz" * 200))
    out.append(("gram7", b"abcdefg" * 150))
    # mixed / natural-text compressible payloads (distinct trigrams, real matches).
    out.append(("words", b"hello world " * 80))
    out.append(("sentence", b"the quick brown fox jumps over " * 60))
    # incompressible-ish (forces the raw/literal decision path) — still must match.
    out.append(("lowcompress", bytes(range(256)) * 8))
    return out


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []
    for label, v in values():
        cmd(od, "FLUSHALL")
        cmd(fr, "FLUSHALL")
        cmd(od, "SET", "k", v)
        cmd(fr, "SET", "k", v)
        do, df = cmd(od, "DUMP", "k"), cmd(fr, "DUMP", "k")
        if do != df:
            fails.append(f"{label} (len {len(v)}): DUMP bytes differ redis={do!r} fr={df!r}")
            continue
        # round-trip the fr DUMP payload back through RESTORE on fr and confirm
        # the value is recovered (parse the $-bulk DUMP reply to get raw bytes).
        if df.startswith(b"$"):
            nl = df.index(b"\r\n")
            payload = df[nl + 2 : nl + 2 + int(df[1:nl])]
            cmd(fr, "DEL", "k2")
            rr = cmd(fr, "RESTORE", "k2", "0", payload)
            if not rr.startswith(b"+OK"):
                fails.append(f"{label}: RESTORE of fr DUMP failed: {rr!r}")
                continue
            if cmd(fr, "GET", "k2") != cmd(fr, "GET", "k"):
                fails.append(f"{label}: RESTORE round-trip value mismatch")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} LZF DUMP divergence(s) vs redis 7.2.4:")
        for x in fails[:10]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — LZF-compressible DUMP byte-exact vs redis 7.2.4 + RESTORE round-trips "
        f"({len(values())} payloads across the match-length overshoot boundary)"
    )


if __name__ == "__main__":
    main()
