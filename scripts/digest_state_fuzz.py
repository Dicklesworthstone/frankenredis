#!/usr/bin/env python3
"""Whole-keyspace DEBUG DIGEST convergence fuzzer vs vendored redis 7.2.4.

random_command_differ / fuzz_untrodden compare per-command REPLIES. That misses a
whole bug class: a mutation that returns the correct reply but leaves DIFFERENT
internal state (a corrupted secondary index, a stale ordered-key entry, a wrong
encoding, a dropped expire). This fuzzer issues an identical deterministic command
stream to both servers, compares every reply, AND every N commands compares the
full-database DEBUG DIGEST — an order-independent hash of every key+value+has-expire
across all DBs. A digest mismatch means the visible replies agreed but the actual
state diverged.

Design constraints that keep the digest deterministically comparable:
  * only STATE-DETERMINISTIC commands (no SPOP/SRANDMEMBER-mutate, no RANDOMKEY).
  * only LONG TTLs (>= 100000s) so nothing expires mid-run (redis mixes only a
    has-expire MARKER into the digest, not the exact time, so equal TTLs are fine;
    we just must avoid a key expiring on one server but not the other).
  * shared small key pool across a few types so collisions exercise WRONGTYPE
    paths (both servers must reject identically — a reply check catches drift).

On a digest mismatch the last `window` commands are printed for triage.

Usage: digest_state_fuzz.py <oracle_port> <fr_port> [seeds] [iters]
       default 6 seeds x 1500 cmds, digest every 25.
       Exit 0=parity, 1=divergence, 2=setup error.
       BOTH servers must be launched with --enable-debug-command yes (this fuzzer's
       only signal is DEBUG DIGEST equality); a preflight check catches the asymmetry.
"""
import socket, sys, random

def C(p): return socket.create_connection(("127.0.0.1", p), timeout=10)
class R:
    def __init__(s, p): s.s=C(p); s.buf=b""
    def _l(s):
        while b"\r\n" not in s.buf: s.buf+=s.s.recv(1<<20)
        l,s.buf=s.buf.split(b"\r\n",1); return l
    def _n(s,n):
        while len(s.buf)<n+2: s.buf+=s.s.recv(1<<20)
        d=s.buf[:n]; s.buf=s.buf[n+2:]; return d
    def read(s):
        l=s._l(); t=l[:1]
        if t in (b'+',b':',b'-'): return l.decode('latin1')
        if t==b'$': n=int(l[1:]); return None if n<0 else s._n(n).decode('latin1')
        if t==b'*': n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n)]
        return l.decode('latin1')
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else x
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()

KEYS = [f"k{i}" for i in range(8)]
SVAL = ["a", "bb", "10", "-7", "hello", "x"*30, "3.14"]
MEMB = ["m0","m1","m2","alpha","beta","42","99"]
LTTL = ["100000", "250000", "500000"]   # long enough to never expire mid-run
SID  = ["1-1","2-1","3-1","5-5","10-1","100-0"]   # explicit stream IDs (deterministic)

def gen(rnd):
    k=lambda: rnd.choice(KEYS)
    m=lambda: rnd.choice(MEMB)
    op=rnd.choice([
        # strings
        lambda:["set",k(),rnd.choice(SVAL)],
        lambda:["set",k(),rnd.choice(SVAL),"EX",rnd.choice(LTTL)],
        lambda:["set",k(),rnd.choice(SVAL),"KEEPTTL"],
        lambda:["append",k(),rnd.choice(SVAL)],
        lambda:["setrange",k(),str(rnd.randint(0,5)),rnd.choice(SVAL)],
        lambda:["incr",k()], lambda:["incrby",k(),str(rnd.randint(-5,5))],
        lambda:["decr",k()], lambda:["setnx",k(),rnd.choice(SVAL)],
        lambda:["getset",k(),rnd.choice(SVAL)], lambda:["getdel",k()],
        lambda:["setbit",k(),str(rnd.randint(0,40)),str(rnd.randint(0,1))],
        lambda:["bitfield",k(),"SET","u8",str(rnd.randint(0,3)*8),str(rnd.randint(0,255))],
        lambda:["incrbyfloat",k(),rnd.choice(["1.5","-0.25","3.0"])],
        lambda:["getex",k(),"PERSIST"], lambda:["getex",k(),"EX",rnd.choice(LTTL)],
        lambda:["bitop",rnd.choice(["AND","OR","XOR"]),k(),k(),k()],
        lambda:["bitop","NOT",k(),k()],
        # lists
        lambda:["rpush",k(),m(),m()], lambda:["lpush",k(),m()],
        lambda:["lpop",k()], lambda:["rpop",k()],
        lambda:["lset",k(),"0",m()], lambda:["linsert",k(),"BEFORE",m(),m()],
        lambda:["lrem",k(),"0",m()], lambda:["ltrim",k(),"0",str(rnd.randint(0,4))],
        lambda:["rpoplpush",k(),k()],
        lambda:["lmove",k(),k(),rnd.choice(["LEFT","RIGHT"]),rnd.choice(["LEFT","RIGHT"])],
        # sets
        lambda:["sadd",k(),m(),m()], lambda:["srem",k(),m()],
        lambda:["smove",k(),k(),m()],
        lambda:["sinterstore",k(),k(),k()], lambda:["sunionstore",k(),k(),k()],
        lambda:["sdiffstore",k(),k(),k()],
        # hashes
        lambda:["hset",k(),m(),rnd.choice(SVAL)], lambda:["hdel",k(),m()],
        lambda:["hincrby",k(),m(),str(rnd.randint(-3,3))], lambda:["hsetnx",k(),m(),rnd.choice(SVAL)],
        lambda:["hincrbyfloat",k(),m(),rnd.choice(["1.5","-0.25"])],
        # zsets
        lambda:["zadd",k(),str(rnd.randint(-5,5)),m()], lambda:["zrem",k(),m()],
        lambda:["zadd",k(),rnd.choice(["GT","LT","NX","XX"]),str(rnd.randint(-5,5)),m()],
        lambda:["zincrby",k(),str(rnd.randint(-3,3)),m()],
        lambda:["zpopmin",k()], lambda:["zpopmax",k()],
        lambda:["zremrangebyrank",k(),"0","0"], lambda:["zremrangebyscore",k(),"-1","1"],
        lambda:["zrangestore",k(),k(),"0","-1"],
        lambda:["zunionstore",k(),"2",k(),k()], lambda:["zdiffstore",k(),"2",k(),k()],
        # streams (explicit IDs -> deterministic; auto-id uses wall-clock, excluded)
        lambda:["xadd",k(),rnd.choice(SID),"f",rnd.choice(SVAL)],
        lambda:["xdel",k(),rnd.choice(SID)], lambda:["xsetid",k(),"500-0"],
        lambda:["xtrim",k(),"MAXLEN",str(rnd.randint(0,3))],
        # key-space / expiry / relocation (deterministic)
        lambda:["del",k()], lambda:["unlink",k()],
        lambda:["expire",k(),rnd.choice(LTTL)], lambda:["persist",k()],
        lambda:["pexpire",k(),"200000000"],
        lambda:["copy",k(),k(),"REPLACE"], lambda:["rename",k(),k()],
        lambda:["renamenx",k(),k()],
        lambda:["move",k(),str(rnd.randint(0,2))],
    ])
    return [str(x) for x in op()]

def digest(srv):
    return srv.cmd("debug","digest")

def run_seed(od, fr, seed, iters, window=25):
    rnd=random.Random(seed)
    for s in (od,fr):
        for db in range(3): s.cmd("select",str(db)); s.cmd("flushall")
        s.cmd("select","0")
    recent=[]
    for i in range(iters):
        cmd=gen(rnd)
        if cmd[0]=="select": continue
        ro=od.cmd(*cmd); rf=fr.cmd(*cmd)
        recent.append((cmd, ro, rf))
        if len(recent)>window: recent.pop(0)
        # reply check (deterministic commands -> replies must match exactly)
        if ro!=rf:
            print(f"[seed {seed} cmd {i}] REPLY DIVERGE {cmd}\n  oracle={ro!r}\n  fr={rf!r}")
            return 1
        if (i+1)%window==0:
            # re-sync current DB to 0 (move/copy may have left us anywhere? no, we never SELECT)
            do, df = digest(od), digest(fr)
            if do!=df:
                print(f"[seed {seed} cmd {i}] STATE DIGEST DIVERGE  oracle={do} fr={df}")
                print("  last commands (cmd / oracle-reply / fr-reply):")
                for c,a,b in recent: print(f"    {c}  ->  O:{a!r}  F:{b!r}")
                return 1
    # final digest
    do, df = digest(od), digest(fr)
    if do!=df:
        print(f"[seed {seed} FINAL] STATE DIGEST DIVERGE  oracle={do} fr={df}")
        return 1
    return 0

def main():
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2]))
    seeds=int(sys.argv[3]) if len(sys.argv)>3 else 6
    iters=int(sys.argv[4]) if len(sys.argv)>4 else 1500
    # Preflight: this fuzzer's entire signal is DEBUG DIGEST equality, so a
    # server that REJECTS DEBUG DIGEST (e.g. started without
    # --enable-debug-command) would surface as a phantom "STATE DIGEST DIVERGE"
    # against the other server's real digest. Detect that config asymmetry up
    # front and exit(2) with a clear setup message, distinct from the exit(1)
    # genuine-divergence path. A valid digest is a '+'-prefixed simple string.
    for label, idx, srv in (("oracle", 1, od), ("fr", 2, fr)):
        rep = digest(srv)
        if not (isinstance(rep, str) and rep.startswith("+")):
            print(f"SETUP ERROR: {label} (port {sys.argv[idx]}) DEBUG DIGEST unavailable: {rep!r}")
            print("  This fuzzer compares whole-keyspace DEBUG DIGEST, so BOTH servers must allow it.")
            print("  Launch each with --enable-debug-command yes (redis AND fr).")
            sys.exit(2)
    for sd in range(seeds):
        if run_seed(od, fr, 4000+sd, iters): print("\nFAIL: state digest divergence"); sys.exit(1)
    print(f"OK: {seeds} seed(s) x {iters} cmds — whole-keyspace DEBUG DIGEST converged at every checkpoint vs redis 7.2.4")

if __name__=="__main__": main()
