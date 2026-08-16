#!/usr/bin/env python3
"""Three-way equivalence for newly front-classified dispatch routes — fr fast route
vs fr generic path vs live Redis 7.2.4.

Covers frankenredis-9u5z9 (ZMPOP) and frankenredis-nscqs (BITOP).

This is the scoped equivalent of `borrowed_fast_routes_agree_with_generic_dispatch_
and_legacy_redis` for these routes only. It exists because that gate asserts on its
FIRST mismatch and is currently RED at reply 110 on an unrelated HRANDFIELD ordering
bug (frankenredis-brs56), which masks every later row including these. When brs56
clears, the corpus rows in that gate are the permanent home and this becomes a fast
scoped re-check rather than the primary evidence.

The fr arms are the SAME ELF: FR_PERF_AB_CASCADE_BYPASS=1 selects the generic path,
unset selects the front-classified fast route. So a difference between them is the
route, not the build. That requires a binary built with
`--features perf-ab-cascade-bypass`; without it both fr arms are the fast route and
the fast-vs-generic column proves nothing, so the run is refused below.

    dispatch_route_differ.py <redis_port> <fr_fast_port> <fr_generic_port>
Exit 0 = all three agree on every case.
"""
import os
import socket
import sys

RS, FF, FG = (int(a) for a in sys.argv[1:4])


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 10)
        self.buf = b""

    def _enc(self, args):
        out = [b"*%d\r\n" % len(args)]
        for a in args:
            b = a.encode() if isinstance(a, str) else a
            out.append(b"$%d\r\n%s\r\n" % (len(b), b))
        return b"".join(out)

    def _line(self):
        while b"\r\n" not in self.buf:
            c = self.s.recv(1 << 20)
            if not c:
                raise EOFError
            self.buf += c
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _read(self):
        line = self._line()
        tag, rest = line[:1], line[1:]
        if tag in (b"+", b":"):
            return tag.decode() + rest.decode()
        if tag == b"-":
            return "ERR:" + rest.decode()
        if tag == b"$":
            n = int(rest)
            if n == -1:
                return "(nil)"
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(1 << 20)
            v, self.buf = self.buf[:n], self.buf[n + 2:]
            return v.decode(errors="replace")
        if tag == b"*":
            n = int(rest)
            if n == -1:
                return "(nil-array)"
            return "[" + ",".join(str(self._read()) for _ in range(n)) + "]"
        raise RuntimeError(f"bad tag {line!r}")

    def cmd(self, *a):
        self.s.sendall(self._enc(list(a)))
        return self._read()


# (command, ...) applied in order to all three engines; every reply compared.
CASES = [
    ("FLUSHALL",),
    ("ZADD", "z:mp", "1", "a", "2", "b", "3", "c", "4", "d"),
    ("SADD", "s:1", "x"),
    # The classified shape, both directions. MIN must take `a`, MAX must take `d`;
    # a route wired to the wrong executor passes a nil-only corpus and fails here.
    ("ZMPOP", "1", "z:mp", "MIN"),
    ("ZMPOP", "1", "z:mp", "MAX"),
    ("ZRANGE", "z:mp", "0", "-1", "WITHSCORES"),
    # The missing-key branch: the shape the 0.7698x/0.7860x loss was measured on.
    ("ZMPOP", "1", "z:absent", "MIN"),
    # Arities the classifier deliberately does NOT claim — must still reach the
    # cascade and answer identically.
    ("ZMPOP", "1", "z:mp", "MIN", "COUNT", "2"),
    ("ZMPOP", "2", "z:absent", "z:mp", "MIN"),
    # (frankenredis-2e4tq) ZRANGE's `*5` option set. The class is minted on ARITY
    # ALONE, so REV/BYSCORE/BYLEX are all claimed as WITHSCORES and the arm now
    # serves them off key_arg3 instead of dropping to generic. MossySparrow's
    # warning applies here and is why all three are listed rather than the one
    # that was measured: a discriminator covering only REV reproduces the bug one
    # keyword over.
    ("ZADD", "z:opt", "1", "a", "2", "b", "3", "c"),
    ("ZRANGE", "z:opt", "0", "-1", "REV"),
    ("ZRANGE", "z:opt", "0", "-1", "WITHSCORES"),
    ("ZRANGE", "z:opt", "1", "3", "BYSCORE"),
    ("ZRANGE", "z:opt", "[a", "[c", "BYLEX"),
    # Reversed/empty ranges: a REV route wired to the forward executor answers
    # these identically to the forward form and passes a same-order corpus.
    ("ZRANGE", "z:opt", "0", "0", "REV"),
    ("ZRANGE", "z:opt", "-1", "-1", "REV"),
    ("ZRANGE", "z:absent", "0", "-1", "REV"),
    # The `*5` token that is NOT an option: must still reach the generic path.
    ("ZRANGE", "z:opt", "0", "-1", "SIDEWAYS"),
    # Wrong type through the new fallback -- the error must be verbatim.
    ("ZRANGE", "s:1", "0", "-1", "REV"),
    # (frankenredis-xqqwv) The SINGLE-FIELD forms. Both classes mint at `arity >= 3`
    # while their arms pinned 4 and 5 and floored the multi parser at 6, so
    # `HMGET key field` and `ZMSCORE key member` -- the smallest legal call each
    # command has -- were claimed and served by nothing. The floors are now 3.
    # These rows are the ONLY coverage that change has: no test in the tree names
    # hmget or zmscore, so the suite going green says nothing about it.
    ("HSET", "h:one", "f1", "v1", "f2", "v2"),
    ("ZADD", "z:one", "1", "m1", "2", "m2"),
    # The newly-served shape, present and absent field/member.
    ("HMGET", "h:one", "f1"),
    ("HMGET", "h:one", "nosuch"),
    ("ZMSCORE", "z:one", "m1"),
    ("ZMSCORE", "z:one", "nosuch"),
    # The neighbours the exact-N parsers still own -- these must not have moved.
    ("HMGET", "h:one", "f1", "f2"),
    ("HMGET", "h:one", "f1", "f2", "nosuch"),
    ("ZMSCORE", "z:one", "m1", "m2"),
    # Missing key: a one-element reply array, which is where a mis-wired route
    # that returns a bare bulk instead of a 1-array would diverge.
    ("HMGET", "h:absent", "f1"),
    ("ZMSCORE", "z:absent", "m1"),
    # Wrong type through the newly-served length.
    ("HMGET", "s:1", "f1"),
    ("ZMSCORE", "s:1", "m1"),
    # Errors must come from the generic path verbatim.
    ("ZMPOP", "1", "s:1", "MIN"),
    ("ZMPOP", "1", "z:mp", "SIDEWAYS"),
    ("ZMPOP", "0", "z:mp", "MIN"),
    ("ZMPOP", "1", "z:mp"),
    # Re-seed and re-run the classified shape so the pop is observed twice.
    ("ZADD", "z:mp2", "10", "p", "20", "q"),
    ("ZMPOP", "1", "z:mp2", "MIN"),
    ("ZMPOP", "1", "z:mp2", "MAX"),
    ("ZMPOP", "1", "z:mp2", "MIN"),
    ("EXISTS", "z:mp2"),

    # ── BITOP (frankenredis-nscqs) ──────────────────────────────────────────
    # UNEQUAL source lengths are the case that matters: upstream zero-extends the
    # shorter operand to the longest, so AND must clear the tail while OR/XOR keep
    # it, and the reply is the LONGEST length. Equal-length operands hide all of it.
    ("SET", "bo:a", "abcdefgh"),
    ("SET", "bo:b", "abc"),
    ("BITOP", "AND", "bo:and", "bo:a", "bo:b"),
    ("GET", "bo:and"),
    ("BITOP", "OR", "bo:or", "bo:a", "bo:b"),
    ("GET", "bo:or"),
    ("BITOP", "XOR", "bo:xor", "bo:a", "bo:b"),
    ("GET", "bo:xor"),
    # Arity 4: NOT and the single-source AND form share the other route.
    ("BITOP", "NOT", "bo:not", "bo:a"),
    ("GET", "bo:not"),
    ("BITOP", "AND", "bo:and1", "bo:a"),
    ("GET", "bo:and1"),
    # A missing source is an empty string upstream, so AND empties the dest and
    # DELETES it, replying 0.
    ("BITOP", "AND", "bo:miss", "bo:a", "bo:absent"),
    ("EXISTS", "bo:miss"),
    ("BITOP", "OR", "bo:miss2", "bo:absent", "bo:absent"),
    ("EXISTS", "bo:miss2"),
    # Errors, verbatim from the generic path.
    ("BITOP", "NOT", "bo:bad", "bo:a", "bo:b"),
    ("BITOP", "NAND", "bo:bad", "bo:a", "bo:b"),
    ("BITOP", "AND", "bo:bad", "s:1", "bo:a"),

    # ── 3-source set stores (frankenredis-804l1) ────────────────────────────
    # The three sources are deliberately ASYMMETRIC so operand ORDER is
    # observable: SDIFFSTORE is not commutative, so a route that reordered or
    # dropped a source passes a symmetric corpus and fails here. The members are
    # chosen so inter/union/diff each yield a DIFFERENT non-empty result.
    ("SADD", "ss:a", "m1", "m2", "m3", "m4"),
    ("SADD", "ss:b", "m2", "m3", "m4", "m5"),
    ("SADD", "ss:c", "m3", "m4", "m5", "m6"),
    ("SINTERSTORE", "ss:i", "ss:a", "ss:b", "ss:c"),
    ("SMEMBERS", "ss:i"),
    ("SUNIONSTORE", "ss:u", "ss:a", "ss:b", "ss:c"),
    ("SCARD", "ss:u"),
    ("SDIFFSTORE", "ss:d", "ss:a", "ss:b", "ss:c"),
    ("SMEMBERS", "ss:d"),
    # Order matters for DIFF: a different first operand is a different answer.
    ("SDIFFSTORE", "ss:d2", "ss:c", "ss:a", "ss:b"),
    ("SMEMBERS", "ss:d2"),
    # The 2-source form must keep working (it was already classified).
    ("SINTERSTORE", "ss:i2", "ss:a", "ss:b"),
    ("SMEMBERS", "ss:i2"),
    # An absent source: INTER with a missing key is empty, which DELETES the dest.
    ("SINTERSTORE", "ss:none", "ss:a", "ss:absent", "ss:c"),
    ("EXISTS", "ss:none"),
    # Four sources stay on the generic path and must answer identically.
    ("SADD", "ss:e", "m4"),
    ("SUNIONSTORE", "ss:u4", "ss:a", "ss:b", "ss:c", "ss:e"),
    ("SCARD", "ss:u4"),
    # Wrong type as a source, verbatim from the generic path.
    ("SINTERSTORE", "ss:bad", "ss:a", "bo:a", "ss:c"),
    # (frankenredis-opmo4) PFADD, LPUSHX and RPUSHX newly reach the floor at ONE
    # value. PFADD's no-op is the fragile case: upstream pfaddCommand fires
    # signalModifiedKey / notifyKeyspaceEvent / server.dirty / HLL_INVALIDATE_CACHE
    # only when `updated != 0`, and pvw3u established fr's notification is
    # centrally dirty-gated — so a route that dirties on a no-op diverges here
    # before it diverges anywhere else.
    ("PFADD", "hll:a", "e1"),
    ("PFCOUNT", "hll:a"),
    # Same element again: must reply 0. PFCOUNT after it is the assertion that
    # matters — a route adding to the wrong register set still replies 1, and a
    # reply-only corpus would pass it.
    ("PFADD", "hll:a", "e1"),
    ("PFCOUNT", "hll:a"),
    ("PFADD", "hll:a", "e2"),
    ("PFCOUNT", "hll:a"),
    # Multi-element PFADD is deliberately NOT claimed: only the values1 parser
    # knows these names, so array_len 4 must reach the cascade and answer
    # identically. If the classifier is ever widened to 3..=20 for these, this row
    # is what catches the packet landing on the generic path instead.
    ("PFADD", "hll:m", "e1", "e2", "e3"),
    ("PFCOUNT", "hll:m"),
    # Wrong type, verbatim from the generic path.
    ("PFADD", "s:1", "e1"),
    # LPUSHX/RPUSHX on a MISSING key must not create it — the branch that
    # separates them from LPUSH/RPUSH and the one a mis-wired executor loses.
    ("LPUSHX", "l:absent", "v1"),
    ("EXISTS", "l:absent"),
    ("RPUSHX", "l:absent", "v1"),
    ("EXISTS", "l:absent"),
    # ...and on a present key must push, at the correct end.
    ("LPUSH", "l:x", "mid"),
    ("LPUSHX", "l:x", "head"),
    ("RPUSHX", "l:x", "tail"),
    ("LRANGE", "l:x", "0", "-1"),
    # Multi-element form, deliberately unclaimed, stated at the values1 parser.
    ("LPUSHX", "l:x", "a", "b"),
    ("LRANGE", "l:x", "0", "-1"),
    ("RPUSHX", "s:1", "v"),
]

def executing_image(conn):
    """Resolve the live server's own executable via its self-reported process_id."""
    info = conn.cmd("INFO", "server")
    pid = next(
        line.split(":", 1)[1]
        for line in info.splitlines()
        if line.startswith("process_id:")
    )
    return os.path.realpath(f"/proc/{int(pid)}/exe")


redis, fast, generic = Conn(RS), Conn(FF), Conn(FG)

# REFUSE rather than report a meaningless column. Without the bypass feature
# compiled in, FR_PERF_AB_CASCADE_BYPASS=1 does nothing and BOTH fr arms run the
# fast route — the fast-vs-generic comparison would then pass by construction while
# testing nothing, which is exactly the false-pass shape this script exists to avoid.
fast_exe, generic_exe = executing_image(fast), executing_image(generic)
if fast_exe != generic_exe:
    raise SystemExit(
        f"REFUSED: the two fr arms are different binaries ({fast_exe} vs {generic_exe}); "
        "fast-vs-generic must be one ELF or a difference is a build artifact"
    )
with open(fast_exe, "rb") as image:
    if b"FR_PERF_AB_CASCADE_BYPASS" not in image.read():
        raise SystemExit(
            f"REFUSED: {fast_exe} was not built with --features perf-ab-cascade-bypass, "
            "so both fr arms are the fast route and the generic column proves nothing"
        )
print(f"one ELF, bypass feature present: {fast_exe}")

bad = 0
print(f"{'case':<38} {'fr fast':<22} {'fr generic':<22} {'redis 7.2.4'}")
print("-" * 108)
for case in CASES:
    r = redis.cmd(*case)
    f = fast.cmd(*case)
    g = generic.cmd(*case)
    label = " ".join(case)
    ok = (f == g) and (f == r)
    if not ok:
        bad += 1
    mark = "" if ok else ("   <-- FAST != GENERIC" if f != g else "   <-- fr != 7.2.4")
    print(f"{label:<38} {f[:21]:<22} {g[:21]:<22} {r[:21]}{mark}")

print()
print(f"{len(CASES)} cases, {bad} disagreement(s)")
sys.exit(0 if bad == 0 else 1)
