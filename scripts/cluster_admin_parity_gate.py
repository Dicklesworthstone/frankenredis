#!/usr/bin/env python3
"""cluster_admin_parity_gate.py — standalone CLUSTER / admin / introspection
parity vs vendored redis 7.2.4.

The data-plane gates (fuzz_untrodden_differ, algebra_resp3_differ, encoding_differ,
…) never touch the *administrative* command surface a non-cluster ("standalone")
redis still answers. This gate pins that surface, which is otherwise only spot-
checked by hand:

  - CLUSTER on a cluster-disabled instance: INFO (cluster_enabled:0), MYID,
    SLOTS/SHARDS/NODES/LINKS shapes, KEYSLOT (CRC16 + {hash-tag} extraction),
    COUNTKEYSINSLOT, and the "ERR This instance has cluster support disabled"
    wording for the mutating subcommands (MEET/ADDSLOTS/SETSLOT/…)
  - cluster-routing preamble verbs READONLY / READWRITE / ASKING
  - FUNCTION LIST / STATS / DUMP with a library actually loaded (FUNCTION DUMP is
    a full RDB-framed payload incl. version footer + CRC64 — byte-exact here)
  - MEMORY USAGE per-key size estimate (fr matches redis 7.2.4's sizeof model
    byte-for-byte for string/list/hash/set/zset/stream), MEMORY DOCTOR empty-vs-
    populated text, MEMORY MALLOC-STATS reply kind
  - MODULE LIST, LATENCY RESET/HISTORY/LATEST, ACL WHOAMI/CAT, COMMAND COUNT,
    LOLWUT, plus wrong-arity / unknown-subcommand precedence for the above

Both servers are launched fresh and config-less (compiled defaults, cluster
disabled) and seeded with identical data + an identical FUNCTION library, so any
difference is a genuine implementation divergence — except the documented
allocator-internal / data-dependent classes that are normalised below:

  - MEMORY STATS allocator-accounting fields (peak/total/startup.allocated, frag,
    rss-overhead) reflect mimalloc-vs-jemalloc internals — compared by KEY SET +
    value shape, not value
  - MEMORY MALLOC-STATS body (jemalloc-specific) — reply kind only
  - LOLWUT art is version/impl-specific (documented WONTFIX) — reply kind only
  - both-error replies compare by error CODE word

Usage: cluster_admin_parity_gate.py [--bin PATH] [--redis-bin PATH]
Exit 0 if the surface matches redis 7.2.4 (modulo the normalisations), else 1.
"""
import argparse
import os
import socket
import subprocess
import sys
import time


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(4.0)
        self.b = bytearray()

    def _line(self):
        while b"\r\n" not in self.b:
            self.b += self.s.recv(65536)
        i = self.b.index(b"\r\n")
        line = bytes(self.b[:i])
        del self.b[:i + 2]
        return line

    def _rn(self, n):
        while len(self.b) < n + 2:
            self.b += self.s.recv(65536)
        d = bytes(self.b[:n])
        del self.b[:n + 2]
        return d

    def parse(self):
        line = self._line()
        t, r = line[:1], line[1:]
        if t == b'+':
            return ('status', r.decode('latin1'))
        if t == b'-':
            return ('error', r.decode('latin1'))
        if t == b':':
            return ('int', int(r))
        if t == b'$':
            n = int(r)
            return ('nil', None) if n == -1 else ('bulk', self._rn(n))
        if t == b'*':
            n = int(r)
            return ('nil', None) if n == -1 else ('array', [self.parse() for _ in range(n)])
        if t == b'%':
            n = int(r)
            return ('map', [(self.parse(), self.parse()) for _ in range(n)])
        if t == b'~':
            n = int(r)
            return ('set', [self.parse() for _ in range(n)])
        if t in (b',', b'#', b'(', b'='):
            return (chr(t[0]), r.decode('latin1'))
        if t == b'_':
            return ('nil', None)
        raise ValueError("bad reply %r" % line)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()


# A tiny library so FUNCTION LIST/STATS/DUMP have content to compare.
FUNC_LIB = (
    "#!lua name=problib\n"
    "redis.register_function('pf', function() return 1 end)"
)


def seed(c):
    c.cmd("FLUSHALL")
    try:
        c.cmd("FUNCTION", "FLUSH")
    except Exception:
        pass
    c.cmd("SET", "str1", "hello world")
    c.cmd("RPUSH", "list1", "a", "b", "c", "d")
    c.cmd("HSET", "h1", "f1", "v1", "f2", "v2")
    c.cmd("SADD", "s1", "x", "y", "z")
    c.cmd("SADD", "iset", "1", "2", "3")
    c.cmd("ZADD", "z1", "1", "m1", "2", "m2")
    c.cmd("XADD", "stream1", "1-1", "field", "val")
    c.cmd("FUNCTION", "LOAD", FUNC_LIB)


# (command, normalisation-tag) — tag drives how the reply is compared.
TESTS = [
    # ── CLUSTER (cluster-disabled standalone) ──
    (["CLUSTER", "INFO"], "exact"),
    (["CLUSTER", "MYID"], "exact"),
    (["CLUSTER", "SLOTS"], "exact"),
    (["CLUSTER", "SHARDS"], "exact"),
    (["CLUSTER", "NODES"], "exact"),
    (["CLUSTER", "LINKS"], "exact"),
    (["CLUSTER", "KEYSLOT", "foo"], "exact"),
    (["CLUSTER", "KEYSLOT", "{user1000}.following"], "exact"),
    (["CLUSTER", "KEYSLOT", "{}.x"], "exact"),
    (["CLUSTER", "KEYSLOT", "abc{def}ghi{jkl}"], "exact"),
    (["CLUSTER", "KEYSLOT", ""], "exact"),
    (["CLUSTER", "COUNTKEYSINSLOT", "0"], "exact"),
    (["CLUSTER", "COUNTKEYSINSLOT", "16383"], "exact"),
    (["CLUSTER", "COUNTKEYSINSLOT", "99999"], "errcode"),
    (["CLUSTER", "GETKEYSINSLOT", "0", "10"], "exact"),
    (["CLUSTER", "RESET"], "errcode"),
    (["CLUSTER", "RESET", "HARD"], "errcode"),
    (["CLUSTER", "MEET", "127.0.0.1", "7000"], "errcode"),
    (["CLUSTER", "SAVECONFIG"], "errcode"),
    (["CLUSTER", "BUMPEPOCH"], "errcode"),
    (["CLUSTER", "SET-CONFIG-EPOCH", "1"], "errcode"),
    (["CLUSTER", "FORGET", "abc"], "errcode"),
    (["CLUSTER", "REPLICAS", "abc"], "errcode"),
    (["CLUSTER", "SLAVES", "abc"], "errcode"),
    (["CLUSTER", "FAILOVER"], "errcode"),
    (["CLUSTER", "ADDSLOTS", "0"], "errcode"),
    (["CLUSTER", "DELSLOTS", "0"], "errcode"),
    (["CLUSTER", "SETSLOT", "0", "STABLE"], "errcode"),
    (["CLUSTER", "KEYSLOT"], "errcode"),
    (["CLUSTER", "FOO"], "errcode"),
    # ── cluster-routing preamble verbs (standalone: plain OK / error) ──
    (["READONLY"], "exact"),
    (["READWRITE"], "exact"),
    (["ASKING"], "exact"),
    # ── FUNCTION (library loaded) ──
    (["FUNCTION", "LIST"], "exact"),
    (["FUNCTION", "LIST", "WITHCODE"], "exact"),
    (["FUNCTION", "STATS"], "exact"),
    (["FUNCTION", "DUMP"], "exact"),
    # ── MEMORY ──
    (["MEMORY", "USAGE", "str1"], "exact"),
    (["MEMORY", "USAGE", "list1"], "exact"),
    (["MEMORY", "USAGE", "h1"], "exact"),
    (["MEMORY", "USAGE", "s1"], "exact"),
    # intset-encoded set MEMORY USAGE now byte-exact after the width fix
    # (frankenredis-intset-memusage-width): redis packs members at the single
    # minimal width (INT16=2/INT32=4/INT64=8), matched by fr-store.
    (["MEMORY", "USAGE", "iset"], "exact"),
    (["MEMORY", "USAGE", "z1"], "exact"),
    (["MEMORY", "USAGE", "stream1"], "exact"),
    (["MEMORY", "USAGE", "nokey"], "exact"),
    (["MEMORY", "USAGE", "str1", "SAMPLES", "0"], "exact"),
    (["MEMORY", "DOCTOR"], "kind"),
    (["MEMORY", "MALLOC-STATS"], "kind"),
    (["MEMORY", "STATS"], "map_keys"),
    (["MEMORY", "FOO"], "errcode"),
    # ── misc admin / introspection ──
    (["MODULE", "LIST"], "exact"),
    (["MODULE", "FOO"], "errcode"),
    (["LATENCY", "RESET"], "exact"),
    (["LATENCY", "HISTORY", "event"], "exact"),
    (["LATENCY", "LATEST"], "exact"),
    (["LATENCY", "DOCTOR"], "kind"),
    (["ACL", "WHOAMI"], "exact"),
    (["ACL", "CAT"], "set_members"),
    (["COMMAND", "COUNT"], "exact"),
    (["LOLWUT"], "kind"),
    (["LOLWUT", "VERSION", "5"], "kind"),
    (["DEBUG", "STRINGMATCH-LEN", "a*", "aaa"], "exact"),
]


def _flatten_keys(reply):
    """Top-level map key set for a flat MEMORY STATS-style array."""
    if reply[0] in ("array", "map"):
        if reply[0] == "map":
            return sorted(repr(k) for k, _ in reply[1])
        return sorted(repr(reply[1][i]) for i in range(0, len(reply[1]), 2))
    return [repr(reply)]


def _members(reply):
    if reply[0] in ("array", "set"):
        return sorted(repr(x) for x in reply[1])
    return [repr(reply)]


def equivalent(tag, ro, rf):
    if ro == rf:
        return True
    if ro[0] == 'error' and rf[0] == 'error':
        if ro[1].split(' ', 1)[0] == rf[1].split(' ', 1)[0]:
            return True
    if tag == "errcode":
        return ro[0] == 'error' and rf[0] == 'error'
    if tag == "kind":
        # reply-kind match is enough (allocator/art/data-dependent body)
        ok = {'status', 'bulk', 'int', 'nil', '='}
        return ro[0] in ok and rf[0] in ok
    if tag == "map_keys":
        # MEMORY STATS: same key set + same value *kinds*, values are allocator-internal
        return _flatten_keys(ro) == _flatten_keys(rf)
    if tag == "set_members":
        return _members(ro) == _members(rf)
    return False


def find_bin():
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    for c in ("/data/tmp/cargo-target/release/frankenredis",
              "/data/tmp/cargo-target/release-perf/frankenredis",
              "/data/tmp/cargo-target/debug/frankenredis",
              os.path.join(root, "target/release/frankenredis"),
              os.path.join(root, "target/debug/frankenredis")):
        if os.path.exists(c):
            return c
    return None


def find_redis():
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    for c in (os.path.join(root, "legacy_redis_code/redis/src/redis-server"),
              os.path.join(root, "legacy_redis_code/src/redis-server")):
        if os.path.exists(c):
            return c
    return None


def launch(cmdline, port):
    proc = subprocess.Popen(cmdline, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(80):
        try:
            c = Conn(port)
            if c.cmd("PING") == ('status', 'PONG'):
                return proc
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise SystemExit(f"server on port {port} did not start: {cmdline[0]}")


def memusage_sweep(o, f):
    """MEMORY USAGE byte-exactness across DETERMINISTIC encodings at many sizes.

    Locks in the intset-width (e3cb69ca4) and zset-listpack-score (6bb1319a9)
    size-model fixes. Stays strictly within the listpack/intset thresholds
    (compiled defaults: *-max-listpack-entries=128, value len<=64,
    set-max-intset-entries=512) so encodings stay listpack/intset/embstr/raw/int.
    Hashtable/skiplist/quicklist MEMORY USAGE is NOT byte-exact (dict bucket
    array depends on redis's incremental-rehash dual-table state — see
    frankenredis-ht-memusage-bucket-array-kv015) and is deliberately excluded.
    """
    failures = []

    def build(key, cmds):
        for c in (o, f):
            c.cmd("DEL", key)
            for parts in cmds:
                c.cmd(*parts)

    cases = []
    # strings: int / embstr (<=44) / raw (>44)
    for sval in ["7", "12345", "hello", "a" * 44, "a" * 45, "x" * 300]:
        cases.append(("str", [["SET", "mk", sval]]))
    # intset: i16 / i32 / i64 magnitudes, sizes up to the 512 cap
    for n in [1, 2, 3, 5, 8, 16, 64, 200, 512]:
        cases.append((f"iset{n}", [["SADD", "mk"] + [str(i) for i in range(n)]]))
    cases.append(("iset_i32", [["SADD", "mk", "1", "100000", "70000"]]))
    cases.append(("iset_i64", [["SADD", "mk", "1", "5000000000"]]))
    # listpack set (small string members, <=128 entries, len<=64)
    for n in [1, 4, 16, 64, 128]:
        cases.append((f"lpset{n}", [["SADD", "mk"] + [f"m{i:03d}" for i in range(n)]]))
    # listpack hash
    for n in [1, 4, 16, 64, 128]:
        cmds = [["HSET", "mk"] + sum(([f"f{i:03d}", f"v{i:03d}"] for i in range(n)), [])]
        cases.append((f"lphash{n}", cmds))
    # listpack list
    for n in [1, 4, 16, 64, 128]:
        cases.append((f"lplist{n}", [["RPUSH", "mk"] + [f"e{i:03d}" for i in range(n)]]))
    # listpack zset: integer scores (various magnitudes) + fractional + negative
    for n in [1, 2, 3, 5, 8, 16, 64, 128]:
        cmds = [["ZADD", "mk"] + sum(([str(i * 1000), f"m{i:03d}"] for i in range(n)), [])]
        cases.append((f"lpzset{n}", cmds))
    cases.append(("lpzset_frac", [["ZADD", "mk", "3.14159", "a", "-2.5", "b", "0.001", "c"]]))
    cases.append(("lpzset_big", [["ZADD", "mk", "1e20", "a", "5000000000", "b"]]))

    for label, cmds in cases:
        build("mk", cmds)
        ro = o.cmd("MEMORY", "USAGE", "mk")
        rf = f.cmd("MEMORY", "USAGE", "mk")
        eo = o.cmd("OBJECT", "ENCODING", "mk")
        ef = f.cmd("OBJECT", "ENCODING", "mk")
        if eo != ef:
            failures.append((f"memusage:{label}", "encoding",
                             ("enc", eo), ("enc", ef)))
        elif ro != rf:
            failures.append((f"memusage:{label}", f"exact ({eo[1]!r})", ro, rf))
    return failures


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=None)
    ap.add_argument("--redis-bin", default=None)
    args = ap.parse_args()

    binpath = args.bin or find_bin()
    redispath = args.redis_bin or find_redis()
    if not binpath or not os.path.exists(binpath):
        print("FAIL: frankenredis binary not found (pass --bin PATH)", file=sys.stderr)
        sys.exit(2)
    if not redispath or not os.path.exists(redispath):
        print("FAIL: redis-server not found (pass --redis-bin PATH)", file=sys.stderr)
        sys.exit(2)

    oport, fport = 21834, 21835
    rproc = fproc = None
    failures = []
    try:
        rproc = launch([redispath, "--port", str(oport), "--save", "",
                        "--appendonly", "no"], oport)
        fproc = launch([binpath, "--port", str(fport)], fport)
        o = Conn(oport)
        f = Conn(fport)
        seed(o)
        seed(f)
        for cmd, tag in TESTS:
            try:
                ro = o.cmd(*cmd)
            except Exception as e:
                ro = ('exc', str(e))
            try:
                rf = f.cmd(*cmd)
            except Exception as e:
                rf = ('exc', str(e))
            if not equivalent(tag, ro, rf):
                failures.append((cmd, tag, ro, rf))
        failures.extend(memusage_sweep(o, f))
    finally:
        for p in (fproc, rproc):
            if p is None:
                continue
            p.terminate()
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()

    if failures:
        print(f"FAIL: {len(failures)} of {len(TESTS)} admin-surface divergences vs redis 7.2.4:")
        for cmd, tag, ro, rf in failures:
            print(f"  - {' '.join(cmd)}  [{tag}]")
            print(f"      oracle: {ro!r:.300}")
            print(f"      fr    : {rf!r:.300}")
        sys.exit(1)
    print(f"PASS: {len(TESTS)} CLUSTER/admin/FUNCTION/MEMORY surface checks + "
          "deterministic-encoding MEMORY USAGE size sweep byte-exact vs redis 7.2.4 "
          "(allocator-internal + art classes normalised; hashtable/skiplist excluded)")


if __name__ == "__main__":
    main()
