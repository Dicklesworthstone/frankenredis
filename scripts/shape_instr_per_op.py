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
    ops = int(args[2]) if len(args) > 2 else 2000
    seeds, cmd = SHAPES[shape]
    workdir = tempfile.mkdtemp(prefix="fr_instr_")
    fr_ipo, fr_lo, fr_hi = instr_per_op(fr_bin, seeds, cmd, ops, workdir, "fr")
    rd_ipo, rd_lo, rd_hi = instr_per_op(REDIS, seeds, cmd, ops, workdir, "redis")
    print("shape %s   N=%d 2N=%d" % (shape, ops, ops * 2))
    print("  fr     Ir(N)=%-14d Ir(2N)=%-14d -> %10.1f instr/op" % (fr_lo, fr_hi, fr_ipo))
    print("  redis  Ir(N)=%-14d Ir(2N)=%-14d -> %10.1f instr/op" % (rd_lo, rd_hi, rd_ipo))
    print("  fr/redis instructions per op: %.4fx" % (fr_ipo / rd_ipo))
    print("  callgrind dumps: %s" % workdir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
