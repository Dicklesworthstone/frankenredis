#!/usr/bin/env python3
"""Self-orchestrating gate: config directives take effect, from argv AND from a
config file, byte-identically to redis 7.2.4 (frankenredis-4ib91).

redis accepts ANY config directive as `--<name> <value>` on the command line --
the form nearly every container image and init script uses -- and honours the
same directives from a redis.conf. fr recognised a fixed list and did two
different wrong things with the rest: argv REFUSED TO START, and the config file
SILENTLY DROPPED them, so `maxmemory-policy allkeys-lru` produced a server
running `noeviction` with nothing in the log. `cluster-enabled`
(frankenredis-inuwt) had been the same bug one directive earlier.

WHY A GATE AND NOT JUST A UNIT TEST: the unit test pins that unnamed directives
reach the passthrough list. It cannot see whether the value then actually takes
effect in a running server, which is the part an operator experiences and the
part that was silently wrong. This asks the two engines the same question --
CONFIG GET after boot -- and compares.

THE NEGATIVE HALF IS PART OF THE CONTRACT. A fix that accepted everything would
pass a gate that only checked good directives, while being WORSE than the bug it
replaced: a silently-accepted typo is unrecoverable where a refused one is not.
So this also asserts that a bogus directive and a bogus value each ABORT startup
on fr, and that a config file carrying one aborts too.

Usage: config_directive_parity_gate.py <redis-bin> <fr-bin> [base_port]
       Exit 0 = parity, 1 = divergence.
"""
import os, socket, subprocess, sys, tempfile, time

REDIS_BIN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else
                            "legacy_redis_code/redis/src/redis-server")
FR_BIN = os.path.abspath(sys.argv[2] if len(sys.argv) > 2 else "/tmp/fr_rdb")
BASE = int(sys.argv[3]) if len(sys.argv) > 3 else 29811

# Ordinary tuning directives, none of which had a dedicated fr flag.
DIRECTIVES = [
    # (frankenredis-5rru3) `bind` earns its place here for two reasons the other
    # entries do not have. It is MULTI-VALUED, so it catches a parser that reads
    # only the first argument -- fr did, which left IPv6 loopback unreachable
    # while `CONFIG GET bind` still echoed the truncated value, so the operator's
    # own introspection confirmed the wrong answer. And the `-` prefix means
    # "bind if possible, skip if not", so a server must honour it rather than
    # refuse to start on a host without IPv6.
    ("bind", "127.0.0.1 -::1"),
    ("hash-max-listpack-entries", "256"),
    ("set-max-intset-entries", "4096"),
    ("zset-max-listpack-entries", "256"),
    ("list-max-listpack-size", "64"),
    ("maxmemory-policy", "allkeys-lru"),
    ("appendfsync", "everysec"),
]


def cmd(port, *args):
    """One command, returning the raw reply bytes (None if unreachable)."""
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=5)
    except OSError:
        return None
    try:
        out = bytearray(b"*%d\r\n" % len(args))
        for a in args:
            b = a.encode()
            out += b"$%d\r\n%s\r\n" % (len(b), b)
        s.sendall(bytes(out))
        time.sleep(0.05)
        return s.recv(65536)
    finally:
        s.close()


def wait_ready(port, proc, timeout=12.0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        if proc.poll() is not None:
            return False
        if cmd(port, "PING"):
            return True
        time.sleep(0.1)
    return False


def start(binary, port, extra, cwd):
    argv = [binary, "--port", str(port)]
    if binary == REDIS_BIN:
        argv += ["--save", "", "--appendonly", "no"]
    argv += extra
    p = subprocess.Popen(argv, cwd=cwd, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL)
    return p, wait_ready(port, p)


def effective(port):
    """CONFIG GET each directive; returns {name: raw reply bytes}."""
    return {n: cmd(port, "CONFIG", "GET", n) for n, _ in DIRECTIVES}


def main():
    fails = []
    procs = []
    try:
        # ---- argv form ----
        argv_flags = [x for n, v in DIRECTIVES for x in (f"--{n}", v)]
        od, fd = tempfile.mkdtemp(), tempfile.mkdtemp()
        rp, ro_ok = start(REDIS_BIN, BASE, argv_flags, od)
        fp, fr_ok = start(FR_BIN, BASE + 1, argv_flags, fd)
        procs += [rp, fp]
        if not ro_ok:
            print("VOID — redis did not start with argv directives")
            return 2
        if not fr_ok:
            fails.append("argv: fr did not start at all with the directives redis accepts")
        else:
            ro, rf = effective(BASE), effective(BASE + 1)
            for name, _ in DIRECTIVES:
                if ro[name] != rf[name]:
                    fails.append(f"argv {name}: redis={ro[name]!r} fr={rf[name]!r}")

        # ---- config-file form ----
        od2, fd2 = tempfile.mkdtemp(), tempfile.mkdtemp()
        body = "".join(f"{n} {v}\n" for n, v in DIRECTIVES)
        for d, port in ((od2, BASE + 2), (fd2, BASE + 3)):
            with open(os.path.join(d, "redis.conf"), "w") as fh:
                fh.write(f"port {port}\nsave \"\"\nappendonly no\n{body}")
        rp2, ro2_ok = start(REDIS_BIN, BASE + 2, ["./redis.conf"], od2)
        # redis takes the conf path positionally; fr takes --config.
        rp2.kill()
        rp2 = subprocess.Popen([REDIS_BIN, "./redis.conf"], cwd=od2,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        ro2_ok = wait_ready(BASE + 2, rp2)
        fp2 = subprocess.Popen([FR_BIN, "--config", "./redis.conf"], cwd=fd2,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        fr2_ok = wait_ready(BASE + 3, fp2)
        procs += [rp2, fp2]
        if not ro2_ok:
            print("VOID — redis did not start from the config file")
            return 2
        if not fr2_ok:
            fails.append("config file: fr did not start")
        else:
            ro, rf = effective(BASE + 2), effective(BASE + 3)
            for name, _ in DIRECTIVES:
                if ro[name] != rf[name]:
                    fails.append(f"conf {name}: redis={ro[name]!r} fr={rf[name]!r}")

        # ---- negative half: fr must REFUSE, not silently accept ----
        for label, extra in (
            ("unknown directive", ["--not-a-real-directive", "5"]),
            ("invalid value", ["--maxmemory-policy", "definitely-not-a-policy"]),
        ):
            d = tempfile.mkdtemp()
            p = subprocess.Popen([FR_BIN, "--port", str(BASE + 4)] + extra, cwd=d,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                rc = p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()
                rc = None
            if rc is None or rc == 0:
                fails.append(
                    f"negative/{label}: fr should ABORT startup (rc={rc}); silently "
                    f"accepting a bad directive is worse than dropping it"
                )
    finally:
        for p in procs:
            try:
                p.kill()
            except Exception:
                pass

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} config-directive divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        return 1
    print(
        f"PASS — {len(DIRECTIVES)} config directives take effect identically to redis "
        "7.2.4 from BOTH argv and a config file, and fr still aborts on an unknown "
        "directive and an invalid value"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
