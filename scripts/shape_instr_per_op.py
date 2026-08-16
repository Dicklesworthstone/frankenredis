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


def run_once(engine: str, seeds, cmd, ops: int, workdir: str, tag: str) -> int:
    out = os.path.join(workdir, "cg.%s.out" % tag)
    port = free_port()
    argv = ["valgrind", "--tool=callgrind", "--callgrind-out-file=" + out,
            "--cache-sim=no", "--branch-sim=no",
            engine, "--port", str(port), "--save", "", "--appendonly", "no"]
    # cwd=workdir: never boot an engine in the repo root, which is shared and may
    # hold a dump.rdb redis refuses to load (frankenredis-7afsd).
    proc = subprocess.Popen(argv, cwd=workdir,
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
        for seed in seeds:
            sock.sendall(resp(*seed.split()))
            sock.recv(4096)
        sock.sendall(resp(*cmd) * ops)
        seen = 0
        while seen < ops:
            chunk = sock.recv(1 << 20)
            if not chunk:
                raise RuntimeError("%s dropped the connection mid-burst" % tag)
            seen += chunk.count(b"\r\n")
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


def instr_per_op(engine: str, seeds, cmd, ops: int, workdir: str, label: str):
    low = run_once(engine, seeds, cmd, ops, workdir, label + ".n")
    high = run_once(engine, seeds, cmd, ops * 2, workdir, label + ".2n")
    return (high - low) / ops, low, high


def main() -> int:
    args = sys.argv[1:]
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
    ops = int(args[2]) if len(args) > 2 else 2000
    seeds, cmd = SHAPES[shape]
    workdir = tempfile.mkdtemp(prefix="fr_instr_")
    fr_ipo, fr_lo, fr_hi = instr_per_op(fr_bin, seeds, cmd, ops, workdir, "fr")
    if fr_only:
        got = dispatch_share(os.path.join(workdir, "cg.fr.2n.out"))
        frac = got[0] if got else float("nan")
        print("LADDER %-18s fr %8.1f instr/op   dispatch %8.1f (%.1f%%)"
              % (shape, fr_ipo, fr_ipo * frac, 100 * frac))
        print("  callgrind dumps: %s" % workdir)
        return 0
    rd_ipo, rd_lo, rd_hi = instr_per_op(REDIS, seeds, cmd, ops, workdir, "redis")
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
