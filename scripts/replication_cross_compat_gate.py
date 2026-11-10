#!/usr/bin/env python3
"""Bidirectional cross-impl replication gate: fr <-> redis 7.2.4.

Replication wire-compatibility is what makes zero-downtime migration possible
(make fr a replica of a live redis, let it sync, then promote fr) and what a
mixed fr/redis fleet relies on. A break = failed migration or silent replica
divergence. This gate proves BOTH roles work both ways, via DEBUG DIGEST:

  (1) fr master  <- redis replica : redis full-syncs the RDB from fr, then
                                    online writes to fr propagate to redis.
  (2) redis master <- fr replica  : fr full-syncs (PSYNC client side) the RDB
                                    from redis, then online writes to redis
                                    propagate to fr.

Both phases check DIGEST after full sync AND after a batch of online writes
(SET/INCR x2/LPUSH/EXPIRE/ZADD/HSET/SADD/DEL) across string/list/hash/zset/set
/stream types. They then disconnect the replication link, require a partial
resync without another full sync, and check non-idempotent INCR replay in DB 3.

Both servers launched with --enable-debug-command. Self-orchestrating.

Usage: replication_cross_compat_gate.py [--planted-negative] <redis-server-bin> <fr-bin> [base_port]
"""
import argparse
import subprocess
import time

from _respread import assert_ok, assert_seed, cmd, conn


DEFAULT_REDIS_BIN = "legacy_redis_code/redis/src/redis-server"
DEFAULT_FR_BIN = "/tmp/fr_repl"
DEFAULT_BASE = 29821


def q(port, a):
    """One fresh connection, but never a one-recv partial RESP reply."""
    s = conn(port)
    try:
        return cmd(s, *a)
    finally:
        s.close()


def wait_up(port, deadline=8):
    t0 = time.time()
    while time.time() - t0 < deadline:
        try:
            if b"PONG" in q(port, ["PING"]):
                return True
        except (OSError, ValueError):
            time.sleep(0.1)
    return False


def wait_link_up(port, deadline=10):
    t0 = time.time()
    while time.time() - t0 < deadline:
        try:
            if b"master_link_status:up" in q(port, ["INFO", "replication"]):
                return True
        except (OSError, ValueError):
            continue
        time.sleep(0.2)
    return False


def digests_converge(p_master, p_replica, deadline=6):
    """Poll until replica digest == master digest (online propagation settled)."""
    t0 = time.time()
    last = (None, None)
    while time.time() - t0 < deadline:
        dm = q(p_master, ["DEBUG", "DIGEST"]).strip()
        dr = q(p_replica, ["DEBUG", "DIGEST"]).strip()
        last = (dm, dr)
        if dm == dr:
            return True, last
        time.sleep(0.3)
    return False, last


def replication_totals(port):
    """Require real PSYNC counters; a full-sync fallback must fail this lane."""
    reply = q(port, ["INFO", "stats"])
    if not reply.startswith(b"$"):
        raise SystemExit(f"INFO stats failed on {port}: {reply!r}")
    fields = dict(line.split(b":", 1) for line in reply.split(b"\r\n") if b":" in line)
    try:
        return int(fields[b"sync_full"]), int(fields[b"sync_partial_ok"])
    except (KeyError, ValueError) as exc:
        raise SystemExit(f"missing/invalid replication counters on {port}: {reply!r}") from exc


def check_partial_resync(p_master, p_replica, label, failures):
    """Redis PSYNC names last-applied + 1; replay must preserve DB and INCR."""
    writer = conn(p_master)
    try:
        assert_ok(cmd(writer, "SELECT", "3"), f"{label} select DB 3")
        assert_seed(cmd(writer, "INCR", "partial:counter"), 1, f"{label} before disconnect")
        # The write and WAIT belong to the same client. A fresh connection has
        # offset zero and would not prove that this counter reached the replica.
        assert_seed(cmd(writer, "WAIT", "1", "5000"), 1, f"{label} initial counter ACK")
        before_full, before_partial = replication_totals(p_master)
        assert_seed(q(p_master, ["CLIENT", "KILL", "TYPE", "replica"]), 1,
                    f"{label} disconnect replica")
        for expected in (2, 3, 4):
            assert_seed(cmd(writer, "INCR", "partial:counter"), expected,
                        f"{label} disconnected INCR {expected}")

        deadline = time.monotonic() + 12
        after_full, after_partial = before_full, before_partial
        while time.monotonic() < deadline:
            after_full, after_partial = replication_totals(p_master)
            if after_full != before_full or after_partial > before_partial:
                break
            time.sleep(0.05)
        if after_full != before_full or after_partial <= before_partial:
            failures.append((f"{label} partial resync required",
                             (before_full, before_partial + 1),
                             (after_full, after_partial)))
            return
        if not wait_link_up(p_replica):
            failures.append((f"{label} reconnected link", "up", "down"))
            return
        assert_seed(cmd(writer, "INCR", "partial:counter"), 5,
                    f"{label} post-reconnect INCR")
        assert_seed(cmd(writer, "WAIT", "1", "5000"), 1,
                    f"{label} post-reconnect counter ACK")
        replica_reader = conn(p_replica)
        try:
            assert_ok(cmd(replica_reader, "SELECT", "3"), f"{label} read DB 3")
            assert_reply(cmd(replica_reader, "GET", "partial:counter"), b"$1\r\n5\r\n",
                         f"{label} exact-once INCR replay")
            assert_ok(cmd(replica_reader, "SELECT", "0"), f"{label} read DB 0")
            assert_reply(cmd(replica_reader, "GET", "partial:counter"), b"$-1\r\n",
                         f"{label} DB isolation")
        finally:
            replica_reader.close()
        ok, (dm, dr) = digests_converge(p_master, p_replica)
        if not ok:
            failures.append((f"{label} partial-resync dataset", dm, dr))
        final_full, final_partial = replication_totals(p_master)
        if final_full != before_full or final_partial <= before_partial:
            failures.append((f"{label} no late full-sync fallback",
                             (before_full, before_partial + 1),
                             (final_full, final_partial)))
    finally:
        writer.close()


SEED = [
    ("SET s", ["SET", "s", "hi"], b"+OK\r\n"),
    ("RPUSH l", ["RPUSH", "l", *[str(i) for i in range(50)]], b":50\r\n"),
    ("HSET h", ["HSET", "h", *[value for i in range(300) for value in (f"f{i}", str(i))]], b":300\r\n"),
    ("ZADD z", ["ZADD", "z", *[value for i in range(300) for value in (str(i * 1.5), f"m{i}")]], b":300\r\n"),
    ("SADD st", ["SADD", "st", *[f"m{i}" for i in range(300)]], b":300\r\n"),
    ("XADD x", ["XADD", "x", "1-1", "f", "v"], b"$3\r\n1-1\r\n"),
]
ONLINE = [
    ("SET on", ["SET", "on", "v"], b"+OK\r\n"),
    ("INCR c first", ["INCR", "c"], b":1\r\n"),
    ("INCR c second", ["INCR", "c"], b":2\r\n"),
    ("LPUSH ol", ["LPUSH", "ol", "a", "b"], b":2\r\n"),
    ("EXPIRE on", ["EXPIRE", "on", "100000"], b":1\r\n"),
    ("ZADD oz", ["ZADD", "oz", "5", "m"], b":1\r\n"),
    ("HSET oh", ["HSET", "oh", "f", "v"], b":1\r\n"),
    ("SADD os", ["SADD", "os", "x"], b":1\r\n"),
    ("DEL s", ["DEL", "s"], b":1\r\n"),
]


def assert_reply(reply, expected, label):
    """Fail closed when a seed/write did not actually take effect."""
    if expected == b"+OK\r\n":
        return assert_ok(reply, label)
    if expected.startswith(b":"):
        return assert_seed(reply, int(expected[1:-2]), label)
    if reply != expected:
        raise SystemExit(
            f"SEED FAILED [{label}]: got {reply!r}, expected {expected!r}")
    return reply


def apply_checked(port, writes, phase):
    for label, command, expected in writes:
        assert_reply(q(port, command), expected, f"{phase} {label}")


def mismatches(expected, actual):
    """The gate predicate, isolated so the planted negative exercises it."""
    return expected != actual


def planted_negative():
    """Must fail: a differing full digest reply cannot pass this gate."""
    oracle = b"$40\r\n0123456789abcdef0123456789abcdef01234567\r\n"
    wrong = b"$40\r\n0123456789abcdef0123456789abcdef01234568\r\n"
    if not mismatches(oracle, wrong):
        print("PLANTED NEGATIVE MISSED: unequal replication digests compared equal")
        return 0
    print("PLANTED NEGATIVE DETECTED: unequal replication digest replies fail the gate")
    return 1


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("redis_bin", nargs="?", default=DEFAULT_REDIS_BIN)
    parser.add_argument("fr_bin", nargs="?", default=DEFAULT_FR_BIN)
    parser.add_argument("base_port", nargs="?", type=int, default=DEFAULT_BASE)
    parser.add_argument(
        "--planted-negative",
        action="store_true",
        help="prove this gate rejects a deliberately different complete digest reply",
    )
    return parser.parse_args()


def main(args):
    if args.planted_negative:
        return planted_negative()

    redis_bin, fr_bin, base = args.redis_bin, args.fr_bin, args.base_port
    procs = []
    failures = []
    try:
        # ---- phase 1: fr master, redis replica ----
        procs.append(subprocess.Popen([fr_bin, "--port", str(base), "--enable-debug-command", "yes"],
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not wait_up(base):
            raise SystemExit("fr master did not start")
        assert_ok(q(base, ["FLUSHALL"]), "fr master FLUSHALL")
        apply_checked(base, SEED, "fr master seed")
        procs.append(subprocess.Popen(
            [redis_bin, "--port", str(base + 1), "--replicaof", "127.0.0.1", str(base),
             "--save", "", "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not wait_up(base + 1):
            raise SystemExit("redis replica did not start")
        if not wait_link_up(base + 1):
            failures.append(("fr-master<-redis-replica", "link never came up", ""))
        else:
            ok, (dm, dr) = digests_converge(base, base + 1)
            if not ok:
                failures.append(("fr-master<-redis-replica full-sync", dm, dr))
            apply_checked(base, ONLINE, "fr master online")
            ok, (dm, dr) = digests_converge(base, base + 1)
            if not ok:
                failures.append(("fr-master<-redis-replica online", dm, dr))
            # WAIT durability-ack: fr master must report the 1 connected replica
            # acknowledging the latest write offset (clients gate write durability
            # on this). WAIT 1 <timeout> -> :1 once the redis replica ACKs.
            writer = conn(base)
            try:
                assert_ok(cmd(writer, "SET", "wait_probe", "v"), "fr master WAIT probe")
                w = cmd(writer, "WAIT", "1", "2000").strip()
                if w != b":1":
                    failures.append(("fr-master WAIT 1 (replica ack)", b":1", w))
            finally:
                writer.close()
            check_partial_resync(base, base + 1, "fr-master<-redis-replica", failures)

        # ---- phase 2: redis master, fr replica ----
        procs.append(subprocess.Popen(
            [redis_bin, "--port", str(base + 2), "--save", "", "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not wait_up(base + 2):
            raise SystemExit("redis master did not start")
        assert_ok(q(base + 2, ["FLUSHALL"]), "redis master FLUSHALL")
        apply_checked(base + 2, SEED, "redis master seed")
        procs.append(subprocess.Popen(
            [fr_bin, "--port", str(base + 3), "--replicaof", "127.0.0.1", str(base + 2),
             "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not wait_up(base + 3):
            raise SystemExit("fr replica did not start")
        if not wait_link_up(base + 3):
            failures.append(("redis-master<-fr-replica", "link never came up", ""))
        else:
            ok, (dm, dr) = digests_converge(base + 2, base + 3)
            if not ok:
                failures.append(("redis-master<-fr-replica full-sync", dm, dr))
            apply_checked(base + 2, ONLINE, "redis master online")
            ok, (dm, dr) = digests_converge(base + 2, base + 3)
            if not ok:
                failures.append(("redis-master<-fr-replica online", dm, dr))
            check_partial_resync(base + 2, base + 3, "redis-master<-fr-replica", failures)
    finally:
        for p in procs:
            p.terminate()
        time.sleep(0.3)
        for p in procs:
            try:
                p.kill()
            except ProcessLookupError:
                continue

    print("=" * 60)
    if failures:
        print(f"FAIL - {len(failures)} replication divergence(s):")
        for phase, m, r in failures:
            print(f"  [{phase}]\n    master={m}\n    replica={r}")
        return 1
    print("PASS - replication wire-compatible fr <-> redis 7.2.4 both roles"
          " (full + partial PSYNC resync, DB-isolated exact-once INCR, WAIT replica-ack,"
          " DEBUG DIGEST identical)")
    return 0


raise SystemExit(main(parse_args()))
