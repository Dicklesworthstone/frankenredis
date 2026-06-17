#!/usr/bin/env python3
"""Self-orchestrating replication-fidelity fuzzer: fr master vs fr replica DEBUG DIGEST.

replication_cross_compat_gate proves a FIXED write batch propagates fr<->redis. This
gate is broader and adversarial: it spins up an fr master + fr replica, then streams
~thousands of randomized WRITES — deliberately including the commands whose effect is
non-deterministic or time-dependent and therefore MUST be propagation-REWRITTEN to
keep a replica consistent: SPOP (random members -> SREM of the chosen members),
auto-id XADD (master picks the id -> replica must get the SAME id), EXPIRE/SETEX/
GETEX (relative TTL -> absolute PEXPIREAT), INCRBYFLOAT/HINCRBYFLOAT (-> SET/HSET of
the result), plus COPY/MOVE/relocation across DBs. Every batch it WAITs for the
replica to ack, then asserts master DEBUG DIGEST == replica DEBUG DIGEST. A
propagation bug (verbatim-forwarding a non-deterministic command, dropping a DB id,
wrong rewrite) makes the replica's whole-keyspace digest diverge from the master's.

DEBUG DIGEST mixes only a has-expire marker (not the exact TTL), so master/replica
are comparable despite ack latency. Uses the digest_state_fuzz RESP client.

Usage: replication_digest_fuzz.py <redis-bin> <fr-bin> [base_port] [seeds] [writes]
       (redis-bin accepted for SELF_ORCH signature; only fr-bin is used.)
"""
import importlib.util, os, random, socket, subprocess, sys, time

REDIS_BIN = sys.argv[1] if len(sys.argv) > 1 else "legacy_redis_code/redis/src/redis-server"
FR_BIN    = sys.argv[2] if len(sys.argv) > 2 else "/tmp/fr_repl"
BASE      = int(sys.argv[3]) if len(sys.argv) > 3 else 29991
SEEDS     = int(sys.argv[4]) if len(sys.argv) > 4 else 4
WRITES    = int(sys.argv[5]) if len(sys.argv) > 5 else 600

_dsf = os.path.join(os.path.dirname(os.path.abspath(__file__)), "digest_state_fuzz.py")
_spec = importlib.util.spec_from_file_location("digest_state_fuzz", _dsf)
M = importlib.util.module_from_spec(_spec); _spec.loader.exec_module(M)

KEYS = [f"k{i}" for i in range(8)]; MEMB = ["m0","m1","m2","aa","bb","42"]; SV = ["a","bb","10","x"*20]
def gen(rnd):
    k=lambda: rnd.choice(KEYS); mb=lambda: rnd.choice(MEMB)
    return [str(x) for x in rnd.choice([
        lambda:["set",k(),rnd.choice(SV)], lambda:["set",k(),rnd.choice(SV),"EX",str(rnd.choice([100,500]))],
        lambda:["setex",k(),str(rnd.choice([100,500])),rnd.choice(SV)], lambda:["getex",k(),"EX","300"],
        lambda:["expire",k(),"400"], lambda:["pexpire",k(),"400000"], lambda:["persist",k()],
        lambda:["incr",k()], lambda:["incrbyfloat",k(),"1.5"], lambda:["append",k(),"z"],
        lambda:["sadd",k(),mb(),mb(),mb()], lambda:["spop",k()], lambda:["spop",k(),"2"],
        lambda:["srem",k(),mb()], lambda:["smove",k(),k(),mb()],
        lambda:["rpush",k(),mb(),mb()], lambda:["lpop",k()], lambda:["lmove",k(),k(),"LEFT","RIGHT"],
        lambda:["hset",k(),mb(),rnd.choice(SV)], lambda:["hincrbyfloat",k(),mb(),"1.5"], lambda:["hdel",k(),mb()],
        lambda:["zadd",k(),str(rnd.randint(-3,3)),mb()], lambda:["zpopmin",k()], lambda:["zincrby",k(),"1.5",mb()],
        lambda:["copy",k(),k(),"REPLACE"], lambda:["move",k(),str(rnd.randint(0,2))], lambda:["del",k()],
        lambda:["xadd",k(),"*","f",rnd.choice(SV)],
    ])()]

def ping(port):
    try:
        s=socket.create_connection(("127.0.0.1",port),timeout=1); s.sendall(b"*1\r\n$4\r\nPING\r\n")
        time.sleep(0.03); d=s.recv(200); s.close(); return b"PONG" in d
    except Exception: return False

def main():
    procs=[]
    try:
        procs.append(subprocess.Popen([FR_BIN,"--port",str(BASE),"--enable-debug-command","yes"],
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        procs.append(subprocess.Popen([FR_BIN,"--port",str(BASE+1),"--replicaof","127.0.0.1",str(BASE),
                                       "--enable-debug-command","yes"],
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        t0=time.time()
        while time.time()-t0<10 and not (ping(BASE) and ping(BASE+1)): time.sleep(0.1)
        if not (ping(BASE) and ping(BASE+1)):
            print("FAIL: fr master/replica did not start"); sys.exit(1)
        m=M.R(BASE); r=M.R(BASE+1)
        # wait for replication link up
        t0=time.time()
        while time.time()-t0<10 and "master_link_status:up" not in (r.cmd("info","replication") or ""):
            time.sleep(0.1)
        for sd in range(SEEDS):
            rnd=random.Random(6600+sd)
            for db in (0,1,2): m.cmd("select",str(db)); m.cmd("flushall")
            m.cmd("select","0")
            recent=[]
            for i in range(WRITES):
                cmd=gen(rnd)
                if cmd[0]=="select": continue
                m.cmd(*cmd); recent.append(cmd); recent=recent[-25:]
                if (i+1)%50==0:
                    m.cmd("wait","1","300")
                    dm=m.cmd("debug","digest"); dr=r.cmd("debug","digest")
                    if dm!=dr:
                        # allow a brief settle for any in-flight ack, then re-check
                        time.sleep(0.2); dm=m.cmd("debug","digest"); dr=r.cmd("debug","digest")
                    if dm!=dr:
                        print(f"[seed {6600+sd} cmd {i}] MASTER != REPLICA digest after {cmd}")
                        print(f"  master ={dm}\n  replica={dr}")
                        for c in recent[-20:]: print("   ", c)
                        sys.exit(1)
        print(f"OK: {SEEDS} seed(s) x {WRITES} writes — fr master/replica DEBUG DIGEST converge (propagation faithful incl SPOP/auto-id-XADD/TTL-rewrite/cross-DB)")
    finally:
        for p in procs:
            p.terminate()
        time.sleep(0.3)
        for p in procs:
            if p.poll() is None: p.kill()

if __name__=="__main__": main()
