#!/usr/bin/env python3
"""zset_memory_profile.py — measure fr vs redis 7.2.4 ACTUAL RSS for skiplist zsets.

This is a MEASUREMENT TOOL (not a pass/fail gate): RSS is allocator- and
host-dependent, so it prints the per-member data-attributable RSS for each side
and the ratio, for tracking the zset-compaction lever (frankenredis-zsetmem).

Why RSS and not used_memory: fr's `used_memory` is a faithful redis-MODEL
estimate (so MEMORY USAGE / maxmemory stay at parity), which by design does NOT
track fr's actual Rust heap. The real vs-redis memory gap only shows in process
RSS. Each side is measured in a FRESH process (a reused process retains
mimalloc/jemalloc free pages from a prior load and reports a bogus ~0 delta).

Root cause of the gap (2026-06-16, fr 1.56x): FullSortedSet stores every member
TWICE — once in `dict: IndexMap<Vec<u8>, f64>` and once in
`ordered: BTreeMap<ScoreMember{member: MemberPart::Actual(Vec<u8>)}>` — each a
separate 24-byte Vec header + heap block. redis keeps one sds per member shared
between its dict and skiplist. Hash/Set are already compacted (Packed listpack +
inline HashFieldBytes/SetMember); zset is the last un-compacted collection.

Usage: zset_memory_profile.py <oracle_port> <fr_port> [nkeys] [nmembers]
  (the caller must have started BOTH servers FRESH — no prior load — for the
   numbers to be meaningful.)
"""
import socket
import sys
import time


def rss_kb(pid):
    try:
        for line in open("/proc/%d/status" % pid):
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    except OSError:
        return -1
    return -1


def pid_on_port(port):
    import subprocess
    # Prefer ss (exact listener), fall back to pgrep on the --port arg.
    try:
        out = subprocess.check_output(["ss", "-ltnp"], text=True)
        for line in out.splitlines():
            if (":%d " % port in line or ":%d\t" % port in line) and "pid=" in line:
                return int(line.split("pid=")[1].split(",")[0])
    except Exception:
        pass
    for pat in ("--port %d" % port, "port %d" % port, "*:%d" % port):
        try:
            out = subprocess.check_output(["pgrep", "-f", pat], text=True).split()
            if out:
                return int(out[0])
        except Exception:
            continue
    return -1


def send(s, args):
    o = b"*%d\r\n" % len(args)
    for x in args:
        xb = x.encode() if isinstance(x, str) else x
        o += b"$%d\r\n%s\r\n" % (len(xb), xb)
    s.sendall(o)


def drain(s):
    s.settimeout(0.6)
    try:
        while s.recv(1 << 20):
            pass
    except socket.timeout:
        pass
    s.settimeout(5)


def measure(port, nkeys, nmembers):
    pid = pid_on_port(port)
    if pid < 0:
        print("  (could not find pid for port %d)" % port)
        return None
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    send(s, ["PING"])
    drain(s)
    time.sleep(0.3)
    base = rss_kb(pid)
    for k in range(nkeys):
        args = ["ZADD", "z%d" % k]
        for m in range(nmembers):
            args += ["%d" % m, "memberval_%d" % m]
        send(s, args)
    drain(s)
    time.sleep(1.2)
    after = rss_kb(pid)
    s.close()
    total = nkeys * nmembers
    return (after - base) / 1024.0, (after - base) * 1024.0 / max(1, total)


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(1)
    oport, fport = int(sys.argv[1]), int(sys.argv[2])
    nkeys = int(sys.argv[3]) if len(sys.argv) > 3 else 500
    nmembers = int(sys.argv[4]) if len(sys.argv) > 4 else 800
    print("zset RSS profile: %d keys x %d small members = %d members"
          % (nkeys, nmembers, nkeys * nmembers))
    res = {}
    for port, name in [(oport, "redis"), (fport, "fr")]:
        r = measure(port, nkeys, nmembers)
        if r:
            res[name] = r
            print("  %-5s: data-RSS %.2f MB, %.0f bytes/member" % (name, r[0], r[1]))
    if "fr" in res and "redis" in res and res["redis"][0] > 0:
        print("  ratio fr/redis = %.2fx" % (res["fr"][0] / res["redis"][0]))


if __name__ == "__main__":
    main()
