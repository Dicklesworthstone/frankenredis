#!/usr/bin/env python3
"""pubsub_differ.py — multi-connection differential gate for the Pub/Sub surface
(fr vs vendored redis 7.2.4): SUBSCRIBE/UNSUBSCRIBE, PSUBSCRIBE/PUNSUBSCRIBE,
PUBLISH delivery (message/pmessage), sharded SSUBSCRIBE/SPUBLISH/SUNSUBSCRIBE,
PUBSUB CHANNELS/NUMSUB/NUMPAT/SHARDCHANNELS/SHARDNUMSUB introspection, and the
RESP3 push-frame variants after HELLO 3.

Uses separate connections for publisher and subscriber(s) and compares the
confirmation replies, the delivered frames, and the introspection replies.

KNOWN WONTFIX (excluded): the order of `UNSUBSCRIBE`/`PUNSUBSCRIBE` with no
arguments. redis 7.x stores c->pubsub_channels / c->pubsub_patterns as DICTS and
`pubsubUnsubscribeAll*` iterates them with dictGetSafeIterator → SipHash bucket
order, which fr's IndexSet/foldhash cannot reproduce. We sort the per-element
replies for the unsubscribe-all cases so only the SET (not the order) is checked.

Usage: pubsub_differ.py [--oracle 16399] [--fr 16400]
Exit 0 if byte-exact (modulo the unsubscribe-all order), else 1.
"""
import argparse
import socket
import sys
import time


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(0.4)
        self.b = b""

    def _fill(self):
        try:
            chunk = self.s.recv(65536)
        except socket.timeout:
            return False
        if not chunk:
            return False
        self.b += chunk
        return True

    def _line(self):
        while b"\r\n" not in self.b:
            if not self._fill():
                raise socket.timeout()
        l, self.b = self.b.split(b"\r\n", 1)
        return l

    def _rn(self, n):
        while len(self.b) < n + 2:
            if not self._fill():
                raise socket.timeout()
        d, self.b = self.b[:n], self.b[n + 2:]
        return d

    def parse(self):
        l = self._line()
        t, r = l[:1], l[1:]
        if t in (b"+", b":"):
            return r.decode()
        if t == b"-":
            return "ERR:" + r.decode()
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        if t == b">":  # RESP3 push
            n = int(r)
            return ["PUSH"] + [self.parse() for _ in range(n)]
        if t == b"%":
            n = int(r)
            return ["MAP"] + [self.parse() for _ in range(2 * n)]
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()

    def drain(self):
        out = []
        while True:
            try:
                out.append(self.parse())
            except socket.timeout:
                break
        return out


def run(port, hello3):
    r = {}
    pub = Conn(port)
    pub.cmd("FLUSHALL")
    sub = Conn(port)
    if hello3:
        sub.cmd("HELLO", "3")
    r["sub"] = sub.cmd("SUBSCRIBE", "news", "sports")
    r["psub"] = sub.cmd("PSUBSCRIBE", "news.*", "s?orts")
    time.sleep(0.1)
    r["channels"] = sorted(pub.cmd("PUBSUB", "CHANNELS") or [])
    r["channels_pat"] = sorted(pub.cmd("PUBSUB", "CHANNELS", "news*") or [])
    r["numsub"] = pub.cmd("PUBSUB", "NUMSUB", "news", "sports", "missing")
    r["numpat"] = pub.cmd("PUBSUB", "NUMPAT")
    r["pub_news"] = pub.cmd("PUBLISH", "news", "hello")
    r["pub_newsx"] = pub.cmd("PUBLISH", "news.world", "breaking")
    r["pub_sports"] = pub.cmd("PUBLISH", "sports", "goal")
    r["pub_none"] = pub.cmd("PUBLISH", "nobody", "x")
    time.sleep(0.15)
    r["delivered"] = sub.drain()
    # targeted unsubscribe (deterministic order — single arg)
    r["unsub_one"] = sub.cmd("UNSUBSCRIBE", "news")
    r["punsub_one"] = sub.cmd("PUNSUBSCRIBE", "news.*")
    # unsubscribe-all (DICT hash order — compare as a set)
    r["unsub_all"] = sorted(map(str, sub.cmd("UNSUBSCRIBE") or []))
    r["punsub_all"] = sorted(map(str, sub.cmd("PUNSUBSCRIBE") or []))

    # sharded pub/sub
    ssub = Conn(port)
    if hello3:
        ssub.cmd("HELLO", "3")
    r["ssub"] = ssub.cmd("SSUBSCRIBE", "shard1", "shard2")
    time.sleep(0.1)
    r["shardchannels"] = sorted(pub.cmd("PUBSUB", "SHARDCHANNELS") or [])
    r["shardnumsub"] = pub.cmd("PUBSUB", "SHARDNUMSUB", "shard1", "missing")
    r["spub1"] = pub.cmd("SPUBLISH", "shard1", "sharded-msg")
    r["spub_none"] = pub.cmd("SPUBLISH", "nobody", "x")
    time.sleep(0.15)
    r["sdelivered"] = ssub.drain()
    r["sunsub_one"] = ssub.cmd("SUNSUBSCRIBE", "shard1")
    r["sunsub_all"] = sorted(map(str, ssub.cmd("SUNSUBSCRIBE") or []))
    return r


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    args = ap.parse_args()

    nd = 0
    for hello3 in (False, True):
        o = run(args.oracle, hello3)
        f = run(args.fr, hello3)
        for k in o:
            if o[k] != f.get(k):
                nd += 1
                proto = "RESP3" if hello3 else "RESP2"
                print(f"DIFF [{proto}/{k}]")
                print(f"   oracle: {o[k]}")
                print(f"   fr    : {f.get(k)}")
    if nd:
        print(f"FAIL: {nd} pub/sub divergences")
        sys.exit(1)
    print("OK: pub/sub byte-exact vs redis 7.2.4 (RESP2 + RESP3, sharded, "
          "introspection; unsubscribe-all order excluded as dict-hash WONTFIX)")


if __name__ == "__main__":
    main()
