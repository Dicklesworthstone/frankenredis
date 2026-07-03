#!/usr/bin/env python3
"""wait_getack_gate.py — acceptance gate for the WAIT/GETACK durability bug (97shd).

Redis's WAIT blocks up to `timeout` and sends REPLCONF GETACK to replicas to solicit an
immediate ack, so `SET k v; WAIT 1 <short>` returns :1 within ~1 RTT. fr's WAIT blocks +
refreshes ack snapshots but NEVER sends GETACK, so it resolves only on the replica's 1Hz
periodic ACK: `WAIT 1 200` undercounts to :0 and successful WAITs are up to ~1s slow.

This gate stands up a real master+replica pair for BOTH redis (control) and fr (subject),
and asserts:
  (1) WAIT-latency: `SET; WAIT 1 <timeout>` with a live synced replica returns :1.
      redis is the live control (must PASS). fr is currently XFAIL (bug 97shd) — reported
      but not failing the gate until the fix lands.
  (2) AOF-pollution guard (the fix's key trap): with a fr master running --aof, after a
      WAIT the on-disk AOF must contain NO 'GETACK' token. The 97shd fix injects GETACK
      into the replication stream; because fr unifies the AOF buffer + repl backlog
      (capture_aof_record), a naive fix would write GETACK into the AOF and corrupt it on
      reload. This guard (GUARDED — fails the gate) catches that regression. It passes
      today (no GETACK is emitted at all yet) and MUST keep passing after the fix.

When the 97shd fix lands: set WAIT_LATENCY_GUARDED=True; the gate then proves the fix and
keeps guarding AOF-pollution.

Usage: wait_getack_gate.py --fr-bin <frankenredis> --redis-bin <redis-server>
"""
import argparse, socket, subprocess, tempfile, time, os, sys, shutil

WAIT_LATENCY_GUARDED = False  # flip True when 97shd fix lands

def enc(a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        if isinstance(x, str): x = x.encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    return o

class Cli:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port)); self.s.settimeout(10); self.b = b""
    def _line(self):
        while b"\r\n" not in self.b:
            d = self.s.recv(65536)
            if not d: raise ConnectionError("closed")
            self.b += d
        i = self.b.index(b"\r\n"); l = self.b[:i]; self.b = self.b[i+2:]; return l
    def reply(self):
        l = self._line(); t = l[:1]
        if t in (b"+", b"-", b":"): return l.decode()
        if t == b"$":
            n = int(l[1:])
            if n < 0: return None
            while len(self.b) < n+2: self.b += self.s.recv(65536)
            d = self.b[:n]; self.b = self.b[n+2:]; return d.decode()
        if t == b"*":
            n = int(l[1:]); return [self.reply() for _ in range(n)] if n >= 0 else None
        return l.decode()
    def cmd(self, *a): self.s.sendall(enc([str(x) for x in a])); return self.reply()

def wait_link(replica_port, timeout=12):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            c = Cli(replica_port)
            info = c.cmd("INFO", "replication")
            if "master_link_status:up" in info: return True
        except Exception: pass
        time.sleep(0.5)
    return False

def spawn(bin_path, port, extra):
    return subprocess.Popen([bin_path, "--port", str(port)] + extra,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

def measure_wait(bin_path, port_m, port_r, is_fr, aofdir=None):
    extra_dbg = ["--enable-debug-command", "yes"] if True else []
    m_extra = list(extra_dbg); r_extra = list(extra_dbg)
    if is_fr and aofdir:
        m_extra += ["--aof", os.path.join(aofdir, "m.aof")]
    else:
        m_extra += ["--appendonly", "no"] if not is_fr else []
        r_extra += ["--appendonly", "no"] if not is_fr else []
    if not is_fr:
        m_extra = ["--appendonly", ("yes" if aofdir else "no"), "--enable-debug-command", "yes"]
        if aofdir: m_extra += ["--dir", aofdir]
        r_extra = ["--appendonly", "no", "--enable-debug-command", "yes"]
    procs = [spawn(bin_path, port_m, m_extra), spawn(bin_path, port_r, r_extra)]
    try:
        time.sleep(1.2)
        Cli(port_r).cmd("REPLICAOF", "127.0.0.1", str(port_m))
        if not wait_link(port_r):
            return None, "replica link never came up"
        m = Cli(port_m)
        m.cmd("SET", "durability_k", "v")
        t0 = time.time(); r = m.cmd("WAIT", "1", "300"); dt = (time.time()-t0)*1000
        return (r, dt), None
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try: p.wait(timeout=3)
            except Exception: p.kill()

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fr-bin", required=True)
    ap.add_argument("--redis-bin", required=True)
    args = ap.parse_args()
    gate_failed = False

    # (1) WAIT latency — redis control then fr subject
    (rr, rerr) = measure_wait(args.redis_bin, 31001, 31003, is_fr=False)
    if rerr: print(f"SETUP-FAIL redis control: {rerr}"); sys.exit(2)
    rres, rdt = rr
    assert rres == ":1", f"redis control WAIT should be :1, got {rres!r} ({rdt:.0f}ms)"
    print(f"CONTROL redis: SET;WAIT 1 300 -> {rres} in {rdt:.0f}ms")

    aofdir = tempfile.mkdtemp(prefix="wait_gate_aof_")
    try:
        (fr, ferr) = measure_wait(args.fr_bin, 31002, 31004, is_fr=True, aofdir=aofdir)
        if ferr: print(f"SETUP-FAIL fr: {ferr}"); sys.exit(2)
        fres, fdt = fr
        ok_latency = (fres == ":1" and fdt < 250)
        if ok_latency:
            print(f"PASS  wait-latency: fr SET;WAIT 1 300 -> {fres} in {fdt:.0f}ms (guarded={WAIT_LATENCY_GUARDED})")
        elif WAIT_LATENCY_GUARDED:
            gate_failed = True
            print(f"FAIL  wait-latency REGRESSION: fr -> {fres} in {fdt:.0f}ms (expected :1 <250ms)")
        else:
            print(f"XFAIL wait-latency (97shd): fr -> {fres} in {fdt:.0f}ms (redis :1 in {rdt:.0f}ms) "
                  f"— WAIT blocks but never sends REPLCONF GETACK; resolves only on 1Hz periodic ack")
        # (2) AOF-pollution guard (GUARDED): the fr master AOF must contain no GETACK.
        getack_in_aof = False
        for root, _, files in os.walk(aofdir):
            for fn in files:
                try:
                    with open(os.path.join(root, fn), "rb") as fh:
                        if b"GETACK" in fh.read().upper():
                            getack_in_aof = True
                except Exception: pass
        if getack_in_aof:
            gate_failed = True
            print("FAIL  aof-pollution: REPLCONF GETACK found in the fr AOF (would replay on reload) "
                  "— the 97shd fix must inject GETACK into the repl stream WITHOUT writing it to the AOF")
        else:
            print("PASS  aof-pollution guard: no GETACK token in the fr AOF")
    finally:
        shutil.rmtree(aofdir, ignore_errors=True)

    print("=" * 60)
    if gate_failed:
        print("WAIT-GETACK GATE: FAIL"); sys.exit(1)
    print("WAIT-GETACK GATE: OK (wait-latency xfail=97shd until fix; aof-pollution guarded)")

if __name__ == "__main__":
    main()
