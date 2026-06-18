#!/usr/bin/env python3
"""Differential gate for Redis 7.0 SHARDED pub/sub: fr vs vendored redis 7.2.4.

Sharded pub/sub (SSUBSCRIBE / SUNSUBSCRIBE / SPUBLISH and the PUBSUB
SHARDCHANNELS / SHARDNUMSUB introspection) is a distinct channel namespace from
regular pub/sub, with its own subscribe-confirmation frames, `smessage` delivery
frames, and per-channel subscriber counts. It was previously ungated here. This
drives a subscribe -> introspect -> publish -> deliver -> unsubscribe flow on
both servers and compares every reply and the delivered message bytes, including
the invariant that regular `PUBSUB CHANNELS` must NOT report shard channels.

Usage: sharded_pubsub_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence, 2 = setup error.
"""
import socket
import sys
import time


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = str(x).encode() if not isinstance(x, bytes) else x
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.05)
    return s.recv(1 << 20)


def drain(s, settle=0.15):
    s.setblocking(False)
    time.sleep(settle)
    buf = b""
    try:
        while True:
            chunk = s.recv(1 << 20)
            if not chunk:
                break
            buf += chunk
    except (BlockingIOError, OSError):
        pass
    s.setblocking(True)
    return buf


def run(port):
    sub, pub = conn(port), conn(port)
    out = {}
    # subscribe confirmation frames (one per channel, in order)
    out["ssubscribe"] = cmd(sub, "SSUBSCRIBE", "sc1", "sc2")
    drain(sub)
    # introspection reflects the active shard subscriptions
    out["shardchannels"] = cmd(pub, "PUBSUB", "SHARDCHANNELS")
    out["shardnumsub"] = cmd(pub, "PUBSUB", "SHARDNUMSUB", "sc1", "sc2", "sc3")
    # publish -> receiver count + delivered smessage frame
    out["spublish_hit"] = cmd(pub, "SPUBLISH", "sc1", "hello")
    out["smessage"] = drain(sub)
    out["spublish_miss"] = cmd(pub, "SPUBLISH", "scX", "x")
    # unsubscribe one channel; introspection updates
    out["sunsubscribe"] = cmd(sub, "SUNSUBSCRIBE", "sc1")
    drain(sub)
    out["shardchannels_after"] = cmd(pub, "PUBSUB", "SHARDCHANNELS")
    # shard channels must NOT leak into the regular pub/sub namespace
    out["regular_channels"] = cmd(pub, "PUBSUB", "CHANNELS")
    out["regular_numsub"] = cmd(pub, "PUBSUB", "NUMSUB", "sc1")
    sub.close()
    pub.close()
    return out


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    try:
        oracle = run(op)
        fr = run(fp)
    except OSError as e:
        print(f"SETUP ERROR: {e}")
        sys.exit(2)
    diffs = 0
    for k in oracle:
        if oracle[k] != fr[k]:
            diffs += 1
            print(f"DIFF {k}\n  redis={oracle[k]!r}\n  fr   ={fr[k]!r}")
    if diffs:
        print(f"\nFAIL — {diffs} sharded pub/sub divergence(s) vs redis 7.2.4")
        sys.exit(1)
    print(f"PASS — sharded pub/sub byte-exact vs redis 7.2.4 ({len(oracle)} steps)")


if __name__ == "__main__":
    main()
