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
import sys
import time  # only for drain()'s settle wait, never for reading a command reply

from _respread import assert_ok, cmd, cmd_n
from _respread import conn as _conn


def conn(p):
    s = _conn(p)
    s.settimeout(1.5)  # bounds drain()'s non-blocking read, not the framed ones
    return s


def drain(s, settle=0.15):
    """Collect an asynchronously-DELIVERED smessage, or confirm none arrived.

    DELIBERATE EXCEPTION to the shared reader (frankenredis-gpry6): every other
    read in this gate is the reply to a command we just sent, so frame
    completeness tells the reader when to stop. A delivered pub/sub message is
    not solicited by the reading connection, so "the server sent nothing more"
    is only observable by waiting. Used ONLY for delivered messages here — the
    command replies and the per-channel SSUBSCRIBE/SUNSUBSCRIBE confirmations go
    through cmd / cmd_n.
    """
    time.sleep(settle)
    try:
        return s.recv(1 << 20)
    except Exception:
        return b""


def sorted_bulks(b):
    return tuple(sorted(re.findall(rb"\$\d+\r\n([^\r]*)\r\n", b)))


def run(p):
    sub, pub = conn(p), conn(p)
    assert_ok(cmd(sub, "FLUSHALL"), "FLUSHALL")
    r = {}
    # one confirmation frame PER CHANNEL — 3 here, 1 for the SUNSUBSCRIBE below
    r["ssub"] = cmd_n(sub, 3, "SSUBSCRIBE", "sc1", "sc2", "sc3")
    r["spub1"] = cmd(pub, "SPUBLISH", "sc1", "hello"); r["msg1"] = drain(sub)
    r["spub2"] = cmd(pub, "SPUBLISH", "sc2", "world"); r["msg2"] = drain(sub)
    r["spub_none"] = cmd(pub, "SPUBLISH", "nope", "x")
    r["shardchannels"] = cmd(pub, "PUBSUB", "SHARDCHANNELS")
    r["shardnumsub"] = cmd(pub, "PUBSUB", "SHARDNUMSUB", "sc1", "sc2", "nope")
    r["shardchannels_pat"] = cmd(pub, "PUBSUB", "SHARDCHANNELS", "sc*")
    r["shardchannels_nomatch"] = cmd(pub, "PUBSUB", "SHARDCHANNELS", "zzz*")
    r["sunsub"] = cmd_n(sub, 1, "SUNSUBSCRIBE", "sc1")
    r["shardchannels2"] = cmd(pub, "PUBSUB", "SHARDCHANNELS")
    r["numpat"] = cmd(pub, "PUBSUB", "NUMPAT")  # shard subs don't count as patterns
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
