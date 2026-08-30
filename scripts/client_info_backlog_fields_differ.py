#!/usr/bin/env python3
"""Live vendored-Redis differential for backed-up CLIENT LIST fields.

The probe keeps a client from reading a multi-megabyte LRANGE response, then reads that client's
CLIENT LIST row from a second connection. Both engines must expose a live output backlog instead
of their idle defaults: positive `oll` and `omem`, plus write interest in `events`.

`oll` and `omem` cannot be compared as equal numbers: Redis counts reply-list nodes and their
allocator footprint while FrankenRedis reports its growable write buffer in fixed protocol chunks.
The test therefore differentially proves the observable state transition in both engines, and
prints the representation-specific values for review rather than asserting a false byte equality.

This gate makes them interesting the cheapest way there is: a probe connection asks for a
reply far larger than its socket buffer and NEVER READS IT, with SO_RCVBUF shrunk so the
kernel stops draining. Upstream must then queue an output list and install a write handler.

Usage: client_info_backlog_fields_differ.py <oracle_port> <fr_port>
       Exit 0 = both engines expose the required active-backlog state, 1 = failure.
"""
import socket
import sys
import time

from _respread import cmd, conn

NAME = "edwnnprobe"
DYNAMIC_FIELDS = ("oll", "omem", "events")
STRUCTURAL_FIELDS = ("rbs", "rbp")
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
    for k in DYNAMIC_FIELDS:
        r, f = redis_row.get(k), fr_row.get(k)
        print(f"{k:<10} {str(r):<18} {str(f):<18} active-state")
    for k in STRUCTURAL_FIELDS:
        print(f"{k:<10} {str(redis_row.get(k)):<18} {str(fr_row.get(k)):<18} "
              "(reported; different reply-buffer models)")
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

    inactive = []
    for engine, row in (("redis", redis_row), ("frankenredis", fr_row)):
        for field in ("oll", "omem"):
            value = row.get(field)
            if value is None or not value.isdecimal() or int(value) == 0:
                inactive.append(f"{engine} {field}={value!r}")
        if "w" not in row.get("events", ""):
            inactive.append(f"{engine} events={row.get('events')!r}")
    if inactive:
        print("FAIL — backlog state was not exposed: " + ", ".join(inactive))
        sys.exit(1)
    print("PASS — both engines expose a real output backlog; numeric accounting is reported "
          "without pretending their different buffer representations are byte-equal.")


if __name__ == "__main__":
    main()
