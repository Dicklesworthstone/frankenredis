#!/usr/bin/env python3
"""client_kill_differ.py — CLIENT KILL filter-matrix differential vs redis 7.2.4.

CLIENT KILL has a rich, multi-connection filter surface (ID / ADDR / LADDR /
TYPE / USER / SKIPME, the legacy `addr:port` form, and AND-combined filters)
that is easy to get subtly wrong. This gate stands up an identical connection
topology on fr and on vendored redis and compares, for each invocation, the
reply CLASS (kill count, +OK, or error class) and — where deterministic — the
actual kill effect (which specific connection died / survived).

To stay deterministic despite asynchronous TCP teardown, the bulk-count filters
(TYPE/USER/etc.) are NOT asserted on absolute counts; instead the gate asserts
ID/ADDR single-target kills (count + which socket closed), SKIPME self-handling,
the legacy-form and new-form replies, and error-class parity for malformed
invocations — all of which are independent of how many stale sockets the kernel
has not yet reaped.

Previously-known frankenredis-2n10q is fixed: CLIENT KILL LADDR must now match
the same uniform laddr reported by CLIENT INFO/LIST. The gate checks the LADDR
invariant (CLIENT LIST count on a laddr == CLIENT KILL LADDR ... SKIPME no kill
count) as a hard failure.

Usage: client_kill_differ.py [--oracle 16399] [--fr 16400]
Exit 0 if fr matches redis, else 1.
"""
import argparse
import socket
import sys
import time


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(2.0)
        self.b = b""

    def _line(self):
        while b"\r\n" not in self.b:
            d = self.s.recv(65536)
            if not d:
                raise EOFError
            self.b += d
        l, self.b = self.b.split(b"\r\n", 1)
        return l

    def _rn(self, n):
        while len(self.b) < n + 2:
            self.b += self.s.recv(65536)
        d, self.b = self.b[:n], self.b[n + 2:]
        return d

    def parse(self):
        l = self._line()
        t, r = l[:1], l[1:]
        if t == b"+":
            return r.decode()
        if t == b":":
            return int(r)
        if t == b"-":
            return "ERR:" + r.decode()
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()

    def alive(self):
        try:
            return self.cmd("PING") == "PONG"
        except Exception:
            return False

    def info_field(self, name):
        for tok in self.cmd("CLIENT", "INFO").split():
            if tok.startswith(name + "="):
                return tok[len(name) + 1:]
        return None

    def close(self):
        try:
            self.s.close()
        except Exception:
            pass


def err_class(x):
    # collapse an error to its leading code word so wording differences don't fail
    if isinstance(x, str) and x.startswith("ERR:"):
        parts = x.split()
        return "ERR:" + (parts[1] if len(parts) > 1 else "")
    return x


class Gate:
    def __init__(self, port):
        self.port = port

    def deterministic(self):
        """Return a dict of {case: reply-class} for the deterministic filters."""
        R = {}
        c = Conn(self.port)
        R["arity_noargs"] = err_class(c.cmd("CLIENT", "KILL"))
        R["id_nonexistent"] = c.cmd("CLIENT", "KILL", "ID", "999999")
        R["addr_bogus_count"] = c.cmd("CLIENT", "KILL", "ADDR", "1.2.3.4:5")
        R["oldform_bogus"] = err_class(c.cmd("CLIENT", "KILL", "1.2.3.4:5"))
        sid = c.cmd("CLIENT", "ID")
        R["id_self_skipme_yes"] = c.cmd("CLIENT", "KILL", "ID", sid, "SKIPME", "yes")
        R["id_noninteger"] = err_class(c.cmd("CLIENT", "KILL", "ID", "abc"))
        R["type_bogus"] = err_class(c.cmd("CLIENT", "KILL", "TYPE", "weird"))
        R["skipme_bogus"] = err_class(c.cmd("CLIENT", "KILL", "SKIPME", "maybe"))
        R["unknown_filter"] = err_class(c.cmd("CLIENT", "KILL", "BOGUS", "x"))
        R["maxage"] = err_class(c.cmd("CLIENT", "KILL", "MAXAGE", "0"))
        R["addr_noval"] = err_class(c.cmd("CLIENT", "KILL", "ADDR"))
        R["user_nonexistent"] = err_class(c.cmd("CLIENT", "KILL", "USER", "ghost"))
        R["type_master"] = c.cmd("CLIENT", "KILL", "TYPE", "master")
        R["type_slave_alias"] = c.cmd("CLIENT", "KILL", "TYPE", "slave")
        c.close()

        # single-target ID kill: count==1 AND that socket dies, sibling lives.
        ctrl = Conn(self.port)
        v0, v1 = Conn(self.port), Conn(self.port)
        vid = v0.cmd("CLIENT", "ID")
        R["id_victim_count"] = ctrl.cmd("CLIENT", "KILL", "ID", vid)
        time.sleep(0.2)
        R["id_victim_dead_sibling_alive"] = (not v0.alive()) and v1.alive()
        for x in (ctrl, v0, v1):
            x.close()

        # single-target ADDR kill: count==1 and that socket dies.
        ctrl = Conn(self.port)
        v0, v1 = Conn(self.port), Conn(self.port)
        va = v0.info_field("addr")
        R["addr_victim_count"] = ctrl.cmd("CLIENT", "KILL", "ADDR", va)
        time.sleep(0.2)
        R["addr_victim_dead_sibling_alive"] = (not v0.alive()) and v1.alive()
        for x in (ctrl, v0, v1):
            x.close()
        return R

    def laddr_invariant(self):
        """(list_count_on_laddr, kill_laddr_count, victims_dead) — the LADDR
        consistency check. Upstream: all three agree (every client shares the
        laddr in this single-listener setup)."""
        ctrl = Conn(self.port)
        laddr = ctrl.info_field("laddr")
        vs = [Conn(self.port) for _ in range(3)]
        time.sleep(0.15)
        listed = sum(1 for ln in ctrl.cmd("CLIENT", "LIST").split("\n")
                     if f"laddr={laddr}" in ln)
        killed = ctrl.cmd("CLIENT", "KILL", "LADDR", laddr, "SKIPME", "no")
        time.sleep(0.2)
        dead = sum(0 if v.alive() else 1 for v in vs)
        for v in vs:
            v.close()
        try:
            ctrl.close()
        except Exception:
            pass
        return (listed, killed, dead)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    args = ap.parse_args()
    rg, fg = Gate(args.oracle), Gate(args.fr)

    rd, fd = rg.deterministic(), fg.deterministic()
    failures = []
    for k in sorted(set(rd) | set(fd)):
        if rd.get(k) != fd.get(k):
            failures.append(f"{k}: redis={rd.get(k)!r} fr={fd.get(k)!r}")

    # LADDR consistency invariant
    rl, fl = rg.laddr_invariant(), fg.laddr_invariant()
    # redis must be self-consistent: list count == kill count == victims dead.
    if not (rl[0] == rl[1] and rl[1] - 0 >= rl[2] >= 0):
        failures.append(f"oracle LADDR self-inconsistent: {rl}")
    if fl != rl:
        failures.append(f"LADDR: redis(list,kill,dead)={rl} fr={fl}")

    if failures:
        print("FAIL: CLIENT KILL divergences:")
        for f in failures:
            print(f"  - {f}")
        sys.exit(1)
    print("OK: CLIENT KILL filter matrix matches redis 7.2.4 "
          "(ID/ADDR/SKIPME/legacy-form/error-class parity)")


if __name__ == "__main__":
    main()
