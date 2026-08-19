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
       [--swap-preload]   reverse which fr arm is preloaded first (33832 A/A attribution)
       [--drift-curve N]  per-trial ms vs trial index for ONE fr arm (33832)
       [--warmup-passes N] discarded passes per arm before timing (default 8)
       [--confirm N]      repeat sampling N times; accept only if EVERY null is in band
Exit 0 always (informational).

`--competitive` is the authentication mode. It needs two independent
FrankenRedis processes and the vendored Redis process live in this one Python
invocation. It rotates all three arms within each sample, emits both running
images from `/proc/<pid>/exe`, and rejects an A/B verdict unless the A/A median
is within 0.98..1.02 with a bootstrap median CI.

PIN THE THREE SERVERS TO CORES OR THIS MODE CANNOT AUTHENTICATE ON A BUSY HOST.
Measured 2026-08-15 at loadavg 11-17: unpinned, the two-process A/A missed the
0.98..1.02 band on SIX consecutive invocations, scattering 0.918-1.058 — i.e.
the null moved more than the 0.44-0.50 effect it was gating, and raising the
trial count from 9 to 72 did not help because the term is not sampling noise.
Pinned to symmetric core sets it authenticated on the first try and reproduced:

    taskset -c 0-3   frankenredis --port A ...
    taskset -c 4-7   frankenredis --port B ...     # same CCD as A, symmetric
    taskset -c 8-11  redis-server  --port R ...

giving A/A 1.010558 CI [1.001541, 1.015901] and 1.008119 CI [0.984687,
1.025978] on two runs.

THOSE CORE NUMBERS ARE NOT PORTABLE, and on thinkstation1 they are the wrong
ones (BlackThrush, 2026-08-19). Pinned exactly as written above, three
consecutive invocations put the two-process A/A null OUTSIDE the band in the
SAME direction -- 1.241194, 1.095013, 1.096331 -- i.e. whichever fr arm sat on
cores 0-3 was consistently the slower one. Systematic, not scattered, so it is
not sampling noise.

The cheap test that identifies it, and which is worth running on any new host
before trusting a null: SWAP the two fr core sets and re-run. If the bias
follows the CORES the null inverts; if it follows the process or the preload
order it does not. Measured here: 1.096331 with fr_a on 0-3, and 0.952773 with
fr_a on 4-7. It inverted, so it is placement -- cores 0-3 carry more of this
box's interrupt and kernel work, and an arm pinned there is not the twin of one
pinned to 4-7.

Moving BOTH fr arms off the low cores (8-11 and 12-15, still one L3 by
/sys/devices/system/cpu/cpu8/cache/index3/shared_cpu_list = 8-15,40-47, with
redis on 16-19) removed the systematic component: the nulls became 1.092844 and
0.972669, scattered rather than one-directional. That still did not
authenticate at loadavg 16-31, which is the honest limit of this instrument --
placement is necessary, quiet is also necessary, and neither substitutes for the
other. The A/A compares two PROCESSES, so it nulls the engine
and the process's core placement together; on a 32-core 4-CCD part an unpinned
pair lands wherever the scheduler puts it and that term swamps the engine. The
`fr_b halves` line is the same-process drift null, DIAGNOSTIC ONLY — it is not
the gate, because the A/B it would authenticate is itself a cross-process
comparison and must be nulled by one.

CONFIRMED INDEPENDENTLY 2026-08-19 (BrownIbis), thinkstation1, fr_a on 8-11,
fr_b on 12-15, redis on 16-19, loadavg 21.8-23.5, iowait 0.93%, both fr arms
reporting the SAME ELF sha256 and the incumbent verified against vendored HEAD:

    --trials 12              A/A 0.999246 CI [0.904607, 1.053946]
    --trials 12 --confirm 3  1 of 3 nulls outside band (0.968775) -> HOLD

The revised pinning reproduces: the A/A medians here are 0.981 and 0.999 rather
than the one-directional 1.24/1.10/1.10 that cores 0-3 produced, so the
systematic placement term really is gone. What remains is scatter, and at this
load the CI is far too wide to gate a 0.5-0.7 effect. Both halves of the earlier
finding hold — placement is necessary, quiet is also necessary — and the
instrument refusing here is it working, not failing. Do not quote the A/B from
a HOLD run.
"""
import hashlib
import itertools
import os
import random
import socket
import statistics
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _incumbent import check_incumbent_provenance  # noqa: E402  (sys.path set above)
import time


def opt(flag, default):
    return sys.argv[sys.argv.index(flag) + 1] if flag in sys.argv else default


SELF_TEST = "--self-test" in sys.argv
# (frankenredis-i41sx) 24 on a 64-way box: high enough that an ordinarily busy host still
# measures, low enough that a neighbouring project's benchmark trips it. Deliberately not
# derived from CPU count -- the number that matters is how many runnable threads are
# competing with THREE pinned servers, not how wide the machine is.
CONTENDED_RUNQ = int(opt("--contended-runq", "24"))
ALLOW_CONTENDED = "--allow-contended" in sys.argv
RS = 17812 if SELF_TEST else (int(sys.argv[1]) if len(sys.argv) > 1 else 17812)
FR = 17811 if SELF_TEST else (int(sys.argv[2]) if len(sys.argv) > 2 else 17811)
TRIALS = int(opt("--trials", "9"))
HASHES = int(opt("--hashes", "2000"))
SETS = int(opt("--sets", "2000"))
ZSETS = int(opt("--zsets", "2000"))
MEMBERS = int(opt("--members", "40"))
SET_KIND = opt("--set-kind", "str")
COMPETITIVE = "--competitive" in sys.argv
# (frankenredis-33832) Discarded passes per arm before timing; see the drift-curve table
# at the warmup loop. Overridable so the count stays a measured knob, not a constant.
WARMUP_PASSES = int(opt("--warmup-passes", "8"))
# (frankenredis-33832) Repeats of the whole sampling within one invocation; the verdict
# accepts only if EVERY round's null is in band. 1 = legacy behaviour, which can no longer
# print an acceptance.
CONFIRM_RUNS = int(opt("--confirm", "1"))
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


def host_snapshot():
    """(loadavg1, running_procs, iowait_pct) — the three numbers that decide whether a
    measurement window is real.

    `running` is field 4 of /proc/loadavg's `R/T` pair, i.e. the instantaneous run queue.
    It is used rather than loadavg because loadavg LAGS: it stays high for minutes after a
    neighbour finishes and stays low for a minute after one starts, which is exactly the
    window in which a measurement gets silently corrupted.
    """
    with open("/proc/loadavg") as fh:
        parts = fh.read().split()
    load1 = float(parts[0])
    running = int(parts[3].split("/")[0])
    iowait = 0.0
    try:
        with open("/proc/stat") as fh:
            for line in fh:
                if line.startswith("cpu "):
                    f = [float(x) for x in line.split()[1:]]
                    total = sum(f)
                    if total > 0:
                        iowait = 100.0 * f[4] / total
                    break
    except OSError:
        pass
    return load1, running, iowait


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


def competitive_verdict(nulls, lo=0.98, hi=1.02, min_runs=2):
    """(frankenredis-33832) A PASS is only a pass if it REPRODUCES.

    Measured the hard way: at --trials 36 this harness produced A/A nulls of 1.010713x
    (accepted) and then 0.930230x (refused) on two back-to-back invocations at the same
    settings and the same loadavg, with the A/B moving 0.602060x -> 0.559893x alongside.
    Banking the first would have recorded a certified 0.60x that the very next run
    contradicts.

    So the verdict takes the FULL LIST of nulls from repeated invocations and passes only
    if EVERY one lands in band. One in-band null among several is evidence the gate is
    flaky, not evidence the arms are equal -- and a flaky gate is not a passed gate.
    """
    outside = [n for n in nulls if not (lo <= n <= hi)]
    if len(nulls) < min_runs:
        # Report the band too when it ALSO fails, so a single out-of-band null does not
        # read as "just needs another run".
        extra = ("" if not outside
                 else " (and it is outside %.2f..%.2f anyway)" % (lo, hi))
        return False, ("only %d null(s)%s; a single in-band result is exactly the lucky "
                       "PASS this guard exists to reject -- repeat the invocation"
                       % (len(nulls), extra))
    if outside:
        return False, ("%d of %d nulls outside %.2f..%.2f: %s"
                       % (len(outside), len(nulls), lo, hi,
                          ", ".join("%.6f" % n for n in outside)))
    return True, "all %d nulls within %.2f..%.2f" % (len(nulls), lo, hi)


def run_drift_curve(arm, redis, trials):
    """(frankenredis-33832) Per-trial RESTORE time for ONE arm against trial INDEX.

    Everything else in this file compares arms. This deliberately does not: the
    two-process A/A was chased through four core placements and then attributed to
    preload order by `--swap-preload`, and warming every arm STILL left the same
    process's own two halves 23 pct apart (fr_b halves null 0.770562x). A term that
    large inside ONE process, on ONE core, running ONE ELF, is not comparable to
    anything -- it has to be characterised on its own before any A/B built on top of
    it can authenticate.

    The SHAPE is the answer, which is why this prints the series rather than a summary
    statistic:
      * a monotone rise implicates GROWTH -- allocator arena, fragmentation, the DUMP
        cache, expired-key bookkeeping accumulating across repeated RESTORE...REPLACE
      * a sawtooth implicates a PERIODIC cycle -- active expire, incremental rehash
      * a flat series with scatter implicates the HOST, and would mean the drift is not
        fr's at all
    """
    keys = [k for k in redis.cmd("KEYS", "*")]
    payloads = [(k, redis.cmd("DUMP", k)) for k in keys]
    if any(payload is None for _, payload in payloads):
        raise RuntimeError("DUMP returned nil while preparing the drift workload")
    time_restore(arm, payloads)  # discarded warmup, same as the competitive path
    # time_restore returns SECONDS (time.perf_counter delta); scale for display.
    series = [1000.0 * time_restore(arm, payloads) for _ in range(trials)]
    print("  trial      ms")
    for i, ms in enumerate(series):
        print("  %5d  %8.2f" % (i, ms))
    n = len(series)
    q = max(1, n // 4)
    quarters = [statistics.median(series[i * q:(i + 1) * q]) for i in range(4)]
    rises = sum(1 for a, b in zip(series, series[1:]) if b > a)
    print("  quartile medians (ms): " + "  ".join("%.2f" % x for x in quarters))
    print("  first/last quartile median ratio: %.4fx" % (quarters[0] / quarters[-1]))
    print("  monotone-rise fraction: %.2f  (0.5 = no trend, ->1.0 = steady growth)"
          % (rises / max(1, n - 1)))
    print("  min %.2f ms  median %.2f ms  max %.2f ms  spread %.1f pct"
          % (min(series), statistics.median(series), max(series),
             100.0 * (max(series) - min(series)) / min(series)))


def run_competitive_restore(fr_a, fr_b, redis):
    """One-invocation RESTORE A/A+A/B measurement with within-round rotation."""
    # (frankenredis-i41sx) CONTENTION PREFLIGHT, and the POST-check that matters more.
    #
    # A window was lost measuring exactly this: the host read runq 5 / idle 88 when checked
    # by hand, and a neighbouring project started a ~43-core benchmark ninety seconds later.
    # Only the A/A null caught it, and it could not say WHY it failed -- that took a
    # separate `ps`, after the window was already gone.
    #
    # So the run records the run queue at BOTH ends. A quiet reading is not a quiet window,
    # and the number that exposes the difference is the delta, not either endpoint.
    load_before, runq_before, iowait_before = host_snapshot()
    if runq_before > CONTENDED_RUNQ and not ALLOW_CONTENDED:
        print("  PREFLIGHT REFUSED: run queue %d exceeds %d — the host is already contended."
              % (runq_before, CONTENDED_RUNQ))
        print("  Nothing measured. Re-run in a quiet window, or pass --allow-contended to "
              "record a deliberately unusable row.")
        return None
    keys = [k for k in redis.cmd("KEYS", "*")]
    payloads = [(k, redis.cmd("DUMP", k)) for k in keys]
    if any(payload is None for _, payload in payloads):
        raise RuntimeError("DUMP unexpectedly returned nil while preparing RESTORE workload")

    arms = {"fr_a": fr_a, "fr_b": fr_b, "redis": redis}
    elapsed = {name: [] for name in arms}
    aa_ratios, ab_ratios = [], []
    # Trials are grouped into ROUNDS of len(ARM_ORDERS), so every arm occupies
    # every position exactly once per round. Without this the balance the
    # self-test asserts only holds when TRIALS happens to be a multiple of 6 —
    # and the default of 9 is not, so orders 0..2 ran twice and 3..5 once.
    rounds = max(1, TRIALS // len(ARM_ORDERS))
    round_min = {name: [] for name in arms}
    # (frankenredis-33832) DISCARDED WARMUP ON EVERY ARM, and it is what finally makes the
    # two-process A/A null mean anything.
    #
    # The null refused four times -- 0.936, 0.686, 1.076, 1.061 -- and placement was
    # eliminated: the last two ran on quiet, same-CCD, cpu0-free symmetric blocks and still
    # came out ~6 pct apart between two processes running the SAME ELF. `--swap-preload`
    # then ATTRIBUTED it. Same cores, only the preload order reversed:
    #
    #     preload order          fr_a      fr_b     slower arm        A/A null
    #     fr_b first (normal)   35.901ms  33.429ms  fr_a (2nd loaded)  1.102522x
    #     fr_a first (swapped)  34.521ms  37.012ms  fr_b (2nd loaded)  0.937629x
    #
    # The slow arm FOLLOWS THE LOAD ORDER, not the core set: whichever fr process is
    # preloaded SECOND is the slower one, and that alone moves the null ~17 pct across 1.0.
    # A second-loaded process starts its timed trials with a colder allocator and colder
    # page cache than the first, and no core mask touches either.
    #
    # One discarded pass per arm pays that cost before the clock starts. It is thrown away,
    # so it cannot enter any sample; the only thing it changes is that arm N is no longer
    # measured in a state arm N-1 was not.
    # (frankenredis-33832) The warmup is EIGHT passes per arm, not one, and the count is
    # measured rather than picked. `--drift-curve` characterised the within-process term:
    #
    #     run       quartile medians (ms)          first/last   monotone-rise fraction
    #     36 trials  52.0  53.0  58.0  58.0          0.892x            0.60
    #     40 trials  48.6  51.7  51.0  52.4          0.929x            0.51
    #
    # The first quartile is the fastest in both, then it PLATEAUS, and a rise fraction of
    # ~0.5 rules out steady growth. So this is a settling transient spanning roughly a
    # quartile -- about ten trials -- not unbounded accumulation. That is precisely why the
    # single discarded pass added earlier did not absorb it: one pass is inside the
    # transient, so the arm was still climbing when the clock started.
    for _ in range(WARMUP_PASSES):
        for arm in arms:
            time_restore(arms[arm], payloads)
    for trial in range(rounds * len(ARM_ORDERS)):
        sample = {}
        for arm in ARM_ORDERS[trial % len(ARM_ORDERS)]:
            sample[arm] = time_restore(arms[arm], payloads)
            elapsed[arm].append(sample[arm])
        aa_ratios.append(sample["fr_a"] / sample["fr_b"])
        ab_ratios.append(sample["redis"] / sample["fr_b"])
        if (trial + 1) % len(ARM_ORDERS) == 0:
            window = slice(-len(ARM_ORDERS), None)
            for arm in arms:
                round_min[arm].append(min(elapsed[arm][window]))

    aa, aa_lo, aa_hi = bootstrap_median_ci(aa_ratios)
    # `--confirm N`: repeat the ENTIRE sampling N-1 more times and keep each round's null,
    # so reproducibility is decided inside one invocation instead of relying on a human to
    # remember to run it twice. The extra rounds reuse the same warmed arms; only the
    # sampling repeats.
    aa_history = []
    for _ in range(max(0, CONFIRM_RUNS - 1)):
        extra = []
        for trial in range(rounds * len(ARM_ORDERS)):
            sample = {}
            for arm in ARM_ORDERS[trial % len(ARM_ORDERS)]:
                sample[arm] = time_restore(arms[arm], payloads)
            extra.append(sample["fr_a"] / sample["fr_b"])
        aa_history.append(statistics.median(extra))
    ab, ab_lo, ab_hi = bootstrap_median_ci(ab_ratios)
    # SAME-PROCESS A/A. The fr_a/fr_b null compares two separate fr PROCESSES, so
    # it nulls the engine and the process placement together — and on a busy
    # many-core host the placement term dominates. Measured on this host: the
    # two-process A/A scattered 0.918-1.057 across five invocations while the A/B
    # it is supposed to authenticate held 0.438-0.501, i.e. the null moved more
    # than the effect it was gating. Splitting ONE arm's own trials into halves
    # removes the placement term entirely, because both halves are the same
    # process on the same core with the same warmed allocator.
    half = len(elapsed["fr_b"]) // 2
    same_proc_aa = None
    if half >= 3:
        first, second = elapsed["fr_b"][:half], elapsed["fr_b"][half:half * 2]
        same_proc_aa = statistics.median(first) / statistics.median(second)
    print("\nCOMPETITIVE RESTORE decode (one invocation; three live arms):")
    for arm in ("fr_a", "fr_b", "redis"):
        print(f"  {arm:>5} median={statistics.median(elapsed[arm]) * 1000:.3f}ms"
              f"  best={min(elapsed[arm]) * 1000:.3f}ms")
    print(f"  A/A null (fr_a/fr_b, two processes) median={aa:.6f}x "
          f"95% CI=[{aa_lo:.6f}, {aa_hi:.6f}]")
    if same_proc_aa is not None:
        print(f"  A/A null (fr_b halves, one process) median={same_proc_aa:.6f}x")
    print(f"  A/B redis/fr_b median={ab:.6f}x 95% CI=[{ab_lo:.6f}, {ab_hi:.6f}]")
    # (frankenredis-33832) THE VERDICT NOW REQUIRES REPRODUCTION, not one in-band null.
    #
    # Measured: at --trials 36 this harness returned 1.010713x (accepted) and then
    # 0.930230x (refused) on two back-to-back invocations, same cores, same ELF, loadavg
    # 18.68 vs 19.49, with the A/B moving 0.602060x -> 0.559893x alongside. Banking the
    # first would have recorded a certified 0.60x that the very next run contradicts, and
    # that single accepted run is the ONLY time this harness has ever authenticated in
    # seven attempts. A gate that passes once in seven is not a gate.
    #
    # So a lone invocation can no longer print an acceptance. It records its null and says
    # what is still needed; `--confirm N` repeats the whole sampling N times in ONE
    # invocation and only accepts when EVERY null lands in band, via `competitive_verdict`.
    load_after, runq_after, iowait_after = host_snapshot()
    print("  host   runq %d -> %d, loadavg %.2f -> %.2f, iowait %.2f -> %.2f pct"
          % (runq_before, runq_after, load_before, load_after, iowait_before, iowait_after))
    contended = runq_after > CONTENDED_RUNQ or runq_before > CONTENDED_RUNQ
    if contended:
        # Stated ABOVE the verdict on purpose: a reader who sees only the null has to go
        # find out why it moved, and by then the window is gone.
        print("  CONTENTION: the run queue crossed %d during this invocation. Any null "
              "outside band is explained by this before it is explained by the engines."
              % CONTENDED_RUNQ)

    nulls = aa_history + [aa]
    accepted, why = competitive_verdict(nulls)
    if accepted and contended:
        # A pass under contention is the dangerous case: the gate is satisfied and the
        # number is still an artifact. Downgrade it rather than print an acceptance.
        print("  VERDICT: HOLD — nulls landed in band BUT the host was contended "
              "(runq %d -> %d). A pass measured against a moving neighbour is not a pass."
              % (runq_before, runq_after))
        return
    if accepted:
        print("  VERDICT: COMPETITIVE ROW — %s; record A/B with its CI" % why)
    elif len(nulls) < 2:
        print("  VERDICT: HOLD — single invocation, null=%.6fx. %s" % (aa, why))
        print("           re-run with --confirm 3 (or repeat by hand): one in-band null is "
              "the lucky PASS this gate now rejects.")
    else:
        print("  VERDICT: HOLD — %s; A/B is not authenticated" % why)


def self_test():
    # (frankenredis-33832) competitive_verdict: a PASS must REPRODUCE.
    # The two cases below are real, taken from back-to-back invocations at --trials 36,
    # same settings and same loadavg: 1.010713x accepted, then 0.930230x refused, with the
    # A/B moving 0.602060x -> 0.559893x alongside. Banking the first would have recorded a
    # certified 0.60x that the next run contradicts.
    ok, why = competitive_verdict([1.010713, 0.930230])
    assert not ok, "a run pair with one out-of-band null must NOT pass: %s" % why
    ok, why = competitive_verdict([1.010713])
    assert not ok, "a SINGLE in-band null is the lucky PASS this guard rejects: %s" % why
    ok, why = competitive_verdict([1.005, 0.995, 1.011])
    assert ok, "three in-band nulls must pass: %s" % why
    ok, _ = competitive_verdict([])
    assert not ok, "no nulls must not pass"
    ok, _ = competitive_verdict([0.98, 1.02])
    assert ok, "the band is inclusive at both edges"
    ok, _ = competitive_verdict([0.9799, 1.0])
    assert not ok, "just outside the low edge must fail"
    print("  competitive_verdict: reproducibility guard OK")

    # (frankenredis-i41sx) host_snapshot is what the contention guard decides on, so it is
    # covered here rather than trusted. It reads /proc, so the assertions are about SHAPE and
    # range -- the values themselves are whatever the host is doing right now.
    load1, running, iowait = host_snapshot()
    assert load1 >= 0.0, "loadavg cannot be negative"
    assert running >= 1, "at least this process is runnable, so running >= 1"
    assert 0.0 <= iowait <= 100.0, "iowait is a percentage: %r" % iowait
    # The guard compares `running` against a threshold, so an int is load-bearing: a float
    # would still compare, and would silently make `--contended-runq` a fuzzy boundary.
    assert isinstance(running, int), "running must be an int for the threshold comparison"
    print("  host_snapshot: runq %d, loadavg %.2f, iowait %.2f pct — shape OK"
          % (running, load1, iowait))

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
    # (frankenredis-33832) PRELOAD ORDER IS A VARIABLE, so make it one.
    #
    # The competitive A/A null refused four times -- 0.936, 0.686, 1.076, 1.061 -- and
    # placement was eliminated as the cause: the last two ran on quiet, same-CCD,
    # cpu0-free symmetric blocks and still came out ~6 pct apart between two processes
    # running the SAME ELF. In both quiet attempts the slower arm was `fr_a`, which is the
    # `--fr-aa-port` connection and is preloaded SECOND here. A first-loaded and a
    # second-loaded process differ in allocator arena state, page-cache warmth and heap
    # layout, and no core mask touches any of that.
    #
    # `--swap-preload` reverses the two fr preloads so the asymmetry can be ATTRIBUTED
    # rather than argued: run it both ways and see what the slow arm follows.
    #   slow arm follows the LOAD ORDER  -> warmup; both fr arms need a discarded pass
    #                                       before the timed trials
    #   slow arm follows the CORE SET    -> spatial after all, and the blocks need
    #                                       identical sibling occupancy, not just size
    # Either answer is progress; a fifth core permutation is not, since the four rows
    # above move the null by less than its own spread.
    swap_preload = "--swap-preload" in sys.argv
    drift_trials = int(opt("--drift-curve", "0"))
    print("preloading identical collection-heavy DB into both...%s"
          % ("  [--swap-preload: fr_a first]" if swap_preload else ""))
    if fr_aa is not None and swap_preload:
        preload(fr_aa)
        preload(fr)
    else:
        preload(fr)
        if fr_aa is not None:
            preload(fr_aa)
    preload(rs)
    if drift_trials:
        print("\nDRIFT CURVE: one fr arm, %d trials, index vs ms "
              "(33832: characterise the within-process term before comparing arms)"
              % drift_trials)
        run_drift_curve(fr, rs, drift_trials)
        return 0
    fk = fr.cmd("DBSIZE"); rk = rs.cmd("DBSIZE")
    print(f"DBSIZE fr={fk.decode()} redis={rk.decode()}")
    if fr_aa is not None:
        aa_k = fr_aa.cmd("DBSIZE")
        if not (fk == aa_k == rk):
            raise RuntimeError(f"DBSIZE mismatch fr_a={aa_k!r} fr_b={fk!r} redis={rk!r}")
        fr_a_sha, fr_b_sha, redis_sha = running_image_sha(fr_aa), running_image_sha(fr), running_image_sha(rs)
        # (cross-project check) VERIFY THE INCUMBENT CHAIN, because this harness attaches to
        # PORTS rather than launching anything: running process -> vendored binary ->
        # vendored source. franken_networkx measured through an artifact 2,751 lines behind
        # its repo and it inverted a ratio by 5.4x; here the same hazard is a redis arm that
        # is not the vendored build, or a vendored build that is not its source.
        vendored_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        vendored_bin = os.path.join(vendored_root, "legacy_redis_code/redis/src/redis-server")
        prov_ok, prov_msg = check_incumbent_provenance(
            vendored_bin, os.path.join(vendored_root, "legacy_redis_code/redis"))
        print("  %s" % prov_msg)
        if not prov_ok:
            raise SystemExit("REFUSED: %s\nEvery ratio here divides by the incumbent; a "
                             "stale or unidentifiable denominator is worse than no "
                             "measurement." % prov_msg)
        vendored_sha = hashlib.sha256(open(vendored_bin, "rb").read()).hexdigest()
        if redis_sha != vendored_sha:
            # A WARNING, not a refusal: someone may deliberately be measuring a different
            # redis build. But it must be said out loud, because the row would otherwise
            # read as if it were against the vendored incumbent.
            print("  WARNING: the redis arm on this port is NOT the vendored binary "
                  "(running %s... vs vendored %s...); this row is not comparable to rows "
                  "taken against the vendored incumbent"
                  % (redis_sha[:12], vendored_sha[:12]))
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
