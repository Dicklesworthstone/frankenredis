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
import select
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
    # THE TABLE IS DATED, THE STRUCTURE IS NOT. Those absolutes predate the gein3 sort
    # removal (fr used to sort the SINTER reply, an O(k log k) cost redis never paid), so
    # they no longer reproduce and should not be diffed against: re-measured after it,
    # k=140 fell 111,315 -> 82,500 and its ratio 0.8371x -> 0.6026x, which is the sort
    # removal showing up exactly where an O(k log k) term should. What reproduces, and what
    # these shapes exist to pin, is the REGIME STRUCTURE -- no crossing at k=14 (measured
    # 0.4058x there after the change, against a model that predicted 1.0), fr ahead across
    # the listpack regime, and the boundary sitting at the encoding change rather than at
    # any point a two-point fit picks out.
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
    """Read `eventloop_cycles` from INFO stats, or None if the engine omits it."""
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
    # (frankenredis-zw36c) The per-PASS denominator, now observed rather than inferred.
    # fr's per-iteration bookkeeping is paid once per pass, so a shape's passes/op is what
    # converts that fixed tax into a per-op cost. A shape at ~1 pass/op cannot show a
    # per-pass lever at all; one at many passes/op is where such a lever is visible.
    fr_p, rd_p = PASSES.get("fr.2n"), PASSES.get("redis.2n")
    if fr_p is not None and rd_p is not None:
        print("  event-loop passes per op:     fr %.3f   redis %.3f   (fr/redis %.2fx)"
              % (fr_p, rd_p, (fr_p / rd_p) if rd_p else float("nan")))
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
