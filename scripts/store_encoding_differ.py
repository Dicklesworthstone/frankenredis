#!/usr/bin/env python3
"""Differential probe of DESTINATION ENCODING for *STORE commands: fr vs redis 7.2.4.

The ZRANGESTORE BYSCORE/BYLEX skiplist bug (t8rma) was a destination-encoding
selection divergence. This probe sweeps the sibling store commands that create a
fresh collection destination, with controlled source encodings and result sizes,
checking OBJECT ENCODING parity + content byte-exactness.

Usage: store_encoding_differ.py <oracle_port> <fr_port>
"""
import socket, sys

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
        self.s.sendall(out); return self.read()
    def _readline(self):
        while b"\r\n" not in self.buf:
            d = self.s.recv(65536)
            if not d: raise EOFError
            self.buf += d
        line, self.buf = self.buf.split(b"\r\n", 1); return line
    def _readn(self, n):
        while len(self.buf) < n + 2:
            d = self.s.recv(65536)
            if not d: raise EOFError
            self.buf += d
        data = self.buf[:n]; self.buf = self.buf[n+2:]; return data
    def read(self):
        line = self._readline(); t, rest = line[:1], line[1:]
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

o = Conn(int(sys.argv[1])); f = Conn(int(sys.argv[2]))
fails = []; npass = 0

def enc(c, k): return c.cmd("OBJECT", "ENCODING", k)
def content_sorted(c, k, typ):
    # sorted content for deterministic compare (encoding doesn't change membership)
    if typ == "set":
        r = c.cmd("SMEMBERS", k)
    elif typ == "zset":
        r = c.cmd("ZRANGE", k, 0, -1, "WITHSCORES")
        return r  # already score-ordered
    elif typ == "list":
        r = c.cmd("LRANGE", k, 0, -1)
        return r
    if r[0] != 'array': return r
    return ('array', sorted((str(x) for x in r[1])))

def check(desc, dsttype, setup):
    global npass
    for c in (o, f):
        c.cmd("FLUSHALL")
        for cmd in setup:
            c.cmd(*cmd)
    eo, ef = enc(o, "dst"), enc(f, "dst")
    co = content_sorted(o, "dst", dsttype) if eo[0] != 'nil' else ('nil', None)
    cf = content_sorted(f, "dst", dsttype) if ef[0] != 'nil' else ('nil', None)
    ok = (eo == ef) and (co == cf)
    if ok:
        npass += 1
    else:
        fails.append((desc, eo, ef, co, cf, setup))

# helper builders
def ints(n, start=1): return [str(i) for i in range(start, start+n)]
def strs(n): return [f"s{i}" for i in range(n)]

# ---- SINTERSTORE / SUNIONSTORE / SDIFFSTORE ----
for op in ("SINTERSTORE", "SUNIONSTORE", "SDIFFSTORE"):
    # int sources (intset), small result
    check(f"{op} intset small", "set",
          [["SADD","a"]+ints(5), ["SADD","b"]+ints(3), [op,"dst","a","b"]])
    # string sources (listpack), small result
    check(f"{op} listpack small", "set",
          [["SADD","a"]+strs(5), ["SADD","b"]+strs(3), [op,"dst","a","b"]])
    # large int source -> result may exceed intset/listpack limits
    check(f"{op} int large(600)", "set",
          [["SADD","a"]+ints(600), ["SADD","b"]+ints(600), [op,"dst","a","b"]])
    # mixed: int + string sources
    check(f"{op} mixed", "set",
          [["SADD","a"]+ints(5)+strs(5), ["SADD","b"]+ints(5)+strs(5), [op,"dst","a","b"]])
    # source with a long member (>64) -> hashtable
    check(f"{op} longval", "set",
          [["SADD","a","x"*70,"y"], ["SADD","b","x"*70,"y"], [op,"dst","a","b"]])
    # listpack count over 128 small values
    check(f"{op} listpack 200 strs", "set",
          [["SADD","a"]+strs(200), ["SADD","b"]+strs(200), [op,"dst","a","b"]])

# ---- ZUNIONSTORE / ZINTERSTORE / ZDIFFSTORE ----
for op in ("ZUNIONSTORE", "ZINTERSTORE", "ZDIFFSTORE"):
    check(f"{op} small", "zset",
          [["ZADD","a","1","x","2","y","3","z"], ["ZADD","b","1","x","2","y"],
           [op,"dst","2","a","b"]])
    check(f"{op} large(200)", "zset",
          [["ZADD","a"]+sum([[str(i),f"m{i}"] for i in range(200)],[]),
           ["ZADD","b"]+sum([[str(i),f"m{i}"] for i in range(200)],[]),
           [op,"dst","2","a","b"]])
    check(f"{op} longmember", "zset",
          [["ZADD","a","1","x"*70], ["ZADD","b","1","x"*70],
           [op,"dst","2","a","b"]])

# ---- SORT ... STORE (creates a list) ----
check("SORT STORE small int", "list",
      [["RPUSH","a","3","1","2"], ["SORT","a","STORE","dst"]])
check("SORT STORE large(200)", "list",
      [["RPUSH","a"]+ints(200), ["SORT","a","STORE","dst"]])
check("SORT STORE alpha longval", "list",
      [["RPUSH","a","x"*70,"y"], ["SORT","a","ALPHA","STORE","dst"]])

# ---- GEORADIUS / GEOSEARCHSTORE (creates a zset) ----
check("GEOSEARCHSTORE small", "zset",
      [["GEOADD","a","13.361389","38.115556","P"],
       ["GEOADD","a","15.087269","37.502669","C"],
       ["GEOSEARCHSTORE","dst","a","FROMLONLAT","15","37","BYRADIUS","400","km","ASC"]])
check("GEORADIUS STORE small", "zset",
      [["GEOADD","a","13.361389","38.115556","P"],
       ["GEOADD","a","15.087269","37.502669","C"],
       ["GEORADIUS","a","15","37","400","km","STORE","dst"]])

# ---- COPY (should preserve source encoding exactly) ----
check("COPY intset", "set",
      [["SADD","a"]+ints(5), ["COPY","a","dst"]])
check("COPY listpack-set", "set",
      [["SADD","a"]+strs(5), ["COPY","a","dst"]])
check("COPY hashtable-set", "set",
      [["SADD","a"]+strs(200), ["COPY","a","dst"]])
check("COPY zset listpack", "zset",
      [["ZADD","a","1","x","2","y"], ["COPY","a","dst"]])
check("COPY zset skiplist", "zset",
      [["ZADD","a"]+sum([[str(i),f"m{i}"] for i in range(200)],[]), ["COPY","a","dst"]])

print(f"PASS={npass} FAIL={len(fails)}")
for desc, eo, ef, co, cf, setup in fails:
    print(f"\n[{desc}]")
    print(f"  enc oracle={eo[1] if eo[0] in ('bulk','status') else eo}  fr={ef[1] if ef[0] in ('bulk','status') else ef}")
    if co != cf:
        print(f"  CONTENT DIVERGES oracle={co}  fr={cf}")
    print(f"  setup={setup[-1]}")
sys.exit(1 if fails else 0)
