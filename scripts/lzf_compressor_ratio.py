"""Re-price the LZF COMPRESSOR ratio, fr vs vendored redis 7.2.4, at the FRAME level.

    python3 scripts/lzf_compressor_ratio.py fr    <fr-elf>    7801 --rdb /tmp/fr.rdb --enable-debug-command yes
    python3 scripts/lzf_compressor_ratio.py redis <redis-elf> 7841 --dir /tmp --dbfilename r.rdb --enable-debug-command yes

Divide the two "per key" numbers to get the ratio. Lives in the repo ON PURPOSE: the
previous version of this harness lived in one agent's scratchpad, which is why the 1.66x
figure sat on the board unre-priced across two shipped slices.

Why frame level and not whole-op: the compressor is a pure compute kernel. Counting the
lzf_compress frame excludes serverCron entirely, which is the elapsed-time work that makes
redis's WHOLE-process denominator drift (0.787 pct across six draws in ledger 1af2d590d).
Both arms therefore measure only the kernel, on byte-identical input, called the same number
of times.

Two-point method: run the identical workload at N and 2N DEBUG RELOADs and difference the
frame totals, so startup, seeding and teardown cancel exactly.
"""

import os
import re
import socket
import subprocess
import sys
import time

S = os.path.dirname(os.path.abspath(__file__))
KEYS = 200
FIELDS = 40


def enc(*a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        b = x.encode() if isinstance(x, str) else x
        o += b"$%d\r\n%s\r\n" % (len(b), b)
    return o


def wait_port(port, proc, timeout=180):
    for _ in range(timeout * 4):
        if proc.poll() is not None:
            raise SystemExit("server died early rc=%s" % proc.returncode)
        try:
            s = socket.create_connection(("127.0.0.1", port), 0.5)
            s.close()
            return
        except OSError:
            time.sleep(0.25)
    raise SystemExit("no listen on %d" % port)


def read_reply(s):
    """Minimal RESP reader: enough for +OK / :int / $bulk / *array."""
    buf = b""

    def need(n):
        nonlocal buf
        while len(buf) < n:
            c = s.recv(65536)
            if not c:
                raise SystemExit("closed")
            buf += c

    def line():
        nonlocal buf
        while b"\r\n" not in buf:
            c = s.recv(65536)
            if not c:
                raise SystemExit("closed")
            buf += c
        i = buf.index(b"\r\n")
        out, buf = buf[:i], buf[i + 2:]
        return out

    def one():
        nonlocal buf
        ln = line()
        t, rest = ln[:1], ln[1:]
        if t in (b"+", b"-", b":"):
            return rest
        if t == b"$":
            n = int(rest)
            if n == -1:
                return None
            need(n + 2)
            v = buf[:n]
            _drop(n + 2)
            return v
        if t == b"*":
            n = int(rest)
            return [one() for _ in range(max(0, n))]
        raise SystemExit("bad tag %r" % ln[:40])

    def _drop(n):
        nonlocal buf
        buf = buf[n:]

    return one()


def run_arm(binary, port, reloads, outfile, extra_args):
    cmd = [
        "valgrind", "--tool=callgrind", "--callgrind-out-file=" + outfile,
        binary, "--port", str(port), "--save", "", "--appendonly", "no",
    ] + extra_args
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_port(port, p)
    s = socket.create_connection(("127.0.0.1", port), 30)
    s.settimeout(600)
    s.sendall(enc("FLUSHALL"))
    read_reply(s)
    # 200 listpack-encoded hashes of 40 fields: the shape the 1.76x was measured on.
    for k in range(KEYS):
        args = ["HSET", "h:%d" % k]
        for f in range(FIELDS):
            args += ["f%d" % f, "v%d" % f]
        s.sendall(enc(*args))
        read_reply(s)
    # Confirm the encoding really is a listpack; an hashtable-encoded key would
    # compress a different payload and silently change what is being compared.
    s.sendall(enc("OBJECT", "ENCODING", "h:0"))
    encd = read_reply(s)
    for i in range(reloads):
        s.sendall(enc("DEBUG", "RELOAD"))
        rep = read_reply(s)
        # An ERROR REPLY IS A COMPLETED REQUEST as far as a naive harness is
        # concerned. The first version of this script read the reply and threw it
        # away, so DEBUG RELOAD was refused ("enable-debug-command") on every
        # iteration and the measurement dutifully reported a free reload and zero
        # compressor frames. Assert the reply, always.
        if rep != b"OK":
            raise SystemExit("DEBUG RELOAD #%d not OK: %r" % (i, rep))
    s.sendall(enc("SHUTDOWN", "NOSAVE"))
    try:
        s.close()
    except OSError:
        pass
    p.wait(timeout=600)
    return encd


def frames(outfile):
    out = subprocess.run(
        ["callgrind_annotate", "--auto=no", "--threshold=99.9", outfile],
        capture_output=True, text=True).stdout
    r = {}
    for ln in out.splitlines():
        m = re.match(r"\s*([\d,]+) \([ \d.]+%\)\s+(.*)", ln)
        if m:
            name = m.group(2).split(" [")[0]
            name = re.sub(r"^\S+?:", "", name)
            r[name] = r.get(name, 0) + int(m.group(1).replace(",", ""))
    return r


def lzf_total(fr_map, needles):
    tot = 0.0
    hits = []
    for k, v in fr_map.items():
        low = k.lower()
        if any(n in low for n in needles) and "decompress" not in low:
            tot += v
            hits.append((v, k))
    return tot, sorted(hits, reverse=True)[:6]


def main():
    which = sys.argv[1]
    binary = sys.argv[2]
    port = int(sys.argv[3])
    extra = sys.argv[4:]
    n, n2 = 4, 8
    o1 = os.path.join(S, "cg.lzfr.%s.n" % which)
    o2 = os.path.join(S, "cg.lzfr.%s.2n" % which)
    e1 = run_arm(binary, port, n, o1, extra)
    e2 = run_arm(binary, port + 1, n2, o2, extra)
    print("%s encoding=%r/%r" % (which, e1, e2))
    a, b = frames(o1), frames(o2)
    needles = ["lzf_compress"]
    ta, ha = lzf_total(a, needles)
    tb, hb = lzf_total(b, needles)
    per_reload = (tb - ta) / (n2 - n)
    print("%s lzf frames at N: %s" % (which, [(int(v), k[:60]) for v, k in ha]))
    print("%s lzf_compress per reload = %.1f ; per key = %.1f"
          % (which, per_reload, per_reload / KEYS))
    tot_a = a.get("PROGRAM TOTALS", 0)
    tot_b = b.get("PROGRAM TOTALS", 0)
    print("%s whole reload per key = %.1f"
          % (which, (tot_b - tot_a) / (n2 - n) / KEYS))


main()
