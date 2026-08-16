#!/usr/bin/env python3
"""Exact instructions/op for one command shape, fr vs vendored redis 7.2.4.

(frankenredis-f99bu) Consumers: frankenredis-nscqs (BITOP), frankenredis-804l1
(3-source set stores), frankenredis-ozrro (borrowed dispatch cascade). Those beads
need a number that survives this host, where load routinely makes wall-clock
ratios inadmissible -- callgrind counts instructions deterministically, so the
same shape measured at load 18 and load 10 gives the same answer.

METHOD: two-point subtraction. Run the identical workload at N and 2N ops and
difference the whole-process totals, so process startup, seeding and teardown
cancel exactly. It does NOT use callgrind_control: this repo's memory records
`callgrind_control -z` perturbing vendored redis into dropping its client, and
per-frame attribution needs a few hundred ops before startup noise stops
dominating anyway.

BUILT-IN CONTROL: run `get_control` alongside whatever you are measuring. fr
retires 0.4645x redis's instructions on GET and is FASTER there, so a shape that
comes out above 1.0x is telling you something route-specific rather than a
whole-process handicap. A run that reports every shape as slow, control included,
is measuring the harness.

TRAP, measured rather than assumed: the instruction ratio is NOT the throughput
ratio, and the error is not even in a consistent direction. sinterstore_3src is
1.3456x instructions and ~1.37x slower (nearly 1:1, work-bound), while bitop_and
is 1.7883x instructions but only ~1.37x slower. Quote instr/op as instr/op; do
not project a throughput win from it.

REPRODUCIBILITY IS ASYMMETRIC, and it is the DENOMINATOR that moves. get_control
measured twice: fr 1341.5 then 1340.2 instr/op (0.1% apart), redis 2887.8 then
3118.0 (8% apart). The subtraction cancels work proportional to OP COUNT, not work
proportional to ELAPSED TIME, and redis's serverCron is the latter -- under
valgrind a run's duration varies, so its background work does not divide out. fr's
single-threaded loop has no comparable timer work, which is why its number is
nearly exact. Treat an fr/redis ratio from ONE pair of runs as carrying roughly
+/-8% on the redis side: fine for 1.35x or 1.79x, useless for adjudicating 1.05x.
Repeat the redis arm if the ratio you care about is close to 1.

Usage: shape_instr_per_op.py <fr_bin> <shape> [ops]   (--list for shapes)
"""
from __future__ import annotations

import os
import re
import socket
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")

SHAPES = {
    "sinterstore_3src": (
        ["SADD sa m1 m2 m3 m4 m5", "SADD sb m3 m4 m5 m6 m7", "SADD sc m4 m5 m6 m7 m8"],
        ["SINTERSTORE", "sidst", "sa", "sb", "sc"],
    ),
    "sunionstore_3src": (
        ["SADD sa m1 m2 m3 m4 m5", "SADD sb m3 m4 m5 m6 m7", "SADD sc m4 m5 m6 m7 m8"],
        ["SUNIONSTORE", "sudst", "sa", "sb", "sc"],
    ),
    "sdiffstore_3src": (
        ["SADD sa m1 m2 m3 m4 m5", "SADD sb m3 m4 m5 m6 m7", "SADD sc m4 m5 m6 m7 m8"],
        ["SDIFFSTORE", "sddst", "sa", "sb", "sc"],
    ),
    "bitop_and": (
        ["SET ba abcdefghijklmnop", "SET bb ponmlkjihgfedcba"],
        ["BITOP", "AND", "bdst", "ba", "bb"],
    ),
    "bitop_not": (["SET ba abcdefghijklmnop"], ["BITOP", "NOT", "bndst", "ba"]),
    # (frankenredis-o3t0q) Below its own control in two balanced-square runs
    # while sitting ABOVE 1.0 in raw terms -- the deficit only appears once the
    # whole-process advantage is divided out, which is why it needs the exact
    # instruction treatment rather than another wall-clock round.
    "pttl": (["SET bb abcdefghijklmnop", "PEXPIRE bb 900000000"], ["PTTL", "bb"]),
    "expiretime": (["SET kk vvvvvvvvvvvvvvvv", "EXPIREAT kk 4102444800"],
                   ["EXPIRETIME", "kk"]),
    # (frankenredis-o3t0q) SECOND control, and it exists to falsify the first.
    # get_control's keyspace has no TTLs at all, so fr's active-expire cycle has
    # nothing to scan; the pttl shape must plant a volatile key, and
    # run_active_expire_cycle showed up at 3.99% of PTTL. Reading a key that HAS a
    # TTL separates "the TTL read is expensive" from "a volatile key in the
    # keyspace is expensive", which the first control cannot do.
    "get_volatile_control": (
        ["SET vv abcdefghijklmnop", "PEXPIRE vv 900000000"], ["GET", "vv"]),
    # (frankenredis-hxgsz) The two worst raw-ratio routes found by the
    # `unswept` sweep. Both are TTL-adjacent WRITES, and both are measured here
    # rather than by wall clock because their nulls would not stand: persist_noop's
    # failing nulls point in OPPOSITE directions across two runs and setex_same's
    # confidence intervals do not overlap, so neither qualifies for null excusal.
    # Instruction counts need no null at all.
    "persist_noop": (["SET s abcdefghijklmnop"], ["PERSIST", "s"]),
    "setex_same": ([], ["SETEX", "wx", "100", "vvvvvvvvvvvvvvvv"]),
    # (frankenredis-iqicb) PSETEX is SETEX's millisecond sibling and sat beside it
    # in the same probe chain. Same shape so the two are directly comparable.
    "psetex_same": ([], ["PSETEX", "wy", "100000", "vvvvvvvvvvvvvvvv"]),
    # (frankenredis-iqicb) The remaining commands that already have a
    # parse_borrowed_plain_*_packet but no floor class. Screened on dispatch share
    # before any of them is touched -- that screen is what correctly excluded
    # RESTORE, whose share is 9.4%.
    # SETNX on an EXISTING key so the op is a no-op reply rather than a write that
    # grows the keyspace across the 2N run.
    "setnx_existing": (["SET nxk vvvvvvvvvvvvvvvv"], ["SETNX", "nxk", "wwww"]),
    # (frankenredis-l9wvl) The keyed-values writes at ONE value. The floor classifier
    # claims these only at array_len 7..=20 (5..18 values), so the single-value form
    # -- the one actually issued most -- falls through the whole probe chain. Each is
    # seeded to be a NO-OP at steady state so the 2N run does not grow the keyspace
    # relative to the N run, which would put real work into the slope and hide the
    # dispatch cost being measured.
    "sadd_existing": (["SADD sd1 m"], ["SADD", "sd1", "m"]),
    "srem_missing": (["SADD sr1 other"], ["SREM", "sr1", "m"]),
    "zrem_missing": (["ZADD zr1 1 other"], ["ZREM", "zr1", "m"]),
    "hdel_1_missing": (["HSET hd1 other v"], ["HDEL", "hd1", "f"]),
    "del_1_missing": ([], ["DEL", "nosuchkey1"]),
    # (frankenredis-l9wvl follow-up) The NO-COUNT pop forms. main.rs pins both as
    # NOT classified on the stated grounds that each "keeps its existing dedicated
    # route" -- the identical reasoning that was overturned for single-key DEL,
    # where the route existed but sat in the cascade so reaching it cost the walk.
    # Measured on a MISSING key so the op is a nil reply that mutates nothing:
    # popping a real list would drain it across the 2N run and put real work into
    # the slope, hiding the dispatch cost this is meant to isolate.
    "lpop_nocount_missing": ([], ["LPOP", "nosuchlist"]),
    "zpopmin_nocount_missing": ([], ["ZPOPMIN", "nosuchzset"]),
    "getset_same": (["SET gsk vvvvvvvvvvvvvvvv"], ["GETSET", "gsk", "vvvvvvvvvvvvvvvv"]),
    "lset_head": (["RPUSH lsk a b c d e f g h"], ["LSET", "lsk", "0", "a"]),
    "incrbyfloat_same": (["SET ibf 1.5"], ["INCRBYFLOAT", "ibf", "0"]),
    "set_same": ([], ["SET", "wk", "vvvvvvvvvvvvvvvv"]),
    # (frankenredis-mnzgy) The NO-OP / MISS family. PERSIST on a non-volatile key,
    # DEL and UNLINK on a key that does not exist: all three should early-return
    # almost free, and all three are among the worst routes measured. Whatever fr
    # pays before discovering there is nothing to do, it pays in full.
    "del_missing": ([], ["DEL", "nosuchkey"]),
    "unlink_missing": ([], ["UNLINK", "nosuchkey"]),
    "pexpire_same": (["SET s abcdefghijklmnop"], ["PEXPIRE", "s", "10000000"]),
    # (frankenredis-f9zmz) Worst rows of the third sweep, plus the pair that
    # makes them readable: TOUCH and EXISTS on the SAME missing key came out
    # 0.8730 and 1.0919, so any explanation has to account for both.
    "lset_same": (["RPUSH l a b c"], ["LSET", "l", "0", "a"]),
    "touch_missing": ([], ["TOUCH", "nosuchkey"]),
    "exists_missing": ([], ["EXISTS", "nosuchkey"]),
    # (frankenredis-c0ts5) Ladder shapes: cheap O(1) reads across every type, so
    # the dispatch cost can be compared at constant (near-zero) real work. Mirrors
    # the registrations in balanced_square_ab's unswept sets.
    "hget": (["HSET h f1 v1 f2 v2 f3 v3"], ["HGET", "h", "f2"]),
    "hlen": (["HSET h f1 v1 f2 v2 f3 v3"], ["HLEN", "h"]),
    "scard": (["SADD st m1 m2 m3 m4 m5"], ["SCARD", "st"]),
    "zcard": (["ZADD z 1 a 2 b 3 c"], ["ZCARD", "z"]),
    "type": (["SET s abcdefghijklmnop"], ["TYPE", "s"]),
    "strlen": (["SET s abcdefghijklmnop"], ["STRLEN", "s"]),
    "sismember": (["SADD st m1 m2 m3"], ["SISMEMBER", "st", "m2"]),
    "hexists": (["HSET h f1 v1"], ["HEXISTS", "h", "f1"]),
    "lindex": (["RPUSH l a b c d e"], ["LINDEX", "l", "2"]),
    "bitcount": (["SET bb abcdefghijklmnop"], ["BITCOUNT", "bb"]),
    "llen": (["RPUSH l a b c d e"], ["LLEN", "l"]),
    "ttl_nonvolatile": (["SET s abcdefghijklmnop"], ["TTL", "s"]),
    # (frankenredis-c0ts5) Boundary probes: writes and variadic-key commands, to
    # test what separates the cheap dispatch regime from the expensive one.
    "hdel_missing": (["HSET h f1 v1"], ["HDEL", "h", "nofield"]),
    "srem_missing": (["SADD st m1"], ["SREM", "st", "nomember"]),
    "getset_same": (["SET gs vvvvvvvvvvvvvvvv"], ["GETSET", "gs", "vvvvvvvvvvvvvvvv"]),
    "setbit_same": (["SET bb abcdefghijklmnop"], ["SETBIT", "bb", "5", "0"]),
    "get_missing": ([], ["GET", "nosuchkey"]),
    # (frankenredis-7xa4m) OUT-OF-SAMPLE routes. The 284.2*parses+69.3 fit was
    # made on 11 routes; these were not among them. Predict from the parse count
    # first, then measure, so the coefficient is tested rather than illustrated.
    "zrem_missing": (["ZADD z 1 a"], ["ZREM", "z", "nomember"]),
    "lrem_missing": (["RPUSH l a b c"], ["LREM", "l", "0", "nosuch"]),
    "memory_usage": (["SET s abcdefghijklmnop"], ["MEMORY", "USAGE", "s"]),
    "expire_same": (["SET s abcdefghijklmnop"], ["EXPIRE", "s", "10000"]),
    # (frankenredis-9tni0) Worst route measured in the campaign: 0.5717 and
    # 0.6251 across two sweeps. Attribute before choosing a lever -- dispatch has
    # been the answer four times and was NOT the answer for the TTL writes.
    "sort_ro_alpha": (["RPUSH sl c a b"], ["SORT_RO", "sl", "ALPHA"]),
    "geoadd_same": (["GEOADD g 13.361389 38.115556 P1"],
                    ["GEOADD", "g", "13.361389", "38.115556", "P1"]),
    "pfadd_same": (["PFADD hll a b c"], ["PFADD", "hll", "a"]),
    # (frankenredis-nkvkp) The routes ozrro's walked-vs-bypassed GAP rejected or
    # left alone. That metric compares the cascade against the GENERIC path and so
    # cannot see the front-classification prize -- it rejected PERSIST at -132/op
    # and front-classification then gave up 3326. Each of these needs a parse
    # count before anyone treats its rejection as settled.
    "hincrbyfloat": (["HSET h f 1"], ["HINCRBYFLOAT", "h", "f", "0"]),
    "hsetnx_existing": (["HSET h f1 v1"], ["HSETNX", "h", "f1", "other"]),
    "sinter_2": (["SADD s1 m1 m2 m3", "SADD s2 m2 m3 m4"], ["SINTER", "s1", "s2"]),
    "mget_3": (["MSET a 1 b 2 c 3"], ["MGET", "a", "b", "c"]),
    "pfadd_existing": (["PFADD hll a b c"], ["PFADD", "hll", "a"]),
    "pexpireat_same": (["SET s abcdefghijklmnop"],
                       ["PEXPIREAT", "s", "4102444800000"]),
    # (frankenredis-m6xu9) The third stranded member of the EXPIRE family. Same
    # arity-3 shape as expire_same and pexpireat_same so the four are directly
    # comparable; the point of the set is that EXPIRE is classified and these are
    # not. Absolute SECONDS, matching the parse_borrowed_plain_expireat_packet
    # route at main.rs:8771.
    "expireat_same": (["SET s abcdefghijklmnop"],
                      ["EXPIREAT", "s", "4102444800"]),
    # (frankenredis-ee41v) ZRANGEBYSCORE with LIMIT reads 0.7979 and 0.7924 while
    # the PLAIN form measured 1.2601 in the zsetreads sweep -- same command, one
    # option, opposite sides of parity. Attribute before assuming which mechanism.
    "zrangebyscore_l": (["ZADD z 1 a 2 b 3 c"],
                        ["ZRANGEBYSCORE", "z", "1", "3", "LIMIT", "0", "2"]),
    "zrangebyscore_plain": (["ZADD z 1 a 2 b 3 c"], ["ZRANGEBYSCORE", "z", "1", "3"]),
    "sintercard_lim": (["SADD s1 m1 m2 m3", "SADD s2 m2 m3 m4"],
                       ["SINTERCARD", "2", "s1", "s2", "LIMIT", "1"]),
    # (frankenredis-q4plk) BASE/OPTION pairs, to test whether the cliff found on
    # ZRANGEBYSCORE (3.0 -> 81.0 parses for two extra tokens) is general or is two
    # anecdotes. Each pair differs ONLY by the option, so the parse-count delta is
    # attributable to the option and nothing else.
    "expire_base": (["SET s abcdefghijklmnop"], ["EXPIRE", "s", "10000"]),
    "expire_nx_opt": (["SET s v", "EXPIRE s 10000"], ["EXPIRE", "s", "500", "NX"]),
    "zadd_base": (["ZADD z 1 a"], ["ZADD", "z", "1", "a"]),
    "zadd_xx_opt": (["ZADD z 1 a"], ["ZADD", "z", "XX", "1", "a"]),
    "sintercard_base": (["SADD s1 m1 m2 m3", "SADD s2 m2 m3 m4"],
                        ["SINTERCARD", "2", "s1", "s2"]),
    "hrandfield_base": (["HSET h f1 v1"], ["HRANDFIELD", "h"]),
    "hrandfield_count": (["HSET h f1 v1"], ["HRANDFIELD", "h", "1"]),
    "getex_base": (["SET gx abcdefghijklmnop"], ["GETEX", "gx"]),
    # (frankenredis-6iq5i) More BASE/OPTION pairs, widening the ranked list for the
    # family the front-classification lever structurally skips.
    "set_base": ([], ["SET", "sk", "vvvvvvvvvvvvvvvv"]),
    "set_ex_opt": ([], ["SET", "sk", "vvvvvvvvvvvvvvvv", "EX", "100"]),
    "set_xx_opt": (["SET sk v"], ["SET", "sk", "vvvvvvvvvvvvvvvv", "XX"]),
    "getex_base2": (["SET gx abcdefghijklmnop"], ["GETEX", "gx"]),
    "getex_ex_opt": (["SET gx abcdefghijklmnop"], ["GETEX", "gx", "EX", "100"]),
    "lpos_base": (["RPUSH l a b c d e"], ["LPOS", "l", "c"]),
    "lpos_count_opt": (["RPUSH l a b c d e"], ["LPOS", "l", "c", "COUNT", "1"]),
    "bitcount_base": (["SET bb abcdefghijklmnop"], ["BITCOUNT", "bb"]),
    "bitcount_range": (["SET bb abcdefghijklmnop"], ["BITCOUNT", "bb", "0", "5"]),
    # The control: a route none of the above levers touch.
    "get_control": (["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
}


def resp(*args) -> bytes:
    out = b"*%d\r\n" % len(args)
    for a in args:
        a = a if isinstance(a, bytes) else str(a).encode()
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


def free_port() -> int:
    probe = socket.socket()
    try:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]
    finally:
        probe.close()


class ReplyCounter:
    """Count COMPLETE top-level RESP replies in a byte stream.

    (frankenredis-58dp8) This exists because the burst loop used to count
    `chunk.count(b"\\r\\n")` and treat every CRLF as one finished op. That is only
    true for single-line replies. `SORT_RO sl ALPHA` on a three-element list
    answers `*3\\r\\n$1\\r\\na\\r\\n$1\\r\\nb\\r\\n$1\\r\\nc\\r\\n` -- SEVEN CRLFs for ONE op -- so
    the loop believed the burst was done after roughly a seventh of it, and the
    `finally` block then terminated the engine while the rest was still in flight.
    The dump that got written covered however much the engine happened to finish
    first, which is a race against process teardown rather than a measurement.

    That silently corrupted the two-point subtraction. Observed on sort_ro_alpha
    at N=6000: Ir(N)=157,246,050 with Ir(2N)=166,702,190, which the old guard
    passed because it only refused Ir(2N) <= Ir(N) -- and the harness printed
    `1576.0 instr/op` and `0.9769x` for a route whose fr arm is ~26,100 instr/op.
    A second run of the same pair printed `826.3` and `0.3481x`. The SAME defect
    also produced honest-looking hard failures (Ir(2N)=48,463,918 against
    Ir(N)=157,032,994), so the loud and the silent cases share one root cause.

    Every shape whose reply is not a single line was affected: sort_ro_alpha,
    mget_3, hmget/zmscore-style multi-bulk, sinter_2. Single-line shapes (GET,
    integers, +OK) were counted correctly, which is why this went unnoticed.

    Handles the RESP2 surface these shapes produce: `+`/`-`/`:` inline, `$` bulk
    (including the `$-1` null), and `*` multibulk (including `*-1` and nesting).
    """

    def __init__(self):
        self.buf = b""
        self.complete = 0

    def feed(self, chunk: bytes) -> None:
        self.buf += chunk
        while True:
            consumed = self._one(self.buf)
            if consumed is None:
                return
            self.buf = self.buf[consumed:]
            self.complete += 1

    def _one(self, buf: bytes):
        """Bytes consumed by one complete reply at the head of `buf`, else None."""
        end = buf.find(b"\r\n")
        if end < 0:
            return None
        tag, head = buf[:1], buf[1:end]
        if tag in (b"+", b"-", b":"):
            return end + 2
        if tag == b"$":
            length = int(head)
            if length < 0:
                return end + 2
            need = end + 2 + length + 2
            return need if len(buf) >= need else None
        if tag == b"*":
            count = int(head)
            if count < 0:
                return end + 2
            offset = end + 2
            for _ in range(count):
                inner = self._one(buf[offset:])
                if inner is None:
                    return None
                offset += inner
            return offset
        raise RuntimeError("unparseable RESP tag %r" % tag)


def total_ir(path: str) -> int:
    """Whole-process Ir from the callgrind summary line."""
    with open(path, "r", errors="replace") as fh:
        for line in fh:
            if line.startswith(("summary:", "totals:")):
                return int(line.split()[1])
    raise RuntimeError("no summary line in %s" % path)


# (frankenredis-rzdi8) Frames that are "getting to the command" rather than
# doing it. Kept explicit rather than inferred: the borrowed parser family is the
# whole point, since an unclassified command attempts several of them against a
# packet that is none of them before falling through to the generic path.
DISPATCH_FRAMES = (
    "process_buffered_frames", "execute_frame_internal", "command_table_index",
    "dispatch_with_client_context", "classify_command", "push_ascii_lowercase_lossy",
    "check_full_command_arity", "execute_dispatch", "parse_command_args_borrowed_into",
    "try_dispatch_floor_classified_action", "parse_borrowed_plain_",
    "effective_command_flags", "canonical_command_fullname",
    # The first version of this list stopped above and UNDERCOUNTED the generic
    # path, which is the path it exists to flag. Differencing UNLINK against DEL
    # frame by frame surfaced four more that only appear once a command misses the
    # borrowed floor: dispatch_argv (+104 instr/op), acl_permission_error_for_argv
    # (+94), borrowed_fast_route_key (+92) and the Utf8Chunks iterator (+132) that
    # push_ascii_lowercase_lossy drives. Together they were 422 instr/op of
    # dispatch reported as if it were work.
    "dispatch_argv", "acl_permission_error_for_argv", "borrowed_fast_route_key",
    "Utf8Chunks", "resolve_command_spec", "lookup_command",
)


def dispatch_share(dump_path):
    """What fraction of a command is spent deciding WHICH command it is.

    Check this BEFORE reaching for a front-classification lever. Measured shares
    so far: a front-classified route (EXISTS on a missing key) sits at 21.5%,
    while unclassified ones sit at 62-66% AND carry 8-14x the absolute dispatch
    cost. A route can also be below parity with dispatch NOT the story at all --
    PEXPIRE is 1.04x on instructions with a 0.90 throughput ratio, so no dispatch
    lever can help it. Assuming instead of checking gets that case wrong.
    """
    out = subprocess.run(["callgrind_annotate", "--auto=no", "--threshold=99.5", dump_path],
                         capture_output=True, text=True, timeout=900).stdout
    disp = attributed = 0
    top = []
    for line in out.splitlines():
        m = re.match(r"\s*([\d,]+) \(\s*[\d.]+%\)\s+(?:\?\?\?|[^\s]+):(.+?) \[", line)
        if not m:
            continue
        ir, fn = int(m.group(1).replace(",", "")), m.group(2).strip()
        attributed += ir
        if any(d in fn for d in DISPATCH_FRAMES):
            disp += ir
            top.append((ir, fn))
    if not attributed:
        return None
    return disp / attributed, sorted(top, reverse=True)[:5]


# (frankenredis-8280l) The FULL generic set. Presence of ALL of these together is
# the reliable sign that a command reaches its executor through the generic path
# rather than the classified route.
#
# This replaces a discriminator I used and was wrong about. I previously tested
# for `execute_plain_<cmd>_borrowed` in the profile and called that structural
# rather than fitted. It is neither: those handlers EXIST in source for every
# route I called handler-less, and are absent from the profile only because they
# are INLINED. No symbol pattern can fix that -- a profile cannot tell you an
# inlined function exists. The generic frames, by contrast, are real call sites
# that show up when they are taken.
#
# MEASURED (frankenredis-94lp3): the discriminating frame is dispatch_with_client_context
# ALONE. Across eight routes it is present in exactly the two on the generic path and
# absent everywhere else, including routes paying 2686-9755 of dispatch through the
# WALK -- so it separates mechanism from magnitude. The other three frames here appear
# in classified routes too (HGET shows three of them, PERSIST four) and carry no
# information; they are kept only so the printout shows what was seen.
GENERIC_PATH_FRAMES = (
    "execute_frame_internal",
    "dispatch_with_client_context",
    "command_table_index",
)
GENERIC_PATH_MARKERS = ("classify_command", "push_ascii_lowercase_lossy")


def dispatch_mechanism(dump_path):
    """Which mechanism is this route paying: the parser walk, or the generic path?

    Returns (label, frames_found). The caller still needs the parse count: a route
    can pay the walk, the generic path, both, or neither.
    """
    out = subprocess.run(["callgrind_annotate", "--auto=no", "--threshold=99.5", dump_path],
                         capture_output=True, text=True, timeout=900).stdout
    present = {f for f in GENERIC_PATH_FRAMES if f in out}
    markers = {m for m in GENERIC_PATH_MARKERS if m in out}
    if len(present) == len(GENERIC_PATH_FRAMES) and markers:
        return "GENERIC PATH", sorted(present | markers)
    return "classified route", sorted(present | markers)


def run_once(engine: str, seeds, cmd, ops: int, workdir: str, tag: str,
             locale: str | None = None) -> int:
    out = os.path.join(workdir, "cg.%s.out" % tag)
    port = free_port()
    argv = ["valgrind", "--tool=callgrind", "--callgrind-out-file=" + out,
            "--cache-sim=no", "--branch-sim=no",
            engine, "--port", str(port), "--save", "", "--appendonly", "no"]
    # cwd=workdir: never boot an engine in the repo root, which is shared and may
    # hold a dump.rdb redis refuses to load (frankenredis-7afsd).
    # (frankenredis-3f7jb) Both engines must be pinned to the SAME locale for a
    # SORT ALPHA row to mean anything: redis byte-compares under C and calls
    # strcoll under a UTF-8 locale, and fr does the same by design (jaezc). An
    # unpinned harness compares whatever each inherited.
    env = None
    if locale:
        env = dict(os.environ, LC_ALL=locale, LC_COLLATE=locale, LANG=locale)
    proc = subprocess.Popen(argv, cwd=workdir, env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sock = None
    try:
        for _ in range(600):
            if proc.poll() is not None:
                raise RuntimeError("%s exited during startup rc=%s" % (tag, proc.returncode))
            try:
                sock = socket.create_connection(("127.0.0.1", port), timeout=5)
                sock.settimeout(300)
                sock.sendall(resp("PING"))
                if b"PONG" in sock.recv(64):
                    break
                sock.close()
                sock = None
            except OSError:
                time.sleep(0.25)
        if sock is None:
            raise RuntimeError("%s never became ready under callgrind" % tag)
        # (frankenredis-58dp8) Seeds are drained by REPLY, not by one recv(): a
        # seed whose reply arrives in two segments used to leave the tail in the
        # socket, where the burst loop then counted it as burst progress.
        for seed in seeds:
            sock.sendall(resp(*seed.split()))
            seed_counter = ReplyCounter()
            while seed_counter.complete < 1:
                chunk = sock.recv(1 << 20)
                if not chunk:
                    raise RuntimeError("%s dropped the connection while seeding" % tag)
                seed_counter.feed(chunk)
        sock.sendall(resp(*cmd) * ops)
        # (frankenredis-58dp8) Wait for `ops` COMPLETE replies. See ReplyCounter
        # for what counting CRLFs instead did to every multi-line-reply shape.
        counter = ReplyCounter()
        while counter.complete < ops:
            chunk = sock.recv(1 << 20)
            if not chunk:
                raise RuntimeError(
                    "%s dropped the connection mid-burst after %d of %d replies"
                    % (tag, counter.complete, ops))
            counter.feed(chunk)
    finally:
        if sock is not None:
            sock.close()
        proc.terminate()
        try:
            proc.wait(timeout=180)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=60)
    return total_ir(out)


def instr_per_op(engine: str, seeds, cmd, ops: int, workdir: str, label: str,
                 locale: str | None = None):
    low = run_once(engine, seeds, cmd, ops, workdir, label + ".n", locale)
    high = run_once(engine, seeds, cmd, ops * 2, workdir, label + ".2n", locale)
    delta = high - low
    # (frankenredis-3f7jb) Two-point subtraction assumes the 2N run does strictly
    # more work than the N run. When a command carries large or VARIABLE one-time
    # initialisation -- SORT ALPHA under a UTF-8 locale loads ICU data on first use
    # -- that can fail, and it failed here: a run produced Ir(2N) < Ir(N) and the
    # harness cheerfully printed "-1.0112x", then on a retry "-479.7188x". A ratio
    # with a negative or implausibly small numerator is not a measurement, and
    # printing one is worse than refusing, because it looks like a result.
    if delta <= 0:
        raise SystemExit(
            "%s: Ir(2N)=%d is NOT greater than Ir(N)=%d. The two-point subtraction "
            "is invalid for this shape -- it has one-time work that did not cancel. "
            "Re-run; if it persists, the shape needs a larger N or a warm-up."
            % (label, high, low))
    if delta < low * 0.01:
        raise SystemExit(
            "%s: Ir(2N)-Ir(N)=%d is under 1%% of Ir(N)=%d, so startup dominates and "
            "the per-op figure is noise. Raise N." % (label, delta, low))
    return delta / ops, low, high


def selftest() -> int:
    """Prove the reply counter on the streams that broke the old CRLF count.

    Each case carries the count the OLD `chunk.count(b"\\r\\n")` would have
    produced, so the test shows the defect rather than only asserting the fix:
    a case where the two agree proves nothing, and every multi-line case is one
    where the old code overcounted and stopped the burst early.
    """
    sort_reply = b"*3\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n"
    cases = [
        ("inline +OK", b"+OK\r\n", 1, 1),
        ("integer", b":1\r\n", 1, 1),
        ("error", b"-ERR nope\r\n", 1, 1),
        ("bulk", b"$3\r\nabc\r\n", 1, 2),
        ("null bulk", b"$-1\r\n", 1, 1),
        ("null array", b"*-1\r\n", 1, 1),
        # The shape that exposed this: SEVEN CRLFs, ONE reply.
        ("SORT_RO 3 elements", sort_reply, 1, 7),
        ("SORT_RO x2", sort_reply * 2, 2, 14),
        ("nested array", b"*2\r\n*1\r\n$1\r\na\r\n:7\r\n", 1, 4),
        # A bulk payload containing CRLF: old code counted the DATA as replies.
        ("bulk with embedded CRLF", b"$4\r\na\r\nb\r\n", 1, 3),
    ]
    failures = 0
    for label, stream, expect, old_would_say in cases:
        counter = ReplyCounter()
        counter.feed(stream)
        # Byte-at-a-time proves the counter survives arbitrary TCP segmentation,
        # which is the condition the burst loop actually runs under.
        split = ReplyCounter()
        for i in range(len(stream)):
            split.feed(stream[i:i + 1])
        ok = counter.complete == expect and split.complete == expect
        if not ok:
            failures += 1
        print("  %-26s replies=%-3d split=%-3d expect=%-3d  old CRLF count=%-3d  %s"
              % (label, counter.complete, split.complete, expect, old_would_say,
                 "ok" if ok else "FAIL"))
    # A truncated reply must NOT count: this is what made the burst loop stop early.
    partial = ReplyCounter()
    partial.feed(sort_reply[:-4])
    if partial.complete != 0:
        failures += 1
        print("  %-26s FAIL: counted an incomplete reply" % "truncated reply")
    else:
        print("  %-26s replies=0 (correctly withheld until complete)  ok" % "truncated reply")
    print("selftest: %d case(s) failed" % failures)
    return 1 if failures else 0


def main() -> int:
    args = sys.argv[1:]
    if "--selftest" in args:
        return selftest()
    if "--list" in args:
        print("shapes: %s" % ", ".join(sorted(SHAPES)))
        return 0
    if len(args) < 2 or args[1] not in SHAPES:
        print("usage: shape_instr_per_op.py <fr_bin> <shape> [ops]   (--list for shapes)",
              file=sys.stderr)
        return 2
    fr_bin = os.path.abspath(args[0])
    shape = args[1]
    # (frankenredis-c0ts5) --fr-only skips the incumbent arm. The dispatch
    # ladder needs fr's own instr/op and dispatch share, not a ratio, and the
    # redis arm is half the wall-clock of every measurement (and the noisy half,
    # at ~8%). Building a ladder across a dozen commands is the case for it.
    fr_only = "--fr-only" in args
    locale = None
    for a in args:
        if a.startswith("--locale="):
            locale = a.split("=", 1)[1]
    ops = int(args[2]) if len(args) > 2 else 2000
    seeds, cmd = SHAPES[shape]
    workdir = tempfile.mkdtemp(prefix="fr_instr_")
    if locale:
        print("  both engines pinned to LC_ALL=%s" % locale)
    fr_ipo, fr_lo, fr_hi = instr_per_op(fr_bin, seeds, cmd, ops, workdir, "fr", locale)
    if fr_only:
        got = dispatch_share(os.path.join(workdir, "cg.fr.2n.out"))
        frac = got[0] if got else float("nan")
        print("LADDER %-18s fr %8.1f instr/op   dispatch %8.1f (%.1f%%)"
              % (shape, fr_ipo, fr_ipo * frac, 100 * frac))
        label, frames = dispatch_mechanism(os.path.join(workdir, "cg.fr.2n.out"))
        print("  mechanism: %s  (generic frames seen: %s)"
              % (label, ", ".join(frames) if frames else "none"))
        print("  callgrind dumps: %s" % workdir)
        return 0
    rd_ipo, rd_lo, rd_hi = instr_per_op(REDIS, seeds, cmd, ops, workdir, "redis", locale)
    print("shape %s   N=%d 2N=%d" % (shape, ops, ops * 2))
    print("  fr     Ir(N)=%-14d Ir(2N)=%-14d -> %10.1f instr/op" % (fr_lo, fr_hi, fr_ipo))
    print("  redis  Ir(N)=%-14d Ir(2N)=%-14d -> %10.1f instr/op" % (rd_lo, rd_hi, rd_ipo))
    print("  fr/redis instructions per op: %.4fx" % (fr_ipo / rd_ipo))
    got = dispatch_share(os.path.join(workdir, "cg.fr.2n.out"))
    if got:
        frac, top = got
        print("  fr dispatch share: %.1f%%  (~%.1f of %.1f instr/op deciding WHICH command)"
              % (100 * frac, fr_ipo * frac, fr_ipo))
        for ir, fn in top:
            print("      %10d  %s" % (ir, fn[:66]))
        print("  compare: a front-classified route (EXISTS on a missing key) is 21.5%;"
              " 62-66% means the dispatch lever has something to bite on.")
    print("  callgrind dumps: %s" % workdir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
