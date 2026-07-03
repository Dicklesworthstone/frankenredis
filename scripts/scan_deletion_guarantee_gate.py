#!/usr/bin/env python3
"""scan_deletion_guarantee_gate.py — Redis SCAN-family mid-scan-deletion guarantee.

Redis guarantees that a full (H|S|Z)SCAN / SCAN iteration returns every element
present from the start to the end of the iteration AT LEAST once, even if OTHER
elements are deleted during the scan (the canonical "scan a batch, delete what you
saw, repeat" loop). A positional/offset cursor violates this: deleting an
already-returned element shifts later elements and the next batch steps over one.

This gate drives exactly that pattern on a large (multi-batch) collection and
asserts no present-throughout element is missed. It runs against fr AND the redis
7.2.4 oracle so the oracle is a live control (redis must always pass).

Status of each variant on fr (frankenredis):
  * SCAN  (keyspace) : FIXED 55cfc0966 (resume-by-key over ordered BTreeSet)      -> GUARDED
  * ZSCAN            : FIXED db57b2734 (resume-by-value over ordered treap)        -> GUARDED
  * HSCAN / SSCAN    : KNOWN STRUCTURAL (bead e3y73): Hash/Set removal reorders
                       (swap-style), so a positional cursor cannot be made
                       deletion-safe without a reverse-binary chaining dictScan
                       (keyspace_dict::KeyDict, unwired, uhthd).               -> XFAIL

The GUARDED variants regressing fails the gate (exit 1). The XFAIL variants are
reported but do not fail the gate; when the structural fix lands, flip them to
GUARDED and this gate proves it.

Usage: scan_deletion_guarantee_gate.py [ORACLE_PORT] [FR_PORT]   (default 28801 28802)
"""
import socket, sys

ORACLE = int(sys.argv[1]) if len(sys.argv) > 1 else 28801
FR = int(sys.argv[2]) if len(sys.argv) > 2 else 28802
N = 500          # elements per collection (forces multi-batch hashtable/skiplist)
COUNT = 10       # SCAN COUNT hint


def enc(args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        a = a.encode() if isinstance(a, str) else a
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


class Conn:
    """RESP connection with a PERSISTENT read buffer, so pipelined replies whose
    bytes arrive together are not lost between read_reply calls."""

    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.s.settimeout(10)
        self.buf = b""

    def _line(self):
        while b"\r\n" not in self.buf:
            d = self.s.recv(65536)
            if not d:
                raise ConnectionError("closed")
            self.buf += d
        i = self.buf.index(b"\r\n")
        line = self.buf[:i]
        self.buf = self.buf[i + 2:]
        return line

    def read_reply(self):
        line = self._line()
        t = line[:1]
        if t in (b"+", b"-", b":"):
            return line[1:].decode()
        if t == b"$":
            n = int(line[1:])
            if n < 0:
                return None
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(65536)
            d = self.buf[:n]
            self.buf = self.buf[n + 2:]
            return d.decode("utf-8", "replace")
        if t == b"*":
            n = int(line[1:])
            return [self.read_reply() for _ in range(n)] if n >= 0 else None
        return line.decode()


def conn(p):
    return Conn(p)


def cmd(s, *a):
    s.s.sendall(enc([str(x) for x in a]))
    return s.read_reply()


def pipe_load(s, cmds):
    for i in range(0, len(cmds), 200):
        chunk = cmds[i:i + 200]
        s.s.sendall(b"".join(enc([str(x) for x in c]) for c in chunk))
        for _ in chunk:
            s.read_reply()


VARIANTS = {
    # name: (build cmds, scan cmd, delete cmd builder, extract-keys-from-batch, member fmt)
    "SCAN": dict(guarded=True),
    "ZSCAN": dict(guarded=True),
    "HSCAN": dict(guarded=False),
    "SSCAN": dict(guarded=False),
}


def member(i):
    return f"m{i:04}"


def run_variant(s, kind):
    cmd(s, "FLUSHALL")
    if kind == "SCAN":
        pipe_load(s, [["SET", member(i), "v"] for i in range(N)])
        scan = lambda cur: cmd(s, "SCAN", cur, "COUNT", COUNT)
        keys_of = lambda batch: batch
        delete = lambda m: cmd(s, "DEL", m)
    elif kind == "ZSCAN":
        pipe_load(s, [["ZADD", "z", str(i), member(i)] for i in range(N)])
        scan = lambda cur: cmd(s, "ZSCAN", "z", cur, "COUNT", COUNT)
        keys_of = lambda batch: batch[0::2]
        delete = lambda m: cmd(s, "ZREM", "z", m)
    elif kind == "HSCAN":
        cmd(s, "CONFIG", "SET", "hash-max-listpack-entries", "8")
        pipe_load(s, [["HSET", "h", member(i), "v"] for i in range(N)])
        scan = lambda cur: cmd(s, "HSCAN", "h", cur, "COUNT", COUNT)
        keys_of = lambda batch: batch[0::2]
        delete = lambda m: cmd(s, "HDEL", "h", m)
    else:  # SSCAN
        cmd(s, "CONFIG", "SET", "set-max-listpack-entries", "8")
        pipe_load(s, [["SADD", "se", member(i)] for i in range(N)])
        scan = lambda cur: cmd(s, "SSCAN", "se", cur, "COUNT", COUNT)
        keys_of = lambda batch: batch
        delete = lambda m: cmd(s, "SREM", "se", m)

    seen, deleted, cursor, step = set(), set(), "0", 0
    while True:
        r = scan(cursor)
        cursor = r[0]
        items = keys_of(r[1] or [])
        for it in items:
            seen.add(it)
        step += 1
        if step == 1 and items:
            for m in sorted(items)[:2]:  # delete two already-returned elements
                delete(m)
                deleted.add(m)
        if cursor == "0":
            break
        if step > 1_000_000:
            raise RuntimeError("scan did not terminate")
    survivors = {member(i) for i in range(N)} - deleted
    missing = sorted(survivors - seen)
    return missing


def main():
    oc, fc = conn(ORACLE), conn(FR)
    gate_failed = False
    for kind, meta in VARIANTS.items():
        om = run_variant(oc, kind)
        fm = run_variant(fc, kind)
        assert not om, f"ORACLE(redis 7.2.4) {kind} violated the guarantee (control broken): {om[:5]}"
        guarded = meta["guarded"]
        if not fm:
            print(f"PASS   {kind:6} fr returns all present-throughout elements (guarded={guarded})")
        elif guarded:
            gate_failed = True
            print(f"FAIL   {kind:6} REGRESSION: fr missed {len(fm)} present-throughout elements: {fm[:5]}")
        else:
            print(f"XFAIL  {kind:6} known-structural (e3y73): fr missed {len(fm)} elements {fm[:3]} "
                  f"— reorder-on-delete needs chaining dictScan (uhthd)")
    print("=" * 60)
    if gate_failed:
        print("SCAN-GUARANTEE GATE: a GUARDED scan regressed — FAIL")
        sys.exit(1)
    print("SCAN-GUARANTEE GATE: guarded scans (SCAN, ZSCAN) hold vs redis 7.2.4; "
          "HSCAN/SSCAN xfail (e3y73)")


if __name__ == "__main__":
    main()
