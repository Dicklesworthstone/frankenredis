#!/usr/bin/env python3
"""Instructions per AOF-replayed command, fr vs LIVE redis, same invocation.

WHY A SEPARATE HARNESS. `restore_instr_per_op.py --op=loadaof` drives `DEBUG
LOADAOF`, which fr implements as a no-op returning OK, so it has never measured an
AOF load. AOF load is only observable across a PROCESS RESTART, which is a different
measurement shape: seed an appendonlydir with one process, then measure a SECOND
process whose whole job is to start up and replay it.

    phase 1  boot, send N writes, shut down cleanly      (NOT measured)
    phase 2  copy the appendonlydir aside, pristine      (the loader mutates it)
    phase 3  boot a fresh process on a fresh copy UNDER CALLGRIND, let it replay,
             verify it actually loaded, shut down, read the dump   (measured)

Two-point at N and 2N so process startup, listener setup and teardown cancel and
what is left is one replayed command.

FAILS CLOSED ON A LOAD THAT DID NOT HAPPEN. `DBSIZE` after the measured boot must
equal the seeded key count. This is not decoration: until edabd8760 `frankenredis
--appendonly yes` came up EMPTY and truncated the AOF, and an unchecked harness would
have reported that as an engine which replays an AOF very fast indeed. A ratio is
only meaningful when both engines did the same work, and here "did any work" has to
be proven, not assumed.

USE writes >= 2000. Measured across sizes, fr is FLAT and redis is not:

    writes    fr        redis     ratio
       300    7834.6    4375.0    1.7908x   <- unstable
       500    7575.0    7006.6    1.0811x   <- unstable
      2000    7770.6    6004.0    1.2942x
      8000    7649.0    5965.2    1.2823x

fr sits at 7,575-7,835 instr/command everywhere, so the swing is entirely in redis's
small-N points: at 300-500 writes the two-point subtraction divides a startup-sized
difference by a small denominator. The converged answer is ~1.28x. Quoting the 1.08x
or the 1.79x would be reading noise as a result.

    aof_load_instr_per_op.py <fr-bin> <writes> [--aa] [--redis=<path>]
"""
import hashlib
import os
import random
import shutil
import socket
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")


def resp(*args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        if isinstance(a, str):
            a = a.encode()
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


def read_reply(sock, buf):
    while True:
        if b"\r\n" in buf:
            line, rest = buf.split(b"\r\n", 1)
            if line[:1] in (b"+", b"-", b":", b"*"):
                return line, rest
            if line[:1] == b"$":
                n = int(line[1:])
                if n == -1:
                    return b"(nil)", rest
                if len(rest) >= n + 2:
                    return rest[:n], rest[n + 2:]
        chunk = sock.recv(1 << 20)
        if not chunk:
            raise RuntimeError("server closed the connection")
        buf += chunk


def elf_identity_from_maps(pid, binary):
    """sha256 of the ELF THIS PROCESS IS RUNNING, proven via /proc/<pid>/maps.

    Same guard as `restore_instr_per_op.py`: `target/release/<bin>` is shared in this
    checkout, so hash the path only while it still resolves to the inode the process
    actually mapped, and refuse otherwise rather than print a hash of bytes nothing
    executed.
    """
    binary = os.path.realpath(binary)
    mapped = None
    with open("/proc/%d/maps" % pid, "r") as fh:
        for line in fh:
            parts = line.split(None, 5)
            if len(parts) < 6:
                continue
            path = parts[5].strip()
            if "x" in parts[1] and os.path.realpath(path) == binary:
                mapped = parts[4]
                break
    if mapped is None:
        return None
    if mapped != str(os.stat(binary).st_ino):
        raise RuntimeError("%s was REPLACED while this arm ran" % binary)
    digest = hashlib.sha256()
    with open(binary, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def total_ir(path):
    """Whole-process Ir, counted ONCE (`totals:` when present, else `summary:`)."""
    total = 0
    directory = os.path.dirname(path)
    for name in os.listdir(directory):
        if not name.startswith(os.path.basename(path)):
            continue
        summary = totals = None
        with open(os.path.join(directory, name), "rb") as fh:
            for raw in fh:
                if raw.startswith(b"summary:"):
                    summary = int(raw.split(b":", 1)[1].split()[0])
                elif raw.startswith(b"totals:"):
                    totals = int(raw.split(b":", 1)[1].split()[0])
        total += totals if totals is not None else (summary or 0)
    return total


def pick_port():
    """A port PROVEN free, not merely one from a range we hope is ours.

    (BlackThrush 2026-08-26) This harness first used fixed ports 47701.. and read
    `DBSIZE=:4002` from a seed directory it had just created empty. The extra keys
    belonged to a PEER AGENT's server -- `fr.bcand` was listening on 47702. On this
    shared host a hardcoded port range is someone else's port range, and the failure
    mode is not a bind error, it is SILENTLY MEASURING ANOTHER AGENT'S DATABASE.

    Probe a random high port and accept only one that REFUSES a connection.
    """
    for _ in range(200):
        candidate = random.randint(20000, 60000)
        try:
            probe = socket.create_connection(("127.0.0.1", candidate), timeout=0.25)
        except OSError:
            return candidate
        else:
            probe.close()
    raise RuntimeError("no free port found")


def wait_ready(proc, port, tries):
    for _ in range(tries):
        if proc.poll() is not None:
            raise RuntimeError("exited during startup rc=%s" % proc.returncode)
        try:
            sock = socket.create_connection(("127.0.0.1", port), timeout=3)
            sock.sendall(resp("PING"))
            reply, _ = read_reply(sock, b"")
            if reply.startswith(b"+PONG"):
                return sock
            sock.close()
        except OSError:
            pass
        time.sleep(0.5)
    proc.kill()
    raise RuntimeError("never became ready on port %d" % port)


def shutdown(proc, sock):
    try:
        sock.sendall(resp("SHUTDOWN", "NOSAVE"))
        sock.close()
    except OSError:
        pass
    try:
        proc.wait(timeout=300)
    except Exception:
        proc.kill()
        proc.wait(timeout=60)


def seed(binary, workdir, writes):
    """Phase 1+2: build an appendonlydir holding `writes` commands, keep it pristine."""
    os.makedirs(workdir, exist_ok=True)
    port = pick_port()
    proc = subprocess.Popen(
        [binary, "--appendonly", "yes", "--save", "", "--dir", workdir,
         "--enable-debug-command", "yes", "--port", str(port)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=workdir)
    try:
        sock = wait_ready(proc, port, 120)
    except Exception:
        proc.kill()
        raise
    buf = b""
    # The seed directory must start EMPTY. If it does not, the AOF being measured
    # carries commands this run did not write and the two-point subtraction is
    # against two different workloads.
    sock.sendall(resp("DBSIZE"))
    start, buf = read_reply(sock, buf)
    if start != b":0":
        shutdown(proc, sock)
        raise RuntimeError("seed dir %s was NOT empty at boot: DBSIZE=%s -- this is "
                           "another server, not ours" % (workdir, start.decode()))
    try:
        for i in range(writes):
            sock.sendall(resp("SET", "k%08d" % i, "v%08d" % i))
        for _ in range(writes):
            _, buf = read_reply(sock, buf)
        sock.sendall(resp("DBSIZE"))
        dbsize, buf = read_reply(sock, buf)
        if dbsize != b":%d" % writes:
            raise RuntimeError("seed did not land: DBSIZE=%s want %d" % (dbsize, writes))
    finally:
        # Never leave a server behind on a raise: the next run would connect to it.
        shutdown(proc, sock)
    pristine = workdir + ".pristine"
    shutil.copytree(os.path.join(workdir, "appendonlydir"),
                    os.path.join(pristine, "appendonlydir"))
    return pristine


def measure(binary, tag, pristine, writes, root):
    """Phase 3: boot UNDER CALLGRIND on a fresh copy, prove the load, read the dump."""
    workdir = os.path.join(root, tag)
    os.makedirs(workdir, exist_ok=True)
    shutil.copytree(os.path.join(pristine, "appendonlydir"),
                    os.path.join(workdir, "appendonlydir"))
    out = os.path.join(workdir, "cg.%s.out" % tag)
    port = pick_port()
    proc = subprocess.Popen(
        ["valgrind", "--tool=callgrind", "--callgrind-out-file=" + out,
         "--collect-systime=no",
         binary, "--appendonly", "yes", "--save", "", "--dir", workdir,
         "--enable-debug-command", "yes", "--port", str(port)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=workdir)
    try:
        sock = wait_ready(proc, port, 1200)
    except Exception:
        proc.kill()
        raise
    sha = elf_identity_from_maps(proc.pid, binary)
    buf = b""
    sock.sendall(resp("DBSIZE"))
    dbsize, buf = read_reply(sock, buf)
    # FAIL CLOSED. An engine that replayed nothing is not a fast engine.
    if dbsize != b":%d" % writes:
        shutdown(proc, sock)
        raise RuntimeError(
            "%s did NOT replay its AOF: DBSIZE=%s, want %d. Refusing to report a "
            "ratio for an arm that did no work." % (tag, dbsize.decode(), writes))
    shutdown(proc, sock)
    ir = total_ir(out)
    if ir == 0:
        raise RuntimeError("%s produced no callgrind total" % tag)
    return ir, sha


def arm(binary, name, writes, root):
    seed_n = seed(binary, os.path.join(root, name + ".seed.n"), writes)
    seed_2n = seed(binary, os.path.join(root, name + ".seed.2n"), writes * 2)
    ir_n, sha = measure(binary, name + ".n", seed_n, writes, root)
    ir_2n, sha2 = measure(binary, name + ".2n", seed_2n, writes * 2, root)
    if sha and sha2 and sha != sha2:
        raise RuntimeError("%s: the two points ran DIFFERENT ELFs" % name)
    per_op = (ir_2n - ir_n) / float(writes)
    print("  %-7s Ir(N)=%-16d Ir(2N)=%-16d -> %12.1f instr/replayed-command"
          % (name, ir_n, ir_2n, per_op))
    return per_op, sha


def main():
    argv = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(argv) < 2:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    binary = os.path.abspath(argv[0])
    writes = int(argv[1])
    aa = "--aa" in sys.argv
    redis = REDIS
    for a in sys.argv[1:]:
        if a.startswith("--redis="):
            redis = os.path.abspath(a.split("=", 1)[1])

    keep = None
    for a in sys.argv[1:]:
        if a.startswith("--keep="):
            keep = os.path.abspath(a.split("=", 1)[1])
    if keep:
        # Callgrind dumps live under <root>/<arm>.<point>/; keeping them is what
        # lets frame_delta.py attribute the per-command cost by FUNCTION.
        os.makedirs(keep, exist_ok=True)
        return _run(binary, writes, aa, redis, keep)
    if "--null-only" in sys.argv:
        # Characterise THIS harness's own null distribution: two independent arms of
        # the SAME ELF, no incumbent. Used to DERIVE the band rather than inherit
        # `restore_instr_per_op.py`'s 0.005, which was measured on a steady-state
        # loop and is wrong for a harness whose unit of work is a PROCESS STARTUP.
        with tempfile.TemporaryDirectory(dir="/data/tmp") as root:
            a, _ = arm(binary, "nullA", writes, root)
            b, _ = arm(binary, "nullB", writes, root)
            print("  null %.6fx   (A=%.1f B=%.1f)" % (b / a, a, b))
        return 0
    with tempfile.TemporaryDirectory(dir="/data/tmp") as root:
        return _run(binary, writes, aa, redis, root)


def _run(binary, writes, aa, redis, root):
    if True:
        # (BlackThrush 2026-08-26) ORDER IS LOAD-BEARING: the A/A arm runs ADJACENT
        # to the arm it is nulling, before the incumbent, not after it.
        #
        # This used to run fr -> redis -> fr_aa, separating the null from its subject
        # by TWO callgrind startups of another engine. Measured, that roughly halved
        # the null's pass rate: back-to-back the null lands inside the 0.005 band in
        # 10 of 12 draws, but separated by the incumbent only ~50 pct of arms passed
        # and across 8 paired A/B draws not ONE had both arms in band. The band was
        # never the problem; the interleaving was.
        fr_per, fr_sha = arm(binary, "fr", writes, root)
        if aa:
            aa_per, aa_sha = arm(binary, "fr_aa", writes, root)
        rd_per, rd_sha = arm(redis, "redis", writes, root)
        print("  fr     in-process ELF sha256 %s" % fr_sha)
        print("  redis  in-process ELF sha256 %s" % rd_sha)
        if aa:
            null = aa_per / fr_per
            verdict = "PASS" if abs(null - 1.0) <= 0.005 else "FAIL"
            print("  A/A null (fr_aa/fr, same ELF, independent processes): %.6fx  [%s, band 0.005]"
                  % (null, verdict))
            if verdict == "FAIL":
                print("  A/A OUTSIDE BAND -- do not quote the fr/redis ratio from this run.")
        print("  fr/redis instructions per replayed command: %.4fx" % (fr_per / rd_per))
    return 0


sys.exit(main())
