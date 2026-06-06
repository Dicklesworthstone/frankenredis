#!/usr/bin/env python3
"""scan_differ.py — differential gate for the SCAN family vs vendored redis 7.2.4:
SCAN / HSCAN / SSCAN / ZSCAN with MATCH, COUNT, TYPE, NOVALUES, plus cursor and
argument validation.

KNOWN WONTFIX (excluded from the comparison): the SCAN *cursor sequence* and the
*order* of returned elements. fr stores keys in a BTreeSet and always completes
in a single pass returning cursor 0, whereas redis traverses dict buckets with
reverse-binary-increment cursors and SipHash bucket order. See
[[project_scan_cursor_architecture]]. We therefore compare only:
  - whether the scan completed (cursor == "0"), and
  - the SET of returned elements (sorted),
which IS well-defined and must match. Errors/replies for option validation
(bad cursor, COUNT<=0, NOVALUES on HSCAN — a 7.4 feature absent in 7.2.4) are
compared verbatim.

Encoding/threshold config is irrelevant to SCAN result sets, so no config
alignment is needed (unlike the encoding differs).

Usage: scan_differ.py [--oracle 16399] [--fr 16400]
Exit 0 if byte-exact (modulo cursor/order), else 1.
"""
import argparse
import socket
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(3.0)
        self.b = b""

    def _line(self):
        while b"\r\n" not in self.b:
            self.b += self.s.recv(65536)
        l, self.b = self.b.split(b"\r\n", 1)
        return l

    def _rn(self, n):
        while len(self.b) < n + 2:
            self.b += self.s.recv(65536)
        d, self.b = self.b[:n], self.b[n + 2:]
        return d

    def parse(self):
        l = self._line()
        t, r = l[:1], l[1:]
        if t in (b"+", b":"):
            return l.decode("latin1")
        if t == b"-":
            return "ERR:" + r.decode("latin1")
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()


def normalize(reply):
    """A successful SCAN reply is [cursor, [elements]]. Normalize to
    (completed?, sorted-elements) so the cursor sequence and element order
    (both WONTFIX) don't cause spurious diffs; pass errors through verbatim."""
    if (
        isinstance(reply, list)
        and len(reply) == 2
        and isinstance(reply[1], list)
    ):
        return (reply[0] == "0", sorted(reply[1]))
    return reply


KEYS_SETUP = [
    ("MSET", "k1", "1", "k2", "2", "k3", "3", "other", "x"),
    ("RPUSH", "lst", "a"),
    ("SADD", "st", "m"),
    ("HSET", "hh", "f", "v"),
    ("ZADD", "zz", "1", "a"),
]
HASH_SETUP = [("HSET", "h", "f1", "1", "f2", "2", "f3", "3", "g1", "9")]
ZSET_SETUP = [("ZADD", "z", "1", "a", "2", "b", "3", "c")]
SET_SETUP = [("SADD", "s", "m1", "m2", "m3", "x9")]
SET_INT = [("SADD", "si", *[str(i) for i in range(50)])]

CASES = [
    # --- SCAN ---
    ("scan_all", KEYS_SETUP, ("SCAN", "0", "COUNT", "100")),
    ("scan_match", KEYS_SETUP, ("SCAN", "0", "MATCH", "k*", "COUNT", "100")),
    ("scan_match_none", KEYS_SETUP, ("SCAN", "0", "MATCH", "zzz*", "COUNT", "100")),
    ("scan_type_string", KEYS_SETUP, ("SCAN", "0", "TYPE", "string", "COUNT", "100")),
    ("scan_type_list", KEYS_SETUP, ("SCAN", "0", "TYPE", "list", "COUNT", "100")),
    ("scan_type_hash", KEYS_SETUP, ("SCAN", "0", "TYPE", "hash", "COUNT", "100")),
    ("scan_type_zset", KEYS_SETUP, ("SCAN", "0", "TYPE", "zset", "COUNT", "100")),
    ("scan_type_set", KEYS_SETUP, ("SCAN", "0", "TYPE", "set", "COUNT", "100")),
    ("scan_type_stream_empty", KEYS_SETUP, ("SCAN", "0", "TYPE", "stream", "COUNT", "100")),
    ("scan_match_type", KEYS_SETUP, ("SCAN", "0", "MATCH", "k*", "TYPE", "string", "COUNT", "100")),
    # NB: a small COUNT yields a PARTIAL scan whose element subset differs by
    # bucket-traversal order (WONTFIX) — only full single-pass scans (COUNT large
    # enough to complete) have a well-defined comparable result set, so we always
    # pass COUNT 100 here.
    # --- SCAN validation ---
    ("scan_count_neg", KEYS_SETUP, ("SCAN", "0", "COUNT", "-1")),
    ("scan_count_zero", KEYS_SETUP, ("SCAN", "0", "COUNT", "0")),
    ("scan_bad_cursor", KEYS_SETUP, ("SCAN", "notanumber")),
    ("scan_garbage_opt", KEYS_SETUP, ("SCAN", "0", "BADOPT")),
    ("scan_arity", [], ("SCAN",)),
    # --- HSCAN ---
    ("hscan_all", HASH_SETUP, ("HSCAN", "h", "0", "COUNT", "100")),
    ("hscan_match", HASH_SETUP, ("HSCAN", "h", "0", "MATCH", "f*", "COUNT", "100")),
    ("hscan_nonexist", [], ("HSCAN", "nope", "0")),
    ("hscan_novalues", HASH_SETUP, ("HSCAN", "h", "0", "NOVALUES")),  # 7.4 → 7.2.4 syntax error
    ("hscan_novalues_match", HASH_SETUP, ("HSCAN", "h", "0", "MATCH", "f*", "NOVALUES")),
    ("hscan_bad_cursor", HASH_SETUP, ("HSCAN", "h", "x")),
    # --- SSCAN ---
    ("sscan_all", SET_SETUP, ("SSCAN", "s", "0", "COUNT", "100")),
    ("sscan_match", SET_SETUP, ("SSCAN", "s", "0", "MATCH", "m*", "COUNT", "100")),
    ("sscan_intset", SET_INT, ("SSCAN", "si", "0", "COUNT", "100")),
    ("sscan_intset_match", SET_INT, ("SSCAN", "si", "0", "MATCH", "1*", "COUNT", "100")),
    ("sscan_nonexist", [], ("SSCAN", "nope", "0")),
    # --- ZSCAN ---
    ("zscan_all", ZSET_SETUP, ("ZSCAN", "z", "0", "COUNT", "100")),
    ("zscan_match", ZSET_SETUP, ("ZSCAN", "z", "0", "MATCH", "a", "COUNT", "100")),
    ("zscan_nonexist", [], ("ZSCAN", "nope", "0")),
    # --- wrong-type ---
    ("hscan_wrongtype", [("SET", "k", "v")], ("HSCAN", "k", "0")),
    ("sscan_wrongtype", [("SET", "k", "v")], ("SSCAN", "k", "0")),
    ("zscan_wrongtype", [("SET", "k", "v")], ("ZSCAN", "k", "0")),
]


def run(c, setup, probe):
    c.cmd("FLUSHALL")
    for cmd in setup:
        c.cmd(*cmd)
    return normalize(c.cmd(*probe))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    args = ap.parse_args()
    o, f = Conn(args.oracle), Conn(args.fr)

    diffs = 0
    for label, setup, probe in CASES:
        ro = run(o, setup, probe)
        rf = run(f, setup, probe)
        if ro != rf:
            diffs += 1
            print(f"DIFF [{label}] {probe}")
            print(f"   oracle: {ro!r}")
            print(f"   fr    : {rf!r}")
    if diffs:
        print(f"\nFAIL: {diffs} SCAN-family divergences")
        sys.exit(1)
    print(f"OK: {len(CASES)} SCAN-family cases byte-exact vs redis 7.2.4 "
          "(cursor sequence + element order excluded as WONTFIX)")


if __name__ == "__main__":
    main()
