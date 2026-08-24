#!/usr/bin/env python3
"""Differential SCAN-CONTRACT test: FrankenRedis vs live Redis 7.2.4.

    python3 scripts/scan_guarantee_differ.py --fr /tmp/fr_bin \
        --redis legacy_redis_code/redis/src/redis-server

WHY THIS IS A CONTRACT TEST AND NOT AN ORDER COMPARISON
-------------------------------------------------------
Redis seeds its dict hash function with random bytes at startup, so SCAN order is
not reproducible even between two runs of the SAME redis binary on the SAME data.
Measured, three fresh 7.2.4 servers, identical inserts, `SCAN 0 COUNT 100`:

    run 1   key:1, myzset, myhash, myset, key:2, key:3, mylist
    run 2   myhash, myset, key:1, myzset, key:2, mylist, key:3
    run 3   myhash, mylist, myset, key:2, key:1, key:3, myzset

So no fixture and no differ can assert "fr returns what redis returns" for SCAN.
What Redis actually promises is a CONTRACT, and that is what this compares:

  G1  a key present for the entire scan is returned AT LEAST ONCE
  G2  no key is returned TWICE within a rehash-free window
  G3  the scan TERMINATES -- cursor comes back to 0 in bounded steps

THE DIFFERENTIAL IS "fr IS NO WORSE THAN REDIS", NOT "fr IS PERFECT". Each guarantee
is evaluated on BOTH engines against the SAME operation schedule, and fr fails only
where redis succeeded. That matters because Redis itself does not promise G2
unconditionally -- background incremental rehashing can legitimately produce
duplicates -- so an absolute assertion would be measuring Redis's scheduler, not
fr's correctness. Anchoring to redis's own observed behaviour makes the test
non-vacuous in the other direction too: if a phase turns out to prove nothing
because redis also violated it, that is reported rather than silently passing.

WHY IT EXISTS (frankenredis-uhthd): the keyspace RAM lever replaces fr's sorted-order
SCAN with a reverse-binary cursor over the hash table. Sorted order is an fr-only
guarantee STRONGER than Redis's, and the fixture that pins it is pinning fr's own
choice, not parity. This differ is what makes that change safe to land: it holds the
real contract fixed while the order underneath it moves. Run it BEFORE the wiring
(fr sorted-order must pass) and AFTER (fr hash-order must still pass).

Exit 0 = fr matched redis on every guarantee. Exit 1 = a real divergence.
"""

import argparse
import hashlib
import platform
import socket
import subprocess
import sys
import tempfile
import time

DEFAULT_KEYS = 5000


def enc(*parts):
    out = b"*%d\r\n" % len(parts)
    for p in parts:
        b = p.encode() if isinstance(p, str) else p
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


class Conn:
    """Minimal RESP2 client with a real reply parser (SCAN returns a nested array)."""

    def __init__(self, port, timeout=120):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.sock.settimeout(timeout)
        self.buf = b""

    def _fill(self):
        chunk = self.sock.recv(1 << 20)
        if not chunk:
            raise SystemExit("server closed the connection")
        self.buf += chunk

    def _line(self):
        while b"\r\n" not in self.buf:
            self._fill()
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _read(self):
        line = self._line()
        tag, rest = line[:1], line[1:]
        if tag in (b"+", b":"):
            return rest
        if tag == b"-":
            raise SystemExit(f"server error reply: {rest!r}")
        if tag == b"$":
            n = int(rest)
            if n == -1:
                return None
            while len(self.buf) < n + 2:
                self._fill()
            payload, self.buf = self.buf[:n], self.buf[n + 2 :]
            return payload
        if tag == b"*":
            n = int(rest)
            if n == -1:
                return None
            return [self._read() for _ in range(n)]
        raise SystemExit(f"unparsed reply tag {line!r}")

    def cmd(self, *parts):
        self.sock.sendall(enc(*parts))
        return self._read()

    def pipeline(self, cmds):
        self.sock.sendall(b"".join(enc(*c) for c in cmds))
        return [self._read() for _ in cmds]


def wait_ready(port, proc, timeout=120):
    for _ in range(timeout * 10):
        if proc.poll() is not None:
            raise SystemExit(f"server exited early rc={proc.returncode}")
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.3).close()
            return
        except OSError:
            time.sleep(0.1)
    raise SystemExit(f"server on port {port} never accepted")


def free_port(start, taken):
    for port in range(start, start + 500):
        if port in taken:
            continue
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
        except OSError:
            taken.add(port)
            return port
    raise SystemExit("no free port")


class Engine:
    def __init__(self, label, binary, port):
        self.label = label
        self.dir = tempfile.mkdtemp(prefix=f"scanguard_{label}_")
        self.proc = subprocess.Popen(
            [
                binary, "--port", str(port),
                "--dir", self.dir,
                "--dbfilename", "scanguard-nonexistent.rdb",
                "--save", "", "--appendonly", "no",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        wait_ready(port, self.proc)
        self.c = Conn(port)

    def flush(self):
        self.c.cmd("FLUSHALL")

    def add(self, keys):
        for i in range(0, len(keys), 1000):
            self.c.pipeline([("SET", k, "v") for k in keys[i : i + 1000]])

    def delete(self, keys):
        for i in range(0, len(keys), 1000):
            self.c.pipeline([("DEL", k) for k in keys[i : i + 1000]])

    def scan_step(self, cursor, count):
        cur, batch = self.c.cmd("SCAN", str(cursor), "COUNT", str(count))
        return int(cur), [b.decode() for b in batch]

    def stop(self):
        try:
            self.proc.kill()
            self.proc.wait(timeout=15)
        except Exception:
            pass


# --------------------------------------------------------------------------
# The three phases. Each returns a dict of observations; the CALLER decides
# pass/fail by comparing fr's observations to redis's, never against a constant.
# --------------------------------------------------------------------------

MAX_STEPS = 2_000_000  # G3: a scan that needs more steps than this has not terminated


def phase_static(eng, n, count):
    """No writes during the scan: G1, G2 and G3 all evaluable."""
    eng.flush()
    keys = [f"k:{i}" for i in range(n)]
    eng.add(keys)
    seen, steps, cursor = [], 0, 0
    while True:
        cursor, batch = eng.scan_step(cursor, count)
        seen.extend(batch)
        steps += 1
        if cursor == 0 or steps > MAX_STEPS:
            break
    unique = set(seen)
    return {
        "terminated": cursor == 0,
        "steps": steps,
        "missing": len(set(keys) - unique),
        "duplicates": len(seen) - len(unique),
        "returned_unique": len(unique),
        "expected": n,
    }


def phase_grow(eng, n, count, grow_per_step):
    """Insert NEW keys between steps to force table growth mid-scan.

    G1 is asserted only over the keys present for the WHOLE scan (the original n).
    Keys added mid-scan may or may not appear -- that is inside the contract -- so
    they are excluded from the missing count rather than counted as failures.
    """
    eng.flush()
    original = [f"k:{i}" for i in range(n)]
    eng.add(original)
    seen, steps, cursor, extra = [], 0, 0, 0
    while True:
        cursor, batch = eng.scan_step(cursor, count)
        seen.extend(batch)
        steps += 1
        if cursor == 0 or steps > MAX_STEPS:
            break
        fresh = [f"grow:{extra + j}" for j in range(grow_per_step)]
        extra += grow_per_step
        eng.add(fresh)
    unique = set(seen)
    return {
        "terminated": cursor == 0,
        "steps": steps,
        "missing": len(set(original) - unique),
        "inserted_midscan": extra,
        "expected": n,
    }


def phase_shrink(eng, n, count, delete_per_step):
    """Delete keys between steps to force rehash/shrink mid-scan.

    Deletions are drawn from a RESERVED half of the keyspace, so the SURVIVING half
    is present for the entire scan and G1 applies to it exactly. Deleting keys the
    scan is also asserting on would make the phase untestable.
    """
    eng.flush()
    survivors = [f"keep:{i}" for i in range(n)]
    doomed = [f"drop:{i}" for i in range(n)]
    eng.add(survivors)
    eng.add(doomed)
    seen, steps, cursor, killed = [], 0, 0, 0
    while True:
        cursor, batch = eng.scan_step(cursor, count)
        seen.extend(batch)
        steps += 1
        if cursor == 0 or steps > MAX_STEPS:
            break
        victims = doomed[killed : killed + delete_per_step]
        killed += len(victims)
        if victims:
            eng.delete(victims)
    unique = set(seen)
    return {
        "terminated": cursor == 0,
        "steps": steps,
        "missing": len(set(survivors) - unique),
        "deleted_midscan": killed,
        "expected": n,
    }


def evaluate(name, r, f):
    """Compare fr's observations to redis's for one phase.

    Returns (failures, vacuous). `vacuous` records guarantees that redis ITSELF did
    not uphold on this schedule — those prove nothing about fr either way and must
    not be silently counted as passes.
    """
    failures, vacuous = [], []

    # G3 -- termination. Absolute: both must terminate.
    if not r["terminated"]:
        vacuous.append(f"{name}/G3: redis itself did not terminate")
    elif not f["terminated"]:
        failures.append(f"{name}/G3: fr scan did not terminate in {f['steps']} steps")

    # G1 -- present-throughout keys returned at least once. fr must be no worse than
    # redis on the SAME schedule.
    if r["missing"] > 0:
        vacuous.append(f"{name}/G1: redis itself missed {r['missing']} present-throughout keys")
    elif f["missing"] > 0:
        failures.append(
            f"{name}/G1: fr missed {f['missing']} of {f['expected']} "
            "keys present for the whole scan"
        )

    # G2 -- no duplicates in a rehash-free window. Only the static phase HAS such a
    # window by construction; grow/shrink deliberately rehash, where the contract
    # permits duplicates for BOTH engines.
    if name == "static":
        if r["duplicates"] > 0:
            vacuous.append(
                f"static/G2: redis returned {r['duplicates']} duplicates with no writes "
                "in flight (background rehash) -- window not rehash-free"
            )
        elif f["duplicates"] > 0:
            failures.append(
                f"static/G2: fr returned {f['duplicates']} duplicates in a rehash-free "
                "window where redis returned none"
            )
    return failures, vacuous


def self_test():
    """Negative control: prove this differ CATCHES a broken SCAN.

    A differential that only ever reports PASS is indistinguishable from one that
    does nothing, and this one reports PASS against today's fr. So each guarantee is
    driven with a synthetic 'fr' observation that violates exactly that guarantee,
    against a healthy redis observation, and the differ must report a failure naming
    it. If any of these came back clean the instrument would be tautological.
    """
    healthy = {
        "terminated": True, "steps": 100, "missing": 0,
        "duplicates": 0, "returned_unique": 5000, "expected": 5000,
    }
    cases = [
        ("static", "G1", {**healthy, "missing": 7}),
        ("static", "G2", {**healthy, "duplicates": 3}),
        ("static", "G3", {**healthy, "terminated": False}),
        ("grow", "G1", {**healthy, "missing": 1}),
        ("shrink", "G1", {**healthy, "missing": 1}),
    ]
    ok = True
    print("=== self-test (negative control) ===")
    for phase, guarantee, broken in cases:
        failures, _ = evaluate(phase, healthy, broken)
        caught = any(f"/{guarantee}" in msg for msg in failures)
        print(f"  {phase:7s} broken {guarantee} -> {'CAUGHT' if caught else 'MISSED'}")
        ok &= caught
    # And the converse: a healthy fr must NOT be flagged, or the differ is useless
    # in the other direction.
    for phase in ("static", "grow", "shrink"):
        failures, _ = evaluate(phase, healthy, healthy)
        print(f"  {phase:7s} healthy      -> {'clean' if not failures else 'FALSE POSITIVE'}")
        ok &= not failures
    print("  self-test PASS" if ok else "  self-test FAIL")
    return 0 if ok else 1


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fr")
    ap.add_argument("--redis")
    ap.add_argument("--keys", type=int, default=DEFAULT_KEYS)
    ap.add_argument("--count", type=int, default=10)
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="run the negative control only: prove the differ catches a broken SCAN",
    )
    args = ap.parse_args()

    if args.self_test:
        return self_test()
    if not args.fr or not args.redis:
        ap.error("--fr and --redis are required unless --self-test is given")

    taken = set()
    fr = Engine("fr", args.fr, free_port(7940, taken))
    rd = Engine("redis", args.redis, free_port(7940, taken))

    failures = []
    vacuous = []
    try:
        phases = [
            ("static", lambda e: phase_static(e, args.keys, args.count)),
            ("grow", lambda e: phase_grow(e, args.keys, args.count, 25)),
            ("shrink", lambda e: phase_shrink(e, args.keys, args.count, 25)),
        ]
        for name, run in phases:
            r = run(rd)
            f = run(fr)
            print(f"\n=== phase {name} ===")
            print(f"  redis  {r}")
            print(f"  fr     {f}")
            phase_failures, phase_vacuous = evaluate(name, r, f)
            failures.extend(phase_failures)
            vacuous.extend(phase_vacuous)

        print("\n=== provenance ===")
        print(f"  host          {platform.node()}")
        print(f"  keys          {args.keys}   COUNT {args.count}")
        print(f"  fr    sha256  {sha256_file(args.fr)}")
        print(f"  redis sha256  {sha256_file(args.redis)}")
        print(
            "  redis version "
            + subprocess.run(
                [args.redis, "--version"], capture_output=True, text=True
            ).stdout.strip()
        )

        print("\n=== verdict ===")
        for v in vacuous:
            print(f"  NOT PROVEN  {v}")
        if failures:
            for f in failures:
                print(f"  FAIL        {f}")
            return 1
        if len(vacuous) == 3 * 3:
            print("  VACUOUS: every guarantee was unprovable on redis; nothing was tested")
            return 1
        print("  PASS: fr matches redis on every guarantee that redis itself upheld")
        return 0
    finally:
        fr.stop()
        rd.stop()


if __name__ == "__main__":
    sys.exit(main())
