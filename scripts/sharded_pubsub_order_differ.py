#!/usr/bin/env python3
"""Differential gate: sharded pub/sub, order-insensitive (frankenredis-5flkx).

Sharded pub/sub (SSUBSCRIBE / SPUBLISH / SUNSUBSCRIBE / PUBSUB SHARDCHANNELS /
PUBSUB SHARDNUMSUB) works on a standalone server. The existing sharded_pubsub_differ
false-fails because it compares the PUBSUB SHARDCHANNELS listing ORDER, which is
unspecified (dict order, like PUBSUB CHANNELS / FUNCTION LIST) and differs between
impls. This gate runs the same sequence on both servers and compares: the SSUBSCRIBE
/SUNSUBSCRIBE confirmations, the SPUBLISH receiver counts, the delivered smessage,
and SHARDNUMSUB byte-exact; SHARDCHANNELS results are compared as a SORTED set so
the unspecified order doesn't cause a false divergence. fr is byte-exact here apart
from that ordering.

Usage: sharded_pubsub_order_differ.py <oracle_port> <fr_port>
       Exit 0 = equivalent, 1 = real divergence.
"""
import re
import socket
import sys
import time


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


def rd(s, settle=0.15):
    time.sleep(settle)
    try:
        return s.recv(1 << 20)
    except Exception:
        return b""


def sorted_bulks(b):
    return tuple(sorted(re.findall(rb"\$\d+\r\n([^\r]*)\r\n", b)))


def run(p):
    sub, pub = conn(p), conn(p)
    send(sub, "FLUSHALL"); rd(sub)
    r = {}
    send(sub, "SSUBSCRIBE", "sc1", "sc2", "sc3"); r["ssub"] = rd(sub)
    send(pub, "SPUBLISH", "sc1", "hello"); r["spub1"] = rd(pub); r["msg1"] = rd(sub)
    send(pub, "SPUBLISH", "sc2", "world"); r["spub2"] = rd(pub); r["msg2"] = rd(sub)
    send(pub, "SPUBLISH", "nope", "x"); r["spub_none"] = rd(pub)
    send(pub, "PUBSUB", "SHARDCHANNELS"); r["shardchannels"] = rd(pub)
    send(pub, "PUBSUB", "SHARDNUMSUB", "sc1", "sc2", "nope"); r["shardnumsub"] = rd(pub)
    send(pub, "PUBSUB", "SHARDCHANNELS", "sc*"); r["shardchannels_pat"] = rd(pub)
    send(pub, "PUBSUB", "SHARDCHANNELS", "zzz*"); r["shardchannels_nomatch"] = rd(pub)
    send(sub, "SUNSUBSCRIBE", "sc1"); r["sunsub"] = rd(sub)
    send(pub, "PUBSUB", "SHARDCHANNELS"); r["shardchannels2"] = rd(pub)
    send(pub, "PUBSUB", "NUMPAT"); r["numpat"] = rd(pub)  # shard subs don't count as patterns
    sub.close(); pub.close()
    return r


SORTED_KEYS = {"shardchannels", "shardchannels_pat", "shardchannels2", "shardchannels_nomatch"}


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    rr, fr = run(op), run(fp)
    fails = []
    for k in rr:
        a, b = rr[k], fr[k]
        if k in SORTED_KEYS:
            if sorted_bulks(a) != sorted_bulks(b):
                fails.append(f"{k} (sorted): redis={a!r} fr={b!r}")
        elif a != b:
            fails.append(f"{k}: redis={a!r} fr={b!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} sharded pub/sub divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — sharded pub/sub equivalent to redis 7.2.4 "
        "(SSUBSCRIBE/SPUBLISH/SUNSUBSCRIBE/SHARDNUMSUB/smessage exact, SHARDCHANNELS order-insensitive)"
    )


if __name__ == "__main__":
    main()
