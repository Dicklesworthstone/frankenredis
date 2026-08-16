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
import re
import socket
import sys
import time

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
    # (frankenredis-ayiy7 / the worst measured ratio) ZRANGEBYSCORE's LIMIT form.
    # Every array length of this command is UNAMBIGUOUS -- 4 plain, 5 WITHSCORES,
    # 7 LIMIT, 8 WITHSCORES LIMIT -- yet only arity 4 is floor-classified, so the
    # LIMIT form walks ~5,485 lines of cascade to reach an arm that already has a
    # zero-copy executor. These rows gate the classification when it lands, and
    # pin current behaviour until then.
    ("ZADD", "z:lim", "1", "a", "2", "b", "3", "c", "4", "d"),
    ("ZRANGEBYSCORE", "z:lim", "1", "4", "LIMIT", "0", "2"),
    ("ZRANGEBYSCORE", "z:lim", "1", "4", "LIMIT", "1", "2"),
    # offset past the end, and a negative count meaning "all from offset" --
    # a route that clamped instead of following redis would pass a naive corpus.
    ("ZRANGEBYSCORE", "z:lim", "1", "4", "LIMIT", "9", "2"),
    ("ZRANGEBYSCORE", "z:lim", "1", "4", "LIMIT", "1", "-1"),
    ("ZRANGEBYSCORE", "z:lim", "1", "4", "LIMIT", "0", "0"),
    # exclusive and infinite bounds through the same form
    ("ZRANGEBYSCORE", "z:lim", "(1", "+inf", "LIMIT", "0", "3"),
    ("ZRANGEBYSCORE", "z:lim", "-inf", "+inf", "LIMIT", "0", "-1"),
    # the neighbouring lengths that must NOT move
    ("ZRANGEBYSCORE", "z:lim", "1", "4"),
    ("ZRANGEBYSCORE", "z:lim", "1", "4", "WITHSCORES"),
    ("ZRANGEBYSCORE", "z:lim", "1", "4", "WITHSCORES", "LIMIT", "0", "2"),
    # missing key and wrong type through the LIMIT form
    ("ZRANGEBYSCORE", "z:absent", "1", "4", "LIMIT", "0", "2"),
    ("ZRANGEBYSCORE", "s:1", "1", "4", "LIMIT", "0", "2"),
    # a non-LIMIT token at the same length must still reach generic verbatim
    ("ZRANGEBYSCORE", "z:lim", "1", "4", "SIDEWAYS", "0", "2"),
    # (frankenredis-6oxxn) The remaining stranded routes, at the arities --stranded
    # reports as SAFE to claim. Landing the gate BEFORE the classification means
    # whoever mints these classes inherits a corpus instead of writing one, and the
    # rows also pin today's cascade behaviour so a regression is visible either way.
    #
    # TOUCH, safe at 2..6. Read-only, so ordering with the rest is harmless.
    ("TOUCH", "t:absent"),
    ("TOUCH", "z:lim"),
    ("TOUCH", "z:lim", "t:absent"),
    ("TOUCH", "z:lim", "t:absent", "s:1"),
    # MSETNX, safe at odd lengths. It is all-or-nothing: the second call must
    # return 0 and leave the FIRST key untouched, which a route that wrote
    # eagerly would fail.
    ("MSETNX", "mn:a", "1"),
    ("MSETNX", "mn:a", "2", "mn:b", "3"),
    ("GET", "mn:a"),
    ("EXISTS", "mn:b"),
    ("MSETNX", "mn:c", "1", "mn:d", "2"),
    ("MGET", "mn:c", "mn:d"),
    # SPUBLISH, safe at 3. No subscribers on either engine, so the receiver count
    # is deterministic.
    ("SPUBLISH", "chan:1", "hello"),
    # LMPOP, safe at 4/5/9/10 -- and length 6 is the AMBIGUOUS one the advisor
    # flags, so both readings are exercised here deliberately.
    ("RPUSH", "l:mp", "a", "b", "c"),
    ("LMPOP", "1", "l:mp", "LEFT"),
    ("LMPOP", "1", "l:mp", "RIGHT"),
    ("LMPOP", "1", "l:absent", "LEFT"),
    ("LMPOP", "1", "l:mp", "LEFT", "COUNT", "2"),
    ("RPUSH", "l:mp2", "x", "y"),
    ("LMPOP", "2", "l:absent", "l:mp2", "LEFT"),
    ("LMPOP", "1", "s:1", "LEFT"),
    # MOVE, safe at 3. Kept LAST of the stateful rows: it removes the key from
    # db 0, so anything after it would see a different keyspace.
    ("SET", "mv:k", "v"),
    ("MOVE", "mv:k", "1"),
    ("EXISTS", "mv:k"),
    ("MOVE", "mv:absent", "1"),
    # (frankenredis-cv3fv) SINTERCARD. Three distinct situations at three lengths,
    # and the corpus separates them deliberately because a fix aimed at one can
    # silently move another:
    #   len 5  CLAIMED and refused -- `(4..=5)` mints the class, sintercard3
    #          validates numkeys != 3 and declines, so `SINTERCARD 1 k LIMIT n`
    #          reaches generic.
    #   len 6  NOT claimed at all, and AMBIGUOUS ((plain) x4 vs LIMIT x2), so
    #          classifying it needs the numkeys bulk read rather than the length.
    #          This is the shape sintercard_lim measured 0.7881 on.
    #   len 8+ unique LIMIT forms, safe to claim on length alone.
    ("SADD", "sc:1", "a", "b", "c", "d"),
    ("SADD", "sc:2", "b", "c", "d", "e"),
    ("SADD", "sc:3", "c", "d", "e", "f"),
    # the claimed-and-refused length
    ("SINTERCARD", "1", "sc:1", "LIMIT", "2"),
    ("SINTERCARD", "3", "sc:1", "sc:2", "sc:3"),
    # the ambiguous length: both readings, same array length
    ("SINTERCARD", "2", "sc:1", "sc:2", "LIMIT", "1"),
    ("SINTERCARD", "4", "sc:1", "sc:2", "sc:3", "sc:1"),
    # LIMIT 0 means "no limit" upstream, not "return nothing" -- a route that
    # treated it as a cap would pass every other row here
    ("SINTERCARD", "2", "sc:1", "sc:2", "LIMIT", "0"),
    # unique LIMIT lengths
    ("SINTERCARD", "3", "sc:1", "sc:2", "sc:3", "LIMIT", "2"),
    # empty intersection and a missing key
    ("SINTERCARD", "2", "sc:1", "s:1"),
    ("SINTERCARD", "1", "sc:absent"),
    ("SINTERCARD", "2", "sc:1", "sc:absent", "LIMIT", "3"),
    # wrong type and a bad numkeys must come back verbatim
    ("SINTERCARD", "1", "z:lim"),
    ("SINTERCARD", "0", "sc:1"),
    # (frankenredis-zadd5) ZADD's single-flag form, array length 5 -- the shape
    # zadd_xx measured 0.8959 on. Unclassified today: the class is
    # `arity >= 8 && even`, so length 5 walks the cascade.
    #
    # The advisor calls length 5 AMBIGUOUS (six readings: NX/XX/GT/LT/CH/INCR) and
    # that is TRUE but not a defect, because two existing parsers cover all six --
    # zadd_flag is generic over the flag argument, and zadd_incr handles INCR, whose
    # reply type differs. Both readings are exercised here so a class minted at 5
    # cannot serve one and strand the other.
    ("DEL", "za:k"),
    ("ZADD", "za:k", "1", "m1"),
    ("ZADD", "za:k", "XX", "5", "m1"),
    ("ZADD", "za:k", "XX", "5", "absent"),
    ("ZADD", "za:k", "NX", "9", "m1"),
    ("ZADD", "za:k", "NX", "9", "m2"),
    ("ZSCORE", "za:k", "m1"),
    ("ZSCORE", "za:k", "m2"),
    # GT/LT only move the score in one direction -- a route wired to a plain add
    # returns the same COUNT here while leaving a different score behind.
    ("ZADD", "za:k", "GT", "1", "m1"),
    ("ZSCORE", "za:k", "m1"),
    ("ZADD", "za:k", "GT", "99", "m1"),
    ("ZSCORE", "za:k", "m1"),
    ("ZADD", "za:k", "LT", "50", "m1"),
    ("ZSCORE", "za:k", "m1"),
    # CH changes the RETURN VALUE, not the data: added vs changed.
    ("ZADD", "za:k", "CH", "77", "m1"),
    # INCR returns the new SCORE, a bulk, not an integer count -- the reading a
    # flag-only route would get wrong in reply TYPE, not just value.
    ("ZADD", "za:k", "INCR", "3", "m1"),
    ("ZADD", "za:k", "INCR", "3", "brand:new"),
    # errors verbatim
    ("ZADD", "za:k", "XX", "notanumber", "m1"),
    ("ZADD", "s:1", "XX", "1", "m1"),
    # (frankenredis-move3) MOVE, measured BELOW PARITY in every run of
    # exists_vs_redis/move_missing and unclassified: no BorrowedDispatchFloorCommand
    # variant, no dedicated parser, executor present (execute_plain_move_borrowed).
    # Cascade depth ~1800. Gate first, because MOVE mutates the KEYSPACE and a
    # classified route that got the db wrong would be silently destructive rather
    # than merely slow.
    #
    # Ordered last of the stateful rows and on its own keys: MOVE removes the key
    # from db 0, so anything reading mv:* afterwards observes a different keyspace.
    ("SET", "mv:src", "v1"),
    ("MOVE", "mv:src", "1"),
    ("EXISTS", "mv:src"),
    # missing key -> 0, and the destination must NOT be created
    ("MOVE", "mv:absent", "1"),
    ("EXISTS", "mv:absent"),
    # moving to the SAME db is an error upstream, not a no-op
    ("SET", "mv:same", "v2"),
    ("MOVE", "mv:same", "0"),
    ("EXISTS", "mv:same"),
    # a key that already exists in the destination must NOT be overwritten:
    # MOVE returns 0 and the source stays put. A route that clobbered would still
    # return 0 here, so the follow-up GET is what catches it.
    ("SET", "mv:dup", "original"),
    ("MOVE", "mv:dup", "1"),
    ("SET", "mv:dup", "second"),
    ("MOVE", "mv:dup", "1"),
    ("GET", "mv:dup"),
    # out-of-range and non-numeric db indices must error verbatim
    ("MOVE", "mv:dup", "99"),
    ("MOVE", "mv:dup", "notanumber"),
    ("MOVE", "mv:dup", "-1"),
    # (frankenredis-8xyox) NEGATIVE CASE for the MOVE allocation change. Hoisting
    # key_owned/db_owned into the builder closure is only safe if the closure still
    # produces the CORRECT argv when it actually runs. The closure is invoked from
    # record_plain_zremrange_borrowed_metrics only on a slowlog / latency / time
    # budget breach, so the fast path never exercises it -- which means a bug there
    # would be invisible to every other MOVE row in this corpus.
    #
    # Forcing slowlog to log everything makes the closure run on every command, and
    # SLOWLOG GET then shows the argv it built. If the hoist captured the wrong
    # slices, or captured a dangling one, this is where it shows.
    ("CONFIG", "SET", "slowlog-log-slower-than", "0"),
    ("SLOWLOG", "RESET"),
    ("SET", "mvneg:k", "v"),
    ("MOVE", "mvneg:k", "1"),
    # the MISS path through the same closure -- the case the hoist changes most,
    # since it previously allocated and now must not
    ("MOVE", "mvneg:absent", "1"),
    ("SLOWLOG", "LEN"),
    ("SLOWLOG", "GET", "3"),
    ("CONFIG", "SET", "slowlog-log-slower-than", "10000"),
    ("SLOWLOG", "RESET"),
    # (frankenredis-zadd6) Array length 6 admits two readings and the arm served
    # only one. Both are exercised here so the fix cannot serve one and strand the
    # other -- which is the regression itself.
    ("DEL", "z6"),
    ("ZADD", "z6", "1", "m1", "2", "m2"),
    ("ZSCORE", "z6", "m1"),
    ("ZSCORE", "z6", "m2"),
    # two flags, one pair -- the reading that was dropped on generic
    ("ZADD", "z6", "XX", "CH", "5", "m1"),
    ("ZSCORE", "z6", "m1"),
    ("ZADD", "z6", "NX", "CH", "9", "m1"),
    ("ZSCORE", "z6", "m1"),
    ("ZADD", "z6", "NX", "CH", "9", "brandnew"),
    ("ZADD", "z6", "GT", "CH", "1", "m1"),
    ("ZSCORE", "z6", "m1"),
    ("ZADD", "z6", "GT", "CH", "99", "m1"),
    ("ZSCORE", "z6", "m1"),
    ("ZADD", "z6", "XX", "CH", "1", "absent"),
    ("ZADD", "s:1", "XX", "CH", "1", "m1"),
    # (frankenredis-f3nry) INCR at array length 6 is the case the fused arity-6
    # parser can get wrong SILENTLY. INCR is not in the flag whitelist and IS in
    # the two-pair reject list, so both readings decline it and it must reach the
    # generic path -- where its reply is a BULK score (or a nil), never the
    # integer count both fast paths emit. A fusion that admitted INCR as a flag,
    # or that dropped the INCR guard from the two-pair branch, would answer these
    # rows with the wrong RESP TYPE while every count-shaped row above stayed
    # green. Both option slots are covered because the guard is per-slot.
    ("ZADD", "z6", "INCR", "CH", "5", "m1"),
    ("ZSCORE", "z6", "m1"),
    ("ZADD", "z6", "GT", "INCR", "5", "m1"),
    ("ZADD", "z6", "GT", "INCR", "-5", "m1"),
    ("ZSCORE", "z6", "m1"),
    ("ZADD", "z6", "INCR", "XX", "2", "absent"),
    ("ZADD", "z6", "INCR", "NX", "2", "m1"),
    # A flag followed by a score is a dangling-element error, not a two-pair
    # write: the error text must come from the generic path verbatim.
    ("ZADD", "z6", "NX", "1", "a", "2"),
    # (frankenredis-4b2o4) LPOS MAXLEN. Array length 5 admits RANK, COUNT and
    # MAXLEN; the floor class claims all three and the arm branches on RANK and
    # COUNT only, so MAXLEN falls to generic. Unlike the ZADD arity-6 case there is
    # NO parser and NO executor for it anywhere, so generic is where it would have
    # gone regardless -- the cost is one wasted parse, not a lost fast path.
    # These rows exist so that if MAXLEN is ever given a route, the behaviour it
    # must reproduce is already pinned.
    ("DEL", "lm"),
    ("RPUSH", "lm", "a", "b", "c", "b", "a"),
    ("LPOS", "lm", "b"),
    ("LPOS", "lm", "b", "MAXLEN", "2"),
    ("LPOS", "lm", "b", "MAXLEN", "0"),
    ("LPOS", "lm", "nosuch", "MAXLEN", "3"),
    # RANK and COUNT at the same length, which the arm DOES serve -- so a future
    # MAXLEN route cannot be added by loosening the branch and stranding these.
    ("LPOS", "lm", "b", "RANK", "2"),
    ("LPOS", "lm", "b", "COUNT", "2"),
    ("LPOS", "s:1", "b", "MAXLEN", "2"),
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

    # ── GEOADD (frankenredis-tyujv) ─────────────────────────────────────────
    # The single-triple form newly reaches the floor. GEOADD's reply counts only
    # ADDED members, so the update case is the one a mis-wired route passes: a
    # route that re-adds instead of updating still replies 0 on the second call
    # if it silently no-ops, and still replies 0 on the third if it ignores the
    # new coordinates. ZSCORE is what separates those — the geohash is an exact
    # integer, so it changes iff the coordinates were actually written.
    ("GEOADD", "geo:a", "13.361389", "38.115556", "palermo"),
    ("ZCARD", "geo:a"),
    ("ZSCORE", "geo:a", "palermo"),
    # Same member, SAME coords: replies 0, score unchanged.
    ("GEOADD", "geo:a", "13.361389", "38.115556", "palermo"),
    ("ZSCORE", "geo:a", "palermo"),
    # Same member, DIFFERENT coords: still replies 0 (not an add) but the score
    # MUST move. A route that dropped the update passes every reply-only row here.
    ("GEOADD", "geo:a", "15.087269", "37.502669", "palermo"),
    ("ZSCORE", "geo:a", "palermo"),
    ("ZCARD", "geo:a"),
    # Second distinct member, so the key holds more than one element.
    ("GEOADD", "geo:a", "15.087269", "37.502669", "catania"),
    ("ZRANGE", "geo:a", "0", "-1"),
    # Multi-triple form: deliberately NOT claimed (no parser exists), so it must
    # reach the cascade and answer identically.
    ("GEOADD", "geo:m", "13.361389", "38.115556", "p1", "15.087269", "37.502669", "p2"),
    ("ZCARD", "geo:m"),
    # Out-of-range longitude: the error must come from the generic path verbatim.
    ("GEOADD", "geo:a", "181.0", "38.115556", "bad"),
    # Non-numeric coordinate, same requirement.
    ("GEOADD", "geo:a", "notanumber", "38.115556", "bad"),
    # Wrong type.
    ("GEOADD", "s:1", "13.361389", "38.115556", "m"),
    # NOTE: GEOPOS is deliberately NOT asserted here. It formats coordinates as
    # floats, and any fr-vs-redis difference in that formatting is a pre-existing
    # encoding question rather than a routing one — including it would make this
    # gate red for a reason that has nothing to do with the floor claim. ZSCORE
    # carries the same information as an exact integer.
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
    # (frankenredis-z2ce3) SET's OPTION FORMS. There is currently no option-form SET
    # coverage in this corpus at all -- the eight existing SET rows are plain `SET k v`
    # used as setup for other commands -- so an ordering or classification change around
    # SET would not be caught here.
    #
    # This matters now because the last untaken dispatch lever is base SET, which sits at
    # 33.9 pct dispatch because it is absent from the floor token table and its cascade
    # arm is ~340 lines past GET's. The two candidate fixes are a floor class for `*3`
    # SET, or moving its arm up beside GET's. BOTH reorder or reclassify around these
    # option forms, and a `*3` claim that reached them would send every one to the GENERIC
    # path -- which is what TOUCH and MSETNX measured (3.28x and 2.67x) when an exact
    # claim left siblings behind.
    #
    # Every write below is followed by a READ that distinguishes it from plain SET. A
    # reply-only corpus passes even if SET NX silently becomes SET, because both return
    # +OK on a missing key -- the divergence is only visible in what the key holds.
    ("DEL", "so:k"),
    ("SET", "so:k", "first"),
    ("SET", "so:k", "second", "NX"),     # key exists -> must NOT overwrite, returns nil
    ("GET", "so:k"),                     # -> "first"
    ("SET", "so:k", "third", "XX"),      # key exists -> must overwrite
    ("GET", "so:k"),                     # -> "third"
    ("SET", "so:absent", "v", "XX"),     # missing -> must NOT create, returns nil
    ("EXISTS", "so:absent"),
    ("SET", "so:nx2", "v", "NX"),        # missing -> must create
    ("GET", "so:nx2"),
    ("SET", "so:k", "fourth", "GET"),    # returns the PRIOR value
    ("GET", "so:k"),
    ("SET", "so:ex", "v", "EX", "100"),
    ("TTL", "so:ex"),                    # a plain-SET mis-route would leave -1
    ("SET", "so:ex", "w", "KEEPTTL"),
    ("TTL", "so:ex"),                    # must still be ~100, not -1
    ("SET", "so:ex", "x"),               # plain SET CLEARS the ttl
    ("TTL", "so:ex"),                    # -> -1, the control for the row above
    # The arity-3 form itself, which is what a move or a floor claim would touch.
    ("SET", "so:plain", "v"),
    ("GET", "so:plain"),

    # ---- SINTER, which had ZERO rows and a LIVE ORDER DIVERGENCE ----
    #
    # (frankenredis-gein3) fr used to `sort_unstable()` the SINTER reply at three sites;
    # redis 7.2.4's sinterGenericCommand (t_set.c:1277) sorts NOTHING -- it qsorts the SETS
    # by cardinality to choose the smallest to iterate, then streams members straight out
    # of that iterator. So the sort was not what kept fr byte-identical to the incumbent,
    # it was what made fr DIVERGE, and with no SINTER row in this corpus nothing caught it.
    # Measured live before the fix: redis "zeta alpha mike bravo", fr "alpha bravo mike zeta".
    #
    # THE MEMBERS ARE INSERTED IN NON-SORTED ORDER ON PURPOSE. With sorted input a
    # re-introduced sort is invisible, because sorted(insertion order) == insertion order
    # and every row stays green while the bug is back. `zeta` first is the whole test.
    #
    # SCOPE, stated because it bounds what these rows prove: every set here stays under
    # set-max-listpack-entries (128), where BOTH engines keep a listpack and iterate it in
    # insertion order, so the order is deterministic and comparable. Above that threshold
    # both engines hash and neither documents an order -- these rows deliberately do not
    # go there, and a corpus row that compared hashtable-regime order would be asserting
    # something redis does not promise.
    ("DEL", "si:a", "si:b", "si:c", "si:str"),
    ("SADD", "si:a", "zeta", "alpha", "mike", "bravo"),
    ("SADD", "si:b", "zeta", "alpha", "mike", "bravo"),
    ("SINTER", "si:a", "si:b"),          # -> insertion order, NOT sorted
    ("SINTERCARD", "2", "si:a", "si:b"),
    # three-way, and a partial intersection so the surviving ORDER is a strict subsequence
    # of the base set's insertion order rather than the whole of it
    ("SADD", "si:c", "mike", "zeta"),
    ("SINTER", "si:a", "si:b", "si:c"),
    ("SINTER", "si:c", "si:a"),          # smallest-set-first pick changes which set drives
    # single member: the degenerate case where order cannot discriminate, kept as a control
    ("SADD", "si:one", "solo"),
    ("SINTER", "si:one", "si:one"),
    # empty intersection, and a missing key, which redis treats as the empty set
    ("SADD", "si:d", "nothing"),
    ("SINTER", "si:a", "si:d"),
    ("SINTER", "si:a", "si:absent"),
    ("SINTER", "si:absent", "si:a"),
    ("SINTERCARD", "2", "si:a", "si:absent"),
    # wrong type through the same route -- error text must come back verbatim
    ("SET", "si:str", "v"),
    ("SINTER", "si:a", "si:str"),
    ("SINTER", "si:str", "si:a"),
    # SINTERSTORE consumes the same survivor walk; the stored set must hold the same
    # members, and SMEMBERS of it must not have been sorted on the way in either
    ("SINTERSTORE", "si:dst", "si:a", "si:b"),
    ("SMEMBERS", "si:dst"),
    ("SCARD", "si:dst"),

    # ---- PING, which had ZERO rows despite owning two cascade arms ----
    #
    # PING is served by parse_borrowed_plain_ping_upper_noarg_packet (an exact match on
    # the literal b"*1\r\n$4\r\nPING\r\n") and by parse_borrowed_plain_ping_packet, which
    # accepts BOTH the bare `*1` form and the `*2` form carrying a message. Two arms, two
    # arities, and not one row in this corpus until now -- so the arity fast-reject in
    # front of them was landing on an untested route.
    #
    # The `*2` row is the load-bearing one: a guard written as "arity == 1", the obvious
    # reading of a command whose common form takes no argument, stops `PING message` from
    # ever reaching its fast arm. That degrades to the generic path rather than answering
    # wrongly, so it is a PERFORMANCE bug the reply cannot show -- but the reply CAN show
    # the RESP type, and the two forms differ there: bare PING is a simple string (+PONG)
    # while PING with a message is a BULK echo of the argument.
    ("PING",),                       # -> +PONG (simple string)
    ("PING", "hello"),               # -> "hello" (BULK, not +PONG)
    ("PING", ""),                    # empty message is still a bulk, not +PONG
    ("PING", "PONG"),                # message that LOOKS like the bare reply
    ("ping",),                       # the exact-literal upper arm must not be the only one
    ("PiNg", "MiXeD"),               # both slots case-folded
    ("PING", "a", "b"),              # *3 -> arity error, verbatim from the generic path

    # ---- SET option forms that have their OWN fast route and NO corpus row ----
    #
    # The block above covers NX, XX, bare GET, EX-seconds and KEEPTTL. But SET is
    # front-classified by SEVEN separate parsers, and four of them are not reached by
    # any row above:
    #
    #   parse_borrowed_plain_set_relexpire_packet       PX half (only EX was covered)
    #   parse_borrowed_plain_set_absexpire_packet       EXAT|PXAT      -- no coverage
    #   parse_borrowed_plain_set_relexpire_get_packet   EX|PX n GET    -- no coverage
    #   parse_borrowed_plain_set_opt_get_packet         NX|XX|KEEPTTL GET, of which
    #                                                   only the *bare* GET was covered
    #   parse_borrowed_plain_set_cond_relexpire_packet  NX|XX EX|PX n, BOTH orders
    #
    # Each is a separate `else if` arm that both parses AND executes without re-entering
    # the generic path, so a wrong one is invisible to every row above. The rows below
    # follow this file's rule: never assert on the reply alone. SET NX GET and SET XX GET
    # return the same old value on an existing key while doing OPPOSITE things to it, and
    # every expiry form replies +OK whether or not the TTL was actually installed -- so
    # each write is followed by a GET/EXISTS/TTL that separates the readings.

    # PX is the is_seconds=false half of the relexpire parser. The reply is +OK either
    # way; only TTL tells PX from EX, and an EX/PX swap reads 100000 here, not 100.
    ("DEL", "so:px"),
    ("SET", "so:px", "v", "PX", "100000"),
    ("TTL", "so:px"),                          # -> 100
    ("PERSIST", "so:px"),                      # -> 1: a TTL really was installed
    ("TTL", "so:px"),                          # -> -1, the control for the row above

    # EXAT/PXAT. A PAST deadline is the time-independent discriminator: absolute means
    # the key is gone immediately, relative would leave it alive for a second. This is
    # the one shape where a mis-read is not a TTL-value bug but a lost/kept key.
    ("DEL", "so:at"),
    ("SET", "so:at", "v", "EXAT", "1"),        # 1970 -> written, already expired
    ("EXISTS", "so:at"),                       # -> 0; read as relative EX this is 1
    ("GET", "so:at"),                          # -> nil
    ("SET", "so:at", "v", "PXAT", "1000"),
    ("EXISTS", "so:at"),                       # -> 0
    # A future absolute deadline, checked WITHOUT reading a ticking TTL: PERSIST returns
    # 1 only if an expiry was actually attached, so this separates EXAT-honoured from
    # EXAT-silently-dropped (which would also reply +OK) with no timing dependence.
    ("SET", "so:at2", "v", "EXAT", str(int(time.time()) + 1000)),
    ("PERSIST", "so:at2"),                     # -> 1
    ("TTL", "so:at2"),                         # -> -1
    ("GET", "so:at2"),                         # -> "v": the value survived

    # SET key value EX|PX n GET (*6). Two independent effects in one route -- return the
    # PRIOR value and install the TTL. A route that does one and forgets the other still
    # replies plausibly, so both are read back.
    ("DEL", "so:eg"),
    ("SET", "so:eg", "one"),
    ("SET", "so:eg", "two", "EX", "100", "GET"),   # -> "one"
    ("GET", "so:eg"),                              # -> "two"
    ("TTL", "so:eg"),                              # -> 100: the EX half applied too
    ("DEL", "so:eg2"),
    ("SET", "so:eg2", "v", "PX", "100000", "GET"), # missing key -> nil
    ("GET", "so:eg2"),                             # -> "v"
    ("TTL", "so:eg2"),                             # -> 100

    # SET key value NX|XX|KEEPTTL GET (*5). NX GET and XX GET on an EXISTING key both
    # reply the old value; they differ only in whether the write landed. A route that
    # ignored the option would pass on replies alone and fail the GET after it.
    ("DEL", "so:og"),
    ("SET", "so:og", "first"),
    ("SET", "so:og", "second", "NX", "GET"),   # exists -> "first", must NOT overwrite
    ("GET", "so:og"),                          # -> "first"
    ("SET", "so:og", "third", "XX", "GET"),    # exists -> "first", MUST overwrite
    ("GET", "so:og"),                          # -> "third"
    ("DEL", "so:og3"),
    ("SET", "so:og3", "v", "XX", "GET"),       # missing -> nil, must NOT create
    ("EXISTS", "so:og3"),                      # -> 0
    ("DEL", "so:og4"),
    ("SET", "so:og4", "v", "NX", "GET"),       # missing -> nil, MUST create
    ("GET", "so:og4"),                         # -> "v"
    # KEEPTTL GET has to preserve an expiry it never parsed -- the failure mode is a
    # plain overwrite, which clears the TTL while returning exactly the right old value.
    ("DEL", "so:kt"),
    ("SET", "so:kt", "a", "EX", "100"),
    ("SET", "so:kt", "b", "KEEPTTL", "GET"),   # -> "a"
    ("TTL", "so:kt"),                          # -> 100, NOT -1
    ("GET", "so:kt"),                          # -> "b"

    # SET key value NX|XX EX|PX n (*6), the lock shape, in BOTH option orders -- the
    # parser has a separate branch for each order and only one of them can be exercised
    # by any single row. The load-bearing row is the TTL after a REFUSED NX: a route that
    # arms the expiry before evaluating the condition replies nil correctly, leaves the
    # value correctly untouched, and is caught ONLY by the TTL not having been re-armed.
    ("DEL", "so:lock"),
    ("SET", "so:lock", "v1", "NX", "EX", "100"),    # missing -> OK
    ("GET", "so:lock"),                             # -> v1
    ("TTL", "so:lock"),                             # -> 100
    ("SET", "so:lock", "v2", "NX", "EX", "200"),    # exists -> nil, nothing changes
    ("GET", "so:lock"),                             # -> v1
    ("TTL", "so:lock"),                             # -> 100, NOT 200
    ("SET", "so:lock", "v3", "XX", "PX", "50000"),  # exists -> OK
    ("GET", "so:lock"),                             # -> v3
    ("TTL", "so:lock"),                             # -> 50
    # option-value-pair first, condition last: the parser's second branch.
    ("DEL", "so:ord"),
    ("SET", "so:ord", "v", "EX", "100", "NX"),
    ("GET", "so:ord"),                              # -> v
    ("TTL", "so:ord"),                              # -> 100
    ("SET", "so:ord", "w", "PX", "50000", "XX"),    # exists -> OK
    ("GET", "so:ord"),                              # -> w
    ("TTL", "so:ord"),                              # -> 50
    ("DEL", "so:ord2"),
    ("SET", "so:ord2", "v", "PX", "100000", "XX"),  # missing -> nil, must NOT create
    ("EXISTS", "so:ord2"),                          # -> 0

    # Options are matched case-insensitively by every one of these parsers
    # (eq_ignore_ascii_case). If the generic path and a fast route ever disagreed on
    # case folding, only a lowercase row would show it.
    ("DEL", "so:cs"),
    ("SET", "so:cs", "v", "ex", "100"),
    ("TTL", "so:cs"),                               # -> 100
    ("SET", "so:cs", "w", "Px", "50000", "gEt"),    # -> "v"
    ("TTL", "so:cs"),                               # -> 50

    # Refusals. Every shape here must decline to the generic path and produce redis's
    # error text VERBATIM; a fast route that computes an expiry from a bad number, or
    # that admits a contradictory option pair, answers +OK where redis errors. The
    # EXISTS at the end is the control that ties them together: not one of these rows
    # is allowed to have written the key, so a route that errors AFTER writing -- which
    # every row-by-row reply check would pass -- is caught there.
    ("DEL", "so:err"),
    ("SET", "so:err", "v", "EX", "0"),
    ("SET", "so:err", "v", "EX", "-1"),
    ("SET", "so:err", "v", "PX", "0"),
    ("SET", "so:err", "v", "EX", "abc"),
    ("SET", "so:err", "v", "EXAT", "0"),
    ("SET", "so:err", "v", "NX", "EX", "0"),
    ("SET", "so:err", "v", "EX", "0", "NX"),
    ("SET", "so:err", "v", "NX", "XX"),
    ("SET", "so:err", "v", "EX", "100", "KEEPTTL"),
    ("EXISTS", "so:err"),                           # -> 0: none of the above wrote

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

def normalize(case, reply):
    """Mask the fields of a reply that CANNOT agree across three separate processes.

    Only SLOWLOG GET is affected. A slowlog entry is
    `:id, :unix_ts, :duration_us, [argv], client_addr, client_name` and two of those
    six are non-deterministic by construction: the duration in microseconds (three
    different engines never execute a command in the same number of microseconds --
    observed redis 5us / fr-fast 4us / fr-gen 2us for the same SET) and the client's
    ephemeral source port. Comparing them made `SLOWLOG GET 3` a PERMANENT red that
    had nothing to do with dispatch, which is worse than no row at all: the script
    exited 1 on every run, so a real disagreement anywhere else in the corpus was
    indistinguishable from the standing noise.

    Masking is deliberately narrow. The row exists (frankenredis-8xyox) to prove the
    hoisted metrics closure builds the correct ARGV, and id, timestamp, argv and
    client name all still compare exactly -- so the assertion the row was written to
    make is fully intact. Delete this function when SLOWLOG GET is no longer compared.
    """
    if len(case) >= 2 and case[0].upper() == "SLOWLOG" and case[1].upper() == "GET":
        reply = re.sub(r"(:\d+,:\d+,):\d+,", r"\1:DUR,", reply)
        reply = re.sub(r"127\.0\.0\.1:\d+", "ADDR", reply)
    return reply


bad = 0
print(f"{'case':<38} {'fr fast':<22} {'fr generic':<22} {'redis 7.2.4'}")
print("-" * 108)
for case in CASES:
    r = normalize(case, redis.cmd(*case))
    f = normalize(case, fast.cmd(*case))
    g = normalize(case, generic.cmd(*case))
    label = " ".join(case)
    ok = (f == g) and (f == r)
    if not ok:
        bad += 1
    mark = "" if ok else ("   <-- FAST != GENERIC" if f != g else "   <-- fr != 7.2.4")
    print(f"{label:<38} {f[:21]:<22} {g[:21]:<22} {r[:21]}{mark}")

print()
print(f"{len(CASES)} cases, {bad} disagreement(s)")
sys.exit(0 if bad == 0 else 1)
