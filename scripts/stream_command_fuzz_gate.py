#!/usr/bin/env python3
"""stream_command_fuzz_gate.py — seeded randomized STREAM-command differential
parity gate vs redis 7.2.4.

Shared small key pool + wrong-type injection + malformed-ID/option permutation
across the whole stream family (XADD/XLEN/XRANGE/XREAD/XREADGROUP/XDEL/XTRIM/
XSETID/XGROUP/XACK/XCLAIM/XAUTOCLAIM/XPENDING/XINFO). Compares reply STRUCTURE +
error CLASS (first error word), masking auto-generated stream IDs/timestamps.

This is the harness that surfaced frankenredis-5r89s (XREADGROUP returned generic
ERR instead of WRONGTYPE/NOGROUP when the ID was also malformed — validation
order) and frankenredis-8t4vl (XTRIM MINID ~ approximate node-boundary trim was a
no-op). Approximate (`~`) MAXLEN/MINID trims are exercised: fr models whole-node
(stream-node-max-entries = 100) eviction to match streamTrim's count exactly.

Self-launches a clean fr + redis pair and sweeps several seeds.
Usage: [--bin FR] [--redis-bin REDIS] [--seeds N] [--iters N]
"""
import argparse, os, random, socket, subprocess, sys, tempfile, time


class C:
    def __init__(s, p):
        s.s = socket.create_connection(("127.0.0.1", p), 5); s.s.settimeout(5); s.b = b""
    def _l(s):
        while b"\r\n" not in s.b:
            d = s.s.recv(65536)
            if not d: raise EOFError
            s.b += d
        l, s.b = s.b.split(b"\r\n", 1); return l
    def rd(s):
        l = s._l(); t, r = l[:1], l[1:]
        if t == b'+': return ('S', r.decode('latin1'))
        if t == b'-': return ('E', r.decode('latin1').split()[0] if r else 'E')
        if t == b':': return ('I', int(r))
        if t == b'$':
            n = int(r)
            if n < 0: return ('N', None)
            while len(s.b) < n + 2: s.b += s.s.recv(65536)
            d = s.b[:n]; s.b = s.b[n + 2:]; return ('B', d.decode('latin1'))
        if t == b'*':
            n = int(r)
            return ('A', None) if n < 0 else ('A', [s.rd() for _ in range(n)])
        return ('?', l.decode('latin1'))
    def cmd(s, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x.encode() if isinstance(x, str) else (str(x).encode() if not isinstance(x, bytes) else x)
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        s.s.sendall(o)
        try: return s.rd()
        except Exception as e: return ('X', str(e))


import re
_ID = re.compile(r'\d+-\d+')


def norm(v):
    tag, val = v
    if tag == 'B' and val and _ID.fullmatch(val): return ('B', '<id>')
    if tag == 'S' and _ID.fullmatch(val or ''): return ('S', '<id>')
    if tag == 'A' and isinstance(val, list): return ('A', [norm(x) for x in val])
    return (tag, val)


KEYS = ["s1", "s2", "str1", "nostream"]
GROUPS = ["g1", "g2"]
CONS = ["c1", "c2"]
IDS = ["*", "0", "0-0", "1-1", "$", ">", "+", "-", "5-5", "999999999999999-0",
       "1-", "-1", "abc", "1-1-1", ""]


def rnd_fields(r):
    out = []
    for _ in range(r.randint(1, 3)):
        out += [r.choice(["f", "g", "h"]), r.choice(["1", "v", "x"])]
    return out


def gen(r):
    c = r.choice(["XADD", "XADD", "XLEN", "XRANGE", "XREVRANGE", "XREAD", "XDEL", "XTRIM",
                  "XSETID", "XGROUP", "XREADGROUP", "XACK", "XCLAIM", "XAUTOCLAIM",
                  "XPENDING", "XINFO", "XADD"])
    k = r.choice(KEYS)
    if c == "XADD":
        args = ["XADD", k]
        if r.random() < 0.3: args += ["NOMKSTREAM"]
        if r.random() < 0.3: args += ["MAXLEN", r.choice(["=", "~"]), str(r.randint(0, 5))]
        return args + [r.choice(IDS)] + rnd_fields(r)
    if c == "XLEN": return ["XLEN", k]
    if c in ("XRANGE", "XREVRANGE"):
        a = [c, k, r.choice(IDS), r.choice(IDS)]
        if r.random() < 0.4: a += ["COUNT", str(r.randint(0, 5))]
        return a
    if c == "XREAD":
        a = ["XREAD"]
        if r.random() < 0.5: a += ["COUNT", str(r.randint(0, 3))]
        return a + ["STREAMS", r.choice(KEYS), r.choice(IDS)]
    if c == "XDEL": return ["XDEL", k, r.choice(IDS)]
    if c == "XTRIM":
        return ["XTRIM", k, r.choice(["MAXLEN", "MINID"]), r.choice(["=", "~"]),
                r.choice([str(r.randint(0, 5)), r.choice(IDS)])]
    if c == "XSETID":
        a = ["XSETID", k, r.choice(IDS)]
        if r.random() < 0.4: a += ["ENTRIESADDED", str(r.randint(0, 9))]
        if r.random() < 0.4: a += ["MAXDELETEDID", r.choice(IDS)]
        return a
    if c == "XGROUP":
        sub = r.choice(["CREATE", "CREATECONSUMER", "DELCONSUMER", "DESTROY", "SETID"])
        a = ["XGROUP", sub, k, r.choice(GROUPS)]
        if sub == "CREATE":
            a += [r.choice(IDS)]
            if r.random() < 0.4: a += ["MKSTREAM"]
        elif sub in ("CREATECONSUMER", "DELCONSUMER"): a += [r.choice(CONS)]
        elif sub == "SETID": a += [r.choice(IDS)]
        return a
    if c == "XREADGROUP":
        return ["XREADGROUP", "GROUP", r.choice(GROUPS), r.choice(CONS), "COUNT",
                str(r.randint(1, 3)), "STREAMS", k, r.choice(IDS)]
    if c == "XACK": return ["XACK", k, r.choice(GROUPS), r.choice(IDS)]
    if c == "XCLAIM": return ["XCLAIM", k, r.choice(GROUPS), r.choice(CONS), str(r.randint(0, 100)), r.choice(IDS)]
    if c == "XAUTOCLAIM": return ["XAUTOCLAIM", k, r.choice(GROUPS), r.choice(CONS), "0", r.choice(IDS)]
    if c == "XPENDING":
        a = ["XPENDING", k, r.choice(GROUPS)]
        if r.random() < 0.5: a += [r.choice(IDS), r.choice(IDS), str(r.randint(1, 5))]
        return a
    if c == "XINFO":
        return ["XINFO", r.choice(["STREAM", "GROUPS", "CONSUMERS"]), k] + \
               ([r.choice(GROUPS)] if r.random() < 0.3 else [])
    return ["XLEN", k]


def sweep(fr, rd, seed, iters):
    diffs = []
    for it in range(iters):
        r = random.Random(seed * 100000 + it)
        if it % 40 == 0:
            for k in KEYS: fr.cmd("DEL", k); rd.cmd("DEL", k)
            fr.cmd("SET", "str1", "hello"); rd.cmd("SET", "str1", "hello")
        cmd = gen(r)
        a = norm(fr.cmd(*cmd)); b = norm(rd.cmd(*cmd))
        if a != b:
            diffs.append((cmd, a, b))
            if len(diffs) >= 10: break
    return diffs


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if C(port).cmd("PING") == ('S', 'PONG'): return True
        except Exception: time.sleep(0.2)
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.environ.get("FR_BIN",
                    "/data/tmp/cargo-target/release/frankenredis"))
    ap.add_argument("--redis-bin", default=os.environ.get("REDIS_BIN",
                    os.path.join(os.path.dirname(__file__), "..",
                                 "legacy_redis_code/redis/src/redis-server")))
    ap.add_argument("--seeds", type=int, default=8)
    ap.add_argument("--iters", type=int, default=700)
    args = ap.parse_args()
    fr_bin = os.path.abspath(args.bin); redis = os.path.abspath(args.redis_bin)
    if not os.path.exists(fr_bin):
        print(f"SKIP: fr binary not found at {fr_bin}"); return 0
    if not os.path.exists(redis):
        print(f"SKIP: redis-server not found at {redis}"); return 0

    rdir = tempfile.mkdtemp(prefix="fr_streamfuzz_")
    fp, rp = free_port(), free_port()
    procs = [
        subprocess.Popen([fr_bin, "--port", str(fp), "--enable-debug-command", "yes"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
        subprocess.Popen([redis, "--port", str(rp), "--dir", rdir, "--save", "",
                          "--appendonly", "no", "--enable-debug-command", "yes"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
    ]
    try:
        if not (wait_up(fp) and wait_up(rp)):
            print("FAIL: servers did not start"); return 1
        total_diffs = 0
        for seed in range(1, args.seeds + 1):
            fr = C(fp); rd = C(rp)
            diffs = sweep(fr, rd, seed, args.iters)
            for cmd, a, b in diffs:
                print(f"  [DIFF seed={seed}] {' '.join(map(str, cmd))}\n    fr={a}\n    rd={b}")
            total_diffs += len(diffs)
    finally:
        for p in procs: p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except Exception: p.kill()

    if total_diffs:
        print(f"FAIL — {total_diffs} stream-command divergence(s) vs redis 7.2.4")
        return 1
    print(f"PASS — stream-command differential parity vs redis 7.2.4 "
          f"({args.seeds} seeds x {args.iters} iters)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
