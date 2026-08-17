#!/usr/bin/env python3
"""Measure fr's WITHIN-PROCESS timing drift using ONE pinned process and ONE core block.

(frankenredis-33832) WHY THIS EXISTS. The certification gate refuses on the
one-process null -- `fr_b halves`, a single process's first-half median against its
second-half median -- because that is the term that actually fails. But getting that
number required running collection_reload_headtohead.py --competitive, which needs
THREE aligned 4-core blocks free simultaneously (two fr arms plus redis).

That precondition is the binding constraint, not load. Measured across two sessions:

    loadavg 10.69 -> 4 blocks     loadavg 14.31 -> 1 block
    loadavg 13.38 -> 2 blocks     loadavg 15.72 -> 3 blocks
    loadavg 13.01 -> 2 blocks     loadavg 16.69 -> 0, then 1, then 2

So the fleet at loadavg ~15 usually offers fewer than three blocks, and a warm-up
sweep -- the experiment that would settle whether the drift is a longer transient or
a second term -- kept going unrun for want of placement it does not actually need.

A single process compared against ITSELF needs ONE block. This probe measures exactly
that: no redis arm, no second fr, no cross-process nulling. It answers a NARROWER
question than the certification (it says nothing about fr vs redis) and it answers the
one the gate is stuck on.

    fr_self_drift_probe.py <fr_binary> [trials] [warmup] [repeats] [fields]

Reports per repeat: the halves ratio, and across repeats the SPREAD, which is the
quantity the gate judges. Pre-registered decision rule from the ledger: if the spread
falls monotonically with warm-up length the drift is a transient longer than ten
trials; if it plateaus near 0.1 there is a second term and no warm-up will certify
this surface.
"""
from __future__ import annotations

import os
import socket
import statistics
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def resp(*args):
    out = [b"*%d\r\n" % len(args)]
    for a in args:
        b = a if isinstance(a, bytes) else str(a).encode()
        out.append(b"$%d\r\n%s\r\n" % (len(b), b))
    return b"".join(out)


def read_reply(sock, buf):
    """Bulk-aware: a DUMP payload is binary and contains CRLF."""
    while b"\r\n" not in buf:
        buf += sock.recv(1 << 20)
    line, rest = buf.split(b"\r\n", 1)
    tag = line[:1]
    if tag in (b"+", b"-", b":"):
        return line, rest
    if tag == b"$":
        n = int(line[1:])
        if n == -1:
            return b"", rest
        while len(rest) < n + 2:
            rest += sock.recv(1 << 20)
        return rest[:n], rest[n + 2:]
    raise RuntimeError("unexpected reply tag %r" % tag)


def quietest_block():
    """The 4-core block with the least combined core+sibling load."""
    out = subprocess.run(["ps", "-eo", "psr,pcpu", "--no-headers"],
                         capture_output=True, text=True, check=False).stdout
    per_core = [0.0] * 64
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 2:
            try:
                per_core[int(parts[0])] += float(parts[1])
            except (ValueError, IndexError):
                pass
    sibling = [0.0] * 32
    for c in range(64):
        sibling[c % 32] += per_core[c]
    blocks = [(sum(sibling[b * 4:b * 4 + 4]), b) for b in range(8)]
    blocks.sort()
    return blocks[0][1], blocks[0][0]


def mean_mhz():
    total = count = 0.0
    with open("/proc/cpuinfo") as fh:
        for line in fh:
            if line.startswith("cpu MHz"):
                total += float(line.split(":")[1])
                count += 1
    return total / count if count else 0.0


def timed_batch(sock, buf, payload, ops):
    """Wall seconds for `ops` RESTOREs, replies fully drained before stopping."""
    one = resp("RESTORE", "dst", "0", payload, "REPLACE")
    start = time.perf_counter()
    sock.sendall(one * ops)
    done = 0
    while done < ops:
        reply, buf = read_reply(sock, buf)
        if reply.startswith(b"-"):
            raise RuntimeError("RESTORE error: %r" % reply)
        done += 1
    return time.perf_counter() - start, buf


def main():
    if not 2 <= len(sys.argv) <= 6:
        print(__doc__.strip().splitlines()[-6], file=sys.stderr)
        return 2
    fr = os.path.abspath(sys.argv[1])
    trials = int(sys.argv[2]) if len(sys.argv) > 2 else 24
    warmup = int(sys.argv[3]) if len(sys.argv) > 3 else 8
    repeats = int(sys.argv[4]) if len(sys.argv) > 4 else 3
    fields = int(sys.argv[5]) if len(sys.argv) > 5 else 64

    subprocess.run([sys.executable, os.path.join(ROOT, "scripts", "assert_fresh_build.py"), fr],
                   check=False)

    block, blockload = quietest_block()
    cores = f"{block * 4}-{block * 4 + 3}"
    port = 47950 + (os.getpid() % 40)
    work = os.path.join(ROOT, "target", "self_drift")
    os.makedirs(work, exist_ok=True)

    print(f"one arm pinned to cores {cores} (block load {blockload:.0f} pct), "
          f"trials={trials} warmup={warmup} repeats={repeats} fields={fields}")
    print(f"loadavg {open('/proc/loadavg').read().split()[0]}  mean MHz {mean_mhz():.0f}")

    proc = subprocess.Popen(
        ["taskset", "-c", cores, fr, "--port", str(port), "--save", "",
         "--appendonly", "no", "--dir", work],
        cwd=work, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sock = None
    ratios = []
    try:
        for _ in range(600):
            if proc.poll() is not None:
                raise RuntimeError("fr exited rc=%s" % proc.returncode)
            try:
                sock = socket.create_connection(("127.0.0.1", port), timeout=5)
                sock.settimeout(600)
                sock.sendall(resp("PING"))
                if b"PONG" in sock.recv(64):
                    break
                sock.close()
                sock = None
            except OSError:
                time.sleep(0.25)
        if sock is None:
            raise RuntimeError("fr never became ready")

        buf = b""
        pairs = []
        for i in range(fields):
            pairs += ["f%04d" % i, "v%04d" % i]
        sock.sendall(resp("HSET", "src", *pairs))
        _, buf = read_reply(sock, buf)
        sock.sendall(resp("DUMP", "src"))
        payload, buf = read_reply(sock, buf)
        if not payload:
            raise RuntimeError("empty DUMP")

        ops = 200
        for rep in range(repeats):
            for _ in range(warmup):
                _, buf = timed_batch(sock, buf, payload, ops)
            times = []
            for _ in range(trials):
                dt, buf = timed_batch(sock, buf, payload, ops)
                times.append(dt)
            half = len(times) // 2
            first = statistics.median(times[:half])
            second = statistics.median(times[half:])
            ratio = first / second
            ratios.append(ratio)
            print(f"  repeat {rep + 1}: halves {ratio:.6f}x  "
                  f"(first {first * 1e3:.2f} ms, second {second * 1e3:.2f} ms)  "
                  f"loadavg {open('/proc/loadavg').read().split()[0]}  MHz {mean_mhz():.0f}")
    finally:
        if sock is not None:
            sock.close()
        proc.terminate()
        try:
            proc.wait(timeout=120)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=60)

    spread = max(ratios) - min(ratios)
    print(f"  halves median {statistics.median(ratios):.6f}x   SPREAD {spread:.4f}   "
          f"(gate band 0.04)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
