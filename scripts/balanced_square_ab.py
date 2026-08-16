#!/usr/bin/env python3
"""Balanced-square vs-incumbent A/B for FrankenRedis, usable on a CONTENDED host.

WHY THIS EXISTS
---------------
Every vs-incumbent harness in this repo gates on the host being quiet, and on a
64-way box shared by tens of agents that gate cannot be met. Measured here today:

  * `scripts/lua_eval_headtohead.sh` refused 21 of 21 invocations at an absolute
    loadavg ceiling; rescaling it per core (2b02caf16) helped, and it STILL
    refused 24 of 24 an hour later at load 27.9 against a 19.20 ceiling.
  * A four-arm throughput harness run at loadavg 58 produced A/A nulls of
    0.85-1.07 and a SAME-BINARY post/pre of 0.96-1.05 — two columns whose true
    value is exactly 1.0000. Recorded as INADMISSIBLE in
    `docs/perf_negative_evidence_ledger.md`.

The same wall was hit independently in franken_networkx, whose sanctioned
harness required five consecutive windows with EVERY cpu idle: its bead
`br-r37-c1-3s8x7` logged 25 consecutive attempts with zero admitted, and a run
aborted after 300 windows on one busy cpu. Its answer, committed as
`/data/projects/franken_networkx/scripts/balanced_square_ab.py` (72761094c), is
the design ported here. Three agents there hand-rolled it in scratchpads before
one committed it properly; this is a port of theirs, not a fourth hand-roll.

THE DESIGN. It does not try to make the host quiet. It makes the COMPARISON
immune to the host being busy:

  * Both arms run INSIDE one round, interleaved as a balanced square
    `A B B A A B B A`. Each arm occupies the same multiset of slot POSITIONS, so
    drift across a round — a peer's build starting, a cache warming, a governor
    step — hits both arms equally instead of biasing whichever went first.
  * Each arm carries its OWN A/A null: that arm's first-half slots divided by its
    second-half slots, which must come out 1.0. The square places the halves
    symmetrically, so a null that departs from 1.0 is drift or contention rather
    than slot position. Contention is therefore CAUGHT PER ROW, after the fact,
    instead of being excluded up front by a gate that can never pass.
  * A row whose null leaves [0.98, 1.02] is reported NULL-FAILED and its ratio is
    NOT a result. Refusing is the point.

This RELAXES NO EVIDENCE STANDARD. The incumbent is a live vendored
`redis-server` started in this same invocation; every arm's ELF SHA-256 is read
from `/proc/<pid>/exe` of the already-running process, so the harness cannot
compare a build against itself by accident; and provenance carries the OBSERVED
thread count, host, governor and runtime ISA. It replaces an unsatisfiable
precondition with a sound experimental design, nothing more.

USAGE
-----
    scripts/balanced_square_ab.py --fr-bin /tmp/fr_head --shapes cascade
    scripts/balanced_square_ab.py --fr-bin /tmp/fr_head --shapes eval --rounds 15

    --shapes     a registered shape set (see --list)
    --rounds     balanced squares per row (default 9)
    --ops        redis-benchmark operations per timed slot (default 50000)
    --pipeline   redis-benchmark -P depth (default 16)
    --expect-elf first 16 hex chars of the fr ELF you INTEND to measure; the run
                 aborts on mismatch, because pointing at a stale /tmp copy is the
                 cheapest way to publish a number about the wrong binary.

Ratio convention is fr_ops_per_sec / redis_ops_per_sec, so > 1 means FrankenRedis
is faster. That is the convention the ledger rows use.

ADDING A SHAPE SET. Append to SHAPE_SETS. A shape is
`(label, [seed commands], [benchmark argv])`. Every shape is error-probed on BOTH
engines before timing, because `redis-benchmark` counts an error reply as a
completed request and a refused command otherwise reads as enormous throughput.
Include at least one row the change under test CANNOT affect, as a control.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import platform
import random
import re
import shutil
import socket
import tempfile
import statistics
import subprocess
import sys
import time

SQUARE = "ABBAABBA"
NULL_BOUND = 0.02

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCH = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-benchmark")
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")
CLI = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-cli")

# Shapes are grouped so a row set can be named on the command line rather than
# re-typed. The trailing control in each set is a command the work under test
# does not touch; a control that moves with the candidate means the row set is
# measuring the harness, not the change.
SHAPE_SETS: dict[str, list[tuple[str, list[str], list[str]]]] = {
    # The nine shapes front-classified onto the dispatch floor (frankenredis-ozrro).
    "cascade": [
        ("sintercard", ["SADD sc:a m1 m2 m3", "SADD sc:b m2 m3 m4"],
         ["SINTERCARD", "2", "sc:a", "sc:b"]),
        # (frankenredis-mnzgy) The next three do NOT meet the admission bar
        # this file states, and scripts/shape_admission_probe.py flags them. They
        # are annotated rather than removed, because deleting another agent's
        # registered shapes is not this audit's call -- but do not read a ratio
        # off them without reading this first.
        #
        # zrandmember/srandmember are RANDOM: the reply differs run to run and
        # between engines, and its LENGTH differs too ("m9" is 2 bytes, "m10" is
        # 3), so the two arms do not write identical byte counts. The per-call
        # work is comparable, so the rows are indicative, not byte-exact.
        ("zrandmember", ["ZADD zz 1 a 2 b 3 c 4 d"], ["ZRANDMEMBER", "zz", "2"]),
        ("srandmember", ["SADD sbig m1 m2 m3 m4 m5 m6 m7 m8 m9 m10"],
         ["SRANDMEMBER", "sbig", "2"]),
        # COPY without REPLACE returned 1 on the FIRST call and 0 on every call
        # after, so 19,999 of 20,000 ops measured the destination-exists early
        # return rather than a copy -- the row was named "copy" and measured a
        # no-op. REPLACE makes every op perform the copy and return 1, which is
        # both what the name promises and stable under repetition.
        ("copy", ["SET kk vvvvvvvvvvvvvvvv"], ["COPY", "kk", "kdst", "REPLACE"]),
        # pttl's VALUE drifts (it returns remaining ms), but the digit count -- and
        # so the reply byte length and the work done -- is constant at this TTL
        # magnitude: 900000000 loses ~100ms over a 20k-op run and stays 9 digits.
        # A SMALLER TTL here would change reply length mid-run and break the row.
        ("pttl", ["SET bb abcdefghijklmnop", "PEXPIRE bb 900000000"], ["PTTL", "bb"]),
        ("expiretime", ["SET kk vvvvvvvvvvvvvvvv", "EXPIREAT kk 4102444800"],
         ["EXPIRETIME", "kk"]),
        ("publish", [], ["PUBLISH", "ch", "hello"]),
        ("getbit", ["SET bb abcdefghijklmnop"], ["GETBIT", "bb", "5"]),
        ("geohash", ["GEOADD gg 13.361389 38.115556 Palermo"], ["GEOHASH", "gg", "Palermo"]),
        # Control: GET is not front-classified by that work.
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # (frankenredis-bcva8/t7qgs/in98j/vlrnn/bj3mq/fhjnd) The zset and scan READ
    # routes that were front-classified onto the dispatch floor and shipped on
    # instruction counts alone. Every shape here is READ-ONLY on purpose: a
    # mutating shape like ZREMRANGEBYLEX or LPOP COUNT drains or empties its key
    # within the first few of redis-benchmark's requests and then measures the
    # absent/empty path for the remaining tens of thousands, which is a steady
    # state neither route was shipped for. Those need a harness that restores
    # state per request and are deliberately NOT faked in here.
    "zsetreads": [
        ("zrevrange", ["ZADD zr 1 a 2 b 3 c 4 d 5 e"], ["ZREVRANGE", "zr", "0", "-1"]),
        ("zrangebyscore", ["ZADD zr 1 a 2 b 3 c 4 d 5 e"],
         ["ZRANGEBYSCORE", "zr", "2", "4"]),
        ("zrevrangebyscore", ["ZADD zr 1 a 2 b 3 c 4 d 5 e"],
         ["ZREVRANGEBYSCORE", "zr", "4", "2"]),
        ("zrevrangebylex", ["ZADD zl 0 a 0 b 0 c 0 d 0 e"],
         ["ZREVRANGEBYLEX", "zl", "[e", "[b"]),
        ("zdiff", ["ZADD zd1 1 a 2 b 3 c", "ZADD zd2 1 b"], ["ZDIFF", "2", "zd1", "zd2"]),
        ("zinter", ["ZADD zd1 1 a 2 b 3 c", "ZADD zd2 1 b"], ["ZINTER", "2", "zd1", "zd2"]),
        ("sscan0", ["SADD ss m1 m2 m3 m4 m5 m6 m7 m8"], ["SSCAN", "ss", "0"]),
        ("hscan0", ["HSET hh f1 v1 f2 v2 f3 v3 f4 v4"], ["HSCAN", "hh", "0"]),
        ("zscan0", ["ZADD zr 1 a 2 b 3 c 4 d 5 e"], ["ZSCAN", "zr", "0"]),
        # Control: GET is not front-classified by that work.
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # The standing Lua target: 50 redis.call('GET') per EVAL.
    "eval": [
        ("eval_50x_get", ["SET k val"],
         ["EVAL", "for i=1,50 do redis.call('GET', KEYS[1]) end return 1", "1", "k"]),
        ("get_control", ["SET k val"], ["GET", "k"]),
    ],
    # Commands that MUTATE their key, measured on their NO-OP path so the square is
    # valid at all. (frankenredis-va5me, frankenredis-5yhyh, frankenredis-wgrny)
    #
    # These three beads were recorded as unmeasurable here, correctly: redis-benchmark
    # fires tens of thousands of identical requests, so a real ZREMRANGEBYRANK /
    # ZREMRANGEBYLEX / LPOP COUNT drains its key within the first few and every
    # remaining request measures the EMPTY case. The ratio you get is then a fiction
    # about a command that stopped running.
    #
    # A no-op shape removes the problem rather than working around it: request 1 and
    # request 50,000 do exactly the same work, so the square measures one steady
    # thing. Each shape below was probed on BOTH engines before being added here —
    # identical non-error reply, and the collection size unchanged after 200
    # repetitions (zr/zl stay at 3, nosuchlist stays absent).
    #
    # This is the DISPATCH-path cost of these commands, which is what the front
    # classification work actually changed; it is NOT a claim about the cost of
    # removing elements, and no row from this set may be quoted as one.
    "mutnoop": [
        # start > stop: an empty rank range, so nothing is removed and 0 comes back.
        ("zremrangebyrank_noop", ["ZADD zr 1 a 2 b 3 c"],
         ["ZREMRANGEBYRANK", "zr", "5", "4"]),
        # min > max lexicographically: an empty lex range, same reasoning.
        ("zremrangebylex_noop", ["ZADD zl 0 a 0 b 0 c"],
         ["ZREMRANGEBYLEX", "zl", "[x", "[a"]),
        # Missing key: the COUNT form returns a null array and creates nothing.
        ("lpop_count_missing", [], ["LPOP", "nosuchlist", "10"]),
        ("rpop_count_missing", [], ["RPOP", "nosuchlist", "10"]),
        # Control: GET is untouched by the dispatch work these rows are about, and
        # without it none of the rows above can be normalised.
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    # Multi-key reads and *STORE writes. (frankenredis-3nn63, frankenredis-gdnqr,
    # frankenredis-fc7w0, frankenredis-uld9l, frankenredis-9601c, frankenredis-8t4uu,
    # frankenredis-ox2xq)
    #
    # The *STORE commands WRITE, but they are safe to hammer because they are
    # IDEMPOTENT: the destination is recomputed from unchanging sources, so request
    # 50,000 produces exactly what request 1 did. That is a different property from
    # the `mutnoop` set above, where the effect ACCUMULATED and the command had to be
    # reduced to a no-op. ZMPOP genuinely pops, so it is measured on a missing key.
    #
    # Every shape here was probed on BOTH engines before registration: identical
    # non-error reply, and the reply UNCHANGED after 200 repetitions.
    # (frankenredis-hxgsz) Routes NO existing set covers. The four sets
    # above are almost entirely reads on keys with no TTL, so whole families --
    # the write path, the container-length reads, the key-metadata reads -- have
    # never been measured against the incumbent at all. Every shape here cleared
    # the same admission bar the others did, probed on BOTH engines before
    # registration: identical non-error reply, and the reply UNCHANGED after 200
    # repetitions (scratchpad/shape_admit_probe.py, 20 admitted, 0 rejected).
    # setex_same is deliberately included: it is the only write here that leaves a
    # TTL behind, and fr's per-command expire cycle makes that a different
    # workload (frankenredis-kiyxn).
    "unswept": [
        ("strlen", ["SET s abcdefghijklmnop"], ["STRLEN", "s"]),
        ("getrange", ["SET s abcdefghijklmnop"], ["GETRANGE", "s", "2", "9"]),
        ("llen", ["RPUSH l a b c d e"], ["LLEN", "l"]),
        ("lrange_5", ["RPUSH l a b c d e"], ["LRANGE", "l", "0", "-1"]),
        ("hlen", ["HSET h f1 v1 f2 v2 f3 v3"], ["HLEN", "h"]),
        ("hget", ["HSET h f1 v1 f2 v2 f3 v3"], ["HGET", "h", "f2"]),
        ("scard", ["SADD st m1 m2 m3 m4 m5"], ["SCARD", "st"]),
        ("zcard", ["ZADD z 1 a 2 b 3 c"], ["ZCARD", "z"]),
        ("type", ["SET s abcdefghijklmnop"], ["TYPE", "s"]),
        ("object_encoding", ["SET s abcdefghijklmnop"], ["OBJECT", "ENCODING", "s"]),
        ("ttl_nonvolatile", ["SET s abcdefghijklmnop"], ["TTL", "s"]),
        ("persist_noop", ["SET s abcdefghijklmnop"], ["PERSIST", "s"]),
        ("set_same", [], ["SET", "wk", "vvvvvvvvvvvvvvvv"]),
        ("setex_same", [], ["SETEX", "wx", "100", "vvvvvvvvvvvvvvvv"]),
        ("setrange_same", ["SET sr abcdefghijklmnop"], ["SETRANGE", "sr", "3", "xy"]),
        ("hset_same", ["HSET h f1 v1"], ["HSET", "h", "f1", "v1"]),
        ("sadd_same", ["SADD st m1"], ["SADD", "st", "m1"]),
        ("zadd_same", ["ZADD z 1 a"], ["ZADD", "z", "1", "a"]),
        ("getex_persist", ["SET gx abcdefghijklmnop"], ["GETEX", "gx", "PERSIST"]),
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
    "storeops": [
        ("exists_8key", ["MSET e1 1 e2 1 e3 1 e4 1 e5 1 e6 1 e7 1 e8 1"],
         ["EXISTS", "e1", "e2", "e3", "e4", "e5", "e6", "e7", "e8"]),
        ("hmget_9field",
         ["HSET hm f1 v1 f2 v2 f3 v3 f4 v4 f5 v5 f6 v6 f7 v7 f8 v8 f9 v9"],
         ["HMGET", "hm", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9"]),
        ("zmscore_9member",
         ["ZADD zm 1 m1 2 m2 3 m3 4 m4 5 m5 6 m6 7 m7 8 m8 9 m9"],
         ["ZMSCORE", "zm", "m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "m9"]),
        ("scan_prefix", ["MSET tenant:needle:1 1 tenant:decoy:1 1 tenant:decoy:2 1"],
         ["SCAN", "0", "MATCH", "tenant:needle:*", "COUNT", "100"]),
        ("zunionstore_2key", ["ZADD za 1 a 2 b 3 c 4 d", "ZADD zb 1 b 2 c 3 d 4 e"],
         ["ZUNIONSTORE", "zdst", "2", "za", "zb"]),
        ("zinterstore_2key", ["ZADD za 1 a 2 b 3 c 4 d", "ZADD zb 1 b 2 c 3 d 4 e"],
         ["ZINTERSTORE", "zidst", "2", "za", "zb"]),
        ("bitop_and", ["SET ba abcdefghijklmnop", "SET bb ponmlkjihgfedcba"],
         ["BITOP", "AND", "bdst", "ba", "bb"]),
        ("bitop_not", ["SET ba abcdefghijklmnop"], ["BITOP", "NOT", "bndst", "ba"]),
        ("sunionstore_3src",
         ["SADD sa m1 m2 m3 m4 m5", "SADD sb m3 m4 m5 m6 m7", "SADD sc m4 m5 m6 m7 m8"],
         ["SUNIONSTORE", "sudst", "sa", "sb", "sc"]),
        ("sinterstore_3src",
         ["SADD sa m1 m2 m3 m4 m5", "SADD sb m3 m4 m5 m6 m7", "SADD sc m4 m5 m6 m7 m8"],
         ["SINTERSTORE", "sidst", "sa", "sb", "sc"]),
        ("sdiffstore_3src",
         ["SADD sa m1 m2 m3 m4 m5", "SADD sb m3 m4 m5 m6 m7", "SADD sc m4 m5 m6 m7 m8"],
         ["SDIFFSTORE", "sddst", "sa", "sb", "sc"]),
        # ZMPOP pops, so it would drain any key it could reach; the missing-key form
        # returns a null array and creates nothing.
        ("zmpop_missing", [], ["ZMPOP", "1", "nosuchzset", "MIN"]),
        # Control: GET is untouched by any of the dispatch work these rows measure.
        ("get_control", ["SET kk vvvvvvvvvvvvvvvv"], ["GET", "kk"]),
    ],
}


def sha256_of(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def running_image_sha(pid: int) -> str:
    """SHA-256 of the image the process is ACTUALLY executing.

    Hashing the path we intended to launch would not catch a stale copy, a
    symlink, or a harness that launched the same binary twice and called one of
    them the candidate. Reading `/proc/<pid>/exe` reports what is running.
    """
    return sha256_of(f"/proc/{pid}/exe")


def observed_threads(pid: int) -> int:
    """Threads the server ACTUALLY has, not the number any flag requested."""
    return len(os.listdir(f"/proc/{pid}/task"))


def provenance(fr_pid: int, redis_pid: int) -> dict:
    governor = "unknown"
    gov_path = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"
    if os.path.exists(gov_path):
        with open(gov_path) as handle:
            governor = handle.read().strip()
    isa = []
    with open("/proc/cpuinfo") as handle:
        flags = handle.read()
    for feature in ("avx512f", "avx2", "avx", "sse4_2"):
        if re.search(rf"\b{feature}\b", flags):
            isa.append(feature)
    with open("/proc/loadavg") as handle:
        loadavg = " ".join(handle.read().split()[:3])
    return {
        "host": socket.gethostname(),
        "kernel": platform.release(),
        "cores": os.cpu_count(),
        "governor": governor,
        "isa": isa[0] if isa else "unknown",
        "loadavg": loadavg,
        "fr_elf_sha256": running_image_sha(fr_pid),
        "redis_elf_sha256": running_image_sha(redis_pid),
        "fr_threads_observed": observed_threads(fr_pid),
        "redis_threads_observed": observed_threads(redis_pid),
    }


def wait_ready(port: int, timeout_s: float = 30.0,
               proc: subprocess.Popen | None = None) -> None:
    """(frankenredis-yaul4) Fail on a DEAD server immediately, and say so.

    Without the `proc` check a server that exits during startup is only noticed
    30s later, and then only as a timeout -- or worse, the run proceeds and
    provenance dies reading /proc/<pid>/exe of a corpse, which surfaces as a bare
    FileNotFoundError naming a pid and nothing else. That is exactly how the
    poisoned repo-root dump.rdb presented here.
    """
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        if proc is not None and proc.poll() is not None:
            raise SystemExit(
                f"server on port {port} exited during startup with rc="
                f"{proc.returncode} before answering PING -- it never ran, so "
                f"there is no measurement here to interpret")
        probe = subprocess.run([CLI, "-p", str(port), "ping"],
                               capture_output=True, text=True)
        if probe.returncode == 0 and "PONG" in probe.stdout:
            return
        time.sleep(0.2)
    raise SystemExit(f"server on port {port} never became ready")


def free_port() -> int:
    """Bind port 0 and hand back what the kernel assigned."""
    probe = socket.socket()
    try:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]
    finally:
        probe.close()


def assert_ours(port: int, proc: subprocess.Popen, label: str) -> None:
    """(frankenredis-yaul4) The process answering on `port` must be the one we
    started. PING proves only that SOMETHING listens: with the old fixed ports a
    peer's server answered, our own engine exited unable to bind, and the run was
    one step away from reporting a ratio measured on somebody else's binary. The
    ELF sha in provenance() does not catch this -- it shas OUR pid, not the pid
    that actually served the traffic."""
    out = subprocess.run([CLI, "-p", str(port), "info", "server"],
                         capture_output=True, text=True)
    match = re.search(r"process_id:(\d+)", out.stdout)
    if not match:
        raise SystemExit(f"{label} on port {port}: INFO server carried no process_id")
    served_by = int(match.group(1))
    if served_by != proc.pid:
        raise SystemExit(
            f"{label} on port {port} is served by pid {served_by}, not the "
            f"process we launched (pid {proc.pid}) -- another agent holds that "
            f"port, so any ratio from this run would describe their binary")


def seed(port: int, commands: list[str]) -> None:
    for command in commands:
        subprocess.run([CLI, "-p", str(port)] + command.split(),
                       capture_output=True, text=True, check=False)


def error_probe(port: int, argv: list[str], engine: str, label: str) -> None:
    """A refused command reads as enormous throughput, so refuse to time it.

    `redis-benchmark` counts an error reply as a completed request. A shape that
    one engine rejects therefore produces a fast, confident, meaningless number.
    """
    out = subprocess.run([CLI, "-p", str(port)] + argv,
                         capture_output=True, text=True).stdout.strip()
    if out.startswith(("ERR", "WRONGTYPE", "NOPERM")) or "unknown command" in out:
        raise SystemExit(f"error probe failed: {engine} rejects `{label}`: {out}")


RPS = re.compile(r"([0-9]+\.[0-9]+) requests per second")


def time_slot(port: int, argv: list[str], ops: int, pipeline: int,
              client_core: str | None) -> float:
    """One timed slot: ops/s for a single redis-benchmark invocation."""
    cmd = [BENCH, "-p", str(port), "-n", str(ops), "-c", "1",
           "-P", str(pipeline), "-q"] + argv
    if client_core:
        cmd = ["taskset", "-c", client_core] + cmd
    out = subprocess.run(cmd, capture_output=True, text=True).stdout
    match = None
    for match in RPS.finditer(out):
        pass
    if match is None:
        raise SystemExit(f"redis-benchmark produced no rate for {argv}:\n{out}")
    return float(match.group(1))


def bootstrap_ci(values: list[float], iters: int = 2000,
                 seed_value: int = 20260814) -> tuple[float, float]:
    rng = random.Random(seed_value)
    n = len(values)
    medians = sorted(
        statistics.median(rng.choices(values, k=n)) for _ in range(iters)
    )
    return medians[int(0.025 * iters)], medians[int(0.975 * iters)]


def run_row(label: str, fr_port: int, redis_port: int, argv: list[str],
            rounds: int, ops: int, pipeline: int,
            client_core: str | None) -> dict:
    # Warm both arms once so neither pays first-touch inside a measured slot.
    time_slot(fr_port, argv, max(ops // 10, 1000), pipeline, client_core)
    time_slot(redis_port, argv, max(ops // 10, 1000), pipeline, client_core)

    ratios, null_redis, null_fr = [], [], []
    for _ in range(rounds):
        a_slots, b_slots = [], []
        for slot in SQUARE:
            if slot == "A":
                a_slots.append(time_slot(redis_port, argv, ops, pipeline, client_core))
            else:
                b_slots.append(time_slot(fr_port, argv, ops, pipeline, client_core))
        # ops/s, so fr/redis > 1 means fr is faster.
        ratios.append(statistics.median(b_slots) / statistics.median(a_slots))
        # Each arm's own first-half / second-half ratio. The square places the
        # halves symmetrically, so a departure from 1.0 is drift or contention,
        # not slot position.
        null_redis.append(statistics.median(a_slots[:2]) / statistics.median(a_slots[2:]))
        null_fr.append(statistics.median(b_slots[:2]) / statistics.median(b_slots[2:]))

    ratio = statistics.median(ratios)
    low, high = bootstrap_ci(ratios)
    n_redis, n_fr = statistics.median(null_redis), statistics.median(null_fr)
    nulls_ok = abs(n_redis - 1.0) <= NULL_BOUND and abs(n_fr - 1.0) <= NULL_BOUND
    if not nulls_ok:
        verdict = "NULL-FAILED"
    elif low <= 1.0 <= high:
        verdict = "STRADDLES-1"
    else:
        verdict = "ADMISSIBLE"
    return {
        "label": label,
        "ratio": ratio,
        "ci": (low, high),
        "null_redis": n_redis,
        "null_fr": n_fr,
        "verdict": verdict,
    }


def main(argv_in: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--fr-bin", required=False)
    parser.add_argument("--shapes", default="cascade")
    parser.add_argument("--rounds", type=int, default=9)
    parser.add_argument("--ops", type=int, default=50000)
    parser.add_argument("--pipeline", type=int, default=16)
    # (frankenredis-yaul4) Ephemeral by default. The fixed pair 27841/27842 was
    # found held by ANOTHER agent's run (an fr_post binary and its redis), so
    # our own fr could not bind and exited while the squatter answered PING.
    parser.add_argument("--fr-port", type=int, default=0)
    parser.add_argument("--redis-port", type=int, default=0)
    parser.add_argument("--client-core", default=None)
    # (frankenredis-xvq1a) Optional server pinning. The per-arm nulls are each
    # arm's own first half over its second half — WITHIN-process drift — while the
    # reported ratio is a CROSS-process comparison, so placement between the two
    # server processes is a term the nulls structurally cannot see. Whether that
    # matters is WORKLOAD-DEPENDENT and was measured both ways on this host:
    #   * RESTORE decode (long ~40ms DEBUG-driven bursts, one connection): a
    #     cross-process A/A between two identical fr servers scattered 0.918-1.058
    #     over six invocations; pinning to symmetric core sets collapsed it to
    #     1.0106 [1.0015, 1.0159] and 1.0081 [0.9847, 1.0260].
    #   * THIS harness's redis-benchmark workload (-c1 -P16, many short ops):
    #     unpinned cross-process A/A came out 0.9974 and 1.0106, and PINNING DID
    #     NOT IMPROVE IT (1.0151 and 0.9896, one row null-failed). So the term is
    #     ~1% here and these flags buy nothing for the registered shape sets.
    # They are kept because the RESTORE result shows the term is real for other
    # workloads, and because a future shape set may be burst-shaped. Off by
    # default: do not pin without first showing --cross-null needs it.
    parser.add_argument("--fr-core", default=None,
                        help="taskset core list for the fr server (e.g. 0-3); "
                             "measured to buy nothing on the current shape sets")
    parser.add_argument("--redis-core", default=None,
                        help="taskset core list for the redis server; use a set "
                             "symmetric with --fr-core (same CCD, same size)")
    # Lets the harness MEASURE its own cross-process null instead of assuming it:
    # run the second arm as another fr, so the reported ratio should be 1.0. This
    # is the flag that settled the question above, and it is the one worth using.
    parser.add_argument("--cross-null", action="store_true",
                        help="replace the redis arm with a SECOND fr server; the "
                             "reported ratio is then a cross-process A/A and must "
                             "come out 1.0. Not a competitive row.")
    parser.add_argument("--expect-elf", default=None)
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args(argv_in)

    if args.list:
        for name, shapes in SHAPE_SETS.items():
            print(f"{name}: {', '.join(label for label, _, _ in shapes)}")
        return 0

    if not args.fr_bin:
        parser.error("--fr-bin is required")
    for path in (BENCH, REDIS, CLI, args.fr_bin):
        if not os.path.exists(path):
            raise SystemExit(f"missing binary: {path}")
    shapes = SHAPE_SETS.get(args.shapes)
    if shapes is None:
        raise SystemExit(f"unknown shape set {args.shapes}; try --list")

    # (frankenredis-yaul4) 0 means "pick a free one"; an explicit --fr-port /
    # --redis-port is still honoured, and identity-checked either way.
    if args.fr_port == 0:
        args.fr_port = free_port()
    if args.redis_port == 0:
        args.redis_port = free_port()

    def pinned(core: str | None, cmd: list[str]) -> list[str]:
        return (["taskset", "-c", core] + cmd) if core else cmd

    if (args.fr_core is None) != (args.redis_core is None):
        parser.error("--fr-core and --redis-core must be given together; pinning "
                     "one server and not the other is worse than pinning neither")

    # (frankenredis-yaul4) Both engines run in a private directory. Bare,
    # they inherit this process's cwd -- normally the repo root, shared by a dozen
    # agents -- and load whatever dump.rdb is sitting there. An fr-written
    # dump.rdb carrying a FUNCTION library redis 7.2.4 refuses makes redis abort
    # during startup, which reached this harness as a FileNotFoundError on
    # /proc/<pid>/exe inside provenance. A perf harness that cannot boot its
    # incumbent has no ratio to report.
    workdir = tempfile.mkdtemp(prefix="fr_balanced_square_")
    fr_bin = os.path.abspath(args.fr_bin)
    fr = subprocess.Popen(
        pinned(args.fr_core, [fr_bin, "--port", str(args.fr_port),
                              "--save", "", "--appendonly", "no"]),
        cwd=workdir, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    second_arm = ([fr_bin, "--port", str(args.redis_port),
                   "--save", "", "--appendonly", "no"] if args.cross_null
                  else [os.path.abspath(REDIS), "--port", str(args.redis_port),
                        "--save", "", "--appendonly", "no"])
    redis = subprocess.Popen(pinned(args.redis_core, second_arm), cwd=workdir,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_ready(args.fr_port, proc=fr)
        wait_ready(args.redis_port, proc=redis)
        assert_ours(args.fr_port, fr, "fr")
        assert_ours(args.redis_port, redis, "second arm")
        prov = provenance(fr.pid, redis.pid)
        if args.expect_elf and not prov["fr_elf_sha256"].startswith(args.expect_elf):
            raise SystemExit(
                f"ELF mismatch: running image is {prov['fr_elf_sha256'][:16]}, "
                f"expected {args.expect_elf}")

        print("== provenance (self-reported from inside the running processes) ==")
        for key, value in prov.items():
            print(f"  {key:24} {value}")
        if args.cross_null:
            print("\n== CROSS-PROCESS A/A MODE: the second arm is another fr, so "
                  "every ratio below must be 1.0. NOT a competitive row. ==")
        if args.fr_core:
            print(f"  servers pinned: fr={args.fr_core} redis={args.redis_core}")
        else:
            print("  servers unpinned (the per-arm nulls are within-process drift "
                  "and do not bound cross-process placement; run --cross-null to "
                  "measure that term rather than assume it — on this workload it "
                  "measured ~1%)")
        print(f"\nsquare={SQUARE}  rounds={args.rounds}  ops/slot={args.ops}"
              f"  -P{args.pipeline}  null bound +/-{NULL_BOUND}")

        rows = []
        for label, seeds, bench_argv in shapes:
            seed(args.fr_port, seeds)
            seed(args.redis_port, seeds)
            error_probe(args.fr_port, bench_argv, "fr", label)
            error_probe(args.redis_port, bench_argv, "redis", label)
            rows.append(run_row(label, args.fr_port, args.redis_port, bench_argv,
                                args.rounds, args.ops, args.pipeline,
                                args.client_core))
            row = rows[-1]
            print(f"  {row['label']:<14} {row['ratio']:.4f}"
                  f"  [{row['ci'][0]:.4f}, {row['ci'][1]:.4f}]"
                  f"  nulls {row['null_redis']:.4f}/{row['null_fr']:.4f}"
                  f"  {row['verdict']}")

        print(f"\nRATIO = fr ops/s / redis ops/s   (>1 means FrankenRedis faster)")
        print(f"{'shape':<14}{'ratio':>9}{'95% CI':>22}"
              f"{'null redis':>12}{'null fr':>10}  verdict")
        for row in rows:
            ci_text = f"[{row['ci'][0]:.4f}, {row['ci'][1]:.4f}]"
            print(f"{row['label']:<14}{row['ratio']:>9.4f}{ci_text:>22}"
                  f"{row['null_redis']:>12.4f}{row['null_fr']:>10.4f}  {row['verdict']}")
        admissible = [r for r in rows if r["verdict"] == "ADMISSIBLE"]
        print(f"\n{len(admissible)} of {len(rows)} rows admissible; "
              f"{sum(1 for r in rows if r['verdict'] == 'NULL-FAILED')} null-failed")
        return 0
    finally:
        for proc in (fr, redis):
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
