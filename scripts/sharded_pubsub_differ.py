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
import re
import sys
import time  # only for drain()'s settle wait, never for reading a command reply

from _respread import cmd, cmd_n, conn


def drain(s, settle=0.15):
    """Collect asynchronously-delivered push frames, or confirm none arrived.

    DELIBERATE EXCEPTION to the shared reader (frankenredis-tesrb): every other
    read in this gate is a reply to a command we just sent, so frame-completeness
    tells the reader when to stop. Here there is no such bound — "the server sent
    nothing more" is only observable by waiting, so this stays a timed
    non-blocking read. It is a settle wait, not a truncating read: the loop below
    consumes everything buffered rather than taking one recv and moving on.
    """
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


def unordered_bulk_array(reply):
    """Canonicalize PUBSUB CHANNELS replies whose member order is unspecified."""
    return tuple(sorted(re.findall(rb"\$\d+\r\n(.*?)\r\n", reply)))


def run(port):
    sub, pub = conn(port), conn(port)
    out = {}
    # subscribe confirmation frames (one per channel, in order)
    out["ssubscribe"] = cmd_n(sub, 2, "SSUBSCRIBE", "sc1", "sc2")
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
        expected = unordered_bulk_array(oracle[k]) if k in {"shardchannels", "shardchannels_after"} else oracle[k]
        actual = unordered_bulk_array(fr[k]) if k in {"shardchannels", "shardchannels_after"} else fr[k]
        if expected != actual:
            diffs += 1
            print(f"DIFF {k}\n  redis={expected!r}\n  fr   ={actual!r}")
    if diffs:
        print(f"\nFAIL — {diffs} sharded pub/sub divergence(s) vs redis 7.2.4")
        sys.exit(1)
    print(f"PASS — sharded pub/sub byte-exact vs redis 7.2.4 ({len(oracle)} steps)")


if __name__ == "__main__":
    main()
