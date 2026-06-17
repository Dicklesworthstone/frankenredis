#!/usr/bin/env python3
"""Self-orchestrating gate: INFO rdb_changes_since_last_save resets on SAVE/BGSAVE,
vs vendored redis 7.2.4.

rdb_changes_since_last_save is the "dirty since last successful save" counter. fr
reported the monotonic total `dirty` directly, so it NEVER reset on save (6 -> 6 ->
7 across SAVE) while redis baselines it (5 -> 0 -> 1). This gate spins up fr (with an
--rdb path so SAVE/BGSAVE actually persist) + a redis oracle in a CLEAN dir, writes a
batch, SAVEs, and asserts the counter COLLAPSES to 0 on BOTH; a single write then
gives delta 1; BGSAVE collapses to 0 again. Absolute pre-save counts are NOT compared
(fr's empty-FLUSHALL dirty quirk) — the invariant is the SAVE reset + post-save delta.

Usage: rdb_changes_save_gate.py <redis-bin> <fr-bin> [base_port]
"""
import importlib.util, os, socket, subprocess, sys, time

REDIS_BIN = sys.argv[1] if len(sys.argv) > 1 else "legacy_redis_code/redis/src/redis-server"
FR_BIN    = sys.argv[2] if len(sys.argv) > 2 else "/tmp/fr_rdb"
BASE      = int(sys.argv[3]) if len(sys.argv) > 3 else 29761
RDIR = "/data/tmp/rdbchg_redis_%d" % os.getpid()
FRDB = "/data/tmp/rdbchg_fr_%d.rdb" % os.getpid()

_dsf = os.path.join(os.path.dirname(os.path.abspath(__file__)), "digest_state_fuzz.py")
_spec = importlib.util.spec_from_file_location("digest_state_fuzz", _dsf)
M = importlib.util.module_from_spec(_spec); _spec.loader.exec_module(M)

def ping(port):
    try:
        s=socket.create_connection(("127.0.0.1",port),timeout=1); s.sendall(b"*1\r\n$4\r\nPING\r\n")
        time.sleep(0.03); d=s.recv(100); s.close(); return b"PONG" in d
    except Exception: return False

def changes(cli):
    info = cli.cmd("info","persistence")
    for line in info.split("\r\n"):
        if line.startswith("rdb_changes_since_last_save:"): return int(line.split(":")[1])
    return -1

def main():
    os.makedirs(RDIR, exist_ok=True)
    procs=[]
    try:
        procs.append(subprocess.Popen([REDIS_BIN,"--port",str(BASE),"--save","","--appendonly","no",
                                       "--dir",RDIR,"--dbfilename","rt.rdb","--enable-debug-command","yes"],
                                      stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL))
        procs.append(subprocess.Popen([FR_BIN,"--port",str(BASE+1),"--rdb",FRDB,"--enable-debug-command","yes"],
                                      stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL))
        t0=time.time()
        while time.time()-t0<10 and not (ping(BASE) and ping(BASE+1)): time.sleep(0.1)
        if not (ping(BASE) and ping(BASE+1)):
            print("FAIL: redis/fr did not start"); sys.exit(1)
        R=M.R(BASE); F=M.R(BASE+1)
        div=0
        def check(label, cond, detail):
            nonlocal div
            if not cond: div+=1; print(f"DIVERGE {label}: {detail}")
        for c in (R,F): c.cmd("flushall")
        for c in (R,F):
            for i in range(20): c.cmd("set",f"k{i}",f"v{i}")
        cr, cf = changes(R), changes(F)
        check("grew", cr>0 and cf>0, f"pre-save changes redis={cr} fr={cf} (both should be >0)")
        for c in (R,F): c.cmd("save")
        cr, cf = changes(R), changes(F)
        check("save-reset", cr==0 and cf==0, f"post-SAVE changes redis={cr} fr={cf} (both must be 0)")
        for c in (R,F): c.cmd("set","one","more")
        cr, cf = changes(R), changes(F)
        check("post-save-delta", cr==1 and cf==1, f"after 1 write redis={cr} fr={cf} (both must be 1)")
        for c in (R,F): c.cmd("bgsave")
        time.sleep(0.4)
        cr, cf = changes(R), changes(F)
        check("bgsave-reset", cr==0 and cf==0, f"post-BGSAVE changes redis={cr} fr={cf} (both must be 0)")
        if div: print(f"\nFAIL: {div} divergence(s)"); sys.exit(1)
        print("OK: rdb_changes_since_last_save resets to 0 on SAVE+BGSAVE, delta tracks writes (fr matches redis 7.2.4)")
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
