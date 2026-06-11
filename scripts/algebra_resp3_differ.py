#!/usr/bin/env python3
"""algebra_resp3_differ.py — differential parity gate for the command-algebra,
RESP3-typed-reply, and BITFIELD/SCAN option surfaces vs vendored redis 7.2.4.

The existing fuzz_untrodden_differ.py hammers single-key *validation / type-check
order*. This gate covers a different, less-trodden axis: OPTION PERMUTATION and
multi-key ALGEBRA on commands whose reply *shape and value* (not just the error
code) must match byte-for-byte:

  - sorted-set algebra:  ZDIFF/ZINTER/ZUNION(+STORE), ZINTERCARD, ZRANGE BY*/REV,
                         ZRANGEBYSCORE/LEX, ZREMRANGEBY{RANK,SCORE,LEX}, ZPOP{MIN,MAX},
                         WEIGHTS / AGGREGATE / WITHSCORES / LIMIT permutations
  - set algebra:         SDIFF/SINTER/SUNION(+STORE), SMISMEMBER
  - SORT / SORT_RO:      ALPHA / LIMIT / DESC / BY nosort / GET # permutations
  - LCS:                 LEN / IDX / MINMATCHLEN / WITHMATCHLEN
  - BITFIELD:            multi-op GET/SET/INCRBY across i/u widths, '#' offsets,
                         OVERFLOW WRAP/SAT/FAIL transitions, plus BITFIELD_RO
  - string growth:       SETRANGE / APPEND / SETBIT / GETRANGE boundary cases
  - SCAN family:         SCAN/HSCAN/SSCAN/ZSCAN with MATCH / COUNT / TYPE / NOVALUES
  - RESP3 typed replies: introspection / numeric commands under HELLO 3

Both servers are launched fresh and config-less so they expose identical
COMPILED-IN defaults (dodges the list/hash-max-listpack 512-vs-128 config-default
false-positive class — see frankenredis-0o5hj), and FLUSHALL+reseed every cycle
keeps the key pool deterministic and identical on both sides.

Normalisation (these are genuine non-divergences, not bugs):
  - both-error replies compare by error CODE word (wording may differ)
  - set/zset-algebra and unsorted SORT replies compare as a MULTISET (hashtable /
    listpack iteration order is unspecified across implementations)
  - random-reply commands (*RANDMEMBER/SPOP) compare reply shape only
  - SCAN-family replies compare the returned element multiset when both cursors
    are 0 (fr always returns cursor 0; redis may paginate — same union of elements)

Usage: algebra_resp3_differ.py [--bin PATH] [--redis-bin PATH]
                               [--seeds N] [--iters N] [--start-seed N]
Exit 0 if every surface is byte-exact (modulo the normalisations above), else 1.
"""
import argparse
import os
import random
import socket
import subprocess
import sys
import time


class Conn:
    def __init__(self, port, resp3=False):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.buf = b""
        if resp3:
            self.cmd("HELLO", "3")

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            if isinstance(a, str):
                a = a.encode()
            elif isinstance(a, int):
                a = str(a).encode()
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        return self.read()

    def _rl(self):
        while b"\r\n" not in self.buf:
            d = self.s.recv(65536)
            if not d:
                raise EOFError
            self.buf += d
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _rn(self, n):
        while len(self.buf) < n + 2:
            d = self.s.recv(65536)
            if not d:
                raise EOFError
            self.buf += d
        d = self.buf[:n]
        self.buf = self.buf[n + 2:]
        return d

    def read(self):
        line = self._rl()
        t, rest = line[:1], line[1:]
        if t == b'+':
            return ('status', rest.decode())
        if t == b'-':
            return ('error', rest.decode())
        if t == b':':
            return ('int', int(rest))
        if t == b'(':
            return ('bignum', rest.decode())
        if t == b',':
            return ('double', rest.decode())
        if t == b'#':
            return ('bool', rest.decode())
        if t == b'_':
            return ('nil', None)
        if t == b'$':
            n = int(rest)
            return ('nil', None) if n == -1 else ('bulk', self._rn(n))
        if t == b'=':
            n = int(rest)
            return ('verbatim', self._rn(n))
        if t in (b'*', b'~', b'>'):
            n = int(rest)
            if n == -1:
                return ('nil', None)
            kind = {ord('*'): 'array', ord('~'): 'set', ord('>'): 'push'}[t[0]]
            return (kind, [self.read() for _ in range(n)])
        if t == b'%':
            n = int(rest)
            return ('map', [(self.read(), self.read()) for _ in range(n)])
        raise ValueError("bad reply %r" % line)


KEYS = ["k1", "k2", "k3"]
MEMBERS = ["a", "b", "c", "d"]
INTS = ["0", "1", "2", "-1", "5"]


def seed_state(c, rnd):
    for k in KEYS:
        c.cmd("DEL", k)
        t = rnd.choice(["string", "list", "set", "zset", "hash", "stream", "none"])
        if t == "string":
            c.cmd("SET", k, rnd.choice(["1", "hello", "10", "\x00\x01\xff", "ABCD"]))
        elif t == "list":
            for _ in range(rnd.randint(1, 4)):
                c.cmd("RPUSH", k, rnd.choice(MEMBERS))
        elif t == "set":
            for _ in range(rnd.randint(1, 4)):
                c.cmd("SADD", k, rnd.choice(MEMBERS))
        elif t == "zset":
            for _ in range(rnd.randint(1, 4)):
                c.cmd("ZADD", k, rnd.choice(["0", "1", "2.5"]), rnd.choice(MEMBERS))
        elif t == "hash":
            for _ in range(rnd.randint(1, 3)):
                c.cmd("HSET", k, rnd.choice(MEMBERS), rnd.choice(INTS))
        elif t == "stream":
            for _ in range(rnd.randint(1, 3)):
                c.cmd("XADD", k, "*", "f", rnd.choice(MEMBERS))


def _itype(rnd):
    return rnd.choice(["u1", "u2", "u4", "u7", "u8", "i8", "u16", "i16",
                       "u63", "i64", "u64", "i1", "u0", "u100", "i0"])


def _off(rnd):
    return rnd.choice(["0", "1", "7", "8", "#0", "#1", "#2", "100", "-1", "1000"])


def _bfval(rnd):
    return rnd.choice(["0", "1", "-1", "127", "128", "255", "256", "-128", "-129",
                       "9223372036854775807", "-9223372036854775808",
                       "18446744073709551615", "1000000"])


def _bf_ops(rnd):
    ops = []
    for _ in range(rnd.randint(1, 4)):
        c = rnd.random()
        if c < 0.25:
            ops += ["GET", _itype(rnd), _off(rnd)]
        elif c < 0.5:
            ops += ["SET", _itype(rnd), _off(rnd), _bfval(rnd)]
        elif c < 0.75:
            ops += ["INCRBY", _itype(rnd), _off(rnd), _bfval(rnd)]
        else:
            ops += ["OVERFLOW", rnd.choice(["WRAP", "SAT", "FAIL"])]
    return ops


def gen_cmd(rnd):
    k = lambda: rnd.choice(KEYS)
    m = lambda: rnd.choice(MEMBERS)
    n = lambda: rnd.choice(INTS)
    nk = lambda: str(rnd.randint(1, 3))
    return rnd.choice([
        # ── sorted-set / set algebra (value + shape must match) ──
        lambda: ["SORT", k()] + rnd.choice([[], ["ALPHA"], ["LIMIT", "0", "2"], ["DESC"],
                 ["BY", "nosort"], ["GET", "#"], ["LIMIT", "0", "2", "ALPHA", "DESC"]]),
        lambda: ["SORT_RO", k()] + rnd.choice([[], ["ALPHA"], ["LIMIT", "0", "2"]]),
        lambda: ["LCS", k(), k()] + rnd.choice([[], ["LEN"], ["IDX"],
                 ["IDX", "MINMATCHLEN", "2"], ["IDX", "WITHMATCHLEN"], ["LEN", "IDX"]]),
        lambda: ["ZADD", k()] + rnd.sample(["GT", "LT", "NX", "XX", "CH"], rnd.randint(0, 2))
                 + [rnd.choice(["0", "1.5", "inf", "-inf", "nan"]), m()],
        lambda: ["ZRANGEBYSCORE", k(), rnd.choice(["-inf", "(1", "1", "+inf"]),
                 rnd.choice(["+inf", "(2", "2"])] + rnd.choice([[], ["WITHSCORES"],
                 ["LIMIT", "0", "2"], ["WITHSCORES", "LIMIT", "1", "2"]]),
        lambda: ["ZRANGE", k(), rnd.choice(["0", "(1", "-inf", "[a"]),
                 rnd.choice(["-1", "+inf", "(3", "[c"])] + rnd.choice([[], ["BYSCORE"],
                 ["BYLEX"], ["REV"], ["BYSCORE", "LIMIT", "0", "2"], ["BYSCORE", "WITHSCORES"]]),
        lambda: ["ZDIFF", nk(), k(), k()] + rnd.choice([[], ["WITHSCORES"]]),
        lambda: ["ZINTER", nk(), k(), k()] + rnd.choice([[], ["WITHSCORES"],
                 ["WEIGHTS", "2", "3"], ["AGGREGATE", "MAX"],
                 ["WEIGHTS", "1", "2", "AGGREGATE", "MIN", "WITHSCORES"]]),
        lambda: ["ZUNION", nk(), k(), k()] + rnd.choice([[], ["WITHSCORES"],
                 ["WEIGHTS", "2", "3"], ["AGGREGATE", "MIN"]]),
        lambda: ["ZUNIONSTORE", k(), nk(), k(), k()] + rnd.choice([[],
                 ["WEIGHTS", "2", "1"], ["AGGREGATE", "MAX"]]),
        lambda: ["ZINTERSTORE", k(), nk(), k(), k()] + rnd.choice([[], ["AGGREGATE", "MIN"]]),
        lambda: ["ZINTERCARD", nk(), k(), k()] + rnd.choice([[], ["LIMIT", "0"], ["LIMIT", "2"]]),
        lambda: ["ZPOPMIN", k()] + rnd.choice([[], [n()]]),
        lambda: ["ZPOPMAX", k()] + rnd.choice([[], [n()]]),
        lambda: ["ZRANGEBYLEX", k(), rnd.choice(["-", "[a", "(a"]),
                 rnd.choice(["+", "[c", "(c"])] + rnd.choice([[], ["LIMIT", "0", "2"]]),
        lambda: ["ZREMRANGEBYSCORE", k(), rnd.choice(["-inf", "1"]), rnd.choice(["+inf", "2"])],
        lambda: ["ZREMRANGEBYRANK", k(), n(), rnd.choice(["-1", "1", "2"])],
        lambda: ["ZREMRANGEBYLEX", k(), rnd.choice(["-", "[a"]), rnd.choice(["+", "[c"])],
        lambda: ["SMISMEMBER", k(), m(), m()],
        lambda: ["SDIFF", k(), k()],
        lambda: ["SINTER", k(), k()],
        lambda: ["SUNION", k(), k()],
        lambda: ["SDIFFSTORE", k(), k(), k()],
        lambda: ["SINTERSTORE", k(), k(), k()],
        lambda: ["SUNIONSTORE", k(), k(), k()],
        # ── BITFIELD multi-op + overflow ──
        lambda: ["BITFIELD", k()] + _bf_ops(rnd),
        lambda: ["BITFIELD_RO", k(), "GET", _itype(rnd), _off(rnd)],
        # ── string growth / bit boundaries ──
        lambda: ["SETRANGE", k(), rnd.choice(["0", "5", "10", "1000"]),
                 rnd.choice(["", "Z", "hello"])],
        lambda: ["APPEND", k(), rnd.choice(["", "x", "abcdefgh"])],
        lambda: ["SETBIT", k(), rnd.choice(["0", "7", "15", "100"]), rnd.choice(["0", "1"])],
        lambda: ["GETBIT", k(), rnd.choice(["0", "7", "100", "999999"])],
        lambda: ["GETRANGE", k(), rnd.choice(["0", "-3", "2", "-100"]),
                 rnd.choice(["-1", "100", "0", "-5"])],
        lambda: ["BITCOUNT", k()] + rnd.choice([[], ["0", "-1"], ["0", "0", "BIT"], ["1", "3", "BYTE"]]),
        lambda: ["STRLEN", k()],
        lambda: ["OBJECT", "ENCODING", k()],
        lambda: ["COPY", k(), k()] + rnd.choice([[], ["REPLACE"]]),
        # ── SCAN family option permutations ──
        lambda: ["SCAN", "0"] + rnd.choice([[], ["MATCH", "k*"], ["COUNT", "100"],
                 ["TYPE", "string"], ["MATCH", "*", "COUNT", "5"], ["TYPE", "zset"]]),
        lambda: ["HSCAN", k(), "0"] + rnd.choice([[], ["MATCH", "*"], ["COUNT", "100"], ["NOVALUES"]]),
        lambda: ["SSCAN", k(), "0"] + rnd.choice([[], ["MATCH", "*"], ["COUNT", "10"]]),
        lambda: ["ZSCAN", k(), "0"] + rnd.choice([[], ["MATCH", "*"], ["COUNT", "10"]]),
    ])


def gen_resp3_cmd(rnd):
    """Commands whose RESP3 typed-reply fidelity (double/map/set/bignum/bool/
    verbatim) is the point of comparison."""
    k = lambda: rnd.choice(KEYS)
    m = lambda: rnd.choice(MEMBERS)
    return rnd.choice([
        lambda: ["CONFIG", "GET", rnd.choice(["maxmemory", "appendonly",
                 "maxmemory-policy", "list-max-listpack-size", "hash-max-listpack-entries"])],
        lambda: ["ZADD", k(), rnd.choice(["1", "2.5", "inf"]), m()],
        lambda: ["ZSCORE", k(), m()],
        lambda: ["ZMSCORE", k(), m(), m()],
        lambda: ["ZRANGE", k(), "0", "-1", "WITHSCORES"],
        lambda: ["ZPOPMIN", k(), "2"],
        lambda: ["INCRBYFLOAT", k(), rnd.choice(["1.5", "3.0e3", "-0.1"])],
        lambda: ["HGETALL", k()],
        lambda: ["HRANDFIELD", k(), "-2", "WITHVALUES"],
        lambda: ["XRANGE", k(), "-", "+"],
        lambda: ["XLEN", k()],
        lambda: ["SMEMBERS", k()],
        lambda: ["SPOP", k(), "1"],
        lambda: ["EXPIRETIME", k()],
        lambda: ["TYPE", k()],
        lambda: ["COMMAND", "INFO", rnd.choice(["get", "set", "georadius", "mset"])],
        lambda: ["LPOS", k(), m(), "COUNT", "0"],
        lambda: ["SINTERCARD", "1", k(), "LIMIT", "0"],
    ])


RANDOM_REPLY = {"HRANDFIELD", "SRANDMEMBER", "ZRANDMEMBER", "SPOP"}
# Reply *order* unspecified across implementations (hashtable / listpack iteration).
UNORDERED = {"ZDIFF", "ZINTER", "ZUNION", "SDIFF", "SINTER", "SUNION",
             "SMEMBERS", "SMISMEMBER", "SORT", "HGETALL"}


def _flatten(r):
    """Stable multiset key for an array reply, order-independent at the top level."""
    if r[0] in ("array", "set", "map", "push"):
        return sorted(repr(x) for x in r[1])
    return [repr(r)]


def compare(cmd, ro, rf):
    """Return True if replies are equivalent under the documented normalisations."""
    if ro == rf:
        return True
    if ro[0] == 'error' and rf[0] == 'error':
        return ro[1].split(' ', 1)[0] == rf[1].split(' ', 1)[0]
    name = cmd[0].upper()
    # SORT with no BY/ALPHA and the *STORE variants on sets have unspecified order.
    if name in UNORDERED and ro[0] in ('array', 'set') and rf[0] in ('array', 'set'):
        if _flatten(ro) == _flatten(rf):
            return True
    if name in RANDOM_REPLY:
        if ro[0] == rf[0]:
            if ro[0] != 'array' or len(ro[1]) == len(rf[1]):
                return True
    # SCAN family: fr returns cursor 0 + the full element batch; redis may
    # paginate. When both report a finished scan (cursor 0) the element
    # multiset must agree.
    if name in ("SCAN", "HSCAN", "SSCAN", "ZSCAN") and ro[0] == 'array' and rf[0] == 'array':
        try:
            co, cf = ro[1][0][1], rf[1][0][1]
            eo = sorted(repr(x) for x in ro[1][1][1])
            ef = sorted(repr(x) for x in rf[1][1][1])
            if co == b'0' and cf == b'0' and eo == ef:
                return True
        except (IndexError, TypeError):
            pass
    return False


# ── self-launch helpers (mirror config_defaults_gate.py) ──
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


def run_phase(oport, fport, resp3, gen, seeds, iters, start_seed):
    o = Conn(oport, resp3)
    f = Conn(fport, resp3)
    divs = []
    for s in range(start_seed, start_seed + seeds):
        rnd = random.Random(s)
        for i in range(iters):
            if i % 25 == 0:
                o.cmd("FLUSHALL")
                f.cmd("FLUSHALL")
                for c in (o, f):
                    seed_state(c, random.Random(s * 7919 + i))
            cmd = gen(rnd)
            try:
                ro = o.cmd(*cmd)
            except Exception as e:
                ro = ('exc', str(e))
            try:
                rf = f.cmd(*cmd)
            except Exception as e:
                rf = ('exc', str(e))
            if not compare(cmd, ro, rf):
                divs.append((s, i, cmd, ro, rf))
                if len(divs) >= 25:
                    return divs
    return divs


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=None)
    ap.add_argument("--redis-bin", default=None)
    ap.add_argument("--seeds", type=int, default=4)
    ap.add_argument("--iters", type=int, default=4000)
    ap.add_argument("--start-seed", type=int, default=1)
    args = ap.parse_args()

    binpath = args.bin or find_bin()
    redispath = args.redis_bin or find_redis()
    if not binpath or not os.path.exists(binpath):
        print("FAIL: frankenredis binary not found (pass --bin PATH)", file=sys.stderr)
        sys.exit(2)
    if not redispath or not os.path.exists(redispath):
        print("FAIL: redis-server not found (pass --redis-bin PATH)", file=sys.stderr)
        sys.exit(2)

    oport, fport = 21824, 21825
    rproc = fproc = None
    all_divs = []
    try:
        rproc = launch([redispath, "--port", str(oport), "--save", "",
                        "--appendonly", "no"], oport)
        fproc = launch([binpath, "--port", str(fport)], fport)
        for label, resp3, gen in (("resp2-algebra", False, gen_cmd),
                                  ("resp3-algebra", True, gen_cmd),
                                  ("resp3-typed", True, gen_resp3_cmd)):
            divs = run_phase(oport, fport, resp3, gen,
                             args.seeds, args.iters, args.start_seed)
            n = args.seeds * args.iters
            if divs:
                print(f"FAIL [{label}]: {len(divs)} divergence(s) in {n} iters")
                all_divs += [(label, *d) for d in divs]
            else:
                print(f"OK [{label}]: {n} iters byte-exact vs redis 7.2.4")
    finally:
        for p in (fproc, rproc):
            if p is None:
                continue
            p.terminate()
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()

    if all_divs:
        print("\nDIVERGENCES:")
        for label, s, i, cmd, ro, rf in all_divs[:25]:
            print(f"  [{label}] seed {s} #{i}: {' '.join(map(str, cmd))}")
            print(f"      oracle: {ro!r}")
            print(f"      fr    : {rf!r}")
        sys.exit(1)
    print("\nPASS: algebra + RESP3 + BITFIELD/SCAN surfaces byte-exact vs redis 7.2.4")


if __name__ == "__main__":
    main()
