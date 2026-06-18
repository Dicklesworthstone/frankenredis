#!/usr/bin/env python3
"""Differential gate: keyspace notifications from the borrowed fast-path writes
(frankenredis-s7eif).

The byte-prefix fast-path packets execute writes via the borrowed runtime path
(INCR/INCRBY/APPEND/SETRANGE/LPUSH/RPUSH/SADD/LPOP/RPOP/ZADD/ZINCRBY/ZPOPMIN/
ZPOPMAX/EXPIRE/GETDEL). A fast path that SKIPPED the keyspace-event firing the
generic path emits would silently break keyspace-notification clients. This gate
enables `notify-keyspace-events KEA`, subscribes to `__keyevent@0__:*`, runs the
fast-path writes, and asserts the emitted event stream is byte-identical to
vendored redis 7.2.4.

Usage: fastpath_keyspace_events_differ.py <oracle_port> <fr_port>
       Exit 0 = identical keyevent stream, 1 = divergence.
"""
import re
import socket
import sys
import time

WRITES = [
    ("INCR", "ctr"),
    ("INCRBY", "ctr", "5"),
    ("DECR", "ctr"),
    ("DECRBY", "ctr", "2"),
    ("APPEND", "s", "x"),
    ("SETRANGE", "s", "3", "yy"),
    ("LPUSH", "l", "a"),
    ("RPUSH", "l", "b", "c"),
    ("SADD", "st", "m", "n"),
    ("LPOP", "l"),
    ("RPOP", "l"),
    ("ZADD", "z", "1", "zm"),
    ("ZINCRBY", "z", "2", "zm"),
    ("ZPOPMIN", "z"),
    ("EXPIRE", "s", "1000"),
    ("GETDEL", "st"),
]


def conn(p):
    s = socket.create_connection(("127.0.0.1", p), timeout=5)
    s.settimeout(1.5)
    return s


def send(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)


def rd(s):
    try:
        return s.recv(1 << 20)
    except Exception:
        return b""


def keyevents(p):
    c = conn(p)
    send(c, "CONFIG", "SET", "notify-keyspace-events", "KEA")
    rd(c)
    send(c, "FLUSHALL")
    rd(c)
    sub = conn(p)
    send(sub, "PSUBSCRIBE", "__keyevent@0__:*")
    time.sleep(0.3)
    rd(sub)
    for args in WRITES:
        send(c, *args)
        rd(c)
        time.sleep(0.02)
    time.sleep(0.3)
    raw = rd(sub)
    # Restore config so a suite run leaves the shared server unpolluted.
    send(c, "CONFIG", "SET", "notify-keyspace-events", "")
    rd(c)
    return re.findall(rb"__keyevent@0__:([a-z_]+)\r", raw)


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    eo, ef = keyevents(op), keyevents(fp)
    print("=" * 60)
    if eo != ef:
        print("FAIL — fast-path keyspace-event stream differs vs redis 7.2.4:")
        print(f"  redis: {eo}")
        print(f"  fr   : {ef}")
        sys.exit(1)
    print(
        f"PASS — fast-path writes emit byte-identical keyspace events vs redis 7.2.4 "
        f"({len(eo)} events across {len(WRITES)} writes)"
    )


if __name__ == "__main__":
    main()
