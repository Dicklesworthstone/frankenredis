#!/usr/bin/env python3
"""Differential keyspace-notification gate: fr vs vendored redis 7.2.4.

Many commands fire specific `__keyevent@N__:<event>` notifications (and the
empty-collection auto-delete fires a trailing `del`). A missing, extra, or
mis-named event is a real, observable parity bug that the reply/digest fuzzers
never see (events are an out-of-band pub/sub side effect). This drives a battery
of mutations under `notify-keyspace-events KEA` and compares the ORDERED list of
`(event, key)` notifications each command produces, one command at a time so the
async capture can't race across commands.

Usage: keyspace_notification_differ.py <oracle_port> <fr_port>
       Exit 0 = event streams byte-exact, 1 = divergence, 2 = setup error.
"""
import socket, sys, time


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def send(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x.encode() if isinstance(x, str) else x
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)


def recv_reply(s):
    s.settimeout(2)
    time.sleep(0.02)
    try:
        return s.recv(1 << 20)
    except socket.timeout:
        return b""


def drain_events(sub, settle=0.18):
    """Collect all complete pub/sub pmessage frames currently buffered."""
    sub.setblocking(False)
    time.sleep(settle)
    buf = b""
    try:
        while True:
            chunk = sub.recv(1 << 20)
            if not chunk:
                break
            buf += chunk
    except (BlockingIOError, socket.error):
        pass
    sub.setblocking(True)
    return buf


def parse_events(blob):
    """Extract (event, key) from `__keyevent@0__:<event>` pmessage frames."""
    import re

    out = []
    # pmessage: *4 / $8 pmessage / pattern / channel '__keyevent@0__:<ev>' / payload <key>
    for m in re.finditer(
        rb"\$\d+\r\n__keyevent@0__:([^\r]+)\r\n\$\d+\r\n([^\r]*)\r\n", blob
    ):
        out.append((m.group(1).decode("latin1"), m.group(2).decode("latin1")))
    return out


def open_pair(port):
    sub = conn(port)
    send(sub, "CONFIG", "SET", "notify-keyspace-events", "KEA")
    recv_reply(sub)
    send(sub, "FLUSHALL")
    recv_reply(sub)
    send(sub, "PSUBSCRIBE", "__keyevent@0__:*")
    recv_reply(sub)
    drain_events(sub)
    cmdc = conn(port)
    send(cmdc, "FLUSHALL")
    recv_reply(cmdc)
    drain_events(sub)
    return sub, cmdc


# Each entry is a list of argv that mutates the keyspace; we compare the events
# each one emits. Sequencing matters (later commands depend on earlier state).
BATTERY = [
    ["SET", "k", "v"],
    ["EXPIRE", "k", "100"],
    ["PERSIST", "k"],
    ["APPEND", "k", "xx"],
    ["SETRANGE", "k", "1", "Y"],
    ["GETSET", "k", "z"],
    ["GETDEL", "k"],
    ["INCR", "n"],
    ["INCRBYFLOAT", "f", "1.5"],
    ["SETEX", "e", "100", "v"],
    ["LPUSH", "l", "a", "b"],
    ["RPOP", "l"],
    ["LSET", "l", "0", "q"],
    ["LREM", "l", "0", "q"],  # removes last element -> lrem + del
    ["SADD", "s", "m"],
    ["SREM", "s", "m"],  # last member -> srem + del
    ["HSET", "h", "fld", "v"],
    ["HDEL", "h", "fld"],  # last field -> hdel + del
    ["ZADD", "z", "1", "mm"],
    ["ZINCRBY", "z", "1", "mm"],
    ["ZREM", "z", "mm"],  # last member -> zrem + del
    ["XADD", "st", "1-1", "ff", "vv"],
    ["XADD", "st", "2-1", "gg", "ww"],
    ["XDEL", "st", "1-1"],
    ["SET", "a", "1"],
    ["COPY", "a", "b"],
    ["RENAME", "a", "c"],
    ["MOVE", "c", "1"],
    ["DEL", "b"],
]


def run_battery(port):
    sub, cmdc = open_pair(port)
    per_cmd = []
    for argv in BATTERY:
        send(cmdc, *argv)
        recv_reply(cmdc)
        per_cmd.append(parse_events(drain_events(sub)))
    return per_cmd


def main():
    oport = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fport = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    oracle = run_battery(oport)
    fr = run_battery(fport)
    diffs = 0
    for argv, oe, fe in zip(BATTERY, oracle, fr):
        if oe != fe:
            diffs += 1
            print(f"DIFF [{' '.join(argv)}]\n  redis={oe}\n  fr   ={fe}")
    if diffs:
        print(f"\nFAIL — {diffs} command(s) with divergent keyspace notifications")
        sys.exit(1)
    print(
        f"PASS — keyspace notifications byte-exact vs redis 7.2.4 "
        f"({len(BATTERY)} mutating commands)"
    )


if __name__ == "__main__":
    main()
