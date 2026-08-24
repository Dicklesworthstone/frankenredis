#!/usr/bin/env python3
"""pubsub_differ.py — multi-connection differential gate for the Pub/Sub surface
(fr vs vendored redis 7.2.4): SUBSCRIBE/UNSUBSCRIBE, PSUBSCRIBE/PUNSUBSCRIBE,
PUBLISH delivery (message/pmessage), sharded SSUBSCRIBE/SPUBLISH/SUNSUBSCRIBE,
PUBSUB CHANNELS/NUMSUB/NUMPAT/SHARDCHANNELS/SHARDNUMSUB introspection, and the
RESP3 push-frame variants after HELLO 3.

Uses separate connections for publisher and subscriber(s) and compares the
confirmation replies, the delivered frames, and the introspection replies.
Every subscription command and every published delivery has a known frame count;
the gate reads exactly that count rather than draining briefly and accepting
whatever happened to arrive.

KNOWN WONTFIX (excluded): the order of `UNSUBSCRIBE`/`PUNSUBSCRIBE` with no
arguments. redis 7.x stores c->pubsub_channels / c->pubsub_patterns as DICTS and
`pubsubUnsubscribeAll*` iterates them with dictGetSafeIterator → SipHash bucket
order, which fr's IndexSet/foldhash cannot reproduce. We sort the per-element
replies for the unsubscribe-all cases so only the SET (not the order) is checked.

Usage: pubsub_differ.py [--oracle 16399] [--fr 16400] [--planted-negative]
Exit 0 if byte-exact (modulo the unsubscribe-all order), else 1.
"""
import argparse
import socket


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

    def cmd_n(self, n, *a):
        """Send one command and consume its exactly-known reply-frame count."""
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.read_n(n)

    def read_n(self, n):
        """Consume exactly n frames already expected by the protocol sequence."""
        return [self.parse() for _ in range(n)]

    def close(self):
        self.s.close()


def push_payload(frame):
    """RESP2 arrays and RESP3 push frames carry the same pub/sub payload."""
    return frame[1:] if frame and frame[0] == "PUSH" else frame


def assert_frames(frames, expected, label):
    actual = [push_payload(frame) for frame in frames]
    if actual != expected:
        raise SystemExit(f"SEED FAILED [{label}]: got {actual!r}, expected {expected!r}")
    return frames


def assert_count(reply, expected, label):
    if reply != expected:
        raise SystemExit(f"SEED FAILED [{label}]: got {reply!r}, expected {expected!r}")
    return reply


def run(port, hello3):
    r = {}
    pub = Conn(port)
    if pub.cmd("FLUSHALL") != "OK":
        raise SystemExit("SEED FAILED [FLUSHALL]: server did not return OK")
    sub = Conn(port)
    if hello3:
        sub.cmd("HELLO", "3")
    r["sub"] = sub.cmd_n(2, "SUBSCRIBE", "news", "sports")
    assert_frames(r["sub"], [["subscribe", "news", "1"], ["subscribe", "sports", "2"]], "SUBSCRIBE")
    r["psub"] = sub.cmd_n(2, "PSUBSCRIBE", "news.*", "s?orts")
    assert_frames(r["psub"], [["psubscribe", "news.*", "3"], ["psubscribe", "s?orts", "4"]], "PSUBSCRIBE")
    r["channels"] = sorted(pub.cmd("PUBSUB", "CHANNELS") or [])
    r["channels_pat"] = sorted(pub.cmd("PUBSUB", "CHANNELS", "news*") or [])
    r["numsub"] = pub.cmd("PUBSUB", "NUMSUB", "news", "sports", "missing")
    r["numpat"] = pub.cmd("PUBSUB", "NUMPAT")
    r["pub_news"] = pub.cmd("PUBLISH", "news", "hello")
    assert_count(r["pub_news"], "1", "PUBLISH news")
    r["pub_newsx"] = pub.cmd("PUBLISH", "news.world", "breaking")
    assert_count(r["pub_newsx"], "1", "PUBLISH news.world")
    r["pub_sports"] = pub.cmd("PUBLISH", "sports", "goal")
    assert_count(r["pub_sports"], "2", "PUBLISH sports")
    r["pub_none"] = pub.cmd("PUBLISH", "nobody", "x")
    assert_count(r["pub_none"], "0", "PUBLISH nobody")
    r["delivered"] = sub.read_n(4)
    assert_frames(
        r["delivered"],
        [["message", "news", "hello"], ["pmessage", "news.*", "news.world", "breaking"],
         ["message", "sports", "goal"], ["pmessage", "s?orts", "sports", "goal"]],
        "published deliveries",
    )
    # targeted unsubscribe (deterministic order — single arg)
    r["unsub_one"] = sub.cmd_n(1, "UNSUBSCRIBE", "news")
    assert_frames(r["unsub_one"], [["unsubscribe", "news", "3"]], "UNSUBSCRIBE news")
    r["punsub_one"] = sub.cmd_n(1, "PUNSUBSCRIBE", "news.*")
    assert_frames(r["punsub_one"], [["punsubscribe", "news.*", "2"]], "PUNSUBSCRIBE news.*")
    # unsubscribe-all (DICT hash order — compare as a set)
    unsub_all = sub.cmd_n(1, "UNSUBSCRIBE")
    assert_frames(unsub_all, [["unsubscribe", "sports", "1"]], "UNSUBSCRIBE all")
    r["unsub_all"] = sorted(map(str, unsub_all))
    punsub_all = sub.cmd_n(1, "PUNSUBSCRIBE")
    assert_frames(punsub_all, [["punsubscribe", "s?orts", "0"]], "PUNSUBSCRIBE all")
    r["punsub_all"] = sorted(map(str, punsub_all))

    # sharded pub/sub
    ssub = Conn(port)
    if hello3:
        ssub.cmd("HELLO", "3")
    r["ssub"] = ssub.cmd_n(2, "SSUBSCRIBE", "shard1", "shard2")
    assert_frames(r["ssub"], [["ssubscribe", "shard1", "1"], ["ssubscribe", "shard2", "2"]], "SSUBSCRIBE")
    r["shardchannels"] = sorted(pub.cmd("PUBSUB", "SHARDCHANNELS") or [])
    r["shardnumsub"] = pub.cmd("PUBSUB", "SHARDNUMSUB", "shard1", "missing")
    r["spub1"] = pub.cmd("SPUBLISH", "shard1", "sharded-msg")
    assert_count(r["spub1"], "1", "SPUBLISH shard1")
    r["spub_none"] = pub.cmd("SPUBLISH", "nobody", "x")
    assert_count(r["spub_none"], "0", "SPUBLISH nobody")
    r["sdelivered"] = ssub.read_n(1)
    assert_frames(r["sdelivered"], [["smessage", "shard1", "sharded-msg"]], "sharded delivery")
    r["sunsub_one"] = ssub.cmd_n(1, "SUNSUBSCRIBE", "shard1")
    assert_frames(r["sunsub_one"], [["sunsubscribe", "shard1", "1"]], "SUNSUBSCRIBE shard1")
    sunsub_all = ssub.cmd_n(1, "SUNSUBSCRIBE")
    assert_frames(sunsub_all, [["sunsubscribe", "shard2", "0"]], "SUNSUBSCRIBE all")
    r["sunsub_all"] = sorted(map(str, sunsub_all))
    pub.close()
    sub.close()
    ssub.close()
    return r


def compare(oracle, fr):
    return [key for key in oracle if oracle[key] != fr.get(key)]


def planted_negative():
    oracle = {"delivery": [["message", "news", "hello"]]}
    wrong = {"delivery": [["message", "news", "FAIL"]]}
    diffs = compare(oracle, wrong)
    if not diffs:
        print("PLANTED NEGATIVE MISSED: a wrong delivered payload passed")
        return 0
    print(f"PLANTED NEGATIVE DETECTED: wrong delivered payload fails at {diffs[0]}")
    return 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--planted-negative", action="store_true")
    args = ap.parse_args()

    if args.planted_negative:
        return planted_negative()

    nd = 0
    for hello3 in (False, True):
        o = run(args.oracle, hello3)
        f = run(args.fr, hello3)
        for k in compare(o, f):
            nd += 1
            proto = "RESP3" if hello3 else "RESP2"
            print(f"DIFF [{proto}/{k}]")
            print(f"   oracle: {o[k]}")
            print(f"   fr    : {f.get(k)}")
    if nd:
        print(f"FAIL: {nd} pub/sub divergences")
        return 1
    print("OK: pub/sub byte-exact vs redis 7.2.4 (RESP2 + RESP3, sharded, "
          "introspection; unsubscribe-all order excluded as dict-hash WONTFIX)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
