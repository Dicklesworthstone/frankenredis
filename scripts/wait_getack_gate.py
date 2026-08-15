#!/usr/bin/env python3
"""wait_getack_gate.py — acceptance gate for the WAIT/GETACK durability bug (97shd).

Redis's WAIT blocks up to `timeout` and sends REPLCONF GETACK to replicas to solicit an
immediate ack, so `SET k v; WAIT 1 <short>` returns :1 within ~1 RTT. fr's WAIT blocks +
refreshes ack snapshots but NEVER sends GETACK, so it resolves only on the replica's 1Hz
periodic ACK: `WAIT 1 200` undercounts to :0 and successful WAITs are up to ~1s slow.

This gate stands up a real master+replica pair for BOTH redis (control) and fr (subject),
and asserts:
  (1) WAIT-latency: `SET; WAIT 1 <timeout>` with a live synced replica returns :1.
      redis is the live control (must PASS). fr is GUARDED as of 2026-08-15: the 97shd
      fix landed (solicit_replica_ack injects REPLCONF GETACK on the replication-only
      path, and a blocked WAIT is pinned to its pre-GETACK write offset), and fr now
      answers :1 in the same 0ms as redis. Regressing either half fails the gate.
  (2) AOF-pollution guard (the fix's key trap): with a fr master running --aof, after a
      WAIT the on-disk AOF must contain NO 'GETACK' token. The 97shd fix injects GETACK
      into the replication stream; because fr unifies the AOF buffer + repl backlog
      (capture_aof_record), a naive fix would write GETACK into the AOF and corrupt it on
      reload. This guard (GUARDED — fails the gate) catches that regression. It passes
      today (no GETACK is emitted at all yet) and MUST keep passing after the fix.

Usage: wait_getack_gate.py --fr-bin <frankenredis> --redis-bin <redis-server>
"""
import argparse, socket, subprocess, tempfile, time, os, sys, shutil

# Flipped True 2026-08-15 on cited evidence: fr master+replica, SET; WAIT 1 300 -> :1 in
# 0ms against the redis 7.2.4 control's :1 in 0ms, AOF clean of GETACK. Both halves of the
# 97shd fix are load-bearing and neither has a unit test that can see the live path, so
# this gate is what stops them regressing: drop the GETACK injection and WAIT goes back to
# resolving on the replica's ~1Hz periodic ack, and move the required_offset capture to
# AFTER solicit_replica_ack and WAIT undercounts to :0 again on the GETACK-inflated offset.
WAIT_LATENCY_GUARDED = True

# How many times to replay the short-timeout WAIT. The pre-fix failure resolved on the
# replica's ~1Hz periodic ack, so a lone sample can land right after one and look fixed.
SHORT_WAIT_REPS = 5

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

def assert_is_ours(port, proc, role):
    """Fail loudly if `port` is served by anything other than the process we just
    spawned. This gate uses fixed ports on a box that runs a dozen agents; if one
    is already held, spawn() dies into DEVNULL and every command below silently
    lands on the OTHER process's server, which reports whatever IT does. That is
    how a fixed-port harness fakes both passes and failures
    (project_e2e_port_bands_and_shared_cwd_flake). Identity, not liveness."""
    if proc.poll() is not None:
        raise RuntimeError(f"{role} on port {port} exited immediately (rc={proc.returncode}) "
                           f"— port already in use?")
    info = Cli(port).cmd("INFO", "server")
    pid = None
    for line in (info or "").splitlines():
        if line.startswith("process_id:"):
            pid = int(line.split(":", 1)[1])
    if pid != proc.pid:
        raise RuntimeError(f"port {port} is NOT our {role}: process_id={pid}, spawned pid={proc.pid}"
                           f" — another server holds the port, the measurement would be fiction")

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
        assert_is_ours(port_m, procs[0], "master")
        assert_is_ours(port_r, procs[1], "replica")
        Cli(port_r).cmd("REPLICAOF", "127.0.0.1", str(port_m))
        if not wait_link(port_r):
            return None, "replica link never came up"
        m = Cli(port_m)
        m.cmd("SET", "durability_k", "v")
        t0 = time.time(); r = m.cmd("WAIT", "1", "300"); dt = (time.time()-t0)*1000
        # (97shd) The bead's sharpest recorded symptom was not the 300ms case but
        # `WAIT 1 200` UNDERCOUNTING to :0 where redis answers :1. Repeat it: the
        # pre-fix failure mode was "resolves on the replica's ~1Hz periodic ack", so
        # a single sample can land just after a periodic ack and look healthy. One
        # bad sample out of SHORT_WAIT_REPS is a failure, not noise.
        shorts = []
        for i in range(SHORT_WAIT_REPS):
            m.cmd("SET", f"durability_short_{i}", "v")
            t0 = time.time(); sr = m.cmd("WAIT", "1", "200"); sdt = (time.time()-t0)*1000
            shorts.append((sr, sdt))
            time.sleep(0.1)
        # Third recorded symptom: idle replica lag ballooned on fr (slave0 lag=116)
        # against redis's ~1. Reported as a differential, never gated on its own — an
        # absolute lag threshold is exactly the kind of timing assertion that flakes.
        time.sleep(1.5)
        lag = None
        for line in (Cli(port_m).cmd("INFO", "replication") or "").splitlines():
            if line.startswith("slave0:"):
                for field in line.split(","):
                    if field.startswith("lag="):
                        lag = int(field.split("=")[1])
        return (r, dt, shorts, lag), None
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
    rres, rdt, rshorts, rlag = rr
    assert rres == ":1", f"redis control WAIT should be :1, got {rres!r} ({rdt:.0f}ms)"
    r_under = sum(1 for s, _ in rshorts if s != ":1")
    assert r_under == 0, f"redis control undercounted WAIT 1 200 in {r_under}/{len(rshorts)} reps"
    print(f"CONTROL redis: SET;WAIT 1 300 -> {rres} in {rdt:.0f}ms; "
          f"WAIT 1 200 -> :1 in {len(rshorts)}/{len(rshorts)} reps; idle slave0 lag={rlag}")

    aofdir = tempfile.mkdtemp(prefix="wait_gate_aof_")
    try:
        (fr, ferr) = measure_wait(args.fr_bin, 31002, 31004, is_fr=True, aofdir=aofdir)
        if ferr: print(f"SETUP-FAIL fr: {ferr}"); sys.exit(2)
        fres, fdt, fshorts, flag = fr
        # (1b) short-timeout undercount — the bead's headline symptom.
        f_under = sum(1 for s, _ in fshorts if s != ":1")
        if f_under == 0:
            print(f"PASS  wait-undercount: fr WAIT 1 200 -> :1 in {len(fshorts)}/{len(fshorts)} reps "
                  f"(guarded={WAIT_LATENCY_GUARDED})")
        elif WAIT_LATENCY_GUARDED:
            gate_failed = True
            print(f"FAIL  wait-undercount REGRESSION: fr WAIT 1 200 answered "
                  f"{[s for s, _ in fshorts]} — redis answers :1 every time")
        else:
            print(f"XFAIL wait-undercount (97shd): fr WAIT 1 200 -> {[s for s, _ in fshorts]}")
        # (1c) idle replica lag, reported as a differential and never gated: an
        # absolute lag threshold is a timing assertion and would flake.
        print(f"INFO  idle slave0 lag: fr={flag} redis={rlag} "
              f"(97shd recorded fr=116 against redis ~1 before the fix)")
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
    guard = "guarded" if WAIT_LATENCY_GUARDED else "xfail=97shd until fix"
    print(f"WAIT-GETACK GATE: OK (wait-latency {guard}; aof-pollution guarded)")

if __name__ == "__main__":
    main()
