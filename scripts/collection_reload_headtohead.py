#!/usr/bin/env python3
"""collection_reload_headtohead.py — head-to-head TIMING of the RDB collection
codec (DEBUG RELOAD = save+load, and isolated RESTORE = decode) for fr vs the
vendored Redis 7.2.4, on a collection-heavy DB.

This fills the measurement gap the scorecard flags as "owed": the existing
reload_*_gate.py scripts assert FIDELITY (byte/digest parity) but never TIME the
codec head-to-head, so the collection encode/decode levers (presize cluster +
BlackThrush's decode_listpack num_elements presize 0ea29b6fe) had no vs-Redis
ratio. DEBUG RELOAD's load half exercises the decode path; RESTORE isolates it.

Both servers must be started with --enable-debug-command yes|local. Under host
contention absolute ms is noisy, so we INTERLEAVE fr/redis trials and report the
median ratio (the ratio is stable even when absolutes drift), plus CV.

Usage: collection_reload_headtohead.py <redis_port> <fr_port> [--trials N]
       [--hashes H] [--sets S] [--zsets Z] [--members M]
       [--set-kind str|int]
       [--competitive --fr-aa-port PORT [--expect-fr-elf SHA256]]
Exit 0 always (informational).

`--competitive` is the authentication mode. It needs two independent
FrankenRedis processes and the vendored Redis process live in this one Python
invocation. It rotates all three arms within each sample, emits both running
images from `/proc/<pid>/exe`, and rejects an A/B verdict unless the A/A median
is within 0.98..1.02 with a bootstrap median CI.
"""
import hashlib
import itertools
import os
import random
import socket
import statistics
import sys
import time


def opt(flag, default):
    return sys.argv[sys.argv.index(flag) + 1] if flag in sys.argv else default


SELF_TEST = "--self-test" in sys.argv
RS = 17812 if SELF_TEST else (int(sys.argv[1]) if len(sys.argv) > 1 else 17812)
FR = 17811 if SELF_TEST else (int(sys.argv[2]) if len(sys.argv) > 2 else 17811)
TRIALS = int(opt("--trials", "9"))
HASHES = int(opt("--hashes", "2000"))
SETS = int(opt("--sets", "2000"))
ZSETS = int(opt("--zsets", "2000"))
MEMBERS = int(opt("--members", "40"))
SET_KIND = opt("--set-kind", "str")
COMPETITIVE = "--competitive" in sys.argv
FR_AA = int(opt("--fr-aa-port", "0"))
EXPECT_FR_ELF = opt("--expect-fr-elf", "")

ARM_ORDERS = tuple(itertools.permutations(("fr_a", "fr_b", "redis")))


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 5)
        self.s.settimeout(30.0)
        self.b = b""

    def _fill(self):
        d = self.s.recv(1 << 16)
        if not d:
            raise EOFError
        self.b += d

    def _line(self):
        while b"\r\n" not in self.b:
            self._fill()
        l, self.b = self.b.split(b"\r\n", 1)
        return l

    def read(self):
        l = self._line()
        t, rest = l[:1], l[1:]
        if t in (b"+", b"-", b":"):
            return rest
        if t == b"$":
            n = int(rest)
            if n < 0:
                return None
            while len(self.b) < n + 2:
                self._fill()
            data, self.b = self.b[:n], self.b[n + 2:]
            return data
        if t == b"*":
            n = int(rest)
            return [self.read() for _ in range(n)]
        return l

    def cmd(self, *args):
        out = [b"*%d\r\n" % len(args)]
        for a in args:
            a = a if isinstance(a, (bytes, bytearray)) else str(a).encode()
            out.append(b"$%d\r\n%s\r\n" % (len(a), a))
        self.s.sendall(b"".join(out))
        return self.read()

    def pipe(self, cmds):
        buf = []
        for args in cmds:
            buf.append(b"*%d\r\n" % len(args))
            for a in args:
                a = a if isinstance(a, (bytes, bytearray)) else str(a).encode()
                buf.append(b"$%d\r\n%s\r\n" % (len(a), a))
        self.s.sendall(b"".join(buf))
        return [self.read() for _ in cmds]


def preload(c):
    c.cmd("FLUSHALL")
    batch = []
    for i in range(HASHES):
        args = ["HSET", f"h:{i}"]
        for j in range(MEMBERS):
            args += [f"f{j}", f"v{j}"]
        batch.append(args)
        if len(batch) >= 200:
            c.pipe(batch); batch = []
    for i in range(SETS):
        args = ["SADD", f"s:{i}"] + [set_member(i, j) for j in range(MEMBERS)]
        batch.append(args)
        if len(batch) >= 200:
            c.pipe(batch); batch = []
    for i in range(ZSETS):
        args = ["ZADD", f"z:{i}"]
        for j in range(MEMBERS):
            args += [j, f"m{j}"]
        batch.append(args)
        if len(batch) >= 200:
            c.pipe(batch); batch = []
    if batch:
        c.pipe(batch)


def set_member(i, j):
    if SET_KIND == "int":
        if i % 3 == 0:
            return j - (MEMBERS // 2)
        if i % 3 == 1:
            return (j * 257) - 12_345
        return (j * 1_048_573) - 2_147_483_000
    if SET_KIND != "str":
        raise ValueError(f"--set-kind must be str or int, got {SET_KIND!r}")
    return f"m{j}"


def time_reload(c):
    t0 = time.perf_counter()
    r = c.cmd("DEBUG", "RELOAD")
    dt = time.perf_counter() - t0
    if r != b"OK":
        raise RuntimeError(f"DEBUG RELOAD failed: {r!r}")
    return dt


def time_dump(c, keys):
    """Pipelined DUMP of every key (isolates the ENCODE half)."""
    t0 = time.perf_counter()
    for i in range(0, len(keys), 500):
        c.pipe([["DUMP", k] for k in keys[i:i + 500]])
    return time.perf_counter() - t0


def time_restore(c, payloads):
    """Pipelined RESTORE ... REPLACE of every payload (isolates the DECODE half)."""
    t0 = time.perf_counter()
    for i in range(0, len(payloads), 500):
        replies = c.pipe([["RESTORE", b"r:" + k, 0, p, b"REPLACE"]
                          for k, p in payloads[i:i + 500]])
        if any(reply != b"OK" for reply in replies):
            raise RuntimeError(f"RESTORE failed: {next(reply for reply in replies if reply != b'OK')!r}")
    return time.perf_counter() - t0


def running_image_sha(c):
    """Read the executing server image through the live server's own PID."""
    info = c.cmd("INFO", "server")
    if not isinstance(info, bytes):
        raise TypeError(f"INFO server did not return a bulk string: {info!r}")
    fields = dict(
        line.split(b":", 1)
        for line in info.splitlines()
        if b":" in line and not line.startswith(b"#")
    )
    raw_pid = fields.get(b"process_id")
    if raw_pid is None:
        raise RuntimeError("INFO server omitted process_id; cannot self-report executing ELF")
    try:
        executable = os.readlink(f"/proc/{int(raw_pid)}/exe")
    except (OSError, ValueError) as error:
        raise RuntimeError(f"resolve /proc/{raw_pid!r}/exe: {error}") from error
    digest = hashlib.sha256()
    with open(executable, "rb") as image:
        for block in iter(lambda: image.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def bootstrap_median_ci(samples, seed=0, resamples=4096):
    """Deterministic percentile bootstrap CI for a sample median."""
    if len(samples) < 3:
        raise ValueError("need at least three paired samples for a bootstrap median CI")
    rng = random.Random(seed)
    medians = []
    for _ in range(resamples):
        medians.append(statistics.median(rng.choice(samples) for _ in samples))
    medians.sort()
    lo = medians[int(0.025 * (resamples - 1))]
    hi = medians[int(0.975 * (resamples - 1))]
    return statistics.median(samples), lo, hi


def run_competitive_restore(fr_a, fr_b, redis):
    """One-invocation RESTORE A/A+A/B measurement with within-round rotation."""
    keys = [k for k in redis.cmd("KEYS", "*")]
    payloads = [(k, redis.cmd("DUMP", k)) for k in keys]
    if any(payload is None for _, payload in payloads):
        raise RuntimeError("DUMP unexpectedly returned nil while preparing RESTORE workload")

    arms = {"fr_a": fr_a, "fr_b": fr_b, "redis": redis}
    elapsed = {name: [] for name in arms}
    aa_ratios, ab_ratios = [], []
    for trial in range(TRIALS):
        sample = {}
        for arm in ARM_ORDERS[trial % len(ARM_ORDERS)]:
            sample[arm] = time_restore(arms[arm], payloads)
            elapsed[arm].append(sample[arm])
        aa_ratios.append(sample["fr_a"] / sample["fr_b"])
        ab_ratios.append(sample["redis"] / sample["fr_b"])

    aa, aa_lo, aa_hi = bootstrap_median_ci(aa_ratios)
    ab, ab_lo, ab_hi = bootstrap_median_ci(ab_ratios)
    print("\nCOMPETITIVE RESTORE decode (one invocation; three live arms):")
    for arm in ("fr_a", "fr_b", "redis"):
        print(f"  {arm:>5} median={statistics.median(elapsed[arm]) * 1000:.3f}ms")
    print(f"  A/A null (fr_a/fr_b) median={aa:.6f}x 95% CI=[{aa_lo:.6f}, {aa_hi:.6f}]")
    print(f"  A/B redis/fr_b median={ab:.6f}x 95% CI=[{ab_lo:.6f}, {ab_hi:.6f}]")
    if not 0.98 <= aa <= 1.02:
        print("  VERDICT: HOLD — A/A median outside 0.98..1.02; A/B is not authenticated")
    else:
        print("  VERDICT: COMPETITIVE ROW — A/A median accepted; record A/B with its CI")


def self_test():
    assert len(ARM_ORDERS) == 6
    for arm in ("fr_a", "fr_b", "redis"):
        assert [order.index(arm) for order in ARM_ORDERS].count(0) == 2
        assert [order.index(arm) for order in ARM_ORDERS].count(1) == 2
        assert [order.index(arm) for order in ARM_ORDERS].count(2) == 2
    assert bootstrap_median_ci([1.0, 1.0, 1.0]) == (1.0, 1.0, 1.0)
    try:
        bootstrap_median_ci([1.0, 1.0])
    except ValueError:
        pass
    else:
        raise AssertionError("short bootstrap sample did not reject")
    print("PASS collection_reload_headtohead self-test")


def main():
    if SELF_TEST:
        self_test()
        return
    if COMPETITIVE and not FR_AA:
        raise SystemExit("--competitive requires --fr-aa-port PORT")

    fr, rs = Conn(FR), Conn(RS)
    fr_aa = Conn(FR_AA) if COMPETITIVE else None
    print(f"fr:{FR} redis:{RS}  hashes={HASHES} sets={SETS} zsets={ZSETS} members={MEMBERS} set_kind={SET_KIND}")
    print("preloading identical collection-heavy DB into both...")
    preload(fr)
    if fr_aa is not None:
        preload(fr_aa)
    preload(rs)
    fk = fr.cmd("DBSIZE"); rk = rs.cmd("DBSIZE")
    print(f"DBSIZE fr={fk.decode()} redis={rk.decode()}")
    if fr_aa is not None:
        aa_k = fr_aa.cmd("DBSIZE")
        if not (fk == aa_k == rk):
            raise RuntimeError(f"DBSIZE mismatch fr_a={aa_k!r} fr_b={fk!r} redis={rk!r}")
        fr_a_sha, fr_b_sha, redis_sha = running_image_sha(fr_aa), running_image_sha(fr), running_image_sha(rs)
        print(f"ELF_SHA256 fr_a={fr_a_sha} fr_b={fr_b_sha} redis={redis_sha}")
        if fr_a_sha != fr_b_sha:
            raise RuntimeError("A/A arms run different FrankenRedis images")
        if EXPECT_FR_ELF and not fr_b_sha.startswith(EXPECT_FR_ELF):
            raise RuntimeError(f"FrankenRedis ELF mismatch: expected {EXPECT_FR_ELF}, got {fr_b_sha}")
        run_competitive_restore(fr_aa, fr, rs)
        return
    # warm one reload each
    time_reload(fr); time_reload(rs)
    fr_t, rs_t, ratios = [], [], []
    for _ in range(TRIALS):
        rt = time_reload(rs); ft = time_reload(fr)   # interleaved
        rs_t.append(rt); fr_t.append(ft); ratios.append(rt / ft)
    def cv(xs):
        return 100 * statistics.pstdev(xs) / statistics.mean(xs)
    print("\nDEBUG RELOAD (save+load round-trip):")
    print(f"  fr    median={statistics.median(fr_t)*1000:.1f}ms  cv={cv(fr_t):.1f}%")
    print(f"  redis median={statistics.median(rs_t)*1000:.1f}ms  cv={cv(rs_t):.1f}%")
    mr = statistics.median(ratios)

    def verdict(r):
        return "fr FASTER" if r > 1.05 else ("redis faster" if r < 0.95 else "parity")
    print(f"  median ratio (redis/fr) = {mr:.3f}x  [{verdict(mr)}]  trials={[round(r,2) for r in ratios]}")

    # Isolate the two halves so the gap can be attributed to encode (DUMP) vs
    # decode (RESTORE). DEBUG RELOAD's load half exercises the decode path
    # (fr-persist decode_listpack + fr-store object rebuild); DUMP exercises the
    # encode path (fr-store dump_key + fr-persist encode_compact_*).
    keys = [k for k in fr.cmd("KEYS", "*")]
    payloads = [(k, rs.cmd("DUMP", k)) for k in keys]
    df, dr = [], []
    for _ in range(max(5, TRIALS // 2)):
        dr.append(time_dump(rs, keys)); df.append(time_dump(fr, keys))   # encode
    rr2, rf2 = [], []
    for _ in range(max(5, TRIALS // 2)):
        rr2.append(time_restore(rs, payloads)); rf2.append(time_restore(fr, payloads))  # decode
    de = statistics.median(dr) / statistics.median(df)
    dd = statistics.median(rr2) / statistics.median(rf2)
    print("\nDUMP (encode half):")
    print(f"  fr median={statistics.median(df)*1000:.1f}ms cv={cv(df):.1f}%  "
          f"redis median={statistics.median(dr)*1000:.1f}ms cv={cv(dr):.1f}%  "
          f"ratio(redis/fr)={de:.3f}x  [{verdict(de)}]")
    print("RESTORE (decode half):")
    print(f"  fr median={statistics.median(rf2)*1000:.1f}ms cv={cv(rf2):.1f}%  "
          f"redis median={statistics.median(rr2)*1000:.1f}ms cv={cv(rr2):.1f}%  "
          f"ratio(redis/fr)={dd:.3f}x  [{verdict(dd)}]")


if __name__ == "__main__":
    main()
