#!/usr/bin/env python3
"""Differential gate: WAIT / WAITAOF (frankenredis-hfzbi).

WAIT/WAITAOF are replication-sync commands, rarely exercised by ordinary tests but
with subtle deterministic edges on a standalone server (0 replicas):
  * WAIT n timeout: with no replicas returns 0 (immediately for timeout 0, after the
    timeout otherwise); non-integer / negative timeout -> error
  * WAITAOF numlocal numreplicas timeout: errors "WAITAOF cannot be used when
    numlocal is set but appendonly is disabled" when numlocal>=1 and AOF is off;
    returns [numlocal, numreplicas] otherwise
This gate pins those byte-exact vs redis 7.2.4. It toggles appendonly (restored
afterward, suite-safe) and uses short timeouts.

Usage: wait_waitaof_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=8)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.02)
    return s.recv(1 << 20)


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    for s in (od, fr):
        cmd(s, "CONFIG", "SET", "appendonly", "no")
        cmd(s, "FLUSHALL")
        cmd(s, "SET", "k", "v")

    # WAIT — standalone, 0 replicas
    chk("wait_0_0", "WAIT", "0", "0")
    chk("wait_0_100", "WAIT", "0", "100")
    chk("wait_1_50", "WAIT", "1", "50")
    chk("wait_notint_n", "WAIT", "x", "0")
    chk("wait_notint_t", "WAIT", "0", "x")
    chk("wait_neg_timeout", "WAIT", "0", "-1")
    chk("wait_arity", "WAIT", "0")
    # WAITAOF — appendonly OFF
    chk("waitaof_local_off_err", "WAITAOF", "1", "0", "50")
    chk("waitaof_0_0_0", "WAITAOF", "0", "0", "0")
    chk("waitaof_0_1_50", "WAITAOF", "0", "1", "50")
    chk("waitaof_notint", "WAITAOF", "x", "0", "0")
    chk("waitaof_neg_timeout", "WAITAOF", "0", "0", "-1")
    chk("waitaof_arity", "WAITAOF", "0", "0")
    # WAITAOF — appendonly ON
    for s in (od, fr):
        cmd(s, "CONFIG", "SET", "appendonly", "yes")
    time.sleep(0.3)
    chk("waitaof_local_on", "WAITAOF", "1", "0", "100")
    chk("waitaof_local2", "WAITAOF", "2", "0", "50")
    for s in (od, fr):
        cmd(s, "CONFIG", "SET", "appendonly", "no")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} WAIT/WAITAOF divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — WAIT/WAITAOF byte-exact vs redis 7.2.4 "
        "(0-replica determinism, timeout/arity errors, WAITAOF appendonly-off error + reply)"
    )


if __name__ == "__main__":
    main()
