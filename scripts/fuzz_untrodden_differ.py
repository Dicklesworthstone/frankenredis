#!/usr/bin/env python3
"""Seeded randomized differential fuzzer: fr (strict) vs vendored redis 7.2.4.

Targets less-trodden command surface with a shared small key pool, wrong-type
injection, and option permutation. Finds validation / type-check-ORDER bugs that
hand-probes and frozen fixtures miss. Normalizes known nondeterministic classes.

Usage: fuzz_untrodden_differ.py <oracle_port> <fr_port> [--seed N] [--iters N]
"""
import socket, sys, random, time

class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.buf = b""
    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            if isinstance(a, str): a = a.encode()
            elif isinstance(a, int): a = str(a).encode()
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        return self.read()
    def _readline(self):
        while b"\r\n" not in self.buf:
            d = self.s.recv(65536)
            if not d: raise EOFError
            self.buf += d
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line
    def _readn(self, n):
        while len(self.buf) < n + 2:
            d = self.s.recv(65536)
            if not d: raise EOFError
            self.buf += d
        data = self.buf[:n]; self.buf = self.buf[n+2:]
        return data
    def read(self):
        line = self._readline()
        t, rest = line[:1], line[1:]
        if t == b'+': return ('status', rest.decode())
        if t == b'-': return ('error', rest.decode())
        if t == b':': return ('int', int(rest))
        if t == b'$':
            n = int(rest)
            if n == -1: return ('nil', None)
            return ('bulk', self._readn(n))
        if t == b'*':
            n = int(rest)
            if n == -1: return ('nil', None)
            return ('array', [self.read() for _ in range(n)])
        raise ValueError("bad type %r" % line)

def norm(r):
    # normalize error to just the code word (first token) for class compare,
    # but keep full message too for reporting
    return r

def err_code(r):
    if r[0] == 'error':
        return r[1].split(' ', 1)[0]
    return None

KEYS = ["k1", "k2", "k3", "k4"]
SMALL_INTS = ["0","1","2","3","-1","10","100"]
MEMBERS = ["a","b","c","d","e","f"]

def seed_state(c, rnd):
    # randomly populate the key pool with varied types so wrong-type paths fire
    for k in KEYS:
        c.cmd("DEL", k)
        t = rnd.choice(["string","list","set","zset","hash","none","intset"])
        if t == "string": c.cmd("SET", k, rnd.choice(SMALL_INTS+["hello","xyz"]))
        elif t == "list":
            for _ in range(rnd.randint(1,5)): c.cmd("RPUSH", k, rnd.choice(MEMBERS))
        elif t == "set":
            for _ in range(rnd.randint(1,5)): c.cmd("SADD", k, rnd.choice(MEMBERS))
        elif t == "intset":
            for _ in range(rnd.randint(1,5)): c.cmd("SADD", k, rnd.choice(SMALL_INTS))
        elif t == "zset":
            # equal scores so ZRANGEBYLEX is well-defined (mixed-score lex is unspecified)
            for _ in range(rnd.randint(1,5)): c.cmd("ZADD", k, "0", rnd.choice(MEMBERS))
        elif t == "hash":
            for _ in range(rnd.randint(1,4)): c.cmd("HSET", k, rnd.choice(MEMBERS), rnd.choice(SMALL_INTS))

def gen_cmd(rnd):
    k = lambda: rnd.choice(KEYS)
    m = lambda: rnd.choice(MEMBERS)
    n = lambda: rnd.choice(SMALL_INTS)
    g = rnd.choice([
        lambda: ["LMPOP", str(rnd.randint(1,3))] + [k() for _ in range(rnd.randint(1,3))] + [rnd.choice(["LEFT","RIGHT"])] + (["COUNT", n()] if rnd.random()<0.5 else []),
        lambda: ["ZMPOP", str(rnd.randint(1,3))] + [k() for _ in range(rnd.randint(1,3))] + [rnd.choice(["MIN","MAX"])] + (["COUNT", n()] if rnd.random()<0.5 else []),
        lambda: ["SINTERCARD", str(rnd.randint(1,3))] + [k() for _ in range(rnd.randint(1,3))] + (["LIMIT", n()] if rnd.random()<0.5 else []),
        lambda: ["ZADD", k()] + rnd.sample(["GT","LT","NX","XX","CH","INCR"], rnd.randint(0,3)) + [n(), m()],
        lambda: ["LPOS", k(), m()] + (["RANK", rnd.choice(["1","-1","2"])] if rnd.random()<0.5 else []) + (["COUNT", n()] if rnd.random()<0.5 else []) + (["MAXLEN", n()] if rnd.random()<0.3 else []),
        lambda: ["COPY", k(), k()] + (["REPLACE"] if rnd.random()<0.5 else []) + (["DB","0"] if rnd.random()<0.3 else []),
        lambda: ["BITCOUNT", k()] + ([n(), n()] + ([rnd.choice(["BIT","BYTE"])] if rnd.random()<0.5 else []) if rnd.random()<0.6 else []),
        lambda: ["BITPOS", k(), rnd.choice(["0","1"])] + ([n()] + ([n()] + ([rnd.choice(["BIT","BYTE"])] if rnd.random()<0.5 else []) if rnd.random()<0.5 else []) if rnd.random()<0.6 else []),
        lambda: ["ZRANGESTORE", k(), k(), n(), n()] + (rnd.sample(["BYSCORE","BYLEX","REV"], rnd.randint(0,1))) + (["LIMIT", n(), n()] if rnd.random()<0.3 else []),
        lambda: ["GETRANGE", k(), n(), rnd.choice(SMALL_INTS+["-1","-2"])],
        lambda: ["SETRANGE", k(), n(), rnd.choice(["","X","abc"])],
        lambda: ["GETEX", k()] + rnd.choice([[], ["PERSIST"], ["EX", n()], ["EXAT", "99999999999"], ["PX", "0"]]),
        lambda: ["OBJECT", "ENCODING", k()],
        lambda: ["SMOVE", k(), k(), m()],
        # NOTE: ZRANGEBYLEX deliberately excluded — this fuzzer issues ZADD with
        # varying scores, and ZRANGEBYLEX is unspecified on non-uniform-score
        # zsets (vendored walks skiplist score-order, fr walks lex-order). That
        # is the documented vgkly WONTFIX, not a real bug. zset_differ.py covers
        # ZRANGEBYLEX with the equal-score precondition it requires.
        lambda: ["ZADD", k(), "GT", "LT", n(), m()],  # mutually exclusive flags
        lambda: ["SRANDMEMBER", k(), rnd.choice(SMALL_INTS+["-3","-1"])],
        lambda: ["SETEX", k(), rnd.choice(["0","-1","10"]), m()],
        lambda: ["LINSERT", k(), rnd.choice(["BEFORE","AFTER"]), m(), m()],
        lambda: ["HRANDFIELD", k(), rnd.choice(SMALL_INTS+["-2"])] + (["WITHVALUES"] if rnd.random()<0.5 else []),
        lambda: ["EXPIRE", k(), n()] + rnd.choice([[], ["NX"],["XX"],["GT"],["LT"]]),
        lambda: ["BITFIELD", k(), "GET", rnd.choice(["u8","i8","u100","u0"]), n()],
        # ── string / bit / counter families (validation + type-check order) ──
        lambda: ["SETBIT", k(), rnd.choice(["0","7","100","-1"]), rnd.choice(["0","1","2"])],
        lambda: ["GETBIT", k(), rnd.choice(["0","7","100","-1"])],
        lambda: ["APPEND", k(), rnd.choice(["","x","abc"])],
        lambda: ["GETDEL", k()],
        lambda: ["GETSET", k(), m()],
        lambda: ["INCRBY", k(), rnd.choice(SMALL_INTS+["abc","9223372036854775807"])],
        lambda: ["DECRBY", k(), rnd.choice(SMALL_INTS+["abc"])],
        lambda: ["INCRBYFLOAT", k(), rnd.choice(["1.5","-2","nan","inf","abc"])],
        lambda: ["INCR", k()],
        lambda: ["STRLEN", k()],
        lambda: ["SETNX", k(), m()],
        lambda: ["PSETEX", k(), rnd.choice(["0","-1","1000"]), m()],
        # ── list move / mutate ──
        lambda: ["RPOPLPUSH", k(), k()],
        lambda: ["LMOVE", k(), k(), rnd.choice(["LEFT","RIGHT"]), rnd.choice(["LEFT","RIGHT"])],
        lambda: ["LREM", k(), rnd.choice(["0","1","-1","2"]), m()],
        lambda: ["LSET", k(), rnd.choice(["0","-1","5","100"]), m()],
        lambda: ["LTRIM", k(), n(), rnd.choice(SMALL_INTS+["-1"])],
        lambda: ["RPUSHX", k(), m()],
        lambda: ["LPUSHX", k(), m()],
        # ── hash / zset counter families ──
        lambda: ["HINCRBY", k(), m(), rnd.choice(SMALL_INTS+["abc"])],
        lambda: ["HINCRBYFLOAT", k(), m(), rnd.choice(["1.5","nan","abc"])],
        lambda: ["HSETNX", k(), m(), n()],
        lambda: ["ZINCRBY", k(), rnd.choice(["1","nan","inf"]), m()],
        lambda: ["ZADD", k(), "INCR", rnd.choice(["1","nan"]), m()],
        lambda: ["ZSCORE", k(), m()],
        lambda: ["ZMSCORE", k(), m(), m()],
        lambda: ["ZADD", k(), "NX", "INCR", "1", m()],  # NX+INCR may return nil
        # ── misc type-sensitive ──
        lambda: ["TYPE", k()],
        lambda: ["PERSIST", k()],
        lambda: ["PTTL", k()],
        lambda: ["OBJECT", "REFCOUNT", k()],
        lambda: ["SINTERCARD", "1", k(), "LIMIT", "0"],
        lambda: ["ZRANGEBYSCORE", k(), rnd.choice(["-inf","(1","1"]), rnd.choice(["+inf","(3","3"]), "LIMIT", n(), rnd.choice(["-1","1","2"])],
    ])
    return g()

def run(oport, fport, seed, iters):
    o = Conn(oport); f = Conn(fport)
    rnd = random.Random(seed)
    divs = []
    for i in range(iters):
        if i % 40 == 0:
            o.cmd("FLUSHALL"); f.cmd("FLUSHALL")
            s2 = random.Random(seed*7919 + i)
            # seed both identically
            for c in (o, f):
                cr = random.Random(seed*7919 + i)
                seed_state(c, cr)
        cmd = gen_cmd(rnd)
        try:
            ro = o.cmd(*cmd)
        except Exception as e:
            ro = ('exc', str(e))
        try:
            rf = f.cmd(*cmd)
        except Exception as e:
            rf = ('exc', str(e))
        # time-relative replies: jitter of a few ms between the two servers is
        # inherent (they execute microseconds apart), not a divergence. Accept
        # when both are ints within tolerance (or both the same sentinel).
        if cmd[0] in ("PTTL", "TTL", "PEXPIRETIME", "EXPIRETIME"):
            if ro[0] == 'int' and rf[0] == 'int':
                tol = 50 if cmd[0] in ("PTTL", "PEXPIRETIME") else 2
                if ro[1] == rf[1] or (ro[1] > 0 and rf[1] > 0 and abs(ro[1]-rf[1]) <= tol):
                    continue
            if ro == rf:
                continue
            divs.append((i, cmd, ro, rf)); continue
        # random-reply commands: compare only reply type + length, not values/order
        RANDOM_CMDS = {"HRANDFIELD", "SRANDMEMBER", "ZRANDMEMBER", "SPOP"}
        if cmd[0] in RANDOM_CMDS:
            def shape(r):
                if r[0] == 'array': return ('array', len(r[1]))
                return (r[0],)
            if shape(ro) != shape(rf):
                co, cf = err_code(ro), err_code(rf)
                if not (co and cf and co == cf):
                    divs.append((i, cmd, ro, rf))
            continue
        # compare: exact, but allow error-message wording to differ if both errors share code
        if ro != rf:
            co, cf = err_code(ro), err_code(rf)
            if co is not None and cf is not None and co == cf:
                continue  # same error class, wording may differ (acceptable)
            # both errors but different code => real divergence
            divs.append((i, cmd, ro, rf))
            if len(divs) >= 30:
                break
    return divs

def main():
    oport = int(sys.argv[1]); fport = int(sys.argv[2])
    seed = 1234; iters = 4000
    for a in sys.argv[3:]:
        if a.startswith("--seed"): seed = int(a.split("=")[1]) if "=" in a else None
    # support positional --seed N --iters N
    args = sys.argv[3:]
    j = 0
    while j < len(args):
        if args[j] == "--seed": seed = int(args[j+1]); j+=2
        elif args[j] == "--iters": iters = int(args[j+1]); j+=2
        else: j+=1
    total = []
    for s in range(seed, seed+5):
        divs = run(oport, fport, s, iters)
        for d in divs:
            total.append((s,)+d)
    if not total:
        print(f"PASS — no true divergences across seeds {seed}..{seed+4} x {iters} iters")
        return 0
    print(f"FOUND {len(total)} divergences:")
    seen = set()
    for s,i,cmd,ro,rf in total:
        key = (cmd[0], str(ro)[:40], str(rf)[:40])
        if key in seen: continue
        seen.add(key)
        print(f"\n[seed {s} iter {i}] CMD: {' '.join(map(str,cmd))}")
        print(f"  oracle: {ro}")
        print(f"  fr    : {rf}")
    return 1

if __name__ == "__main__":
    sys.exit(main())
