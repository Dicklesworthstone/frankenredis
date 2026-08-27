#!/usr/bin/env python3
"""Stream save/reload state-convergence fuzzer vs live redis 7.2.4.

WHY THIS EXISTS. `digest_state_fuzz.py` emits ZERO stream commands and never issues
`DEBUG RELOAD`; `aof_roundtrip_digest_fuzz.py` reuses its generator, so it inherits the
same blind spot. **The stream RDB round trip -- the path that encodes consumer groups,
PELs and macro-node listpacks and reads them back -- has had no randomized
differential coverage.** Reply-level differs cannot see it: a save that writes stale
bytes returns +OK and only diverges after the reload.

It is also the prerequisite safety net for the save-side blob cache sized at 18.2 pct
of the stream reload arm (`22f0e2a78`). That cache must invalidate on CONSUMER GROUP
mutations as well as entry mutations -- `XGROUP CREATE` alone moves a stream's DUMP
from 108 B to 124 B -- and `stream_groups` lives on the Store, OUTSIDE
`Value::Stream`, so the value's `modification_count` cannot gate it. An invalidation
that wide has to be PROVEN by a differential, not argued in review: a stale cached
save is silent data loss, strictly worse than a slow save.

Deterministic by construction so both engines can be compared byte-for-byte:
explicit stream IDs (never `*`), fixed consumer names, no time-dependent XCLAIM
arguments, no XAUTOCLAIM cursors.

    stream_reload_digest_fuzz.py [seeds] [rounds] [--fr=<bin>] [--redis=<bin>]
"""
import os
import random
import socket
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FR = os.path.join(ROOT, "target/release/frankenredis")
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")
for a in sys.argv[1:]:
    if a.startswith("--fr="):
        FR = os.path.abspath(a.split("=", 1)[1])
    if a.startswith("--redis="):
        REDIS = os.path.abspath(a.split("=", 1)[1])
pos = [a for a in sys.argv[1:] if not a.startswith("--")]
SEEDS = int(pos[0]) if pos else 3
ROUNDS = int(pos[1]) if len(pos) > 1 else 6
PER_ROUND = 40
KEYS = ["st1", "st2", "st3"]
GROUPS = ["ga", "gb"]
CONSUMERS = ["c1", "c2"]


def resp(*args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        if isinstance(a, str):
            a = a.encode()
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


def read_reply(sock, buf):
    """Full RESP reader -- arrays are consumed RECURSIVELY.

    A reader that returns on `*` without consuming its elements desyncs the stream
    and every later reply is read from the wrong offset. That produced a nonsense
    2-byte DUMP while probing this very surface, so the recursion is load-bearing.
    """
    def one(buf):
        while b"\r\n" not in buf:
            buf = pull(buf)
        line, rest = buf.split(b"\r\n", 1)
        tag = line[:1]
        if tag in (b"+", b"-", b":"):
            return line, rest
        if tag == b"$":
            n = int(line[1:])
            if n == -1:
                return b"$-1", rest
            while len(rest) < n + 2:
                rest = pull(rest)
            return line + b":" + rest[:n], rest[n + 2:]
        if tag == b"*":
            n = int(line[1:])
            if n == -1:
                return b"*-1", rest
            parts = [line]
            for _ in range(n):
                item, rest = one(rest)
                parts.append(item)
            return b"|".join(parts), rest
        raise RuntimeError("bad RESP tag %r" % tag)

    def pull(b):
        chunk = sock.recv(1 << 20)
        if not chunk:
            raise RuntimeError("server closed")
        return b + chunk

    return one(buf)


def pick_port():
    for _ in range(200):
        c = random.randint(20000, 60000)
        try:
            probe = socket.create_connection(("127.0.0.1", c), timeout=0.25)
        except OSError:
            return c
        probe.close()
    raise SystemExit("no free port")


def boot(binary, workdir):
    os.makedirs(workdir, exist_ok=True)
    port = pick_port()
    proc = subprocess.Popen(
        [binary, "--port", str(port), "--save", "", "--appendonly", "no",
         "--dir", workdir, "--enable-debug-command", "yes"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=workdir)
    for _ in range(90):
        if proc.poll() is not None:
            raise RuntimeError("%s exited at startup rc=%s" % (binary, proc.returncode))
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=2)
            s.sendall(resp("PING"))
            r, _ = read_reply(s, b"")
            if r.startswith(b"+PONG"):
                return proc, s
        except OSError:
            time.sleep(0.4)
    proc.kill()
    raise RuntimeError("%s never became ready" % binary)


def stop(proc, sock):
    try:
        sock.sendall(resp("SHUTDOWN", "NOSAVE"))
        sock.close()
    except OSError:
        pass
    try:
        proc.wait(timeout=60)
    except Exception:
        proc.kill()


def gen(rnd, seq):
    """One deterministic stream command. Explicit IDs only -- never `*`."""
    k = rnd.choice(KEYS)
    g = rnd.choice(GROUPS)
    c = rnd.choice(CONSUMERS)
    r = rnd.random()
    if r < 0.34:
        return ("XADD", k, "%d-%d" % (seq + 1, rnd.randint(1, 3)),
                "f%d" % rnd.randint(0, 4), "v%d" % rnd.randint(0, 9))
    if r < 0.42:
        return ("XADD", k, "%d-1" % (seq + 1), "a", "1", "b", "2", "c", "3")
    if r < 0.50:
        return ("XGROUP", "CREATE", k, g, "0", "MKSTREAM")
    if r < 0.56:
        return ("XREADGROUP", "GROUP", g, c, "COUNT", "2", "STREAMS", k, ">")
    if r < 0.62:
        return ("XACK", k, g, "%d-1" % rnd.randint(1, max(1, seq)))
    if r < 0.68:
        return ("XDEL", k, "%d-1" % rnd.randint(1, max(1, seq)))
    if r < 0.74:
        return ("XTRIM", k, "MAXLEN", str(rnd.randint(0, 12)))
    if r < 0.80:
        return ("XGROUP", "CREATECONSUMER", k, g, c)
    if r < 0.85:
        return ("XGROUP", "DELCONSUMER", k, g, c)
    if r < 0.90:
        return ("XSETID", k, "%d-9" % (seq + 5))
    if r < 0.95:
        return ("XGROUP", "SETID", k, g, "0")
    return ("XGROUP", "DESTROY", k, g)


def main():
    work = tempfile.mkdtemp(prefix="stream_reload_fuzz_", dir=os.environ.get("TMPDIR", "/tmp"))
    procs = []
    try:
        rp, rs = boot(REDIS, os.path.join(work, "redis"))
        procs.append((rp, rs))
        fp, fs = boot(FR, os.path.join(work, "fr"))
        procs.append((fp, fs))
        rbuf = fbuf = b""
        issued = 0
        for sd in range(SEEDS):
            rnd = random.Random(4200 + sd)
            for engine, sock, buf in (("redis", rs, None), ("fr", fs, None)):
                sock.sendall(resp("FLUSHALL"))
            _, rbuf = read_reply(rs, rbuf)
            _, fbuf = read_reply(fs, fbuf)
            for rd_i in range(ROUNDS):
                for _ in range(PER_ROUND):
                    issued += 1
                    cmd = gen(rnd, issued)
                    rs.sendall(resp(*cmd))
                    fs.sendall(resp(*cmd))
                    ro, rbuf = read_reply(rs, rbuf)
                    fo, fbuf = read_reply(fs, fbuf)
                    if ro != fo:
                        print("[seed %d round %d] REPLY DIVERGE %s\n  redis=%r\n  fr   =%r"
                              % (4200 + sd, rd_i, " ".join(cmd), ro[:160], fo[:160]))
                        return 1
                # DEBUG RELOAD both, then compare the whole-keyspace digest. A save
                # that wrote stale bytes returns +OK and only shows up here.
                for sock in (rs, fs):
                    sock.sendall(resp("DEBUG", "RELOAD"))
                rrel, rbuf = read_reply(rs, rbuf)
                frel, fbuf = read_reply(fs, fbuf)
                if not (rrel.startswith(b"+OK") and frel.startswith(b"+OK")):
                    print("[seed %d round %d] DEBUG RELOAD failed redis=%r fr=%r"
                          % (4200 + sd, rd_i, rrel[:80], frel[:80]))
                    return 1
                for sock in (rs, fs):
                    sock.sendall(resp("DEBUG", "DIGEST"))
                rdig, rbuf = read_reply(rs, rbuf)
                fdig, fbuf = read_reply(fs, fbuf)
                if rdig != fdig:
                    print("[seed %d round %d] DIGEST DIVERGE AFTER RELOAD\n"
                          "  redis=%r\n  fr   =%r" % (4200 + sd, rd_i, rdig, fdig))
                    return 1

                # DUMP / RESTORE round trip, per key. The reload check above covers
                # the RDB FILE path; this covers the PAYLOAD path, which is a
                # DIFFERENT decoder -- when an empty grouped stream was being
                # dropped on reload (abf460569) DUMP/RESTORE round-tripped it
                # perfectly, so one path passing says nothing about the other.
                #
                # Two assertions per key: the payloads are BYTE-IDENTICAL across
                # engines (fr claims byte-exact DUMP), and restoring fr's own
                # payload into a fresh key reproduces the whole-keyspace digest
                # relationship -- i.e. the restored copy equals redis's restored
                # copy.
                for k in KEYS:
                    for sock in (rs, fs):
                        sock.sendall(resp("DUMP", k))
                    rdmp, rbuf = read_reply(rs, rbuf)
                    fdmp, fbuf = read_reply(fs, fbuf)
                    # NOT asserted: byte-identical stream DUMP across engines.
                    # Measured and REJECTED as a gate condition -- after deletions
                    # redis PRESERVES its existing macro-node structure while fr
                    # RE-PACKS into fresh nodes, so the payloads differ in length
                    # (488 B vs 463 B on the first shape that hit it) while
                    # describing the same stream. What matters is that each
                    # engine's OWN payload restores to equal state, which is what
                    # the digest below checks.
                    if (rdmp == b"$-1") != (fdmp == b"$-1"):
                        print("[seed %d round %d] one engine has %s and the other "
                              "does not (redis=%r fr=%r)"
                              % (4200 + sd, rd_i, k, rdmp[:12], fdmp[:12]))
                        return 1
                    if rdmp == b"$-1":
                        continue
                    rraw = rdmp.split(b":", 1)[1]
                    fraw = fdmp.split(b":", 1)[1]
                    rs.sendall(resp("RESTORE", k + "_rt", "0", rraw, "REPLACE"))
                    fs.sendall(resp("RESTORE", k + "_rt", "0", fraw, "REPLACE"))
                    rr, rbuf = read_reply(rs, rbuf)
                    fr_, fbuf = read_reply(fs, fbuf)
                    if rr != fr_:
                        print("[seed %d round %d] RESTORE reply diverges for %s\n"
                              "  redis=%r\n  fr   =%r"
                              % (4200 + sd, rd_i, k, rr[:120], fr_[:120]))
                        return 1
                for sock in (rs, fs):
                    sock.sendall(resp("DEBUG", "DIGEST"))
                rdig2, rbuf = read_reply(rs, rbuf)
                fdig2, fbuf = read_reply(fs, fbuf)
                if rdig2 != fdig2:
                    print("[seed %d round %d] DIGEST DIVERGE AFTER DUMP/RESTORE\n"
                          "  redis=%r\n  fr   =%r" % (4200 + sd, rd_i, rdig2, fdig2))
                    return 1
                # Drop the round-trip copies so the next round's state is the
                # generator's, not an accumulation of restored duplicates.
                for sock in (rs, fs):
                    sock.sendall(resp("DEL", *[k + "_rt" for k in KEYS]))
                _, rbuf = read_reply(rs, rbuf)
                _, fbuf = read_reply(fs, fbuf)
        print("OK: %d seed(s) x %d cycles, %d stream commands -- fr matches redis 7.2.4 "
              "reply-for-reply, on whole-keyspace DEBUG DIGEST across every DEBUG "
              "RELOAD, and on a per-key DUMP + RESTORE round trip of each engine's "
              "own payload" % (SEEDS, ROUNDS, issued))
        return 0
    finally:
        for proc, sock in procs:
            stop(proc, sock)


sys.exit(main())
