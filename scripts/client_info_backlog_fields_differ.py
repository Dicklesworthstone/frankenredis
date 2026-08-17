#!/usr/bin/env python3
"""Differential gate: the CLIENT LIST fields that describe a BACKED-UP client
(frankenredis-edwnn).

fr renders `rbs`, `rbp`, `oll`, `omem` and `events` as CONSTANTS, and each constant equals
upstream's DEFAULT — 16384 / 16384 / 0 / 0 / "r". So on an idle connection running a small
command both engines agree on all five, which is why no live differential ever caught them.
They only separate once the field becomes interesting.

This gate makes them interesting the cheapest way there is: a probe connection asks for a
reply far larger than its socket buffer and NEVER READS IT, with SO_RCVBUF shrunk so the
kernel stops draining. Upstream must then queue an output list and install a write handler.

MEASURED 2026-08-17 against vendored redis-server 7.2.4 (git sha1 d2c8a4b9) and fr ELF
de1fe57e68ce633a, ~4MB reply:

    field      redis 7.2.4        fr
    rbs        1024               16384      <- upstream's resize cron SHRANK it
    rbp        0                  16384      <- upstream's peak decayed
    oll        68                 0          <- 68 queued output blocks
    omem       1394272            0          <- 1.4 MB of queued output
    events     rw                 r          <- upstream installed a write handler

THE CONSEQUENCE, which is the reason this is a gate and not a note: a client with 1.4 MB
backed up reports `oll=0 omem=0 events=r` in fr. It looks IDLE AND HEALTHY at exactly the
moment upstream shows it backing up, so a dashboard or a CLIENT KILL policy ported from
Redis cannot see the condition it exists to catch.

`tot-mem` is reported but NOT asserted. fr computes it (it is not a constant) and the call
site declares it a lower-bound estimate (frankenredis-tepuj), yet it read 4180919 against
upstream's 1416672 here — 2.95x HIGHER, not lower. Two explanations fit: the estimate is
not the lower bound it claims, or fr really does hold ~3x the memory because it materialises
the whole reply into one buffer where upstream queues 68 blocks. THIS PROBE CANNOT
DISTINGUISH THEM, so it prints the pair and asserts nothing. Whoever separates those two
should file the result; do not read the number as a verdict.

Usage: client_info_backlog_fields_differ.py <oracle_port> <fr_port>
       Exit 0 = all five fields agree, 1 = divergence.
       Currently EXITS 1 BY DESIGN — a red gate tracking an open divergence. It goes green
       when the five fields are computed rather than pinned.
"""
import socket
import sys
import time

from _respread import cmd, conn

NAME = "edwnnprobe"
ASSERTED = ("rbs", "rbp", "oll", "omem", "events")
REPORTED_ONLY = ("tot-mem",)
KEY = "edwnn:backlog:probe"


def row_of(reader, name):
    """The CLIENT LIST row for `name`, or None so a missing row is a HARNESS failure."""
    raw = cmd(reader, "CLIENT", "LIST").decode("utf8", "replace")
    for line in raw.splitlines():
        fields = dict(p.split("=", 1) for p in line.split(" ") if "=" in p)
        if fields.get("name") == name:
            return fields
    return None


def observe(port):
    seeder = conn(port)
    cmd(seeder, "DEL", KEY)
    payload = "x" * 512
    for _ in range(40):
        cmd(seeder, "RPUSH", KEY, *([payload] * 200))  # ~4 MB total reply
    seeder.close()

    probe = conn(port)
    cmd(probe, "CLIENT", "SETNAME", NAME)
    # Shrink the receive buffer BEFORE asking, so the kernel stops draining early and the
    # server is forced to queue rather than write the whole reply inline.
    probe.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
    key = KEY.encode()
    probe.sendall(
        b"*4\r\n$6\r\nLRANGE\r\n$%d\r\n%s\r\n$1\r\n0\r\n$2\r\n-1\r\n" % (len(key), key)
    )
    time.sleep(1.5)  # let the socket fill and the backlog form

    reader = conn(port)
    row = row_of(reader, NAME)
    reader.close()
    probe.close()
    cleanup = conn(port)
    cmd(cleanup, "DEL", KEY)
    cleanup.close()
    return row


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    redis_row, fr_row = observe(op), observe(fp)

    if redis_row is None or fr_row is None:
        print("HARNESS FAILURE — probe row absent from CLIENT LIST "
              f"(redis={redis_row is not None}, fr={fr_row is not None}); "
              "the gate measured nothing")
        sys.exit(1)

    print("=" * 72)
    print(f"{'field':<10} {'redis 7.2.4':<18} {'fr':<18} verdict")
    print("-" * 72)
    diverged = []
    for k in ASSERTED:
        r, f = redis_row.get(k), fr_row.get(k)
        same = r == f
        if not same:
            diverged.append((k, r, f))
        print(f"{k:<10} {str(r):<18} {str(f):<18} {'agree' if same else 'DIVERGE'}")
    for k in REPORTED_ONLY:
        print(f"{k:<10} {str(redis_row.get(k)):<18} {str(fr_row.get(k)):<18} "
              "(reported, not asserted — see module docstring)")
    print("=" * 72)

    # A backlog that did not form would make every field agree at its default and the gate
    # would PASS while proving nothing. Upstream is the reference: if IT did not queue, the
    # workload failed, not the engine under test.
    if redis_row.get("oll") in (None, "0"):
        print("HARNESS FAILURE — upstream reports oll=0, so no output backlog formed and "
              "these fields were never made interesting. Raise the reply size or lower "
              "SO_RCVBUF; do not read the comparison above.")
        sys.exit(1)

    if diverged:
        print(f"FAIL — {len(diverged)} of {len(ASSERTED)} backlog field(s) diverge "
              f"(frankenredis-edwnn; each is a constant in fr equal to upstream's default):")
        for k, r, f in diverged:
            print(f"  {k}: redis={r!r} fr={f!r}")
        sys.exit(1)
    print(f"PASS — all {len(ASSERTED)} backlog fields agree with a real backlog present "
          "(upstream oll=%s). frankenredis-edwnn is fixed." % redis_row.get("oll"))


if __name__ == "__main__":
    main()
