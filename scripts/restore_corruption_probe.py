#!/usr/bin/env python3
"""Does fr's unconditional RESTORE validation actually REJECT what redis accepts?

fr costs 2.21x redis on hash RESTORE. Redis's marginal profile shows ZERO per-field
work: `sanitize-dump-payload` defaults to `no`, so redis header-checks and trusts.
That makes fr's 74,741 Ir of validation either (a) a safety property redis declines,
or (b) wasted work -- i.e. a real loss. The difference is whether fr rejects a
payload whose CRC is VALID but whose listpack interior is garbage.

Corrupt a byte, recompute the CRC-64/Jones footer so the checksum still passes, and
offer it to both. The CRC implementation self-checks against the intact payload's
own footer first, so a wrong polynomial cannot fake this result.
"""
import os, socket, subprocess, sys, time

# redis crc64.c: MSB-first table over POLY with refin/refout (crc_reflect on both
# ends), init 0, xorout 0. The equivalent LSB-first table uses the REFLECTED poly.
POLY = 0xad93d23594c935a9
def _reflect(v, w):
    r = 0
    for _ in range(w):
        r = (r << 1) | (v & 1); v >>= 1
    return r
RPOLY = _reflect(POLY, 64)
_T = []
for i in range(256):
    c = i
    for _ in range(8):
        c = (c >> 1) ^ (RPOLY if c & 1 else 0)
    _T.append(c)

def crc64(b, crc=0):
    for x in b:
        crc = _T[(crc ^ x) & 0xff] ^ (crc >> 8)
    return crc

def resp(*a):
    out = b"*%d\r\n" % len(a)
    for x in a:
        if isinstance(x, str): x = x.encode()
        out += b"$%d\r\n%s\r\n" % (len(x), x)
    return out

def read_reply(s, buf):
    while True:
        if b"\r\n" in buf:
            line, rest = buf.split(b"\r\n", 1)
            if line[:1] in (b"+", b"-", b":"): return line, rest
            if line[:1] == b"$":
                n = int(line[1:])
                if n == -1: return b"(nil)", rest
                if len(rest) >= n + 2: return rest[:n], rest[n+2:]
        c = s.recv(1 << 20)
        if not c: raise RuntimeError("closed")
        buf += c

def start(binary, port, wd, extra=()):
    os.makedirs(wd, exist_ok=True)
    p = subprocess.Popen([binary, "--port", str(port), "--save", "", "--appendonly", "no",
                          "--dir", wd, "--enable-debug-command", "yes", *extra],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=wd)
    for _ in range(60):
        try: return p, socket.create_connection(("127.0.0.1", port), timeout=2)
        except OSError: time.sleep(0.5)
    p.kill(); raise SystemExit("no start " + binary)

def reseal(payload, pos, xor):
    body = bytearray(payload[:-8])
    body[pos] ^= xor
    return bytes(body) + crc64(bytes(body)).to_bytes(8, "little")

fr, rd = sys.argv[1], sys.argv[2]
N = 20  # small enough that the listpack is stored UNCOMPRESSED -> targeted corruption
procs = []
try:
    pa, sa = start(fr, 48821, "/data/tmp/claude-1000/frx-bt/cp/fr"); procs.append(pa)
    pb, sb = start(rd, 48822, "/data/tmp/claude-1000/frx-bt/cp/rd"); procs.append(pb)
    ba = bb = b""
    f = []
    for i in range(N): f += ["f%04d" % i, "v%04d" % i]
    sb.sendall(resp("HSET", "src", *f)); _, bb = read_reply(sb, bb)
    sb.sendall(resp("DUMP", "src")); pay, bb = read_reply(sb, bb)
    print("payload %d B, type byte %d" % (len(pay), pay[0]))

    # SELF-CHECK the CRC before trusting any verdict built on it.
    want = int.from_bytes(pay[-8:], "little"); got = crc64(pay[:-8])
    print("crc self-check: footer=%016x computed=%016x  %s"
          % (want, got, "OK" if want == got else "WRONG POLYNOMIAL -- verdicts below are void"))
    if want != got: raise SystemExit(1)

    cases = [("intact", pay)]
    # Corrupt bytes inside the listpack body (past the 1-byte type + length prefix
    # and past the 6-byte listpack header), resealing the CRC each time.
    for off in (12, 20, 28, 36):
        if off < len(pay) - 8:
            cases.append(("byte@%d^0xff" % off, reseal(pay, off, 0xff)))

    print("\n%-18s | %-38s | %s" % ("case", "fr", "redis (default sanitize=no)"))
    for name, p in cases:
        outs = []
        for s, buf, tag in ((sa, ba, "fr"), (sb, bb, "rd")):
            try:
                s.sendall(resp("RESTORE", "d_" + name.replace("@","_").replace("^","_"), "0", p, "REPLACE"))
                r, nb = read_reply(s, buf)
                if tag == "fr": ba = nb
                else: bb = nb
                v = r.decode("latin1")[:36]
                if v.startswith("+OK"):
                    # accepted -- can it be READ without dying?
                    s.sendall(resp("HLEN", "d_" + name.replace("@","_").replace("^","_")))
                    r2, nb2 = read_reply(s, nb)
                    if tag == "fr": ba = nb2
                    else: bb = nb2
                    v = "ACCEPTED, HLEN=%s" % r2.decode("latin1")[:12]
                outs.append(v)
            except Exception as e:
                outs.append("SERVER DIED: %s" % type(e).__name__)
        print("%-18s | %-38s | %s" % (name, outs[0], outs[1]))
finally:
    for p in procs:
        try: p.kill()
        except Exception: pass
