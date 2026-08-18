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

import hashlib
import os
import re
import select
import shutil
import socket
import difflib
import subprocess

import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _incumbent import (  # noqa: E402  (sys.path set immediately above)
    check_incumbent_provenance as _check_incumbent,
    incumbent_provenance,
)

# (frankenredis-eh2ct) Cache simulation is OFF by default: it roughly triples
# callgrind's runtime, and every existing row was taken without it. `--cache-sim`
# turns it on for a stall investigation. Simulated D1/LL misses are DETERMINISTIC, so
# unlike an IPC census they can be taken on a contended host — which is the entire
# reason this option exists.
#
# CALIBRATION, measured against hardware so the next user does not over-read a ratio.
# On GEOSEARCH the simulator reported fr taking 3.6855x redis's L1 read misses.
# `perf stat` on the same shape, both engines back to back in one window, reported
# 1.4722x on misses per op and 1.5698x on miss RATE (3.02% vs 1.92%), with IPC 1.387
# vs 1.818. So the DIRECTION was right and the MAGNITUDE was overstated by about 2.5x.
# Treat a simulated ratio as a sign and a rank, never as a size — the sibling proxy
# `--branch-sim` was worse still, coming out at parity where hardware showed 6x, which
# is why it stays off permanently below.
CACHE_SIM = [False]

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")


# (frankenredis-sf510) EVALSHA shape bodies and their SHA1s. The sha is computed at import so
# the shape's argv can be a literal, which is what the harness requires; it is the sha1 of the
# exact bytes handed to SCRIPT LOAD, so a mismatch would surface immediately as NOSCRIPT rather
# than as a quiet mismeasurement.
_EVALSHA_BODY_SMALL = "return 1"
# Padding is a Lua COMMENT: it grows the bytes the cache key spans without adding any work for
# the interpreter to do, which is exactly the variable under test.
_EVALSHA_BODY_LARGE = "return 1 --" + ("x" * 4000)


def _sha1_hex(body):
    import hashlib
    return hashlib.sha1(body.encode()).hexdigest()

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
    # (frankenredis-dzik2) LPUSHX / RPUSHX. The keyed-values PARSER at main.rs:25694
    # serves nine commands; the floor classifier's matches! arm lists SIX. PFADD,
    # LPUSHX and RPUSHX are the three it omits, so all three are stranded in the
    # cascade despite the machinery already handling them. Measured on a MISSING key:
    # both are no-ops there (reply 0, create nothing), so the 2N run does not grow the
    # keyspace and the slope isolates dispatch rather than real work.
    "lpushx_missing": ([], ["LPUSHX", "nosuchlist", "v"]),
    # (frankenredis-dzik2 follow-up) The arity-5 MIS-CLAIM shapes QuietHarbor found.
    # main.rs:16122 maps (5, Lpos) -> LposRank and :16119 maps (5, Zrange) -> a
    # WITHSCORES-shaped class. But arity 5 does not imply RANK or WITHSCORES: the
    # option KEYWORD is what discriminates, and the classifier cannot see it. So
    # LPOS k e COUNT n and ZRANGE k s e REV are claimed by arms whose parser will
    # decline -- and a floor decline falls through to GENERIC, not back to the
    # cascade. These pair with the RANK/WITHSCORES forms the arms DO serve, so the
    # two can be compared directly on one binary.
    "lpos_count": (["RPUSH lp a b c d e f g h"], ["LPOS", "lp", "e", "COUNT", "1"]),
    # (frankenredis-uu33c) GETEX at arity 4. main.rs:16223 claims EVERY arity-4 GETEX
    # as GetexExpire, but that arm tests only EX and PX (19167-19168). EXAT and PXAT
    # are equally arity 4, so they are claimed and then DECLINED -- and a floor
    # decline falls through to GENERIC rather than back to the cascade. EX is the
    # form the arm serves and is the control.
    "getex_ex":   (["SET gx vvvvvvvvvvvvvvvv"], ["GETEX", "gx", "EX", "10000"]),
    "getex_exat": (["SET gx2 vvvvvvvvvvvvvvvv"], ["GETEX", "gx2", "EXAT", "4102444800"]),
    "getex_pxat": (["SET gx3 vvvvvvvvvvvvvvvv"], ["GETEX", "gx3", "PXAT", "4102444800000"]),
    "lpos_rank": (["RPUSH lp2 a b c d e f g h"], ["LPOS", "lp2", "e", "RANK", "1"]),
    # (frankenredis-ozrro) PLAIN ZRANGE — no WITHSCORES/REV/LIMIT. The existing zrange_rev and
    # zrange_withscores shapes exercise OPTION forms, which that floor arm's own comment says fall
    # through to the generic path, so neither could measure a change to the plain arm. I nearly
    # measured the wrong shape and reported a null.
    "zrange_plain": (["ZADD zp 1 a 2 b 3 c"], ["ZRANGE", "zp", "0", "-1"]),
    "zrange_rev": (["ZADD zr 1 a 2 b 3 c"], ["ZRANGE", "zr", "0", "-1", "REV"]),
    "zrange_withscores": (["ZADD zr2 1 a 2 b 3 c"],
                          ["ZRANGE", "zr2", "0", "-1", "WITHSCORES"]),
    "rpushx_missing": ([], ["RPUSHX", "nosuchlist", "v"]),
    "zpopmin_nocount_missing": ([], ["ZPOPMIN", "nosuchzset"]),
    "getset_same": (["SET gsk vvvvvvvvvvvvvvvv"], ["GETSET", "gsk", "vvvvvvvvvvvvvvvv"]),
    "lset_head": (["RPUSH lsk a b c d e f g h"], ["LSET", "lsk", "0", "a"]),
    "incrbyfloat_same": (["SET ibf 1.5"], ["INCRBYFLOAT", "ibf", "0"]),
    # (frankenredis-iqicb) The NON-DYADIC counterpart, and the reason it exists is that
    # `incrbyfloat_same` measures the exact-decimal fast path's BEST case and cannot
    # measure anything else.
    #
    # That path is exact only when 5^|exp| divides the significand -- i.e. only for
    # DYADIC rationals (halves, quarters, eighths). 1.5 is 3/2 and qualifies. Ordinary
    # money-shaped values do NOT: 0.1 = 1/10, 0.01 = 1/100 and 3.14 = 157/50 all keep a
    # factor of 5 in the denominator, have no exact binary value, and take the unchanged
    # bignum path.
    #
    # So the pair isolates the fast path itself with everything else held: same command,
    # same arity, same executor, same reply shape -- only the operand's representability
    # differs. Quoting incrbyfloat_same alone reports the ceiling as though it were the
    # average.
    #
    # 0.3333333333333333 is 3333333333333333 * 10^-16 and 5^16 does not divide it, so the
    # fast path must decline. Incrementing by 0 keeps the stored value fixed across ops
    # so every iteration is identical, matching incrbyfloat_same's construction.
    "incrbyfloat_nondyadic": (
        ["SET ibfnd 0.3333333333333333"],
        ["INCRBYFLOAT", "ibfnd", "0"],
    ),
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
    # (frankenredis-p98mw) MULTI-KEY TOUCH, the arity-3 form, paired with touch_missing
    # (arity 2) so the two differ ONLY by key count.
    #
    # This exists because of a mistake in my own lever. I front-classified TOUCH at
    # `(2, Touch)` EXACTLY, on the stated reasoning that only the single-key form had a
    # borrowed parser. A later source sweep found parser call sites pinning TOUCH at
    # arities 3, 4 and 5 as well, so the exact-arity claim left those three stranded.
    # There was no shape that could see it: touch_missing is arity 2 and measures the
    # form I DID classify, so it reported healthy while three siblings walked the
    # cascade. A lever's own shape cannot detect the shapes the lever excluded.
    "touch_2": (["SET tk1 v", "SET tk2 v"], ["TOUCH", "tk1", "tk2"]),
    # (frankenredis-p98mw) MSETNX at the arity I CLAIMED (3) and the arity I EXCLUDED
    # (5), the same pairing that exposed multi-key TOUCH at 3.2848x.
    #
    # I front-classified MSETNX at `(3, Msetnx)` exactly, reasoning that only the
    # one-pair form had a borrowed parser. No shape existed for either arity, so that
    # claim has never been measured and neither has its exclusion. A source sweep
    # separately flagged MSETNX arity 5 as having a parser reachable only from the
    # cascade.
    #
    # Both seeded so mk1 already exists: MSETNX is all-or-nothing, so every op returns
    # 0 and writes nothing, making each iteration identical. Without that the first op
    # would succeed and the rest fail, measuring two different paths in one average.
    "msetnx_1": (["SET mk1 v"], ["MSETNX", "mk1", "mv1"]),
    "msetnx_2": (["SET mk1 v"], ["MSETNX", "mk1", "mv1", "mk2", "mv2"]),
    # (frankenredis-p98mw) The TWO commands of that bead's six that are still stranded --
    # MSETNX, GEOADD, LMPOP and TOUCH have since been classified, which the bead does not
    # say. Both were UNMEASURED rather than cheap: neither had a shape, so nothing in the
    # corpus could see their cascade walk.
    #
    # Both deliberately take the MISS path, so the reply is stable across every iteration
    # and the shape cannot drift into measuring a first-request-only effect (the `copy`
    # no-op that `shape_work_audit.py` exists to catch). MOVE on a present key would
    # succeed once and then return 0 forever, which is exactly that trap.
    "move_missing": ([], ["MOVE", "nosuchkey", "1"]),
    "spublish_nosub": ([], ["SPUBLISH", "shardchan", "m"]),
    "exists_missing": ([], ["EXISTS", "nosuchkey"]),

    # (frankenredis-copydeficit, third sweep) EXISTS multi-key. The stranded-parser sweep
    # puts exists_two..exists_eight at cascade positions 160-166 of 166 -- the LAST SEVEN
    # ARMS WALKED -- while no class entry claims arities 3..9. exists_1 is the in-family
    # control: same command, same executor family, but a different (claimed) route, so a
    # difference between it and exists_2 is the routing and not the key count.
    # ZRANGEBYLEX/ZREVRANGEBYLEX base arity 4 at cascade positions 141 and 140 of 166, and
    # neither name appears in the floor name table AT ALL, so they can never be classified.
    # zrange_4 is the control: the same arity on a sibling zset range command that IS
    # classified, so the pair isolates routing from range work.
    "zrangebylex_4": (["ZADD zl 0 a 0 b 0 c 0 d"], ["ZRANGEBYLEX", "zl", "-", "+"]),
    "zrevrangebylex_4": (["ZADD zl 0 a 0 b 0 c 0 d"], ["ZREVRANGEBYLEX", "zl", "+", "-"]),
    "zrange_4": (["ZADD zl 0 a 0 b 0 c 0 d"], ["ZRANGE", "zl", "0", "-1"]),

    "exists_1": (["SET ek1 v"], ["EXISTS", "ek1"]),
    "exists_2": (["SET ek1 v", "SET ek2 v"], ["EXISTS", "ek1", "ek2"]),
    "exists_4": (["SET ek1 v", "SET ek2 v", "SET ek3 v", "SET ek4 v"],
                 ["EXISTS", "ek1", "ek2", "ek3", "ek4"]),
    "exists_8": (["SET ek%d v" % i for i in range(1, 9)],
                 ["EXISTS"] + ["ek%d" % i for i in range(1, 9)]),
    # (frankenredis-c0ts5) Ladder shapes: cheap O(1) reads across every type, so
    # the dispatch cost can be compared at constant (near-zero) real work. Mirrors
    # the registrations in balanced_square_ab's unswept sets.
    "hget": (["HSET h f1 v1 f2 v2 f3 v3"], ["HGET", "h", "f2"]),
    "hlen": (["HSET h f1 v1 f2 v2 f3 v3"], ["HLEN", "h"]),
    "scard": (["SADD st m1 m2 m3 m4 m5"], ["SCARD", "st"]),
    "zcard": (["ZADD z 1 a 2 b 3 c"], ["ZCARD", "z"]),
    "type": (["SET s abcdefghijklmnop"], ["TYPE", "s"]),
    # (frankenredis-iqicb) ZERO-DOSE BASELINES. `PING` takes no key and no argument, so it
    # never reaches `parse_borrowed_plain_set_bulk` and never touches the store: it is the
    # null BY CONSTRUCTION for any key-, argument- or store-side lever, and the cheapest
    # command on the board for reading the fixed per-command floor. `echo_arg` is the
    # one-argument sibling -- it parses a bulk argument but still touches no key -- which
    # separates "parses an argument" from "looks something up".
    "ping": ([], ["PING"]),
    "echo_arg": ([], ["ECHO", "abcdefgh"]),
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
    # (frankenredis-z2ce3) The THREE-element sibling above cannot see any lever whose cost
    # is per-element and whose saving is per-COMPARISON: at n=3 a sort does ~3 comparisons,
    # so "n log n comparisons" and "n elements" are the same number and a decorate-style
    # change is pure overhead. This 64-element variant separates them (~350 comparisons
    # against 64 elements). Mixed case so collation, not byte order, decides.
    "sort_ro_alpha_64": (
        ["RPUSH sl64 " + " ".join(f"w{i:02d}{'Ab'[i % 2]}" for i in range(64))],
        ["SORT_RO", "sl64", "ALPHA"],
    ),
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
    # (frankenredis-gein3 / frankenredis-ozrro) THE COMMANDS THE CORPUS COULD NOT SEE.
    # The dispatch screen ranks the shapes the harness HAS, and these are unclassified in
    # `borrowed_dispatch_floor_command` AND had no shape at all, so every "the
    # front-classification surface is ~one command" reading was drawn from a corpus that
    # excluded them. That is not the same as their being cheap: this ledger records the
    # LTRIM cascade walk at 15,736 instr/op, five times the largest dispatch cost in the
    # whole ranked table.
    #
    # Each is seeded to be a NO-OP AT STEADY STATE, because the two-point subtraction
    # requires the 2N run to do the same work per op as the N run. A shape that mutates
    # puts real work into the slope and hides the dispatch cost being measured:
    #   ltrim_noop      keeps the whole list (0 -1), so nothing is removed
    #   spop_missing    missing key -> nil, creates nothing
    #   srandmember_1   read-only by definition
    #   smove_missing   member absent from the source -> 0, moves nothing
    #   rpoplpush_missing  missing source -> nil, both keys untouched
    #   hset_same       field already holds this value -> reply 0, no growth
    "ltrim_noop": (["RPUSH tl a b c d e"], ["LTRIM", "tl", "0", "-1"]),
    # (frankenredis-ozrro) FROM THE BLIND-SPOT LIST, which scripts/corpus_coverage.py now
    # computes: 78 of 218 commands are BOTH unclassified at the floor AND issued by no
    # shape. That is not "fine", it is UNKNOWN — the previous six drawn from this list
    # produced three routes at 1.43x-1.48x, one the largest dispatch cost ever measured
    # here. These four are the highest-traffic shapeable entries left.
    #
    # Steady-state no-ops, as the two-point subtraction requires:
    #   mset_2              rewrites the values already present -> reply +OK, no growth
    #   hincrby_zero        increments by 0 -> field value unchanged
    #   zremrangebyscore_none  score window holds no member -> removes nothing
    #   zrandmember_1       read-only by definition
    "mset_2": (["MSET mk1 v1 mk2 v2"], ["MSET", "mk1", "v1", "mk2", "v2"]),
    "hincrby_zero": (["HSET hib f 5"], ["HINCRBY", "hib", "f", "0"]),
    "zremrangebyscore_none": (["ZADD zrs 1 a 2 b"],
                              ["ZREMRANGEBYSCORE", "zrs", "100", "200"]),
    "zrandmember_1": (["ZADD zrm 1 a 2 b 3 c"], ["ZRANDMEMBER", "zrm"]),
    # (frankenredis-ozrro) Second batch from the blind-spot list, chosen by traffic. The
    # first batch found HINCRBY at 1.2944x with a complete borrowed fast path and no floor
    # entry, so this is the cheapest known way to find the next one.
    #
    # Steady-state no-ops again:
    #   zrangebylex / zlexcount / zrevrangebyscore / substr  read-only by definition
    #   zremrangebyrank_none  rank window past the end -> removes nothing
    #   renamenx_exists       destination already exists -> reply 0, renames nothing
    #   zunionstore_2         destination is rewritten with the SAME union every time
    "zrangebylex": (["ZADD zbl 0 a 0 b 0 c"], ["ZRANGEBYLEX", "zbl", "[a", "[c"]),
    "zlexcount": (["ZADD zlc 0 a 0 b 0 c"], ["ZLEXCOUNT", "zlc", "[a", "[c"]),
    "zrevrangebyscore": (["ZADD zrbs 1 a 2 b 3 c"], ["ZREVRANGEBYSCORE", "zrbs", "3", "1"]),
    "zremrangebyrank_none": (["ZADD zrr 1 a 2 b"], ["ZREMRANGEBYRANK", "zrr", "100", "200"]),
    "substr": (["SET sbk abcdefghijklmnop"], ["SUBSTR", "sbk", "0", "3"]),
    # (frankenredis-p98mw) The stranded routes that still have NO floor entry, each with a
    # working borrowed parser and executor already in the cascade. PING is deliberately NOT
    # here: it is fast-pathed at main.rs:3800/6181, AHEAD of the floor call at 6899, so it
    # is not stranded and a floor entry would be dead weight.
    #
    # Screened on dispatch SHARE before any of them is touched, because depth past the
    # floor says how far the walk is and not what it costs -- that is the screen that
    # correctly excluded RESTORE at 9.4 pct.
    #
    # Steady-state no-ops so the 2N run does not grow the keyspace relative to N:
    #   dump_small      read-only
    #   randomkey_one   read-only, ONE key seeded so the reply is deterministic
    #   lmpop_missing   missing key -> nil, pops nothing
    "dump_small": (["SET dk abcdefghijklmnop"], ["DUMP", "dk"]),
    "randomkey_one": (["SET rk1 vvvvvvvvvvvvvvvv"], ["RANDOMKEY"]),
    "lmpop_missing": ([], ["LMPOP", "1", "nosuchlist", "LEFT", "COUNT", "1"]),
    "renamenx_exists": (["SET rnsrc v1", "SET rndst v2"], ["RENAMENX", "rnsrc", "rndst"]),
    "zunionstore_2": (["ZADD zu1 1 a 2 b", "ZADD zu2 3 c"],
                      ["ZUNIONSTORE", "zudst", "2", "zu1", "zu2"]),
    # (frankenredis-ozrro) THIRD blind-spot batch. The front-classification vein is closed
    # (zero unclassified commands left at any useful depth), so the only way to find another
    # above-parity route is to make more of the [C] list measurable — commands with NO
    # borrowed parser and NO executor, which go straight to GENERIC, the most expensive
    # route there is. Chosen by traffic among the shapeable ones.
    #
    # Steady-state no-ops, as the two-point subtraction requires:
    #   keys_star / scan_zero / lcs_2   read-only
    #   zinterstore_2 / zrangestore_all / pfmerge_2  destination rewritten with the SAME
    #                                    result every time, so the keyspace stops changing
    #                                    after the first call
    "keys_star": (["SET ks1 a", "SET ks2 b"], ["KEYS", "*"]),
    # (frankenredis-gvm6z) The SIZE SIBLING, and it exists because `keys_star` above
    # seeds exactly TWO keys. `KEYS *` is O(keyspace), so at n=2 essentially none of the
    # op is the scan: it is fixed cost plus dispatch, and this repo has already been
    # burned twice by reading a one-point shape as if it measured the COMMAND (SORT at
    # n=3 vs n=64, and the SINTER k-crossover fitted across an encoding boundary).
    #
    # Measured on ELF 3f027a4f, two draws: `keys_star` is fr 5657.5/5648.4 against redis
    # 5512.0/5455.6 -- 1.0264x and 1.0353x, the only shape in that screen above 1.0 --
    # with 34.9 pct of it dispatch, IDENTICAL to a tenth across both draws. Dispatch is a
    # per-CALL cost, so on a two-key keyspace it is nearly the whole of the deficit; it
    # cannot be, at 64. Quoting the n=2 number as "KEYS is behind" would be an INTERCEPT
    # claim wearing the command's name.
    #
    # 32x the keys against the same fixed cost, so the two points separate the intercept
    # from the slope. Read-only, so still a steady-state no-op for the two-point
    # subtraction. Distinct prefix from `ks1`/`ks2` so the two shapes cannot alias if a
    # future harness change ever seeds them into one server.
    "keys_star_64": (
        [" ".join(["MSET"] + [f"kb{i:02d} v{i:02d}" for i in range(64)])],
        ["KEYS", "*"],
    ),
    # (frankenredis-gvm6z) The THIRD point, and it is not optional. A two-point fit is
    # what produced this repo's refuted SINTER k=14.2 crossover -- it drew one line
    # across a regime boundary and banked the intersection. Two points cannot tell a
    # line from a curve, so 2 and 64 alone cannot license a slope. n=16 sits between
    # them: the linear model fitted on 2 and 64 PREDICTS fr 10600 / redis 13372 here,
    # and a measurement that misses those falsifies the model rather than decorating it.
    "keys_star_16": (
        [" ".join(["MSET"] + [f"kc{i:02d} v{i:02d}" for i in range(16)])],
        ["KEYS", "*"],
    ),
    "scan_zero": (["SET sc1 a", "SET sc2 b"], ["SCAN", "0"]),
    # (frankenredis-o500d) FUNCTION LOAD, added because 8ab6f07af made fr EXECUTE the
    # library body at load time and I never measured what that cost. Upstream executes it
    # too, so this is a real vs-incumbent comparison rather than a self-speedup: before the
    # change fr was doing strictly LESS work than redis here and any favourable ratio would
    # have been measuring the missing execution, not speed.
    #
    # REPLACE makes every op identical: the library exists from the first op onward, so the
    # 2N run does not grow the keyspace and the slope is load work rather than insertion.
    # (frankenredis-gvm6z) MEASURED: fr 48,480.8 instr/op with dispatch 4,501.7 (9.3 pct),
    # GENERIC PATH. That dispatch figure is the LARGEST absolute generic block measured in this
    # campaign -- above the ZRANGESTORE arity-6 option forms (3,876), georadius_ro (3,293) and
    # pfmerge_2 (3,013). fr-only, at loadavg 18.2-20.2, which is fine for this quantity: the
    # A/A floor is ~4.46 instr ABSOLUTE, so 4,501 is not in question.
    #
    # BUT READ THIS BEFORE CLAIMING IT. FUNCTION is a CONTAINER command, and the generic frames
    # here include `push_ascii_lowercase_lossy` and the container fullname path -- which is
    # exactly what frankenredis-fpqns is attacking right now (reused buffer for the canonical
    # `parent|sub` histogram key; every container command was still on
    # `canonical_command_fullname`, allocating two owned Strings). So an unknown share of this
    # 4,501 is already someone else's in-flight work, and the recoverable remainder is NOT
    # 4,501 - 815. Re-measure after fpqns lands before sizing a lever here.
    #
    # A floor class is structurally possible -- `FUNCTION LOAD REPLACE <lib>` is arity 4, so
    # (4, Function) with the arm discriminating on the subcommand -- but it needs main.rs and
    # an executor, and the subcommand discriminant means the same both-variants trap the
    # arity-6 work paid for: FUNCTION at arity 4 is not only LOAD.
    "function_load": ([], ["FUNCTION", "LOAD", "REPLACE", "#!lua name=fnperf\nredis.register_function('fnperf_f', function(keys, args) return 1 end)\n"]),

    # (frankenredis-kbyhy / frankenredis-sf510) THE SCRIPTING CACHE-KEY SHAPES.
    #
    # Both beads were filed from SOURCE READING under a build hold and are UNMEASURED. These
    # shapes are the gate they specified, and they exist so the first person with a build can
    # settle both in one run instead of re-deriving the plan.
    #
    # THE PREDICTION UNDER TEST, and why one size cannot test it: the compiled-chunk cache
    # (lua_eval.rs LUA_COMPILED_CHUNK_CACHE) is keyed on SOURCE BYTES, so every caller must
    # MATERIALISE and HASH the full text before it can discover the chunk is already compiled.
    # If that is real, instr/op rises with LIBRARY / SCRIPT SIZE while the invoked function is
    # byte-identical. A single size measures an intercept and calls it the command --
    # frankenredis-eh2ct found 21 of 39 size-sensitive rows doing exactly that, and one of them
    # INVERTED when measured at a second size. Hence a ladder, not a point.
    #
    # FLAT ACROSS THE LADDER = NO LEVER. Close both beads in that case; do not go looking for a
    # smaller effect.
    #
    # The invoked body is identical in every rung. Only the SURROUNDING library grows, which is
    # the whole point: the useful work is constant and only the text the cache key spans changes.
    "fcall_lib1": (
        [["FUNCTION", "LOAD", "REPLACE", "#!lua name=fcl1\n"
          + "redis.register_function('fcl1_f0', function(keys, args) return 1 end)\n"]],
        ["FCALL", "fcl1_f0", "0"]),
    "fcall_lib8": (
        [["FUNCTION", "LOAD", "REPLACE", "#!lua name=fcl8\n"
          + "".join("redis.register_function('fcl8_f%d', function(keys, args) return 1 end)\n" % i
                    for i in range(8))]],
        ["FCALL", "fcl8_f0", "0"]),
    # (frankenredis-kbyhy) THE DESIGN-DECIDING SHAPE. fcall_lib32 confounds two variables:
    # it has 32x the TEXT and 32x the CLOSURES. This one has the text of a 32-function library
    # and the closures of a 1-function library -- 31 comment lines instead of 31 registrations.
    #
    # If it costs like fcall_lib32, the cost is TEXT SIZE and caching the rebuilt wrapper fixes
    # it (contained change, fr-command only).
    # If it costs like fcall_lib1, the cost is CLOSURE COUNT -- FCALL evaluates the whole
    # library chunk per call, defining every function before calling one -- and no amount of
    # caching the wrapper text helps; the fix is structural.
    "fcall_lib1_pad": (
        [["FUNCTION", "LOAD", "REPLACE", "#!lua name=fclp\n"
          + "".join("-- padding line %d to match a 32-function library's text size\n" % i
                    for i in range(31))
          + "redis.register_function('fclp_f0', function(keys, args) return 1 end)\n"]],
        ["FCALL", "fclp_f0", "0"]),

    "fcall_lib32": (
        [["FUNCTION", "LOAD", "REPLACE", "#!lua name=fcl32\n"
          + "".join("redis.register_function('fcl32_f%d', function(keys, args) return 1 end)\n" % i
                    for i in range(32))]],
        ["FCALL", "fcl32_f0", "0"]),

    # EVALSHA: the SHA is computed here rather than captured at runtime, because the shape's
    # argv must be a literal. sha1 of the exact bytes SCRIPT LOAD is given.
    # The body returns 1 in every rung; only the trailing comment padding grows, so the script
    # the cache key spans differs while the executed work does not.
    "evalsha_small": (
        [["SCRIPT", "LOAD", _EVALSHA_BODY_SMALL]],
        ["EVALSHA", _sha1_hex(_EVALSHA_BODY_SMALL), "0"]),
    "evalsha_large": (
        [["SCRIPT", "LOAD", _EVALSHA_BODY_LARGE]],
        ["EVALSHA", _sha1_hex(_EVALSHA_BODY_LARGE), "0"]),


    # (frankenredis-ozrro) The arity-4 SCAN option forms, front-classified in b631dd1f9.
    # Each is ONE option: the two-option forms keep the generic route by design, so a
    # two-option shape here would measure the thing that did NOT change.
    "scan_count": (["SET sc1 a", "SET sc2 b"], ["SCAN", "0", "COUNT", "100"]),
    "scan_match": (["SET sc1 a", "SET sc2 b"], ["SCAN", "0", "MATCH", "sc*"]),
    "scan_type": (["SET sc1 a", "SET sc2 b"], ["SCAN", "0", "TYPE", "string"]),
    # (frankenredis-ozrro) The two-option form at arity 6 — what client scan_iter helpers
    # actually emit. MATCH+COUNT is the canonical pair; the arity-8 three-option form is
    # deliberately absent because it stays on the generic route by design.
    "scan_iter": (["SET sc1 a", "SET sc2 b"],
                  ["SCAN", "0", "MATCH", "sc*", "COUNT", "100"]),
    "lcs_2": (["SET lc1 ohmytext", "SET lc2 mynewtext"], ["LCS", "lc1", "lc2"]),
    # (frankenredis-gvm6z) The SIZE SIBLING for LCS, and the reason it exists is that
    # `lcs_2` measured 1.1085x -- the worst cell on my screen -- over strings of EIGHT and
    # NINE characters. LCS is O(n*m), so `lcs_2` runs a 72-cell DP: at 7,132 instr/op with
    # 2,451 of that dispatch, almost none of the op is the algorithm. This repo has now
    # read a one-point shape as a command claim three times (SORT n=3, the refuted SINTER
    # k-crossover, and KEYS n=2 where 1.0353x inverted to 0.6806x by n=64), so LCS does
    # not get quoted until it has a second point.
    #
    # 64x64 = 4,096 cells against 72 -- 57x the DP work on the same fixed cost. The two
    # points are a FALSIFICATION TEST with both outcomes named in advance:
    #   * ratio stays near 1.11  -> the deficit is in the DP kernel itself, and dispatch
    #                               is not the lever; attack the inner loop.
    #   * ratio falls toward the ~0.4-0.6 control band -> `lcs_2` was measuring the
    #                               intercept, and 1.1085x must never be quoted bare.
    # Strings differ every 8th character so the DP is neither degenerate-equal nor
    # degenerate-disjoint, and both are read-only, so the shape stays a steady-state
    # no-op for the two-point subtraction.
    "lcs_64": (
        ["SET lcA " + "abcdefgh" * 8, "SET lcB " + "abcdxfgh" * 8],
        ["LCS", "lcA", "lcB"],
    ),
    # (frankenredis-p98mw) THE THIRD LCS REGIME, and the reason it exists is that lcs_2 and
    # lcs_64 sit on the SAME SIDE of a boundary neither of them crosses.
    #
    # Read from the source, not guessed: `build_lcs_dp` (fr-command) selects the
    # Crochemore-Iliopoulos-Pinzon-Rytter bit-parallel LCS when `a.len() <= 64` OR
    # `b.len() <= 64` -- an O(n+m) build of Allison-Dix vectors with O(1) cell lookup.
    # When BOTH strings exceed a machine word it falls back to `LcsDp::Full`, the classic
    # flat O(n*m) u32 matrix, which is the SAME algorithm redis runs.
    #
    # So lcs_2 (8x9) and lcs_64 (64x64) are both bit-parallel: one measures that regime's
    # INTERCEPT, the other its SLOPE advantage. 64x64 is the LAST size before the cliff.
    # Neither says anything about LCS above 64 bytes, which is where a real LCS call lives.
    #
    # FALSIFICATION TEST, both outcomes named in advance:
    #   * lcs_65 lands near lcs_64's ratio -> the fallback is not the cliff it looks like
    #     and the matrix arm is competitive; leave it alone.
    #   * lcs_65 jumps toward or above 1.0 while lcs_64 sits far below -> confirmed cliff at
    #     the word boundary, and the lever is a MULTI-WORD Allison-Dix vector ([u64; W] with
    #     carry propagation through the `v = (v+u)|(v-u)` recurrence), which carries the
    #     bit-parallel advantage past 64 bytes instead of surrendering to redis's algorithm.
    #
    # 65 is chosen deliberately over a round number: it is the SMALLEST input that takes the
    # fallback, so the two arms differ by ONE byte of input and ~nothing of real work. A
    # discontinuity there cannot be explained by problem size.
    # 128 is the same regime with 4x the cells, to separate a fixed fallback cost from an
    # O(n*m) one. Both read-only, so both stay steady-state no-ops for the subtraction.
    "lcs_65": (
        ["SET lcE " + ("abcdefgh" * 8) + "x", "SET lcF " + ("abcdxfgh" * 8) + "y"],
        ["LCS", "lcE", "lcF"],
    ),
    "lcs_128": (
        ["SET lcG " + "abcdefgh" * 16, "SET lcH " + "abcdxfgh" * 16],
        ["LCS", "lcG", "lcH"],
    ),
    "zinterstore_2": (["ZADD zi1 1 a 2 b", "ZADD zi2 3 b"],
                      ["ZINTERSTORE", "zidst", "2", "zi1", "zi2"]),
    # (frankenredis-ozrro) NULL-CONTROL shape for the miss-tax measurement. ZINTERCARD is
    # verified unclassified — no floor entry, no borrowed parser, no cascade arm, no
    # executor — so flipping FR_PERF_AB_CASCADE_BYPASS must change only the cost of the
    # classification ATTEMPT, never a route. Read-only, so it is a true steady-state no-op.
    "zintercard_2": (["ZADD zc1 1 a 2 b", "ZADD zc2 3 b"],
                     ["ZINTERCARD", "2", "zc1", "zc2"]),
    # (frankenredis-ozrro) The SAME unclassified command at a HIGHER arity, so the miss-tax
    # measurement becomes a within-command dose-response instead of a cross-command
    # comparison. If the tax scales with arity, this must exceed zintercard_2; if the tax is
    # a floor-wide constant, the two must agree. Command identity, token length and reply
    # shape are all held fixed — only the argument count moves.
    "zintercard_limit": (["ZADD zc1 1 a 2 b", "ZADD zc2 3 b"],
                         ["ZINTERCARD", "2", "zc1", "zc2", "LIMIT", "5"]),
    # (frankenredis-ozrro) PUBSUB sits at cascade arm ~127 of 163 — the DEEPEST unclassified
    # arm — with two borrowed executors already present (numpat, numsub). The depth law
    # predicts ~5,467 instr/op of dispatch there, but that law was fitted on arms 76-103 and
    # extrapolating it to 127 is exactly the predicted-not-measured error this ledger has a
    # correction about, so these shapes exist to MEASURE it before any floor entry is written.
    #
    # pubsub_channels is the DECLINE control and the interesting one: the arity-2 parser
    # accepts ANY subcommand and the executor refuses anything but NUMPAT, so CHANNELS walks
    # the whole cascade only to end up generic. It should show the walk cost without the
    # executor's work.
    "pubsub_numpat": ([], ["PUBSUB", "NUMPAT"]),
    "pubsub_numsub": ([], ["PUBSUB", "NUMSUB", "ch1"]),
    "pubsub_channels": ([], ["PUBSUB", "CHANNELS"]),
    # (frankenredis-fpqns) CONTAINER COMMANDS ON THE GENERIC ROUTE, added to give the
    # SUBCOMMAND_TABLE levers a PATH-SHARING control. The lever-1 row had none: its only
    # candidates, pubsub_numsub and memory_usage, turned out to be front-classified and so
    # never execute the code under test (9abeaa5c1). A control must share the CODE PATH, and
    # dispatch share is what tells you which path a shape is on before you change anything --
    # a front-classified route reads a few hundred instr/op, a generic one reads thousands.
    #
    # All three are read-only and steady-state, so the 2N run does not grow state.
    "client_info": ([], ["CLIENT", "INFO"]),
    "object_encoding": (["SET oe abcdefghijklmnop"], ["OBJECT", "ENCODING", "oe"]),
    "config_get_one": ([], ["CONFIG", "GET", "maxmemory"]),

    # (frankenredis-e6c9t follow-on) THE TABLE-WALK FAMILY, which is where this campaign's two
    # largest deficits both came from and where neither was being looked for. PUBSUB CHANNELS
    # (2.47x) and CONFIG GET (5.90x) are the same shape of command: answer a question from a
    # static registry. Both were found BY ACCIDENT, as controls for something else, and the
    # reason is stated in e6c9t's own bead -- "no shape existed for it before, so nobody had
    # measured it". These four close the rest of that family, so the next one is found on
    # purpose:
    #   INFO            builds a large report string from live counters (two sizes, so a
    #                   per-section cost can be separated from the fixed report cost)
    #   COMMAND COUNT   answers from the command table without emitting it
    #   COMMAND DOCS    emits ONE command's metadata out of that same table
    #   CLIENT LIST     walks the connection registry
    # All four are read-only, take no key, and are safe to repeat, so the two-point slope
    # subtraction applies unchanged.
    # (frankenredis-e6c9t) THE GLOB HALF OF CONFIG GET, which had no shape at all.
    # `config_get_one` is a LITERAL request and since the literal index landed it no longer
    # touches the ordered static walk, so it cannot measure that walk any more. A wildcard
    # request still walks all ~190 entries of CONFIG_STATIC_PARAMS. Neither of these patterns
    # is one of the two hard-coded early-return globs (`maxmemory*`, `lazyfree*`), so both
    # genuinely enter the loop; `*` is additionally what monitoring clients actually send.
    # (frankenredis-copydeficit) COPY is the worst measured cell on the board with replicated
    # standing: balanced_square_ab puts it at 0.8105x and 0.7916x fr/redis ops/s (ADMISSIBLE
    # both, get_control admissible alongside), i.e. Redis is ~1.25x faster and fr is 18-21 pct
    # behind even at its most favourable interval end. No instruction shape existed for it,
    # which is why the deficit has never been attributed. Same argv as the throughput shape so
    # the two instruments describe the same work.
    "copy_replace": (["SET kk vvvvvvvvvvvvvvvv"], ["COPY", "kk", "kdst", "REPLACE"]),

    # (frankenredis-copydeficit, second instance) BITPOS. The class claims arity 3 and 5;
    # the command also has arity 4 (`key bit start`) and 6 (`... start end BYTE|BIT`), and
    # BOTH are unambiguous. What makes this the same defect rather than a new route: the
    # executor's own argc arithmetic is `4 + end.is_some() + unit.is_some()`, so it was
    # written for 4, 5 and 6, and parse_borrowed_plain_bitpos_{start,unit}_packet already
    # exist -- they simply have no floor arm, only a cascade one. bitpos_range is the
    # already-classified sibling and is the control that separates "this form is stranded"
    # from "BITPOS is expensive".
    "bitpos_start": (["SET bp k 0", "SETBIT bp 100 1"], ["BITPOS", "bp", "1", "2"]),
    "bitpos_range": (["SET bp k 0", "SETBIT bp 100 1"], ["BITPOS", "bp", "1", "2", "-1"]),
    "bitpos_unit": (["SET bp k 0", "SETBIT bp 100 1"],
                    ["BITPOS", "bp", "1", "2", "-1", "BYTE"]),
    "bitpos_plain": (["SET bp k 0", "SETBIT bp 100 1"], ["BITPOS", "bp", "1"]),

    "config_get_star": ([], ["CONFIG", "GET", "*"]),
    "config_get_prefix_glob": ([], ["CONFIG", "GET", "repl-*"]),

    "info_default": ([], ["INFO"]),
    "info_section": ([], ["INFO", "server"]),
    "command_count": ([], ["COMMAND", "COUNT"]),
    "command_docs_one": ([], ["COMMAND", "DOCS", "GET"]),
    "client_list": ([], ["CLIENT", "LIST"]),

    # (frankenredis-gvm6z) FOUR SHAPES FROM THE [C] BLIND SPOT. corpus_coverage.py puts the
    # blind spot at 53 commands, 50 of them with no borrowed machinery at all. Most of that
    # 50 is unshapeable by construction — blocking reads (BLPOP/BZPOPMAX), pub/sub, EXEC,
    # SELECT, and admin verbs whose cost is not a per-command figure. These four are the
    # ones that ARE shapeable, and all four are flagged `readonly` in COMMAND_TABLE, so
    # each is a steady-state no-op for the two-point subtraction by construction rather
    # than by argument.
    #
    # The precedent for spending shapes here: the last six drawn from this list produced
    # THREE routes at 1.43x-1.48x, and my own zrangestore_all draw found the largest
    # absolute dispatch cost in the campaign. A blind-spot command is UNKNOWN, not fine.
    "zunion_2": (["ZADD zu1 1 a", "ZADD zu2 2 b"], ["ZUNION", "2", "zu1", "zu2"]),
    "georadius_ro_1": (
        ["GEOADD grk 13.361389 38.115556 P1"],
        ["GEORADIUS_RO", "grk", "13.361389", "38.115556", "200", "km"],
    ),
    # (frankenredis-eh2ct) THE SAME SHAPE the throughput board certifies, registered here
    # so one shape can be read on both metrics. `geosearch_1` below is close but NOT
    # identical -- one member instead of two, and no ASC -- and comparing an instruction
    # ratio from one shape against a throughput ratio from another is exactly the
    # cross-shape error the size pairs exist to remove. balanced_square_ab certified this
    # shape at 1.0202 raw / 0.9162 control-normalised (fr BEHIND); this entry is what makes
    # the instruction reading comparable to that number rather than merely adjacent to it.
    "geosearch_2": (
        ["GEOADD g 13.361389 38.115556 P1", "GEOADD g 15.087269 37.502669 P2"],
        ["GEOSEARCH", "g", "FROMLONLAT", "15", "37", "BYRADIUS", "200", "km", "ASC"],
    ),
    "geosearch_1": (
        ["GEOADD gsk 13.361389 38.115556 P1"],
        ["GEOSEARCH", "gsk", "FROMLONLAT", "13.361389", "38.115556", "BYRADIUS", "200", "km"],
    ),
    # Summary form against a group with an EMPTY pending list: still exercises the whole
    # lookup/group-resolve path, and reads nothing that changes.
    # (frankenredis-gvm6z) THE POPULATED SIBLING, required by my own retry predicate before
    # xpending_empty's 1.6060x may be quoted as anything. XPENDING's summary form walks the
    # group's pending-entries list, so the empty-PEL shape measures the INTERCEPT and
    # nothing else -- the same trap that made keys_star (1.0353x -> 0.6806x), lcs_2
    # (1.1085x -> 0.1190x) and zrangestore_all (0.7926x -> 0.0389x) misread as command
    # claims. XREADGROUP with `>` moves the entries into the PEL, where they stay: nothing
    # here ACKs, so the pending set is identical on every call and XPENDING is read-only,
    # which keeps the two-point subtraction valid.
    "xpending_populated": (
        [" ".join(["XADD", "xpps", f"{i}-1", "f", "v"]) for i in range(1, 33)]
        + [
            "XGROUP CREATE xpps xppg 0",
            "XREADGROUP GROUP xppg c1 COUNT 32 STREAMS xpps >",
        ],
        ["XPENDING", "xpps", "xppg"],
    ),
    "xpending_empty": (
        ["XADD xps 1-1 f v", "XGROUP CREATE xps xpg 0"],
        ["XPENDING", "xps", "xpg"],
    ),
    "zrangestore_all": (["ZADD zrsrc 1 a 2 b 3 c"],
                        ["ZRANGESTORE", "zrdst", "zrsrc", "0", "-1"]),
    # (frankenredis-gvm6z) The SIZE SIBLING. `zrangestore_all` has THREE members, so its
    # 0.7926x is an intercept reading, exactly like keys_star at n=2 (1.0353x -> 0.6806x
    # by n=64) and lcs_2 at 8x9 chars (1.1085x -> 0.1190x at 64x64). The DISPATCH figure
    # it reported -- 3,787.6 instr/op, the largest measured in this campaign -- is a
    # per-CALL constant and so is size-independent; the RATIO is not, and must not be
    # quoted without its member count.
    #
    # 64 members against the same fixed cost. Rank mode over the full range, so the copy
    # STEADY STATE, and it is load-bearing: ZRANGESTORE is a WRITE, so the two-point
    # subtraction is only valid because the destination is rewritten with the IDENTICAL
    # result every call. Call 1 creates it and appears once in BOTH the N and 2N runs, so
    # it cancels; calls 2..N are indistinguishable from each other.
    # is O(n) on both engines and the slopes are directly comparable. Destination is
    # rewritten with the identical result every call, so the keyspace stops changing
    # after the first and the two-point subtraction still sees a steady-state no-op.
    # (frankenredis-gvm6z) THE OPTION FORMS, which the floor class deliberately does NOT
    # claim. `(5, Zrangestore)` is arity-5 EXACTLY because the arm's parser is a `*5`
    # prefix literal; BYSCORE/BYLEX/REV/LIMIT are arity 6+ and still take the GENERIC
    # path. That was an argued decision with no number attached, and "the class is correct
    # to stop at 5" and "the option forms are cheap" are different claims. These measure
    # the second one.
    #
    # Both are steady-state no-ops: REV over the full range and BYSCORE over the whole
    # score band each rewrite the destination with the identical result every call, so the
    # keyspace stops changing after the first and the two-point subtraction holds.
    "zrangestore_rev": (
        ["ZADD zrvsrc 1 a 2 b 3 c"],
        ["ZRANGESTORE", "zrvdst", "zrvsrc", "0", "-1", "REV"],
    ),
    "zrangestore_byscore": (
        ["ZADD zrbsrc 1 a 2 b 3 c"],
        ["ZRANGESTORE", "zrbdst", "zrbsrc", "1", "3", "BYSCORE"],
    ),
    # (frankenredis-gvm6z) SIZE SIBLINGS for the two option forms. My own retry predicate
    # asked for these before 0.8127x / 0.7260x are quoted as anything: both option shapes
    # hold THREE members, and the classified base form moved 0.7926x -> 0.0389x between 3
    # and 64. If the option ratios collapse the same way, those two numbers are confirmed
    # intercept readings and nobody can cite them as "ZRANGESTORE REV is 0.81x".
    #
    # The DISPATCH figure is the part that must NOT move: it is a per-call constant, and
    # the base form held it to 0.008 pct across two option forms. Predicted here: ~3,800
    # instr/op at 64 members, unchanged from 3. A dispatch figure that moved with member
    # count would falsify the per-call claim these rows rest on.
    "zrangestore_rev_64": (
        [" ".join(["ZADD", "zrv64src"] + [f"{i} m{i:02d}" for i in range(64)])],
        ["ZRANGESTORE", "zrv64dst", "zrv64src", "0", "-1", "REV"],
    ),
    "zrangestore_byscore_64": (
        [" ".join(["ZADD", "zrb64src"] + [f"{i} m{i:02d}" for i in range(64)])],
        ["ZRANGESTORE", "zrb64dst", "zrb64src", "0", "63", "BYSCORE"],
    ),
    # (frankenredis-gvm6z) ABOVE the listpack threshold. `zset-max-listpack-entries`
    # defaults to 128 (redis config.c:3219), so a 64-member destination is a LISTPACK and a
    # 200-member one is a SKIPLIST. At n=64 redis costs 10,927 instr per member in rank
    # mode, 5,533 in REV and 2,419 in BYSCORE -- and BYSCORE is the one form upstream
    # forces to skiplist (zsetTypeCreate(-1,0)). These two shapes test whether the
    # destination ENCODING is what separates them.
    #
    # PREDICTIONS, recorded before the run so the result can falsify them:
    #   * if encoding is the driver -> at n=200 both are skiplists, so rank and REV
    #     CONVERGE and both per-member rates fall toward BYSCORE's ~2,419.
    #   * if they stay ~2x apart -> encoding is NOT the driver and the rank/REV split has
    #     another cause, which no amount of listpack reasoning will explain.
    #
    # Both are WRITES and both are steady-state for the same reason as the 64-member
    # pair: the destination is rewritten with the identical result on every call.
    "zrangestore_200": (
        [" ".join(["ZADD", "zr200src"] + [f"{i} m{i:03d}" for i in range(200)])],
        ["ZRANGESTORE", "zr200dst", "zr200src", "0", "-1"],
    ),
    "zrangestore_rev_200": (
        [" ".join(["ZADD", "zrv200src"] + [f"{i} m{i:03d}" for i in range(200)])],
        ["ZRANGESTORE", "zrv200dst", "zrv200src", "0", "-1", "REV"],
    ),
    # (frankenredis-gvm6z) THE LAST UNMEASURED ZRANGESTORE SURFACE. The arity-6 option forms
    # are now all floor-classified; the LIMIT forms are arity 9 and still take the generic
    # path. My own retry predicate named them as the only ZRANGESTORE work left.
    #
    # PREDICTION, registered before the run: if this is on the generic path its dispatch
    # should land near the ~3,800-4,100 instr/op the arity-6 forms paid BEFORE they were
    # classified, because dispatch on this route is a per-call constant (held to +2 pct over a
    # 67x member span). If it comes in near the classified band (~690-815) then something
    # already claims it and the surface is not open at all.
    #
    # Steady state: the destination is rewritten with the identical result every call, the
    # same argument the other write-issuing ZRANGESTORE shapes rest on.
    "zrangestore_limit": (
        ["ZADD zrlsrc 1 a 2 b 3 c"],
        ["ZRANGESTORE", "zrldst", "zrlsrc", "1", "3", "BYSCORE", "LIMIT", "0", "2"],
    ),
    "zrangestore_64": (
        [" ".join(["ZADD", "zr64src"] + [f"{i} m{i:02d}" for i in range(64)])],
        ["ZRANGESTORE", "zr64dst", "zr64src", "0", "-1"],
    ),
    "pfmerge_2": (["PFADD pf1 a b c", "PFADD pf2 c d"], ["PFMERGE", "pfdst", "pf1", "pf2"]),
    "spop_missing": ([], ["SPOP", "nosuchset"]),
    "srandmember_1": (["SADD srm m1 m2 m3"], ["SRANDMEMBER", "srm"]),
    "smove_missing": (["SADD smsrc a b", "SADD smdst c"],
                      ["SMOVE", "smsrc", "smdst", "nosuchmember"]),
    "rpoplpush_missing": (["RPUSH rplhdst x"], ["RPOPLPUSH", "nosuchlist", "rplhdst"]),
    # (frankenredis-ozrro) HMSET is the last floor WRITE arm still calling a non-gate executor
    # variant, so it pays the write gate PER PACKET where the cascade amortises it per pass.
    # Idempotent: the same fields are set to the same values every call, so the keyspace stops
    # changing after the first.
    "hmset_2": (["HMSET h0 f1 v1 f2 v2"], ["HMSET", "h0", "f1", "v1", "f2", "v2"]),
    "hset_same": (["HSET hs f v"], ["HSET", "hs", "f", "v"]),
    # (frankenredis-gein3) Every other SINTER shape returns TWO OR THREE members, so a
    # lever whose cost is O(k log k) in RESULT cardinality — the reply sort fr does and
    # redis does not — is arithmetically invisible on all of them: sorting 3 refs is tens
    # of instructions against sinter_9's 12,114. This shape returns 512, which is the only
    # way the corpus can see that sort at all. Same structural fix as sort_ro_alpha_64:
    # a shape whose N is tiny cannot show a lever whose cost scales with N.
    # (frankenredis-gein3) CROSSOVER BRACKET. The banked row deriving a fr/redis crossover
    # at "k=14.2" fitted ONE line through k=2 and k=512 -- two points that sit in DIFFERENT
    # COMPLEXITY REGIMES, because a set flips from listpack to hashtable at
    # set-max-listpack-entries (128 on both engines, verified with OBJECT ENCODING). A
    # listpack intersection is O(n*m) linear scans; a hashtable one is O(n) probes. No
    # single line through those two points means anything, and the crossover it predicted
    # does not exist: at k=14 the measured ratio is 0.4273x, i.e. fr is 2.3x FASTER.
    #
    # These shapes put points ON the predicted crossing and on BOTH SIDES of the real
    # boundary, so the model is tested rather than trusted. Measured 2026-08-16:
    #
    #     k      2    8   12   14   16   24   48  100  128 | 140    256    512
    #     ratio .509 .440 .431 .427 .426 .418 .429 .459 .459| .837  1.435  1.773
    #     encoding  <------------ listpack ------------>   | <--- hashtable --->
    #
    # Absolute cost FALLS across the boundary (fr 386,695 -> 111,315; redis 841,704 ->
    # 132,985) because the complexity class changes there. fr leads the ENTIRE listpack
    # regime and is behind only above it, so fr's worst SINTER ratio lives entirely in the
    # hashtable regime -- which is where a lever belongs, not at k=14.
    #
    # Both sets hold the SAME n members, so the intersection cardinality IS n and only the
    # RESULT SIZE varies between these shapes. If set-max-listpack-entries is ever changed
    # on either engine the boundary moves and every regime label above moves with it.
    #
    # REGENERATED 2026-08-16 AFTER THE BUSY-SPIN REMOVAL (7462aa5d3). The table above is
    # kept as history because it is what the crossover argument was built on; these are the
    # numbers that describe the code as it stands. Same shapes, same harness, loadavg
    # 13.4-15.9, mean CPU MHz 2825-3279, ELF 2550a666795d8d14:
    #
    #     k      fr instr/op      redis      ratio    passes/op   encoding
    #       2        4,279.1     8,684.0    0.4928      0.001     listpack
    #       8        6,515.6    15,385.9    0.4235      0.002     listpack
    #      14       10,684.6    26,672.1    0.4006      0.004     listpack
    #      48       61,569.1   149,391.3    0.4121      0.020     listpack
    #     128      373,418.5   841,447.5    0.4438      0.060     listpack
    #     140       50,664.8   132,904.2    0.3812      0.067     hashtable
    #     256       89,876.2   240,784.1    0.3733      0.187     hashtable
    #     512      180,474.4   467,160.7    0.3863      0.440     hashtable
    #
    # THE HASHTABLE-REGIME DEFICIT IS GONE. It read 0.837x / 1.435x / 1.773x -- fr behind
    # and worsening with k -- and now reads 0.381x / 0.373x / 0.386x, fr about 2.6x ahead
    # and FLAT. fr's absolutes fell 54-78 pct there (k=512: 837,755 -> 180,474) while
    # redis's are unchanged, so the move is entirely fr's. passes/op says why: 0.440 at
    # k=512 against the 248 that the event loop used to spin.
    #
    # So fr now leads across the ENTIRE range, both encodings, 0.373x-0.493x, and there is
    # no crossover at any k. The encoding boundary is still visible as a cost STEP in both
    # engines -- absolutes fall crossing 128 because a listpack intersection is O(n*m) and a
    # hashtable one is O(n) -- but it no longer produces a deficit. Keep these shapes: they
    # are what proves the boundary is a step and not a crossing.
    "sinter_k8": (
        ["SADD kb8_1 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007",
         "SADD kb8_2 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007"],
        ["SINTER", "kb8_1", "kb8_2"],
    ),
    "sinter_k12": (
        ["SADD kb12_1 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011",
         "SADD kb12_2 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011"],
        ["SINTER", "kb12_1", "kb12_2"],
    ),
    "sinter_k14": (
        ["SADD kb14_1 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013",
         "SADD kb14_2 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013"],
        ["SINTER", "kb14_1", "kb14_2"],
    ),
    "sinter_k16": (
        ["SADD kb16_1 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015",
         "SADD kb16_2 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015"],
        ["SINTER", "kb16_1", "kb16_2"],
    ),
    "sinter_k24": (
        ["SADD kb24_1 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023",
         "SADD kb24_2 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023"],
        ["SINTER", "kb24_1", "kb24_2"],
    ),
    "sinter_k48": (
        ["SADD kb48_1 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023 m0024 m0025 m0026 m0027 m0028 m0029 m0030 m0031 m0032 m0033 m0034 m0035 m0036 m0037 m0038 m0039 m0040 m0041 m0042 m0043 m0044 m0045 m0046 m0047",
         "SADD kb48_2 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023 m0024 m0025 m0026 m0027 m0028 m0029 m0030 m0031 m0032 m0033 m0034 m0035 m0036 m0037 m0038 m0039 m0040 m0041 m0042 m0043 m0044 m0045 m0046 m0047"],
        ["SINTER", "kb48_1", "kb48_2"],
    ),
    "sinter_k100": (
        ["SADD kb100_1 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023 m0024 m0025 m0026 m0027 m0028 m0029 m0030 m0031 m0032 m0033 m0034 m0035 m0036 m0037 m0038 m0039 m0040 m0041 m0042 m0043 m0044 m0045 m0046 m0047 m0048 m0049 m0050 m0051 m0052 m0053 m0054 m0055 m0056 m0057 m0058 m0059 m0060 m0061 m0062 m0063 m0064 m0065 m0066 m0067 m0068 m0069 m0070 m0071 m0072 m0073 m0074 m0075 m0076 m0077 m0078 m0079 m0080 m0081 m0082 m0083 m0084 m0085 m0086 m0087 m0088 m0089 m0090 m0091 m0092 m0093 m0094 m0095 m0096 m0097 m0098 m0099",
         "SADD kb100_2 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023 m0024 m0025 m0026 m0027 m0028 m0029 m0030 m0031 m0032 m0033 m0034 m0035 m0036 m0037 m0038 m0039 m0040 m0041 m0042 m0043 m0044 m0045 m0046 m0047 m0048 m0049 m0050 m0051 m0052 m0053 m0054 m0055 m0056 m0057 m0058 m0059 m0060 m0061 m0062 m0063 m0064 m0065 m0066 m0067 m0068 m0069 m0070 m0071 m0072 m0073 m0074 m0075 m0076 m0077 m0078 m0079 m0080 m0081 m0082 m0083 m0084 m0085 m0086 m0087 m0088 m0089 m0090 m0091 m0092 m0093 m0094 m0095 m0096 m0097 m0098 m0099"],
        ["SINTER", "kb100_1", "kb100_2"],
    ),
    "sinter_k128": (
        ["SADD kb128_1 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023 m0024 m0025 m0026 m0027 m0028 m0029 m0030 m0031 m0032 m0033 m0034 m0035 m0036 m0037 m0038 m0039 m0040 m0041 m0042 m0043 m0044 m0045 m0046 m0047 m0048 m0049 m0050 m0051 m0052 m0053 m0054 m0055 m0056 m0057 m0058 m0059 m0060 m0061 m0062 m0063 m0064 m0065 m0066 m0067 m0068 m0069 m0070 m0071 m0072 m0073 m0074 m0075 m0076 m0077 m0078 m0079 m0080 m0081 m0082 m0083 m0084 m0085 m0086 m0087 m0088 m0089 m0090 m0091 m0092 m0093 m0094 m0095 m0096 m0097 m0098 m0099 m0100 m0101 m0102 m0103 m0104 m0105 m0106 m0107 m0108 m0109 m0110 m0111 m0112 m0113 m0114 m0115 m0116 m0117 m0118 m0119 m0120 m0121 m0122 m0123 m0124 m0125 m0126 m0127",
         "SADD kb128_2 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023 m0024 m0025 m0026 m0027 m0028 m0029 m0030 m0031 m0032 m0033 m0034 m0035 m0036 m0037 m0038 m0039 m0040 m0041 m0042 m0043 m0044 m0045 m0046 m0047 m0048 m0049 m0050 m0051 m0052 m0053 m0054 m0055 m0056 m0057 m0058 m0059 m0060 m0061 m0062 m0063 m0064 m0065 m0066 m0067 m0068 m0069 m0070 m0071 m0072 m0073 m0074 m0075 m0076 m0077 m0078 m0079 m0080 m0081 m0082 m0083 m0084 m0085 m0086 m0087 m0088 m0089 m0090 m0091 m0092 m0093 m0094 m0095 m0096 m0097 m0098 m0099 m0100 m0101 m0102 m0103 m0104 m0105 m0106 m0107 m0108 m0109 m0110 m0111 m0112 m0113 m0114 m0115 m0116 m0117 m0118 m0119 m0120 m0121 m0122 m0123 m0124 m0125 m0126 m0127"],
        ["SINTER", "kb128_1", "kb128_2"],
    ),
    "sinter_k140": (
        ["SADD kb140_1 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023 m0024 m0025 m0026 m0027 m0028 m0029 m0030 m0031 m0032 m0033 m0034 m0035 m0036 m0037 m0038 m0039 m0040 m0041 m0042 m0043 m0044 m0045 m0046 m0047 m0048 m0049 m0050 m0051 m0052 m0053 m0054 m0055 m0056 m0057 m0058 m0059 m0060 m0061 m0062 m0063 m0064 m0065 m0066 m0067 m0068 m0069 m0070 m0071 m0072 m0073 m0074 m0075 m0076 m0077 m0078 m0079 m0080 m0081 m0082 m0083 m0084 m0085 m0086 m0087 m0088 m0089 m0090 m0091 m0092 m0093 m0094 m0095 m0096 m0097 m0098 m0099 m0100 m0101 m0102 m0103 m0104 m0105 m0106 m0107 m0108 m0109 m0110 m0111 m0112 m0113 m0114 m0115 m0116 m0117 m0118 m0119 m0120 m0121 m0122 m0123 m0124 m0125 m0126 m0127 m0128 m0129 m0130 m0131 m0132 m0133 m0134 m0135 m0136 m0137 m0138 m0139",
         "SADD kb140_2 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023 m0024 m0025 m0026 m0027 m0028 m0029 m0030 m0031 m0032 m0033 m0034 m0035 m0036 m0037 m0038 m0039 m0040 m0041 m0042 m0043 m0044 m0045 m0046 m0047 m0048 m0049 m0050 m0051 m0052 m0053 m0054 m0055 m0056 m0057 m0058 m0059 m0060 m0061 m0062 m0063 m0064 m0065 m0066 m0067 m0068 m0069 m0070 m0071 m0072 m0073 m0074 m0075 m0076 m0077 m0078 m0079 m0080 m0081 m0082 m0083 m0084 m0085 m0086 m0087 m0088 m0089 m0090 m0091 m0092 m0093 m0094 m0095 m0096 m0097 m0098 m0099 m0100 m0101 m0102 m0103 m0104 m0105 m0106 m0107 m0108 m0109 m0110 m0111 m0112 m0113 m0114 m0115 m0116 m0117 m0118 m0119 m0120 m0121 m0122 m0123 m0124 m0125 m0126 m0127 m0128 m0129 m0130 m0131 m0132 m0133 m0134 m0135 m0136 m0137 m0138 m0139"],
        ["SINTER", "kb140_1", "kb140_2"],
    ),
    "sinter_k256": (
        ["SADD kb256_1 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023 m0024 m0025 m0026 m0027 m0028 m0029 m0030 m0031 m0032 m0033 m0034 m0035 m0036 m0037 m0038 m0039 m0040 m0041 m0042 m0043 m0044 m0045 m0046 m0047 m0048 m0049 m0050 m0051 m0052 m0053 m0054 m0055 m0056 m0057 m0058 m0059 m0060 m0061 m0062 m0063 m0064 m0065 m0066 m0067 m0068 m0069 m0070 m0071 m0072 m0073 m0074 m0075 m0076 m0077 m0078 m0079 m0080 m0081 m0082 m0083 m0084 m0085 m0086 m0087 m0088 m0089 m0090 m0091 m0092 m0093 m0094 m0095 m0096 m0097 m0098 m0099 m0100 m0101 m0102 m0103 m0104 m0105 m0106 m0107 m0108 m0109 m0110 m0111 m0112 m0113 m0114 m0115 m0116 m0117 m0118 m0119 m0120 m0121 m0122 m0123 m0124 m0125 m0126 m0127 m0128 m0129 m0130 m0131 m0132 m0133 m0134 m0135 m0136 m0137 m0138 m0139 m0140 m0141 m0142 m0143 m0144 m0145 m0146 m0147 m0148 m0149 m0150 m0151 m0152 m0153 m0154 m0155 m0156 m0157 m0158 m0159 m0160 m0161 m0162 m0163 m0164 m0165 m0166 m0167 m0168 m0169 m0170 m0171 m0172 m0173 m0174 m0175 m0176 m0177 m0178 m0179 m0180 m0181 m0182 m0183 m0184 m0185 m0186 m0187 m0188 m0189 m0190 m0191 m0192 m0193 m0194 m0195 m0196 m0197 m0198 m0199 m0200 m0201 m0202 m0203 m0204 m0205 m0206 m0207 m0208 m0209 m0210 m0211 m0212 m0213 m0214 m0215 m0216 m0217 m0218 m0219 m0220 m0221 m0222 m0223 m0224 m0225 m0226 m0227 m0228 m0229 m0230 m0231 m0232 m0233 m0234 m0235 m0236 m0237 m0238 m0239 m0240 m0241 m0242 m0243 m0244 m0245 m0246 m0247 m0248 m0249 m0250 m0251 m0252 m0253 m0254 m0255",
         "SADD kb256_2 m0000 m0001 m0002 m0003 m0004 m0005 m0006 m0007 m0008 m0009 m0010 m0011 m0012 m0013 m0014 m0015 m0016 m0017 m0018 m0019 m0020 m0021 m0022 m0023 m0024 m0025 m0026 m0027 m0028 m0029 m0030 m0031 m0032 m0033 m0034 m0035 m0036 m0037 m0038 m0039 m0040 m0041 m0042 m0043 m0044 m0045 m0046 m0047 m0048 m0049 m0050 m0051 m0052 m0053 m0054 m0055 m0056 m0057 m0058 m0059 m0060 m0061 m0062 m0063 m0064 m0065 m0066 m0067 m0068 m0069 m0070 m0071 m0072 m0073 m0074 m0075 m0076 m0077 m0078 m0079 m0080 m0081 m0082 m0083 m0084 m0085 m0086 m0087 m0088 m0089 m0090 m0091 m0092 m0093 m0094 m0095 m0096 m0097 m0098 m0099 m0100 m0101 m0102 m0103 m0104 m0105 m0106 m0107 m0108 m0109 m0110 m0111 m0112 m0113 m0114 m0115 m0116 m0117 m0118 m0119 m0120 m0121 m0122 m0123 m0124 m0125 m0126 m0127 m0128 m0129 m0130 m0131 m0132 m0133 m0134 m0135 m0136 m0137 m0138 m0139 m0140 m0141 m0142 m0143 m0144 m0145 m0146 m0147 m0148 m0149 m0150 m0151 m0152 m0153 m0154 m0155 m0156 m0157 m0158 m0159 m0160 m0161 m0162 m0163 m0164 m0165 m0166 m0167 m0168 m0169 m0170 m0171 m0172 m0173 m0174 m0175 m0176 m0177 m0178 m0179 m0180 m0181 m0182 m0183 m0184 m0185 m0186 m0187 m0188 m0189 m0190 m0191 m0192 m0193 m0194 m0195 m0196 m0197 m0198 m0199 m0200 m0201 m0202 m0203 m0204 m0205 m0206 m0207 m0208 m0209 m0210 m0211 m0212 m0213 m0214 m0215 m0216 m0217 m0218 m0219 m0220 m0221 m0222 m0223 m0224 m0225 m0226 m0227 m0228 m0229 m0230 m0231 m0232 m0233 m0234 m0235 m0236 m0237 m0238 m0239 m0240 m0241 m0242 m0243 m0244 m0245 m0246 m0247 m0248 m0249 m0250 m0251 m0252 m0253 m0254 m0255"],
        ["SINTER", "kb256_1", "kb256_2"],
    ),
    "sinter_big": (
        ["SADD bg1 " + " ".join(f"m{i:04d}" for i in range(512)),
         "SADD bg2 " + " ".join(f"m{i:04d}" for i in range(512))],
        ["SINTER", "bg1", "bg2"],
    ),
    # (frankenredis-gein3) SINTER's siblings at large RESULT cardinality. Every other
    # set-algebra shape in this file is seeded at two or three members, which is the near
    # side of a crossover SINTER was shown to have: it read 0.52x at k=2 and 1.38x at
    # k=512, so "SUNION/SDIFF/SINTERSTORE are comfortably ahead" is a statement about
    # their SEED, not about the commands. These three make the far side visible.
    #
    # SUNION returns 1024 (disjoint halves) and SDIFF 512 (nothing shared), so each one
    # emits a large reply; SINTERSTORE intersects 512 and writes them to a key instead of
    # replying, which is the control that separates REPLY-DELIVERY cost from the set work
    # -- the distinction that decided SINTER, where fr's deficit turned out to be
    # reply-bound rather than probe-bound.
    "sunion_big": (
        ["SADD ub1 " + " ".join(f"m{i:04d}" for i in range(512)),
         "SADD ub2 " + " ".join(f"n{i:04d}" for i in range(512))],
        ["SUNION", "ub1", "ub2"],
    ),
    "sdiff_big": (
        ["SADD db1 " + " ".join(f"m{i:04d}" for i in range(512)),
         "SADD db2 " + " ".join(f"n{i:04d}" for i in range(512))],
        ["SDIFF", "db1", "db2"],
    ),
    # (frankenredis-gein3) THE ISOLATION SHAPE. sinter_big says fr is 1.1193x at k=512;
    # sinterstore_big does NOT explain why, because it does not REMOVE the reply, it swaps
    # it for a destination-set build that costs MORE than the reply did (+531,769 instr/op
    # for fr). A control that substitutes costlier work cannot isolate the cost it replaced.
    #
    # SINTERCARD does the SAME probes and emits ONE INTEGER. Verified in source, because
    # "same command family" is not the same as "same code path":
    #   - Store::sintercard's generic arm is `for member in min_set.iter() { for s in
    #     &other_sets { if !s.contains(member) ... } }` — structurally identical to
    #     sinter_borrow_scan's loop, through the same GenericSet::contains.
    #   - Members are STRINGS, so `min_set.as_int_slice()` is None and the int-set fast
    #     path (which SINTER would not take either) is bypassed.
    #   - NO `LIMIT`, so `declustered` is false and the scan is start=0 stride=1, i.e. the
    #     same order and the same number of probes rather than an early-stopping sample.
    #
    # So sinter_big MINUS sintercard_big is fr's per-member EMIT cost, and the reply here
    # is small enough that this shape should be STABLE where sinter_big is 40 pct variable.
    "sintercard_big": (
        ["SADD cb1 " + " ".join(f"m{i:04d}" for i in range(512)),
         "SADD cb2 " + " ".join(f"m{i:04d}" for i in range(512))],
        ["SINTERCARD", "2", "cb1", "cb2"],
    ),
    "sinterstore_big": (
        ["SADD sb1 " + " ".join(f"m{i:04d}" for i in range(512)),
         "SADD sb2 " + " ".join(f"m{i:04d}" for i in range(512))],
        ["SINTERSTORE", "sbdst", "sb1", "sb2"],
    ),
    # (frankenredis-gein3) THE STABLE MID POINT, and it exists because the SINTER
    # crossover has been argued three times from two shapes that cannot settle it.
    # sinter_2 returns 2 members and reproduces to ~1.4 pct; sinter_big returns 512 and
    # does NOT reproduce -- 32 pct on the fr arm even after the harness drain fix,
    # because a reply that large takes many event-loop passes and fr pays per-pass
    # bookkeeping whose count depends on timing. Every per-member number in this ledger
    # so far, including the +39.3 pct per-member figure and the k=14 crossover, comes
    # from a two-point fit with sinter_big as one of the points.
    #
    # Same TWO keys as sinter_2 and the same fully overlapping seed, so sinter_2 ->
    # sinter_mid varies result cardinality and NOTHING else -- which is what a slope
    # requires and what sinter_9 (which varies KEY count instead) cannot give.
    #
    # MEASURED: fr reproduces to 0.07 pct here over four runs and redis to 0.62 pct.
    #
    # WHAT THIS PAIR DOES NOT LICENSE, learned by making the mistake: a two-point fit
    # is only a per-member cost if the cost is LINEAR in members, and SINTER's is not.
    # k=2->32 costs 910 instr per extra member and k=32->128 costs 3,568, in BOTH
    # engines -- 7.34x cost for 16x the members, then 11.84x for 4x. So any "instr per
    # member" quoted from two points is really an average over whatever regime changes
    # sit between them. Use three points and report the segments.
    "sinter_mid": (
        ["SADD md1 " + " ".join(f"m{i:04d}" for i in range(32)),
         "SADD md2 " + " ".join(f"m{i:04d}" for i in range(32))],
        ["SINTER", "md1", "md2"],
    ),
    # (frankenredis-gein3) THE BRACKET POINT. sinter_mid at k=32 is stable to 0.07 pct
    # and sinter_big at k=512 is not stable at all, so the transition is somewhere
    # between them. 128 members is ~1.4 KB of reply, just ABOVE the ~1 KB line where
    # this file's NOTE guard says the fr arm starts depending on timing, so this shape
    # is a direct test of that threshold rather than another data point beside it: if
    # the 1 KB line is right, this one is unstable and k=32 is the last clean point.
    "sinter_128": (
        ["SADD c81 " + " ".join(f"m{i:04d}" for i in range(128)),
         "SADD c82 " + " ".join(f"m{i:04d}" for i in range(128))],
        ["SINTER", "c81", "c82"],
    ),
    # (frankenredis-9hnxt) SINTER at NINE keys, the pair for sinter_2. keys_multi is
    # the only parser either SINTER call site uses and it refuses arr_len < 10, i.e.
    # fewer than nine keys -- and unlike MGET there are no exact-N SINTER parsers to
    # fall back to. So sinter_2 has NO borrowed route at all while sinter_9 does.
    # Same command, same executor, with and without a working parser: the pairing
    # isolates "no fast route" from "simply expensive", which a flat profile alone
    # cannot distinguish. Every set holds the same three members so the intersection
    # result is identical and only the key COUNT differs.
    "sinter_9": ([
        "SADD n1 m1 m2 m3", "SADD n2 m1 m2 m3", "SADD n3 m1 m2 m3",
        "SADD n4 m1 m2 m3", "SADD n5 m1 m2 m3", "SADD n6 m1 m2 m3",
        "SADD n7 m1 m2 m3", "SADD n8 m1 m2 m3", "SADD n9 m1 m2 m3",
    ], ["SINTER", "n1", "n2", "n3", "n4", "n5", "n6", "n7", "n8", "n9"]),
    # (frankenredis-gein3) LARGE-INTERSECTION SINTER. These exist because sinter_2 and
    # sinter_9 CANNOT measure anything that scales with the RESULT: every set they seed
    # holds the same three members, so both return 2-3 members no matter how many keys
    # are involved. That is deliberate and correct for what they isolate (with vs
    # without a borrowed parser), but it means any lever whose cost is O(result) scores
    # ~0 on them and gets filed REJECT on a FALSE NEGATIVE -- which then stops the next
    # person retrying it. fr sorts the SINTER reply (fr-store 20672/20741/20761) where
    # redis sorts nothing; that sort is O(k log k) in the RESULT, so it is invisible at
    # k=3 and only becomes measurable here.
    #
    # THE SHAPES FORM TWO ORTHOGONAL AXES, and reading a result without knowing which
    # axis it moves along will attribute the wrong cause:
    #
    #     sinter_2  vs  sinter_9              KEY COUNT varies (2 -> 9)
    #                                         result HELD at 2-3 members
    #     sinter_2  vs  sinter_large_generic  RESULT SIZE varies (2 -> 1000)
    #                                         key count HELD at 2, and since both are
    #                                         arity 3 they take the SAME dispatch route,
    #                                         so dispatch is held constant too
    #
    # That second pairing is the controlled one for anything O(result): per-key work,
    # dispatch and parser are all identical between the two, so a difference between
    # them is result-processing and nothing else. Do not compare sinter_9 against
    # sinter_large_generic -- that moves BOTH axes at once and is uninterpretable.
    #
    # BOTH ARMS ARE COVERED SEPARATELY AND MUST STAY THAT WAY. sinter_borrow_scan takes
    # a different path per encoding and its own comment records that a byte-sort of
    # decimal representations is NOT value order, so the intset arm materializes where
    # the generic arm streams borrowed refs. A single shape would exercise one of them
    # and silently leave the other unmeasured.
    #
    # The intset shape stays at 500 members on purpose: set-max-intset-entries defaults
    # to 512, so 1000 integers would convert away from intset and this would quietly
    # become a second copy of the generic arm.
    "sinter_large_generic": ([
        "SADD lg1 " + " ".join("s%d" % i for i in range(1000)),
        "SADD lg2 " + " ".join("s%d" % i for i in range(1000)),
    ], ["SINTER", "lg1", "lg2"]),
    "sinter_large_intset": ([
        "SADD li1 " + " ".join(str(i) for i in range(500)),
        "SADD li2 " + " ".join(str(i) for i in range(500)),
    ], ["SINTER", "li1", "li2"]),
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
    # (frankenredis-p98mw) ZADD at array length 6, the TWO-PAIR form, paired with
    # zadd_base (length 4) so the two differ only by pair count.
    #
    # The classifier claims ZADD at 4, 5 and >= 8-even. Six and seven are a GAP in the
    # middle of the claimed set -- not an exclusion at the edge, which is what the TOUCH
    # and MSETNX cases were -- and parse_borrowed_plain_zadd2_packet pins *6 with its
    # own executor call, so the shape has a route it cannot reach.
    #
    # Seeded with BOTH members already present so every op is an update returning 0 and
    # the sorted set never changes size. Seeding only "z 1 a" would make the first op
    # insert "b" and the rest update, averaging two paths into one figure.
    "zadd_2pair": (["ZADD z 1 a 2 b"], ["ZADD", "z", "1", "a", "2", "b"]),
    # (frankenredis-p98mw) ZRANK at the arity that was ALREADY classified (3) and the
    # WITHSCORE form at arity 4 that was not, so the pair differs only by the option.
    # Both are pure reads of an existing member, so every op is identical and the 2N run
    # does not diverge from the N run.
    "zrank_base": (["ZADD zk 1 m"], ["ZRANK", "zk", "m"]),
    "zrank_withscore": (["ZADD zk 1 m"], ["ZRANK", "zk", "m", "WITHSCORE"]),
    "zadd_xx_opt": (["ZADD z 1 a"], ["ZADD", "z", "XX", "1", "a"]),
    # (frankenredis-z2ce3) The OTHER arity-6 ZADD reading: `ZADD key flag1 flag2 score
    # member`, which parse_borrowed_plain_zadd_flag2_packet serves. Array length 6 is
    # shared with the two-pair form, and for a while the floor claimed both while its arm
    # called only the two-pair parser -- so this reading was claimed, declined, and sent
    # to the GENERIC path. It was invisible to the suite because zadd_2pair exercises the
    # form that WAS served and reads healthy either way; nothing here could tell a
    # half-chained arm from a working one. This shape is what makes that distinguishable.
    #
    # GT with a score EQUAL to the seeded one so the condition is false and the member is
    # never updated: every op is identical and the 2N run cannot diverge from the N run.
    # Using a HIGHER score would update on the first op and no-op thereafter, averaging
    # two paths into one figure -- the same construction error the zadd_2pair comment
    # above warns about.
    "zadd_2flag": (["ZADD z 1 a"], ["ZADD", "z", "GT", "CH", "1", "a"]),
    # (frankenredis-f3nry) The SECOND branch of the arity-5 arm, which is the same
    # blind spot one arity down. `zadd_xx_opt` above is the FIRST branch and reads
    # healthy whether or not the arm chains, exactly as zadd_2pair did at length 6 --
    # so nothing in this suite could see the INCR reading paying zadd_flag's failed
    # parse ahead of it. Pair it with zadd_xx_opt: same command, same array length,
    # same key, differing only in which branch of the arm serves them.
    #
    # INCREMENT OF ZERO, and that is load-bearing rather than lazy. ZADD INCR is a
    # MUTATING read: any nonzero step grows the score every op, so its bulk reply
    # gains digits across the run (3 -> 12000 over 4000 ops) and the double-to-string
    # cost is not constant -- the 2N run would then do strictly more work per op than
    # the N run and the two-point subtraction would attribute the difference to
    # dispatch. A zero step still takes the full INCR path, still replies with the
    # score, and leaves every op identical. Same failure this file's zadd_2flag and
    # zadd_2pair comments warn about, arrived at from the mutation side.
    "zadd_incr": (["ZADD z 5 a"], ["ZADD", "z", "INCR", "0", "a"]),
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
    # (frankenredis-f2zrr) THE CONTROL FOR THE SetOpt4 FUSION, and it is the whole reason
    # the fusion can be told apart from a reordering. SetOpt4 chains two same-arity
    # parsers: set_nx is tried FIRST and set_xx only after it fails, so `SET k v XX`
    # parses the identical 3-bulk prefix TWICE. NX is already first-in-group and pays one
    # parse, so a fusion must move XX down to NX's cost and leave NX UNCHANGED.
    #
    # That asymmetry is what makes this a real null rather than a convenient one: simply
    # SWAPPING the two arms would also fix XX, but it would push NX up by the same ~222
    # instr. A row where both move is a reordering wearing a fusion's costume.
    # Seeded so EVERY op is the same steady-state no-op (key exists -> NX declines, nil
    # reply, no write), rather than op 1 writing and the rest declining.
    "set_nx_opt": (["SET nxk2 v"], ["SET", "nxk2", "vvvvvvvvvvvvvvvv", "NX"]),
    "getex_base2": (["SET gx abcdefghijklmnop"], ["GETEX", "gx"]),
    "getex_ex_opt": (["SET gx abcdefghijklmnop"], ["GETEX", "gx", "EX", "100"]),
    "lpos_base": (["RPUSH l a b c d e"], ["LPOS", "l", "c"]),
    "lpos_count_opt": (["RPUSH l a b c d e"], ["LPOS", "l", "c", "COUNT", "1"]),
    "bitcount_base": (["SET bb abcdefghijklmnop"], ["BITCOUNT", "bb"]),
    "bitcount_range": (["SET bb abcdefghijklmnop"], ["BITCOUNT", "bb", "0", "5"]),
    # (frankenredis-f2zrr) The UNIT form at array length 5, the N+1 shape for the
    # arity-4 range claim. parse_borrowed_plain_bitcount_unit_packet exists and the
    # classifier claims BITCOUNT only at 2 and 4, so this is the same stranded-sibling
    # shape that measured 3.2848x on TOUCH and 2.6703x on MSETNX.
    #
    # WRITTEN BEFORE ANY LEVER, deliberately. On ZRANK I classified first and wrote the
    # shape after, which made that lever's delta permanently unmeasurable. The shape has
    # to exist first or the before/after cannot be taken.
    "bitcount_unit": (["SET bb abcdefghijklmnop"], ["BITCOUNT", "bb", "0", "5", "BYTE"]),
    # (frankenredis-2e4tq) The arity-keyed mis-claim, second instance. ZRANGE k s e
    # WITHSCORES and ZRANGE k s e REV are both *5, and (5, Zrange) maps
    # unconditionally to ZrangeWithscores.
    "zrange_ws": (["ZADD z 1 a 2 b 3 c"], ["ZRANGE", "z", "0", "-1", "WITHSCORES"]),
    "zrange_rev": (["ZADD z 1 a 2 b 3 c"], ["ZRANGE", "z", "0", "-1", "REV"]),
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


def total_events(path: str) -> dict:
    """Whole-process event totals, keyed by the dump's own `events:` header.

    (frankenredis-eh2ct) Reads every column rather than just Ir, so a cache-simulated
    run yields D1/LL misses from the same dump the instruction count comes from. The
    header is authoritative: callgrind emits a different column set depending on
    --cache-sim, and positionally assuming "Ir is first, misses are next" is how a
    row silently reports Dr as D1mr.
    """
    events, totals = None, None
    with open(path, "r", errors="replace") as fh:
        for line in fh:
            if line.startswith("events:"):
                events = line.split()[1:]
            elif line.startswith(("summary:", "totals:")):
                totals = [int(v) for v in line.split()[1:]]
                break
    if events is None or totals is None:
        raise RuntimeError("no events/summary line in %s" % path)
    return dict(zip(events, totals))


def total_ir(path: str) -> int:
    """Whole-process Ir from the callgrind summary line."""
    return total_events(path)["Ir"]


# (frankenredis-rzdi8, frankenredis-7so0e) Frames that are "getting to the command"
# rather than doing it. THE LIST NOW LIVES IN `frame_delta.py` and is imported there by
# `dispatch_share` below, because two copies drifted: this one was missing
# `classify_borrowed_dispatch_floor_packet` -- the floor classifier itself, worth 112-157
# instr/op -- so the metric steering the front-classification campaign was not counting
# the campaign's own function. One definition, one answer.


def dispatch_share(dump_n, dump_2n, ops, whole_op):
    """What fraction of a command is spent deciding WHICH command it is.

    Check this BEFORE reaching for a front-classification lever. A route can be
    below parity with dispatch NOT the story at all -- PEXPIRE is 1.04x on
    instructions with a 0.90 throughput ratio, so no dispatch lever can help it.
    Assuming instead of checking gets that case wrong.

    (frankenredis-7so0e) THIS USED TO TAKE THE 2N DUMP ALONE, and the caller then
    multiplied the resulting share by the clean two-point instructions/op. A share
    of one population -- a dump that still contains startup, seeding and teardown --
    times a rate from another is not a per-op quantity, and the error ran in BOTH
    directions: on `SORT_RO ... ALPHA` the old form printed 2,048.3 / 2,517.9 /
    2,535.3 / 3,358.5 across four arms whose true dispatch is 3,116.0 in every one
    of them, bit-identical across a 21x element span and two ELFs. It also
    manufactured a 487 instr/op "reduction" out of a change confined to a collation
    comparator. Four rows across two agents were withdrawn on it (`b9c288a1d`).

    It now differences the N and 2N dumps exactly like every other per-op number
    here, and it borrows `frame_delta.py`'s parser and frame list so the two tools
    cannot drift apart again -- the previous local copy of the list was missing
    `classify_borrowed_dispatch_floor_packet`, i.e. the floor classifier itself,
    which is 112-157 instr/op and hits CLASSIFIED routes hardest (58% of
    `get_control`'s dispatch against 7.6% of `SORT_RO`'s) on screens whose whole
    purpose is ranking classified against generic.

    Returns `(fraction_of_whole_op, top_frames)` so both call sites and the printed
    format are unchanged; `whole_op * fraction` is now the true per-op dispatch.
    Still a FLOOR, not a value: the frame list is hand-maintained.
    """
    import frame_delta

    try:
        rows, _process = frame_delta.frame_deltas(dump_n, dump_2n, ops)
    except (OSError, subprocess.SubprocessError, RuntimeError):
        return None
    disp = 0.0
    top = []
    for ipo, fn in rows:
        if any(d in fn for d in frame_delta.DISPATCH_FRAMES):
            disp += ipo
            top.append((ipo, fn))
    if not whole_op:
        return None
    return disp / whole_op, sorted(top, reverse=True)[:5]


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


# (frankenredis-94lp3 CORRECTION, 2026-08-17) PRESENCE IS NOT THE SIGNAL. COST IS.
#
# The rule above tested whether the frame names APPEAR in the annotate output. They
# appear in a front-classified route too, because SEEDING and STARTUP run through the
# generic path a handful of times before the measured burst begins — so every shape that
# carries seed commands was labelled GENERIC PATH regardless of the route it takes.
#
# MEASURED on `hget`, which commit 3ece2ea92 front-classified with a cached read gate:
#
#     dispatch_with_client_context      728 Ir      <- 0.01 pct of the run, ~0.18/op
#     execute_frame_internal          1,646 Ir
#     command_table_index             1,572 Ir
#     classify_command                  456 Ir
#     (the route itself is 1,784 instr/op x 4,000 ops = ~7.1M Ir)
#
# 728 instructions is a few CALLS, not 4,000 of them. A route on the generic path pays
# this frame on EVERY op and reads in the hundreds of instr/op; a classified one pays it
# only for whatever ran outside the burst. Three routes I measured in one sitting —
# `hget`, `type`, `ttl_nonvolatile` — were all mislabelled GENERIC, and the label sent me
# looking for a missing floor-table entry for TYPE that has existed all along.
#
# So the discriminator is the frame's SHARE, not its presence. The threshold is set well
# below any real generic route (they read 5-40 pct here) and well above seed residue
# (0.01 pct), so it does not need to be tuned finely to be right.
GENERIC_PATH_MIN_SHARE_PCT = 1.0

_ANNOTATE_ROW = re.compile(r"^\s*([\d,]+)\s*\(\s*([\d.]+)%\)", re.M)


def _frame_share_pct(annotate_text: str, frame: str) -> float:
    """Largest percentage callgrind_annotate attributes to a row naming `frame`.

    Returns 0.0 when the frame does not appear at all. Takes the MAX rather than the sum
    because a frame can legitimately appear on several rows (different call chains) and
    any one of them exceeding the bar is enough to say the path was taken per-op.
    """
    best = 0.0
    for line in annotate_text.splitlines():
        if frame not in line:
            continue
        match = _ANNOTATE_ROW.match(line)
        if match:
            best = max(best, float(match.group(2)))
    return best


def classify_dispatch_mechanism(annotate_text: str):
    """Pure half of `dispatch_mechanism`, so the rule is testable without a dump.

    Returns (label, frames_seen). GENERIC PATH requires the DISCRIMINATING frame to carry
    a material share, not merely to be present.
    """
    present = {f for f in GENERIC_PATH_FRAMES if f in annotate_text}
    markers = {m for m in GENERIC_PATH_MARKERS if m in annotate_text}
    seen = sorted(present | markers)
    share = _frame_share_pct(annotate_text, "dispatch_with_client_context")
    if share >= GENERIC_PATH_MIN_SHARE_PCT and len(present) == len(GENERIC_PATH_FRAMES):
        return "GENERIC PATH", seen
    if present or markers:
        # Say WHY it is not generic, so a reader does not re-derive this the hard way.
        return ("classified route (generic frames present but "
                "dispatch_with_client_context is %.2f pct — seed/startup residue, "
                "not per-op)" % share), seen
    return "classified route", seen


def classify_dispatch_mechanism_two_point(per_op_frames):
    """Mechanism from the TWO-POINT per-frame deltas, where the answer is categorical.

    (frankenredis-94lp3, 2026-08-17) MY OWN THRESHOLD WAS WEAKER THAN I CLAIMED, and this
    replaces it with a measurement that needs no threshold at all.

    The single-dump form above tests `dispatch_with_client_context`'s share against a 1.0 pct
    bar, and I justified that bar as "three orders of magnitude clear of both sides". That
    compared it to SEED RESIDUE at 0.01 pct, which is the wrong side to measure against: a
    gate's margin is set by the CLOSEST case on the far side of the bar, not the farthest.
    Measured across 35 dumps on disk, the closest GENERIC route sits at 2.56 pct — a margin of
    2.56x, not 1000x. A generic route roughly 3x more expensive than that one would fall under
    the bar and be reported CLASSIFIED, which is a false negative in exactly the direction that
    hides work. (`feedback_a_gate_must_link_its_threshold_to_its_own_null`, applied to my own
    gate rather than someone else's.)

    Two-point removes the judgement call. Startup and seeding appear identically in the N and
    2N dumps and cancel EXACTLY, so the frame's per-op cost is:

        23 front-classified shapes   0.000 instr/op   (exactly zero, not "small")
        12 generic-path shapes       2.562-5.043 pct of the op

    Zero versus nonzero is not a threshold, it is a fact about which code ran. `per_op_frames`
    is the {function: instr_per_op} mapping `frame_delta` already produces for the fixed
    `dispatch_share`, so this costs no extra work where that is already computed.
    """
    per_op = 0.0
    for fn, ir in per_op_frames.items():
        if "dispatch_with_client_context" in fn:
            per_op += ir
    if per_op > 0.0:
        return "GENERIC PATH", per_op
    return "classified route", per_op


def dispatch_mechanism(dump_path):
    """Which mechanism is this route paying: the parser walk, or the generic path?

    Returns (label, frames_found). The caller still needs the parse count: a route
    can pay the walk, the generic path, both, or neither.
    """
    out = subprocess.run(["callgrind_annotate", "--auto=no", "--threshold=99.5", dump_path],
                         capture_output=True, text=True, timeout=900).stdout
    return classify_dispatch_mechanism(out)


# (frankenredis-zw36c) Event-loop PASS COUNT per op, sampled per run.
#
# fr pays nine frames of bookkeeping ONCE PER EVENT-LOOP PASS whether or not there is work
# to do, and redis has no per-iteration counterpart of comparable weight. That tax has been
# unadjudicable because every instrument here divides by OPS while the cost is per PASS, so
# the denominator was never observed -- only inferred through a reply-size proxy. Two
# separate agents hit that wall and said so in the ledger (f43f75333 and the zw36c row).
#
# Both engines expose `eventloop_cycles` in INFO stats (redis 7.0+; fr mirrors the field),
# so the denominator is simply readable. Sampling it either side of the burst turns
# "passes per op" into a measured quantity and makes per-pass levers divisible for the
# first time.
#
# The two INFO commands are themselves ops and cost a pass or so each; against N=2000 that
# is under 0.1 pct and it biases BOTH arms identically, so it cannot manufacture a
# difference between them. Reported, not corrected for.
PASSES: dict[str, float] = {}


def _eventloop_cycles(sock) -> int | None:
    """Read `eventloop_cycles` from INFO stats, or None if the engine omits it.

    (frankenredis-gein3) THE COMPARABILITY OF THIS FIELD IS SOURCE-VERIFIED, because a
    `passes per op` ratio between two engines is meaningless if they count a "pass" at
    different granularities, and this harness has published a 1,700x difference on it:

      redis  server.c:1760 `durationAddSample(EL_DURATION_TYPE_EL, el_duration)` — once
             per event-loop iteration, in the beforeSleep/afterSleep pair.
      fr     main.rs:5438 `runtime.record_eventloop_cycle(..)` — ONE call site, in the
             main loop body, once per iteration.

    Both are once-per-iteration, so the ratio is real rather than an artefact of where
    the counter sits. IF A SECOND fr CALL SITE EVER APPEARS, or that one moves inside an
    inner loop, every `passes per op` row in the ledger silently becomes wrong — check
    this before trusting a pass-count comparison.

    Returns None, never 0, when the field is absent or unparseable: a silent zero would
    read as "this engine never cycles", which is the most flattering possible answer and
    would be indistinguishable from a genuinely tight loop.
    """
    sock.sendall(resp("INFO", "stats"))
    buf = b""
    while b"\r\n\r\n" not in buf and not buf.endswith(b"\r\n"):
        chunk = sock.recv(1 << 20)
        if not chunk:
            return None
        buf += chunk
        if b"eventloop_cycles" in buf and buf.endswith(b"\r\n"):
            break
    for line in buf.split(b"\r\n"):
        if line.startswith(b"eventloop_cycles:"):
            try:
                return int(line.split(b":", 1)[1])
            except ValueError:
                return None
    return None


class _FakeSock:
    """Minimal socket stand-in for `_eventloop_cycles`: replays a canned INFO reply."""

    def __init__(self, payload: bytes):
        self._payload = payload
        self._sent = False

    def sendall(self, _data):
        return None

    def recv(self, _n):
        if self._sent:
            return b""
        self._sent = True
        return self._payload


def _selftest_eventloop_cycles() -> None:
    """Guard the contract the `passes per op` metric rests on.

    The failure this exists for is NOT a crash: it is `_eventloop_cycles` returning 0
    instead of None when a field is missing, which would report an engine as doing zero
    event-loop passes — the most flattering answer available, and one that looks like a
    real result rather than a parse failure.
    """
    ok = _eventloop_cycles(_FakeSock(b"# Stats\r\neventloop_cycles:12345\r\n"))
    assert ok == 12345, ok

    # Field present among many, with the interesting neighbours upstream also emits.
    many = (
        b"# Stats\r\nexpired_keys:3\r\neventloop_cycles:77\r\n"
        b"eventloop_duration_sum:900\r\ninstantaneous_eventloop_cycles_per_sec:4\r\n"
    )
    assert _eventloop_cycles(_FakeSock(many)) == 77

    # Upstream's real neighbour: `instantaneous_eventloop_cycles_per_sec` contains
    # `eventloop_cycles` as a substring. It is safe for a simple reason -- the colon
    # disambiguates -- so on its own it does NOT test prefix-vs-substring matching, and
    # claiming it did would be a test that names an invariant without exercising it.
    only_instantaneous = b"# Stats\r\ninstantaneous_eventloop_cycles_per_sec:4\r\n"
    assert _eventloop_cycles(_FakeSock(only_instantaneous)) is None

    # THIS is the case that discriminates: a field ENDING in the name we want, colon and
    # all. A prefix match rejects it; a substring match would return 9 and silently report
    # another engine's counter as ours. Verified by mutation: relaxing the check to
    # `b"eventloop_cycles:" in line` reddens here and nowhere else.
    suffixed = b"# Stats\r\nshard_eventloop_cycles:9\r\n"
    assert _eventloop_cycles(_FakeSock(suffixed)) is None

    # Absent, empty and unparseable must all be None -- never 0.
    assert _eventloop_cycles(_FakeSock(b"# Stats\r\nexpired_keys:3\r\n")) is None
    assert _eventloop_cycles(_FakeSock(b"")) is None
    assert _eventloop_cycles(_FakeSock(b"# Stats\r\neventloop_cycles:abc\r\n")) is None


def _sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def emit_bench_elf_sha(engine: str, tag: str) -> str:
    """Report the SHA-256 of the ELF this arm is about to run, and prove it held still.

    A TRUE `/proc/self/exe` self-report is IMPOSSIBLE for this harness and it is worth
    saying why rather than leaving the next reader to assume it was skipped out of
    laziness. Every arm runs under callgrind, and `/proc/<pid>/exe` of that process
    resolves to `/usr/libexec/valgrind/callgrind-amd64-linux`, not to the engine —
    verified directly, 2026-08-17. The guest ELF never appears as anybody's `exe`.

    So this hashes the file, which leaves exactly one hole: the binary being REPLACED
    between the hash and the run. `target/release/<bin>` is a rendezvous in a shared
    checkout, so that is a real event here, not a hypothetical. The caller closes it by
    re-hashing after the arm finishes and refusing to report a SHA that moved.
    """
    return _sha256_file(engine)


def _wipe(path: str) -> None:
    """Remove a per-point working directory. Confined to paths under the harness workdir."""
    shutil.rmtree(path, ignore_errors=True)


def run_once(engine: str, seeds, cmd, ops: int, workdir: str, tag: str,
             locale: str | None = None) -> int:
    sha_before = emit_bench_elf_sha(engine, tag)
    out = os.path.join(workdir, "cg.%s.out" % tag)
    port = free_port()
    argv = ["valgrind", "--tool=callgrind", "--callgrind-out-file=" + out,
            "--cache-sim=%s" % ("yes" if CACHE_SIM[0] else "no"),
            # --branch-sim stays OFF permanently: it was measured as a PROXY for the
            # hardware branch-miss axis and REJECTED, because simulated mispredicts
            # came out at parity while hardware showed 6x. Leaving it off keeps a
            # known-useless column out of the dump. (frankenredis-lua census)
            "--branch-sim=no",
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
    # PER POINT, not per harness run (frankenredis-pcio8). Both slope points used the shared
    # `workdir` as cwd AND as the engine's data dir. A slope's two points must differ in
    # NOTHING but the repeated operation, and they did not: whichever point ran first left
    # state behind that the second then booted on, so the second paid startup work the first
    # never did and the subtraction stopped cancelling exactly. The two ENGINES shared the
    # directory too, so redis could boot onto a file fr had written. The same defect was
    # already fixed in command_profile_frames.py; this is the other half of that bead.
    point = os.path.join(workdir, "point.%s" % tag)
    _wipe(point)
    os.makedirs(point, exist_ok=True)
    argv = argv + ["--dir", point]
    proc = subprocess.Popen(argv, cwd=point, env=env,
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
            # (frankenredis-sf510) A seed may be a LIST of already-separated arguments as well
            # as a whitespace-splittable string. Splitting is fine for `SET k v` and impossible
            # for anything whose argument legitimately CONTAINS spaces -- a Lua body handed to
            # SCRIPT LOAD or FUNCTION LOAD is one argument with spaces and newlines in it, and
            # `.split()` would shatter it into a wrong-arity command that fails at seed time.
            # Strings keep their existing behaviour exactly, so no registered shape changes.
            sock.sendall(resp(*(seed if isinstance(seed, (list, tuple)) else seed.split())))
            seed_counter = ReplyCounter()
            while seed_counter.complete < 1:
                chunk = sock.recv(1 << 20)
                if not chunk:
                    raise RuntimeError("%s dropped the connection while seeding" % tag)
                seed_counter.feed(chunk)
        # (frankenredis-gein3) DRAIN WHILE SENDING. This loop used to be
        # `sendall(payload)` followed by a recv loop, which never read a byte until
        # the last command had been handed to the kernel. For a shape whose reply is
        # a few bytes that is harmless -- the replies fit in the socket buffers and
        # nothing accumulates. For a shape with a LARGE reply it is fatal to the
        # measurement: sinter_big returns 512 members (~5.6 KB), so at 800 ops the
        # server is holding megabytes it cannot write, its write buffer grows, and
        # how much growth work it does depends on WHEN the kernel accepts writes
        # relative to when the client happens to call recv. That is timing, and
        # timing under valgrind is not reproducible.
        #
        # MEASURED, which is why this changed: three runs of the same ELF and shape
        # at N=800 gave fr 630,043 / 472,292 / 552,633 instr/op -- a 33.4 pct spread
        # and a vs-redis ratio ranging 1.0139 to 1.3565 -- while redis, which appends
        # reply BLOCKS instead of growing one buffer, stayed inside 1.5 pct across
        # every N. The same shape at N=400 reproduced to 1.7 pct. The instrument, not
        # the engine, decided where the crossover appeared.
        #
        # Selecting on writability and readability together keeps the server's INPUT
        # pipelined exactly as before -- it still sees a continuous command stream and
        # still batches per wakeup -- while bounding what it has to buffer on the way
        # out. Small-reply shapes are unaffected by construction (they never backlog),
        # and that is asserted rather than assumed: see the reproduction table in the
        # commit that introduced this.
        cycles_before = _eventloop_cycles(sock)
        payload = resp(*cmd) * ops
        counter = ReplyCounter()
        sock.setblocking(False)
        sent = 0
        reply_bytes = [0]
        try:
            while counter.complete < ops:
                want_write = sent < len(payload)
                readable, writable, _ = select.select(
                    [sock], [sock] if want_write else [], [], 300)
                if not readable and not writable:
                    raise RuntimeError(
                        "%s stalled mid-burst after %d of %d replies (%d of %d bytes sent)"
                        % (tag, counter.complete, ops, sent, len(payload)))
                if writable:
                    try:
                        sent += sock.send(payload[sent:sent + (1 << 20)])
                    except BlockingIOError:
                        pass
                if readable:
                    try:
                        chunk = sock.recv(1 << 20)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        raise RuntimeError(
                            "%s dropped the connection mid-burst after %d of %d replies"
                            % (tag, counter.complete, ops))
                    reply_bytes[0] += len(chunk)
                    counter.feed(chunk)
        finally:
            sock.setblocking(True)
        cycles_after = _eventloop_cycles(sock)
        if cycles_before is not None and cycles_after is not None:
            PASSES[tag] = (cycles_after - cycles_before) / ops
    finally:
        if sock is not None:
            sock.close()
        proc.terminate()
        try:
            proc.wait(timeout=180)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=60)
    # The ELF must be the SAME one this arm started with. In a shared checkout
    # `target/release/<bin>` is a rendezvous, so a peer's build landing mid-arm would
    # otherwise be reported under the SHA of a binary that no longer exists.
    sha_after = _sha256_file(engine)
    if sha_after != sha_before:
        raise RuntimeError(
            "%s: the ELF CHANGED under the arm (%s then %s). This measurement is void; "
            "copy the binary to a private path and re-run." % (tag, sha_before, sha_after))
    print("    %-6s bench_elf_sha256=%s  (harness-computed and re-verified after the "
          "arm; NOT a /proc/self/exe self-report, which callgrind makes impossible)"
          % (tag, sha_before), flush=True)
    # (frankenredis-gein3) ADMISSIBILITY GUARD, and it exists because a row was banked
    # that this would have stopped: SINTER "1.3764x at 512 members", from one pair on a
    # shape whose fr arm spans 1.0139x to 1.3565x run to run.
    #
    # The defect class is specific. Above roughly a kilobyte of reply per op the server
    # cannot hand every reply to the kernel as it is produced, so the event loop spins
    # more times to write them out, and fr pays a FIXED bookkeeping tax per iteration --
    # a WriterCompletion channel try_recv, drain_writer_completions,
    # drain_pubsub_outboxes, drain_sharded_set_get_completions, apply_pending_client_
    # unblocks, deliver_monitor_output and two clock reads -- whether or not there is
    # anything to do. Iteration count depends on timing; timing under valgrind is not
    # reproducible; so fr's instruction count stops being a function of the workload.
    # Redis has no per-iteration counterpart and its arm stays inside 1.5 pct.
    #
    # Callgrind's determinism covers the COUNTER, not the WORKLOAD. This prints the
    # reply volume so the reader can see which regime a shape is in.
    #
    # DELETION CONDITION: remove this when fr's per-iteration drains are guarded on
    # cheap emptiness checks, and the same three-run spread on sinter_big comes in
    # under the ~0.6 pct instr/op noise floor.
    # THRESHOLD CORRECTED BY MEASUREMENT (frankenredis-gein3). I first set this line at
    # 1 KB from the sinter_big evidence alone, and sinter_128 then falsified it: at
    # 1,414 bytes/op its fr arm reproduces to 0.037 pct over four runs, tighter than
    # any small shape in the corpus. The stable/unstable bracket that is actually
    # measured is 1,414 B/op stable against 5,638 B/op unstable, so the line goes
    # between them and the wording drops the claim that everything above it IS
    # timing-dependent -- which was an inference, not an observation.
    per_op = reply_bytes[0] / ops if ops else 0
    if per_op > 2048 and tag.startswith("fr"):
        print("  NOTE %-6s reply volume %.0f bytes/op. MEASURED: stable at 1,414 B/op"
              " (sinter_128, 0.04 pct over four runs), NOT stable at 5,638 B/op"
              " (sinter_big, 12.8-33.4 pct). This shape is above the last point known"
              " stable, so repeat it and quote a range rather than banking one pair."
              % (tag, per_op), file=sys.stderr)
    return total_ir(out)


def window_provenance() -> str:
    """One line describing the WINDOW a measurement was taken in.

    (frankenredis-1cmy9) This harness recorded nothing about the window, and its
    REDIS arm is window-sensitive in a way its fr arm is not: `serverCron` does work
    proportional to ELAPSED time, so a slower or more contended window makes redis
    retire MORE instructions per op while fr's single-threaded loop is unaffected.
    Measured consequence, same shape and same binaries within one sitting: the redis
    arm of `sort_ro_alpha` read 1613.3, 1654.8, 2373.4, 2407.1 and 4707.5 instr/op --
    a 2.9x spread on the DENOMINATOR -- with nothing in the output to tell the runs
    apart. Two rows from different windows were therefore not comparable and looked
    it, which is exactly what the standing orders' load-and-MHz provenance rule
    exists to prevent.

    Cheap enough to call per arm: two small /proc reads, no subprocess.
    """
    try:
        with open("/proc/loadavg") as handle:
            loadavg = " ".join(handle.read().split()[:3])
    except OSError:
        loadavg = "unknown"
    mhz = "unknown"
    try:
        with open("/proc/cpuinfo") as handle:
            speeds = [
                float(line.split(":", 1)[1])
                for line in handle
                if line.startswith("cpu MHz")
            ]
        if speeds:
            # Mean AND max: the governor varies per core, so a single core's figure
            # is not the machine's. A wide mean-to-max gap is itself the warning.
            mhz = "mean %.0f max %.0f" % (sum(speeds) / len(speeds), max(speeds))
    except OSError:
        pass
    return "loadavg %s | cpu MHz %s" % (loadavg, mhz)


def instr_per_op(engine: str, seeds, cmd, ops: int, workdir: str, label: str,
                 locale: str | None = None):
    # Provenance is captured per ARM and on BOTH sides of it, because the window can
    # move mid-arm: a drift between these two lines is the reason to distrust the row.
    before = window_provenance()
    low = run_once(engine, seeds, cmd, ops, workdir, label + ".n", locale)
    high = run_once(engine, seeds, cmd, ops * 2, workdir, label + ".2n", locale)
    print("  %-5s window: %s" % (label, before))
    print("  %-5s window: %s   (after)" % (label, window_provenance()))
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
    """Prove the reply counter on the streams that broke the old CRLF count, and the
    `eventloop_cycles` parser contract the `passes per op` metric rests on.

    Each case carries the count the OLD `chunk.count(b"\\r\\n")` would have
    produced, so the test shows the defect rather than only asserting the fix:
    a case where the two agree proves nothing, and every multi-line case is one
    where the old code overcounted and stopped the burst early.
    """
    # (frankenredis-gvm6z) THE "did you mean" CONTRACT. The failure is one I made twice
    # in one window: `corpus_coverage.py` reports COMMAND names and this harness keys on
    # SHAPE names, so `hset` and `mset` were passed here, produced no fr arm, and the
    # reply was a generic usage line that mentioned neither the shape nor the right name.
    # Two runs were lost to it. These pin the two real cases plus the prefix fallback.
    assert "hset_same" in suggest_shapes("hset"), suggest_shapes("hset")
    assert "mset_2" in suggest_shapes("mset"), suggest_shapes("mset")
    assert "zrangestore_all" in suggest_shapes("zrangestore"), suggest_shapes("zrangestore")
    # A name matching nothing must yield nothing rather than a misleading suggestion.
    assert suggest_shapes("zzzzzzzzzzzz_no_such", ["get_control"]) == []

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
    _selftest_eventloop_cycles()
    print("  %-26s None-not-zero contract  ok" % "eventloop_cycles parser")

    # (frankenredis-94lp3 CORRECTION) The mechanism label must key on COST, not presence.
    # Case 1 is the REAL hget annotate shape, numbers copied from the dump: every generic
    # frame present, all of them seed/startup residue. The old presence rule called this
    # GENERIC PATH, which is how three front-classified routes got mislabelled in one
    # sitting. Case 2 is a genuine generic route, where the same frame carries per-op cost.
    classified_text = (
        "     1,646 ( 0.02%)  ???:<fr_runtime::Runtime>::execute_frame_internal [x]\n"
        "     1,572 ( 0.02%)  ???:fr_command::command_table_index [x]\n"
        "       728 ( 0.01%)  ???:<fr_runtime::Runtime>::dispatch_with_client_context [x]\n"
        "       456 ( 0.01%)  ???:fr_command::classify_command [x]\n"
        "       120 ( 0.00%)  ???:fr_command::push_ascii_lowercase_lossy [x]\n"
    )
    generic_text = (
        " 1,204,331 (14.90%)  ???:<fr_runtime::Runtime>::execute_frame_internal [x]\n"
        "   372,118 ( 4.60%)  ???:fr_command::command_table_index [x]\n"
        "   688,004 ( 8.51%)  ???:<fr_runtime::Runtime>::dispatch_with_client_context [x]\n"
        "   201,776 ( 2.50%)  ???:fr_command::classify_command [x]\n"
        "   150,900 ( 1.87%)  ???:fr_command::push_ascii_lowercase_lossy [x]\n"
    )
    # (frankenredis-94lp3) The TWO-POINT form, and the margin measurement that motivated it.
    # Case 1 is every front-classified shape measured: the frame's per-op delta is EXACTLY
    # zero, because it runs only during seed/startup and cancels in the subtraction. Case 2 is
    # the closest generic route on record (2.56 pct of its op) — the case that sets the single
    # dump gate's real margin at 2.56x, not the 1000x I originally claimed against seed residue.
    for frames, expect, name in (
            ({"<fr_runtime::Runtime>::dispatch_with_client_context": 0.0,
              "frankenredis::process_buffered_frames": 112.0}, "classified route",
             "two-point: classified"),
            ({"<fr_runtime::Runtime>::dispatch_with_client_context": 330.0,
              "frankenredis::process_buffered_frames": 280.0}, "GENERIC PATH",
             "two-point: generic")):
        got, per_op = classify_dispatch_mechanism_two_point(frames)
        if got == expect:
            print("  %-26s %s (%.1f instr/op)  ok" % ("mechanism: " + name, expect, per_op))
        else:
            failures += 1
            print("  %-26s FAIL: got %r, wanted %s" % ("mechanism: " + name, got, expect))

    for label_text, expect, name in (
            (classified_text, "classified", "seed residue (real hget dump)"),
            (generic_text, "GENERIC PATH", "true generic route")):
        got, _frames = classify_dispatch_mechanism(label_text)
        if got.startswith(expect):
            print("  %-26s %s  ok" % ("mechanism: " + name, expect))
        else:
            failures += 1
            print("  %-26s FAIL: got %r, wanted %s" % ("mechanism: " + name, got, expect))
    # The defect itself, pinned: under the OLD presence-only rule the hget dump satisfied
    # every condition. If someone reverts to presence testing, this goes red.
    _old_rule_would_say_generic = (
        all(f in classified_text for f in GENERIC_PATH_FRAMES)
        and any(m in classified_text for m in GENERIC_PATH_MARKERS))
    if not _old_rule_would_say_generic:
        failures += 1
        print("  %-26s FAIL: the regression fixture no longer reproduces the old defect"
              % "mechanism: fixture")
    else:
        print("  %-26s old presence rule would say GENERIC  ok" % "mechanism: defect shown")

    # (frankenredis-ozrro) Argument parsing must not depend on flag POSITION. The
    # regression this pins: `<bin> <shape> --fr-only` read "--fr-only" as the ops count
    # and died, and since the traceback goes to stderr a caller grepping stdout for a
    # result line saw an empty arm rather than a failure.
    argcases = [
        (["fr", "get_control"], ["fr", "get_control"]),
        (["fr", "get_control", "--fr-only"], ["fr", "get_control"]),
        (["fr", "get_control", "4000", "--fr-only"], ["fr", "get_control", "4000"]),
        (["fr", "get_control", "--fr-only", "4000"], ["fr", "get_control", "4000"]),
        (["--fr-only", "fr", "get_control"], ["fr", "get_control"]),
        (["fr", "get_control", "--locale=C", "--fr-only"], ["fr", "get_control"]),
    ]
    for given, want in argcases:
        got = _positional_args(given)
        if got != want:
            failures += 1
            print("  %-26s FAIL: %r -> %r, want %r" % ("arg parsing", given, got, want))
    # And the ops default must survive a trailing flag rather than raising.
    for given, want_ops in [(["fr", "s"], 2000), (["fr", "s", "--fr-only"], 2000),
                            (["fr", "s", "500", "--fr-only"], 500)]:
        p = _positional_args(given)
        ops = int(p[2]) if len(p) > 2 else 2000
        if ops != want_ops:
            failures += 1
            print("  %-26s FAIL: %r -> ops=%d, want %d" % ("ops default", given, ops, want_ops))
    print("  %-26s flag position independent  ok" % "arg parsing")

    # (frankenredis-ozrro) The null-noise helpers, pinned against the two live claims they
    # were derived to adjudicate. If either assertion flips, a banked conclusion is wrong.
    #
    #   ZINTERCARD arity 4 vs arity 6 miss tax: 348.5 vs 352.1, arms ~6,851 and ~7,454.
    #   Banked as FLAT. Must be inside noise.
    #
    #   LCS vs ZINTERCARD miss tax: 302.2 vs 348.5, arms ~6,798 and ~6,851.
    #   Banked as REAL and command-specific. Must be far outside noise.
    flat = delta_sigma(352.1 - 348.5, 6851.0, 7454.0)
    real = delta_sigma(348.5 - 302.2, 6851.0, 6798.0)
    if not flat < 1.0:
        failures += 1
        print("  %-26s FAIL: arity dose-response reads %.2f sigma, banked as flat"
              % ("null noise", flat))
    # (frankenredis-gvm6z) I briefly lowered this to 3.0 on a sample-size-confounded noise
    # re-derivation and have restored it. See NULL_HALF_RANGE_PCT above: the demotion of this
    # claim from 7.16 to 3.81 sigma is WITHDRAWN.
    if not real > 5.0:
        failures += 1
        print("  %-26s FAIL: lcs-vs-zintercard reads %.2f sigma, banked as real"
              % ("null noise", real))
    # A delta must be scored against BOTH arms in quadrature, never one arm alone --
    # single-arm noise overstates significance by ~1.4x.
    one_arm = 100.0 / null_noise_instr(7000.0)
    two_arm = delta_sigma(100.0, 7000.0, 7000.0)
    if not abs(one_arm / two_arm - 2 ** 0.5) < 0.01:
        failures += 1
        print("  %-26s FAIL: quadrature not applied to delta noise" % "null noise")
    # The gate must sit ABOVE the median OBSERVED deviation, or half of all good runs fail
    # by construction -- the frankenpandas failure this was computed in answer to.
    #
    # (frankenredis-gvm6z) THIS COMPARISON USED TO BE `NULL_GATE_PCT > NULL_HALF_RANGE_PCT`,
    # which is `3 * x > x`: a TAUTOLOGY. It could not fail for any positive constant, so the
    # guard written to catch frankenpandas's failure was structurally incapable of catching
    # it. Comparing against the OBSERVED sample instead makes it falsifiable -- and it is
    # now close, 1.04x, because a quiet six-draw group came in at 0.320 pct.
    observed = sorted(NULL_OBSERVED_HALF_RANGE_PCT)
    mid = len(observed) // 2
    observed_median = (observed[mid] if len(observed) % 2
                       else (observed[mid - 1] + observed[mid]) / 2)
    if not NULL_GATE_PCT > observed_median:
        failures += 1
        print("  %-26s FAIL: gate %.3f%% at or below OBSERVED median %.3f%%"
              % ("null noise", NULL_GATE_PCT, observed_median))
    print("  %-26s flat=%.2f sigma  real=%.1f sigma  gate=%.3f%% vs observed median"
          " %.3f%% (%.2fx)  ok"
          % ("null noise", flat, real, NULL_GATE_PCT, observed_median,
             NULL_GATE_PCT / observed_median))

    print("selftest: %d case(s) failed" % failures)
    return 1 if failures else 0


def provenance_self_test() -> int:
    """`--self-test`: the incumbent-provenance guard, including the case it exists for."""
    real = "Redis server v=7.2.4 sha=d2c8a4b9:0 malloc=jemalloc-5.3.0 bits=64"
    ok, msg = incumbent_provenance(real, "d2c8a4b91e8c9f")
    assert ok, "a matching clean build must verify: %s" % msg
    # THE CROSS-PROJECT CASE. franken_networkx measured through an artifact 2,751 lines and
    # twelve days behind its repo and it INVERTED a ratio by 5.4x. Here that is the vendored
    # source moving while the checked-in binary does not.
    ok, msg = incumbent_provenance(real, "ffffffff1234")
    assert not ok and "INCUMBENT DRIFT" in msg, "moved HEAD must refuse: %s" % msg
    # A dirty build has no identifiable source at all.
    ok, msg = incumbent_provenance("Redis server v=7.2.4 sha=d2c8a4b9:1", "d2c8a4b91e8c")
    assert not ok and "DIRTY" in msg, "dirty build must refuse: %s" % msg
    ok, _ = incumbent_provenance("Redis server v=7.2.4", "d2c8a4b91e8c")
    assert not ok, "a version string with no sha stamp must refuse"
    ok, _ = incumbent_provenance(real, None)
    assert not ok, "an unreadable source HEAD must refuse -- unverifiable is not verified"
    # Short-vs-long sha comparison must not false-positive on a prefix mismatch.
    ok, _ = incumbent_provenance(real, "d2c8a4b8ffff")
    assert not ok, "a one-character sha difference must refuse"
    # And the LIVE tree, so the test fails if this checkout's own arms have drifted.
    live_ok, live_msg = _check_incumbent(
        REDIS, os.path.join(ROOT, "legacy_redis_code/redis"))
    assert live_ok, "this checkout's incumbent arm is not verifiable: %s" % live_msg
    print("  incumbent provenance guard OK  (%s)" % live_msg)
    print("PASS shape_instr_per_op self-test")
    return 0


# (frankenredis-ozrro) MEASURED A/A precision of this harness's fr arm, so a future claim
# can be checked against noise instead of eyeballed. Median half-range over NINE groups of
# same-(ELF, arm, shape) repeats accumulated across this campaign:
#
#     min 0.011%   median 0.067%   max 0.481%
#
# The max is the one pair measured across a loadavg 13->44 spike and is the ONLY group that
# would fail a 3x-median gate. This was computed in answer to a fleet-wide check prompted by
# frankenpandas, whose 2% null limit sat exactly AT its median deviation, so half its runs
# failed by construction and good measurements were discarded. This harness had no null gate
# at all, which is the opposite failure: nothing was being discarded, and nothing was being
# checked either.
# (frankenredis-gvm6z) MEASURED, then CORRECTED -- and the correction restores this constant
# to the 0.067 it had. I briefly changed it to 0.126 and I was wrong; the reasoning was
# sample-size confounded and this is the record of that.
#
# WHAT I MEASURED, in the quietest window of the campaign (1-min 9.3-10.1 with the 5- and
# 15-minute at 6.2-6.9 and 5.8-6.0), six `--fr-only` draws each of two shapes on ONE ELF:
#
#     get_control  mean 1,305.2   stdev 3.12 = 0.239 pct   half-range 3.70 = 0.283 pct
#     lcs_2        mean 7,290.5   stdev 6.52 = 0.089 pct   half-range 9.20 = 0.126 pct
#
# THE VALID FINDING: noise as a PERCENTAGE is size-dependent. Both groups are n=6, so sample
# size cancels between them, and the small shape is 2.7x the large one by stdev (2.25x by
# half-range). A single scalar percentage cannot describe both regimes. `null_noise_instr`
# takes the arm's magnitude already, so the fix is to make the PERCENTAGE a function of it --
# stdev/sqrt(size) is 0.0864 and 0.0764, agreeing to 13 pct, so k*sqrt(x) is the shape to
# test. That needs a THIRD size and must be measured in a window with no build running: a
# noisy window inflates the spread, which can only spuriously favour the flat-percentage
# model, so measuring it badly gives a wrong answer in a KNOWN direction.
#
# THE ERROR I MADE, and it is the reason this comment is long: I compared my 6-draw
# HALF-RANGE against a constant calibrated from groups whose n was never recorded, and
# concluded the constant was 1.9-4.2x too small. Half-range is SAMPLE-SIZE DEPENDENT --
# E[range] = d2(n)*sigma, and d2(6)/d2(2) = 2.25 -- so a 6-draw half-range is 2.25x a 2-draw
# one for IDENTICAL underlying noise. My 0.126 pct expressed as a 2-draw equivalent is
# 0.056 pct, which is BELOW 0.067. If those nine groups were pairs, this constant was never
# too small and may be slightly conservative.
#
# So the constant goes back to 0.067 and the assertion it pins goes back to 5.0. I had
# demoted a banked 7.16-sigma conclusion to 3.81 on that confounded comparison; that demotion
# is WITHDRAWN. The lesson worth keeping: never compare dispersion statistics across groups of
# different or unrecorded n. Report SIGMA, which is n-independent, not half-range.
NULL_HALF_RANGE_PCT = 0.067
# (frankenredis-gvm6z) OBSERVED half-ranges, so the gate can be checked against DATA rather
# than against the constant it is algebraically derived from. The nine groups above were
# recorded only as min/median/max, so those three are all that can be reconstructed of them;
# the fourth is a group of six get_control draws measured on one ELF at loadavg 14.09-14.29
# with the 1-minute FLAT across all six -- quiet, unspiked, and 4.8x the calibrated median.
# It is recorded here because the calibration attributes its own 0.481 max to a loadavg
# 13->44 spike, and this group had no spike to blame.
# (frankenredis-gvm6z) NOT COMPARABLE ACROSS n, and left as the pair-scale sample it was:
# my two n=6 groups (0.283, 0.126) belong on a 2.25x-larger scale than any pair in it, so
# adding them would raise this median for a reason that is arithmetic, not noise. Their
# 2-draw equivalents are 0.126 and 0.056.
NULL_OBSERVED_HALF_RANGE_PCT = [0.011, 0.067, 0.481, 0.320]
# 3x the median: loose enough that 8 of 9 observed groups pass, tight enough to catch the
# spike-contaminated one. A gate AT the median would reject half of all good runs.
NULL_GATE_PCT = 3 * NULL_HALF_RANGE_PCT


# (frankenredis-gvm6z) MEASURED, THREE SIZES, PREDICTIONS REGISTERED FIRST. Six `--fr-only`
# draws each on ONE ELF (f985f0c2), reported as SIGMA because sigma is sample-size independent
# and half-range is not (that confound is recorded above):
#
#     size  1,305.2 instr/op   sigma 3.12   sigma-pct 0.2390
#     size  7,290.5 instr/op   sigma 6.52   sigma-pct 0.0894
#     size 108,610.1 instr/op  sigma 3.73   sigma-pct 0.0034
#
# THREE MODELS WERE PREDICTED BEFORE THE THIRD POINT RAN, AND ALL THREE ARE FALSIFIED:
#     FLAT sigma-pct (0.089)  predicted 96.7 instr   measured 0.04x of it
#     SQRT k=0.0814           predicted 26.8 instr   measured 0.14x
#     power law slope -0.572  predicted 20.7 instr   measured 0.18x
# My own sqrt hypothesis was the second of those. It is wrong.
#
# WHAT THE DATA SAYS: sigma is a small CONSTANT NUMBER OF INSTRUCTIONS -- 3.12 / 6.52 / 3.73,
# a 2.09x spread while the shape size spans 83x. That is what a two-point subtraction should
# leave behind: the work cancels, and what survives is a few instructions of per-run jitter
# that does not scale with the work at all.
#
# WHY THE OLD PERCENTAGE LOOKED CORRECT, and this is the useful part: 0.067 pct of ~7,000 is
# 4.88 instr, within 10 pct of the true absolute sigma. The constant was well calibrated AT
# its calibration size and wrong away from it -- understating noise 5x at 1,305 instr/op
# (anti-conservative, effects look more significant than they are) and overstating it 16x at
# 108,610 (conservative). A single percentage errs in OPPOSITE DIRECTIONS depending on shape
# size, which is worse than erring consistently.
#
# The pinned claims are essentially unmoved by the correction -- flat 0.53 -> 0.57 sigma, real
# 7.16 -> 7.35 -- because both of their arms sit at ~7,000, exactly where the percentage was
# accidentally right. No banked conclusion changes.
NULL_SIGMA_INSTR = 4.46


def null_noise_instr(instr_per_op):
    """A/A noise for a single measurement, in ABSOLUTE instr/op.

    `instr_per_op` is accepted and deliberately UNUSED: the measurement above shows the noise
    does not scale with the arm's magnitude. The parameter is kept so callers and the
    quadrature helper below need no change, and so that a future size-dependent model has
    somewhere to go if a fourth size ever contradicts this one.
    """
    del instr_per_op  # noise is size-independent; see NULL_SIGMA_INSTR
    return NULL_SIGMA_INSTR


def delta_sigma(delta, arm_a_instr_per_op, arm_b_instr_per_op):
    """How many noise-sigma a measured DELTA between two arms represents.

    A delta is the difference of two noisy measurements, so its noise is the two arms'
    noise added in quadrature — NOT one arm's. Reporting a delta against single-arm noise
    overstates significance by ~1.4x, which is exactly the size of error that makes an
    in-noise effect look publishable.
    """
    noise = (null_noise_instr(arm_a_instr_per_op) ** 2
             + null_noise_instr(arm_b_instr_per_op) ** 2) ** 0.5
    if noise == 0:
        return float("inf")
    return abs(delta) / noise


def _window_verdict(kind):
    """(fit, one_line) from scripts/certification_window.py, or (None, reason) if unavailable.

    (frankenredis-ozrro) IMPORTED rather than reimplemented. A second copy of the thresholds
    would drift from the first, and this harness has already been bitten by a guard that
    tested its own constants instead of the world.

    Every measurement this harness prints now carries its own window verdict, because the
    alternative is what I have been doing by hand: deciding after the fact whether a number
    counts as certified. Four consecutive fleet "clean window" reports were wrong; a stamp on
    the output is not something a later reader has to reconstruct.
    """
    gate = os.path.join(ROOT, "scripts", "certification_window.py")
    if not os.path.exists(gate):
        return None, "certification_window.py not present"
    try:
        r = subprocess.run([sys.executable, gate, "--for", kind],
                           capture_output=True, text=True, timeout=20, check=False)
    except Exception as exc:                                    # noqa: BLE001
        return None, f"window gate failed to run: {exc}"
    load = mhz = builds = "?"
    reasons = []
    for line in r.stdout.splitlines():
        line = line.strip()
        if line.startswith("loadavg "):
            load = line[len("loadavg "):]
        elif line.startswith("cpu MHz "):
            mhz = line[len("cpu MHz "):]
        elif line.startswith("builds "):
            builds = line[len("builds "):].strip()
        elif line.startswith("- "):
            reasons.append(line[2:])
    fit = r.returncode == 0
    summary = ("load %s | MHz %s | builds %s" % (load, mhz, builds))
    if reasons:
        summary += "  [%s]" % "; ".join(reasons)
    return fit, summary


def _positional_args(args):
    """Positional arguments only, with `--flags` removed regardless of position.

    (frankenredis-ozrro) Split out so it is testable. Reading ops from a raw args[2] made
    the documented `<bin> <shape> --fr-only` crash, and because the traceback goes to
    stderr a caller grepping stdout saw an empty arm rather than an error.
    """
    return [a for a in args if not a.startswith("--")]


def suggest_shapes(name: str, shapes=None) -> list[str]:
    """Shape names close to `name`, for the "did you mean" line.

    (frankenredis-gvm6z) The mistake this exists for: `corpus_coverage.py` reports
    COMMANDS while SHAPES are named after the shape, so the natural error is passing
    `hset` when the shape is `hset_same`.

    I first wrote this with a prefix-match FALLBACK for when ratio similarity returns
    nothing, reasoning that a short command name scores badly against a longer shape
    name. Then I measured it: across sixteen real command names (hset, mset, zrangestore,
    keys, lcs, spop, sort_ro, pfmerge, zunion, geosearch, scan, dump, randomkey, xpending,
    bitcount, smove) `difflib` returned candidates EVERY time, so the fallback never fired
    once. It was unreachable, and the only self-test I could write for it needed a
    contrived fixture — which is the tell. Removed rather than carried untested; if ratio
    finds nothing the caller still prints the shape count and points at `--list`.
    """
    return difflib.get_close_matches(name, SHAPES if shapes is None else shapes,
                                     n=5, cutoff=0.4)


def main() -> int:
    args = sys.argv[1:]
    if "--self-test" in args:
        return provenance_self_test()
    if "--cache-sim" in args:
        CACHE_SIM[0] = True
        args = [a for a in args if a != "--cache-sim"]
    if "--selftest" in args:
        return selftest()
    if "--list" in args:
        print("shapes: %s" % ", ".join(sorted(SHAPES)))
        return 0
    # (frankenredis-ozrro) Positionals are separated from flags ONCE, up front, so flag
    # ORDER never changes how the binary, shape or ops count are read.
    positional = _positional_args(args)
    if len(positional) < 2:
        print("usage: shape_instr_per_op.py <fr_bin> <shape> [ops] [--fr-only] "
              "[--locale=X]   (--list for shapes)", file=sys.stderr)
        return 2
    if positional[1] not in SHAPES:
        # (frankenredis-gvm6z) A WRONG SHAPE NAME AND A WRONG ARGUMENT COUNT ARE
        # DIFFERENT MISTAKES and used to print the same generic usage line. The failure
        # this fixes is one I made: the corpus tool and this harness key on different
        # things -- corpus_coverage.py lists COMMANDS (`hset`, `mset`) while SHAPES are
        # named after the shape (`hset_same`, `mset_2`) -- so reading a command name out
        # of the coverage report and passing it here is the natural error, and the reply
        # was a usage line that never mentioned the shape or hinted the right name.
        print(f"error: no such shape {positional[1]!r}", file=sys.stderr)
        near = suggest_shapes(positional[1])
        if near:
            print("   did you mean: %s" % ", ".join(near), file=sys.stderr)
        print("   --list prints all %d shapes" % len(SHAPES), file=sys.stderr)
        return 2
    fr_bin = os.path.abspath(positional[0])
    shape = positional[1]
    # (frankenredis-c0ts5) --fr-only skips the incumbent arm. The dispatch
    # ladder needs fr's own instr/op and dispatch share, not a ratio, and the
    # redis arm is half the wall-clock of every measurement (and the noisy half,
    # at ~8%). Building a ladder across a dozen commands is the case for it.
    fr_only = "--fr-only" in args
    locale = None
    for a in args:
        if a.startswith("--locale="):
            locale = a.split("=", 1)[1]
    # (frankenredis-ozrro) ops comes from the POSITIONAL args only. It used to read args[2]
    # directly, so the documented `<bin> <shape> --fr-only` — the flag's most natural
    # invocation, and the one the comment above implies — died with
    #   ValueError: invalid literal for int() with base 10: '--fr-only'
    # The flag only worked if you happened to pass an explicit ops count before it. The
    # traceback goes to stderr, so a caller that greps stdout for a result line sees an
    # EMPTY ARM rather than a failure, which is how this survived: I lost a whole
    # before-arm to it and read the silence as "no output" instead of "crashed".
    ops = int(positional[2]) if len(positional) > 2 else 2000
    seeds, cmd = SHAPES[shape]
    prov_ok, prov_msg = _check_incumbent(
        REDIS, os.path.join(ROOT, "legacy_redis_code/redis"))
    print("  %s" % prov_msg)
    if not prov_ok:
        raise SystemExit("REFUSED: %s\n"
                         "Every ratio this harness prints divides by that binary; a stale "
                         "or unidentifiable denominator is worse than no measurement." % prov_msg)
    # (frankenredis-ozrro) Stamp the window BEFORE measuring, and pick the strictness from
    # what this invocation will actually print: --fr-only produces no denominator, and fr's Ir
    # is load-immune (0.65 pct across six sessions spanning loadavg 14-66), so it is held to
    # the lenient gate. A ratio run is held to the strict one.
    kind = "fr-only" if fr_only else "ratio"
    fit, window = _window_verdict(kind)
    if fit is None:
        print("  WINDOW: UNKNOWN (%s) — label any number from this run by hand" % window)
    elif fit:
        print("  WINDOW: FIT for %s — %s" % (kind, window))
    else:
        print("  WINDOW: UNFIT for %s — %s" % (kind, window))
        print("  WINDOW: this run is SIZING, not certified. Do not promote it without a"
              " FIT window.")
    workdir = tempfile.mkdtemp(prefix="fr_instr_")
    if locale:
        print("  both engines pinned to LC_ALL=%s" % locale)
    fr_ipo, fr_lo, fr_hi = instr_per_op(fr_bin, seeds, cmd, ops, workdir, "fr", locale)
    if fr_only:
        got = dispatch_share(os.path.join(workdir, "cg.fr.n.out"),
                             os.path.join(workdir, "cg.fr.2n.out"), ops, fr_ipo)
        frac = got[0] if got else float("nan")
        fr_p = PASSES.get("fr.2n")
        if fr_p is not None:
            print("  event-loop passes per op: %.3f" % fr_p)
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
    if CACHE_SIM[0]:
        # (frankenredis-eh2ct) Per-op simulated misses, differenced the same way Ir is,
        # so startup cancels. D1mr/DLmr are the memory-stall axis; a ratio far above
        # the instruction ratio is the signature of "fewer instructions, more cycles".
        fr_lo_ev = total_events(os.path.join(workdir, "cg.fr.n.out"))
        fr_hi_ev = total_events(os.path.join(workdir, "cg.fr.2n.out"))
        rd_lo_ev = total_events(os.path.join(workdir, "cg.redis.n.out"))
        rd_hi_ev = total_events(os.path.join(workdir, "cg.redis.2n.out"))
        print("  simulated misses per op (2N-N differenced):")
        print("    %-6s %12s %12s %8s" % ("event", "fr", "redis", "fr/redis"))
        for ev in ("Dr", "D1mr", "DLmr", "Dw", "D1mw", "DLmw"):
            if ev not in fr_hi_ev or ev not in rd_hi_ev:
                continue
            f = (fr_hi_ev[ev] - fr_lo_ev[ev]) / ops
            r = (rd_hi_ev[ev] - rd_lo_ev[ev]) / ops
            ratio = ("%.4fx" % (f / r)) if r else "n/a"
            print("    %-6s %12.1f %12.1f %8s" % (ev, f, r, ratio))
    # (frankenredis-zw36c) The per-PASS denominator, now observed rather than inferred.
    # fr's per-iteration bookkeeping is paid once per pass, so a shape's passes/op is what
    # converts that fixed tax into a per-op cost. A shape at ~1 pass/op cannot show a
    # per-pass lever at all; one at many passes/op is where such a lever is visible.
    fr_p, rd_p = PASSES.get("fr.2n"), PASSES.get("redis.2n")
    if fr_p is not None and rd_p is not None:
        print("  event-loop passes per op:     fr %.3f   redis %.3f   (fr/redis %.2fx)"
              % (fr_p, rd_p, (fr_p / rd_p) if rd_p else float("nan")))
    got = dispatch_share(os.path.join(workdir, "cg.fr.n.out"),
                         os.path.join(workdir, "cg.fr.2n.out"), ops, fr_ipo)
    if got:
        frac, top = got
        print("  fr dispatch share: %.1f%%  (~%.1f of %.1f instr/op deciding WHICH command)"
              % (100 * frac, fr_ipo * frac, fr_ipo))
        for ir, fn in top:
            print("      %10.1f  %s" % (ir, fn[:66]))
        print("  compare: a front-classified route (EXISTS on a missing key) is 21.5%;"
              " 62-66% means the dispatch lever has something to bite on.")
    print("  callgrind dumps: %s" % workdir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
