#!/usr/bin/env python3
"""Self-orchestrating AOF round-trip fidelity fuzzer: fr write -> restart -> reload.

aof_cross_compat_gate proves fr can LOAD a redis-7 multi-part appendonlydir. This
gate tests the other half of durability: that fr's OWN appendonlydir (base.rdb +
incr.aof + manifest) faithfully persists arbitrary state across a process restart.
It spins up fr with --aof + a redis oracle, streams a deterministic random
multi-DB write mix to both (reusing digest_state_fuzz's generator), periodically
BGREWRITEAOFs (exercising the base-rewrite + incr-replay boundary), then KILLS and
RESTARTS fr so it reloads purely from the AOF — asserting fr's whole-keyspace DEBUG
DIGEST is (a) UNCHANGED by the restart (round-trip lossless) and (b) still equal to
redis's. An AOF encode/replay regression — a dropped command, a wrong propagation
rewrite, a lost TTL/DB-placement — surfaces as a digest that shifts across the
restart or diverges from redis.

Usage: aof_roundtrip_digest_fuzz.py <redis-bin> <fr-bin> [base_port] [seeds] [rounds]
"""
import importlib.util, os, random, socket, subprocess, sys, time

REDIS_BIN = sys.argv[1] if len(sys.argv) > 1 else "legacy_redis_code/redis/src/redis-server"
FR_BIN    = sys.argv[2] if len(sys.argv) > 2 else "/tmp/fr_aof"
BASE      = int(sys.argv[3]) if len(sys.argv) > 3 else 29671
SEEDS     = int(sys.argv[4]) if len(sys.argv) > 4 else 2
ROUNDS    = int(sys.argv[5]) if len(sys.argv) > 5 else 4
PER = 150

_dsf = os.path.join(os.path.dirname(os.path.abspath(__file__)), "digest_state_fuzz.py")
_spec = importlib.util.spec_from_file_location("digest_state_fuzz", _dsf)
M = importlib.util.module_from_spec(_spec); _spec.loader.exec_module(M)

AOFDIR = "/data/tmp/fr_aofgate_%d" % os.getpid()
AOF = os.path.join(AOFDIR, "app.aof")

def ping(port):
    try:
        s=socket.create_connection(("127.0.0.1",port),timeout=1); s.sendall(b"*1\r\n$4\r\nPING\r\n")
        time.sleep(0.03); d=s.recv(100); s.close(); return b"PONG" in d
    except Exception: return False

def start_fr():
    p=subprocess.Popen([FR_BIN,"--port",str(BASE+1),"--aof",AOF,"--enable-debug-command","yes"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    t0=time.time()
    while time.time()-t0<12 and not ping(BASE+1): time.sleep(0.1)
    return p

def main():
    os.makedirs(AOFDIR, exist_ok=True)
    procs=[]
    try:
        procs.append(subprocess.Popen([REDIS_BIN,"--port",str(BASE),"--save","","--appendonly","no",
                                       "--enable-debug-command","yes"],
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        t0=time.time()
        while time.time()-t0<10 and not ping(BASE): time.sleep(0.1)
        fr_proc=start_fr(); procs.append(fr_proc)
        if not (ping(BASE) and ping(BASE+1)):
            print("FAIL: redis/fr did not start"); sys.exit(1)
        od=M.R(BASE); fr=M.R(BASE+1)
        for s in (od,fr):
            for db in range(3): s.cmd("select",str(db)); s.cmd("flushall")
            s.cmd("select","0")
        fr.cmd("bgrewriteaof"); time.sleep(0.5)
        for sd in range(SEEDS):
            rnd=random.Random(8800+sd)
            if sd>0:
                for s in (od,fr):
                    for db in range(3): s.cmd("select",str(db)); s.cmd("flushall")
                    s.cmd("select","0")
            for r in range(ROUNDS):
                for _ in range(PER):
                    cmd=M.gen(rnd)
                    if cmd[0]=="select": continue
                    ro=od.cmd(*cmd); rf=fr.cmd(*cmd)
                    if ro!=rf:
                        print(f"[seed {8800+sd}] REPLY DIVERGE {cmd}\n  oracle={ro!r}\n  fr={rf!r}"); sys.exit(1)
                if r % 2 == 1:
                    fr.cmd("bgrewriteaof"); time.sleep(0.6)
                d_before=fr.cmd("debug","digest"); d_redis=od.cmd("debug","digest")
                # kill + restart fr -> reload from AOF
                fr_proc.terminate()
                try: fr_proc.wait(timeout=5)
                except Exception: fr_proc.kill()
                time.sleep(0.3)
                fr_proc=start_fr(); procs[-1]=fr_proc; fr=M.R(BASE+1)
                d_after=fr.cmd("debug","digest")
                tag="(post-rewrite)" if r%2==1 else ""
                if d_after!=d_before:
                    print(f"[seed {8800+sd} round {r}{tag}] AOF restart changed fr digest:")
                    print(f"  before={d_before}\n  after ={d_after}"); sys.exit(1)
                if d_after!=d_redis:
                    print(f"[seed {8800+sd} round {r}{tag}] fr(after AOF restart) != redis:")
                    print(f"  fr   ={d_after}\n  redis={d_redis}"); sys.exit(1)
        print(f"OK: {SEEDS} seed(s) x {ROUNDS} restart cycles — fr AOF round-trip preserves DEBUG DIGEST + matches redis 7.2.4")
    finally:
        for p in procs:
            try: p.terminate()
            except Exception: pass
        time.sleep(0.3)
        for p in procs:
            try:
                if p.poll() is None: p.kill()
            except Exception: pass

if __name__=="__main__": main()
