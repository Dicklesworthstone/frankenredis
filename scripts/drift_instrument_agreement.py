#!/usr/bin/env python3
"""Do the ONE-PROCESS and THREE-ARM instruments measure the same drift term?

(frankenredis-33832) THE OPEN QUESTION, and it now blocks the certification more than
load or placement do. Two instruments measure fr's within-process timing drift:

    scripts/fr_self_drift_probe.py          ONE pinned fr process, one core block
    collection_reload_headtohead.py         redis + TWO fr arms, three aligned blocks
      (its `A/A null (fr_b halves, one process)` line)

At warmup 8 they agreed to 0.004 -- spread 0.0915 against 0.0958, measured minutes
apart. At warmup 24 they do not: the standalone probe put the median within 0.9 pct of
unity on 10 runs across two windows, while the three-arm gate read 0.942 / 1.181 /
0.907 in the same window. One of them is not measuring what it says.

That was tempting to fix by raising the gate's trial count until the two agreed, which
is indistinguishable from tuning a gate until it passes. This runs them BACK TO BACK at
IDENTICAL warmup and trials instead, so the comparison is a measurement rather than an
adjustment.

    drift_instrument_agreement.py <fr_binary> [trials] [warmup] [repeats]

REFUSES TO REPORT A ONE-SIDED RESULT. If fewer than three aligned blocks are free the
three-arm arm cannot run, and this exits 3 saying the comparison did NOT happen, rather
than printing the standalone number as though it were an agreement.
"""
from __future__ import annotations

import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPTS = os.path.join(ROOT, "scripts")


def free_blocks():
    """Aligned 4-core blocks under 50 pct combined core+sibling load."""
    out = subprocess.run(["ps", "-eo", "psr,pcpu", "--no-headers"],
                         capture_output=True, text=True, check=False).stdout
    per_core = [0.0] * 64
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 2:
            try:
                per_core[int(parts[0])] += float(parts[1])
            except ValueError:
                pass
    sibling = [0.0] * 32
    for c in range(64):
        sibling[c % 32] += per_core[c]
    return [b for b in range(8)
            if sum(sibling[b * 4:b * 4 + 4]) < 50]


def loadavg():
    return open("/proc/loadavg").read().split()[0]


def standalone(fr, trials, warmup, repeats):
    """Median of the one-process probe's halves ratio."""
    out = subprocess.run(
        [sys.executable, os.path.join(SCRIPTS, "fr_self_drift_probe.py"),
         fr, str(trials), str(warmup), str(repeats), "64"],
        capture_output=True, text=True, check=False).stdout
    m = re.search(r"halves median ([0-9.]+)x", out)
    return (float(m.group(1)) if m else None), out


def three_arm(fr, trials, warmup, blocks):
    """The three-arm harness's own one-process null, at the same parameters."""
    import socket
    import tempfile
    import time

    redis = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")
    rs, fa, fb = 47971, 47972, 47973
    work = tempfile.mkdtemp(dir="/data/tmp", prefix="agree.")
    procs = []
    cores = lambda b: f"{b * 4}-{b * 4 + 3}"
    try:
        for binary, port, block in ((redis, rs, blocks[0]),
                                    (fr, fa, blocks[1]),
                                    (fr, fb, blocks[2])):
            d = os.path.join(work, str(port))
            os.makedirs(d, exist_ok=True)
            procs.append(subprocess.Popen(
                ["taskset", "-c", cores(block), binary, "--port", str(port),
                 "--save", "", "--appendonly", "no", "--dir", d,
                 "--enable-debug-command", "yes"],
                cwd=d, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        for port in (rs, fa, fb):
            for _ in range(240):
                try:
                    s = socket.create_connection(("127.0.0.1", port), 1)
                    s.sendall(b"*1\r\n$4\r\nPING\r\n")
                    if s.recv(64):
                        s.close()
                        break
                except OSError:
                    time.sleep(0.25)
            else:
                return None, f"port {port} never answered PING"
        out = subprocess.run(
            [sys.executable, os.path.join(SCRIPTS, "collection_reload_headtohead.py"),
             str(rs), str(fa), "--competitive", "--fr-aa-port", str(fb),
             "--trials", str(trials), "--warmup-passes", str(warmup)],
            capture_output=True, text=True, check=False, timeout=1800).stdout
        m = re.search(r"fr_b halves, one process\) median=([0-9.]+)x", out)
        return (float(m.group(1)) if m else None), out
    finally:
        for p in procs:
            p.terminate()
            try:
                p.wait(timeout=60)
            except subprocess.TimeoutExpired:
                p.kill()


# 0.04 is the band the certification judges this term against, so a disagreement wider
# than the band means the two instruments cannot both be gating the same quantity.
AGREEMENT_BAND = 0.04


def verdict(one, three):
    """(exit_code, message) for a pair of medians. Split out so it is testable
    without three free core blocks -- the condition that makes this whole comparison
    hard to run in the first place."""
    # The epsilon is not decoration: abs(1.0 - 1.04) is 0.040000000000000036 in
    # binary floating point, so a pair sitting EXACTLY on the band read as
    # disagreeing. Caught by the boundary case below, not by reading the line.
    if abs(one - three) <= AGREEMENT_BAND + 1e-9:
        return 0, f"AGREE within the {AGREEMENT_BAND} band the certification uses."
    return 1, (
        "DISAGREE by more than the band. The difference is the HARNESS, not the "
        "workload -- same binary, same warmup, same trials, same window. Audit the "
        "three-arm path before trusting any certification it gates.")


def main():
    if not 2 <= len(sys.argv) <= 5:
        print("usage: drift_instrument_agreement.py <fr_binary> [trials] [warmup] [repeats]",
              file=sys.stderr)
        return 2
    fr = os.path.abspath(sys.argv[1])
    trials = int(sys.argv[2]) if len(sys.argv) > 2 else 12
    warmup = int(sys.argv[3]) if len(sys.argv) > 3 else 24
    repeats = int(sys.argv[4]) if len(sys.argv) > 4 else 3

    subprocess.run([sys.executable, os.path.join(SCRIPTS, "assert_fresh_build.py"), fr],
                   check=False)

    blocks = free_blocks()
    print(f"trials={trials} warmup={warmup} repeats={repeats}  "
          f"loadavg {loadavg()}  free blocks {blocks}")

    if len(blocks) < 3:
        print(f"COMPARISON NOT RUN: {len(blocks)} free blocks, three needed for the "
              f"three-arm instrument.", file=sys.stderr)
        print("  Reporting nothing rather than a one-sided number -- a standalone "
              "reading alone is not an agreement test.", file=sys.stderr)
        return 3

    one, _ = standalone(fr, trials, warmup, repeats)
    three, three_out = three_arm(fr, trials, warmup, blocks)
    if one is None or three is None:
        print(f"COMPARISON NOT RUN: an arm produced no median "
              f"(one-process={one}, three-arm={three}).", file=sys.stderr)
        if three is None:
            print(three_out[-400:], file=sys.stderr)
        return 3

    print(f"  one-process probe (1 block) : {one:.6f}x")
    print(f"  three-arm harness (3 blocks): {three:.6f}x")
    print(f"  |difference|                : {abs(one - three):.6f}")
    rc, message = verdict(one, three)
    print(message, file=sys.stderr if rc else sys.stdout)
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
