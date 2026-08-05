#!/usr/bin/env python3
"""Differential gate: zset lexicographic range parsing (frankenredis-zavq6).

ZRANGEBYLEX / ZREVRANGEBYLEX / ZLEXCOUNT / ZRANGE ... BYLEX parse lex bounds with a
mandatory prefix byte: `[m` (inclusive), `(m` (exclusive), `+` (max), `-` (min). A
bare bound, `+x`, or an empty string is a "not valid string range" error. Member
order at equal score is the byte order of the members. This surface (bound parsing,
malformed-bound errors, reversed/empty ranges, LIMIT, REV) had no dedicated gate.
Compares every form byte-for-byte vs vendored redis 7.2.4.

Usage: zset_lex_range_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys

# all members share score 0 so ordering is purely lexicographic; includes the
# empty-string member and a shared-prefix pair (b / ba).
# Members share score 0 so ordering is purely lexicographic. Includes the empty
# member, a shared-prefix pair (b / ba), and two NON-ASCII members so the binary
# cases below compare against something that actually exists — a lex bound of
# "[\xff" over an all-ASCII set matches nothing on either engine, which passes
# while testing nothing. (frankenredis-r9ei8)
MEMBERS = ["a", "b", "c", "d", "e", "f", "ba", "", "\x00bin", "\xfe", "\xff"]

CASES = [
    ["ZRANGEBYLEX", "z", "-", "+"],
    ["ZRANGEBYLEX", "z", "[b", "[d"],
    ["ZRANGEBYLEX", "z", "(b", "(d"],
    ["ZRANGEBYLEX", "z", "[b", "(d"],
    ["ZRANGEBYLEX", "z", "[c", "+"],
    ["ZRANGEBYLEX", "z", "-", "[c"],
    ["ZRANGEBYLEX", "z", "(a", "+"],
    ["ZRANGEBYLEX", "z", "[", "+"],          # bound "[" => include the empty member
    ["ZRANGEBYLEX", "z", "[b", "[bz"],       # shared-prefix range (b, ba)
    ["ZRANGEBYLEX", "z", "[d", "[b"],        # reversed => empty
    ["ZRANGEBYLEX", "z", "+", "-"],          # reversed => empty
    ["ZRANGEBYLEX", "z", "-", "+", "LIMIT", "2", "3"],
    ["ZRANGEBYLEX", "z", "-", "+", "LIMIT", "2", "-1"],
    ["ZRANGEBYLEX", "z", "-", "+", "LIMIT", "100", "5"],
    ["ZRANGEBYLEX", "z", "b", "d"],          # ERR not valid string range (no prefix)
    ["ZRANGEBYLEX", "z", "+x", "-"],         # ERR
    ["ZRANGEBYLEX", "z", "", "+"],           # ERR (empty bound)
    ["ZREVRANGEBYLEX", "z", "+", "-"],
    ["ZREVRANGEBYLEX", "z", "[d", "[b"],
    ["ZREVRANGEBYLEX", "z", "(d", "(b"],
    ["ZREVRANGEBYLEX", "z", "[b", "[d"],     # reversed => empty
    ["ZREVRANGEBYLEX", "z", "-", "+", "LIMIT", "0", "3"],
    ["ZLEXCOUNT", "z", "-", "+"],
    ["ZLEXCOUNT", "z", "[b", "[d"],
    ["ZLEXCOUNT", "z", "(a", "(c"],
    ["ZLEXCOUNT", "z", "x", "y"],            # ERR
    ["ZRANGE", "z", "[b", "[d", "BYLEX"],
    ["ZRANGE", "z", "[d", "[b", "BYLEX", "REV"],
    ["ZRANGE", "z", "-", "+", "BYLEX", "LIMIT", "1", "2"],
    ["ZRANGEBYLEX", "nope", "-", "+"],       # missing key => empty
    ["ZLEXCOUNT", "nope", "-", "+"],
    # Members that ARE the bound sigils. The prefix byte is stripped before the
    # member is compared, so a member literally named "[", "(", "+" or "-" is
    # only reachable as `[[`, `[(`, `[+`, `[-` — the place a parser is most
    # likely to confuse a sigil for content.
    ["ZRANGEBYLEX", "z", "[[", "[["],
    ["ZRANGEBYLEX", "z", "[(", "[("],
    ["ZRANGEBYLEX", "z", "[+", "[+"],
    ["ZRANGEBYLEX", "z", "[-", "[-"],
    ["ZRANGEBYLEX", "z", "([", "[+"],
    ["ZLEXCOUNT", "z", "[-", "[+"],
    # Binary members: a NUL and a high byte must order and render byte-exactly.
    ["ZRANGEBYLEX", "z", "[\x00", "[\x00z"],
    ["ZRANGEBYLEX", "z", "[\xff", "+"],
    # LIMIT edges the original set did not reach.
    ["ZRANGEBYLEX", "z", "-", "+", "LIMIT", "0", "0"],
    ["ZRANGEBYLEX", "z", "-", "+", "LIMIT", "-1", "3"],
    ["ZREVRANGEBYLEX", "z", "+", "-", "LIMIT", "2", "-1"],
    ["ZRANGE", "z", "-", "+", "BYLEX", "REV"],          # REV with min/max unswapped => empty
    ["ZRANGE", "z", "+", "-", "BYLEX", "REV", "LIMIT", "1", "2"],
    ["ZRANGE", "z", "[b", "[d", "BYLEX", "LIMIT", "1", "1"],
    # LIMIT is only legal for BYLEX/BYSCORE — plain ZRANGE must reject it.
    ["ZRANGE", "z", "0", "-1", "LIMIT", "0", "1"],
    # Malformed LIMIT arguments.
    ["ZRANGEBYLEX", "z", "-", "+", "LIMIT", "x", "1"],
    ["ZRANGEBYLEX", "z", "-", "+", "LIMIT", "1"],
    # Wrong type: the lex path must report WRONGTYPE like every other reader.
    ["ZRANGEBYLEX", "str", "-", "+"],
    ["ZLEXCOUNT", "str", "-", "+"],
]

# A member set large enough that a wide range's reply spans multiple TCP
# segments — the case that silently truncated under the old single-recv reader.
WIDE_MEMBERS = [f"w{i:016d}" for i in range(4000)]

WIDE_CASES = [
    ["ZRANGEBYLEX", "wide", "-", "+"],
    ["ZREVRANGEBYLEX", "wide", "+", "-"],
    ["ZLEXCOUNT", "wide", "-", "+"],
    ["ZRANGEBYLEX", "wide", "[w0000000000001000", "[w0000000000003000"],
    ["ZRANGE", "wide", "-", "+", "BYLEX", "LIMIT", "1500", "1200"],
]


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def _frame_len(buf, i=0):
    """Byte length of the complete RESP frame at buf[i:], or None if partial.

    Needed because the reply to a wide lex range is far larger than one TCP
    segment. The original reader slept 20ms and took a single recv(), which
    silently truncates: a 4000-member ZRANGEBYLEX returns 65536 bytes of a much
    longer reply. With the old 8-member fixture that never showed, but it made
    the gate unsafe to widen — and two replies both truncated at the same offset
    compare EQUAL while differing past it, i.e. a false PASS. (frankenredis-zavq6)
    """
    nl = buf.find(b"\r\n", i)
    if nl < 0:
        return None
    kind, head = buf[i:i + 1], buf[i + 1:nl]
    if kind in (b"+", b"-", b":", b",", b"#", b"("):
        return nl + 2 - i
    if kind == b"$":
        n = int(head)
        if n < 0:
            return nl + 2 - i
        end = nl + 2 + n + 2
        return end - i if len(buf) >= end else None
    if kind in (b"*", b"~", b">", b"%"):
        n = int(head)
        if n < 0:
            return nl + 2 - i
        if kind == b"%":
            n *= 2
        pos = nl + 2
        for _ in range(n):
            sub = _frame_len(buf, pos)
            if sub is None:
                return None
            pos += sub
        return pos - i
    raise ValueError(f"unrecognised RESP type byte {kind!r}")


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        # latin-1, NOT utf-8: the binary-member cases below name specific bytes
        # with \xNN escapes, and utf-8 would send 0xff as 0xc3 0xbf — valid
        # UTF-8 — so the binary cases would test nothing. (frankenredis-r9ei8)
        x = x if isinstance(x, bytes) else str(x).encode("latin-1")
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    buf = b""
    while True:
        try:
            if buf and _frame_len(buf) is not None:
                return buf
        except ValueError:
            return buf
        chunk = s.recv(1 << 20)
        if not chunk:
            raise OSError("server closed the connection mid-reply")
        buf += chunk


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
        args = ["ZADD", "z"]
        for m in MEMBERS:
            args += ["0", m]
        added = cmd(s, *args)
        # The seed must actually land, or every case below compares two empty
        # ranges and the gate passes vacuously.
        expected = b":%d\r\n" % len(set(MEMBERS))
        if added != expected:
            print(f"SEED FAILED: ZADD z returned {added!r}, expected {expected!r}")
            sys.exit(1)
        # A string key so the WRONGTYPE cases have something to trip on.
        cmd(s, "SET", "str", "notazset")
        wide = ["ZADD", "wide"]
        for m in WIDE_MEMBERS:
            wide += ["0", m]
        added = cmd(s, *wide)
        expected = b":%d\r\n" % len(WIDE_MEMBERS)
        if added != expected:
            print(f"SEED FAILED: ZADD wide returned {added!r}, expected {expected!r}")
            sys.exit(1)

    fails = []
    for argv in CASES + WIDE_CASES:
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        if ro != rf:
            fails.append(f"{' '.join(argv)!r}: redis={ro!r} fr={rf!r}")

    # The wide cases only mean something if their replies really do exceed one
    # segment; assert that rather than assuming it, so this can never quietly
    # decay back into a single-recv-sized comparison.
    widest = cmd(od, "ZRANGEBYLEX", "wide", "-", "+")
    if len(widest) <= (1 << 16):
        print(
            f"HARNESS DEFECT: widest reply is only {len(widest)} bytes, so the "
            "multi-segment read path is not being exercised"
        )
        sys.exit(1)

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} zset lex-range divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — zset lexicographic range byte-exact vs redis 7.2.4 "
        f"({len(CASES) + len(WIDE_CASES)} cases: ZRANGEBYLEX/ZREVRANGEBYLEX/"
        "ZLEXCOUNT/ZRANGE BYLEX, bounds/sigil-members/binary/errors/LIMIT/REV, "
        f"incl. {len(WIDE_CASES)} multi-segment replies up to {len(widest)} bytes)"
    )


if __name__ == "__main__":
    main()
