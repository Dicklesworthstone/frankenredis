#!/usr/bin/env python3
"""resp3_type_fidelity_gate.py — hard gate on RESP3 wire-type fidelity.

Launches a clean redis-server and the built frankenredis, then asserts the exact
RESP3 (and RESP2-downgrade) wire bytes match vendored Redis 7.2.4 for every RESP3
type. Locks in the multi-turn RESP3 type-fidelity work:

  * DEBUG PROTOCOL <type> for all 13 types under both HELLO 2 and HELLO 3 — the
    canonical per-type probe (Double `,` / BigNumber `(` / Set `~` / Map `%` /
    Verbatim `=` / Bool `#` / Push `>` / Attribute `|` / null `_`).
      frankenredis-nxw4z (RESP3 typed frames), 01weh (attribute), 0gz4g (Bool).
  * Real command paths: ZSCORE / ZMPOP / ZRANGE WITHSCORES emit Double under
    RESP3 (ta2i2/sk4ss family); HGETALL emits a Map; a Lua boolean returned under
    redis.setresp(3) is a Bool that downgrades to :1/:0 on RESP2 (vr8rg/0gz4g);
    MEMORY STATS ratio fields are Doubles, not bulk strings (ta2i2) — checked by
    value-frame TYPE only since the allocator values diverge.

Non-deterministic replies (set/hash element order, allocator values) are compared
by TYPE/shape, not content. Exit 0 if byte-exact, else 1. Usage:
  resp3_type_fidelity_gate.py [--bin FR] [--redis-bin REDIS]
"""
import argparse
import os
import socket
import subprocess
import sys
import time

FR_PORT = 21833
REDIS_PORT = 21834


def find_bin():
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    for c in ("/data/tmp/cargo-target/release/frankenredis",
              "/data/tmp/cargo-target/debug/frankenredis",
              os.path.join(root, "target/release/frankenredis"),
              os.path.join(root, "target/debug/frankenredis")):
        if os.path.exists(c):
            return c
    return None


def find_redis():
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    for c in (os.path.join(root, "legacy_redis_code/redis/src/redis-server"),
              os.path.join(root, "legacy_redis_code/src/redis-server")):
        if os.path.exists(c):
            return c
    return None


def raw(port, hello3, *cmd):
    s = socket.create_connection(("127.0.0.1", port), 3)
    s.settimeout(4)
    if hello3:
        s.sendall(b"*2\r\n$5\r\nHELLO\r\n$1\r\n3\r\n")
        time.sleep(0.03)
        s.recv(1 << 16)
    out = b"*%d\r\n" % len(cmd)
    for x in cmd:
        x = x if isinstance(x, bytes) else str(x).encode()
        out += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(out)
    time.sleep(0.05)
    d = b""
    while True:
        try:
            c = s.recv(1 << 20)
        except socket.timeout:
            break
        if not c:
            break
        d += c
        if len(c) < 65536:
            break
    s.close()
    return d


def wait_up(port):
    for _ in range(60):
        try:
            if raw(port, False, "PING").startswith(b"+PONG"):
                return
        except OSError:
            time.sleep(0.1)
    raise SystemExit(f"server on port {port} did not start")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=None)
    ap.add_argument("--redis-bin", default=None)
    args = ap.parse_args()
    binpath = args.bin or find_bin()
    redispath = args.redis_bin or find_redis()
    if not binpath:
        print("FAIL: frankenredis binary not found (pass --bin)", file=sys.stderr)
        sys.exit(2)
    if not redispath:
        print("FAIL: redis-server not found (pass --redis-bin)", file=sys.stderr)
        sys.exit(2)

    # redis needs enable-debug-command via config file; fr takes the flag.
    here = os.path.dirname(os.path.abspath(__file__))
    conf = os.path.join(here, ".resp3gate.redis.conf")
    with open(conf, "w") as fh:
        fh.write("enable-debug-command yes\nsave \"\"\nappendonly no\n")

    failures = []
    rproc = fproc = None
    try:
        rproc = subprocess.Popen([redispath, conf, "--port", str(REDIS_PORT)],
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        fproc = subprocess.Popen([binpath, "--port", str(FR_PORT),
                                  "--enable-debug-command", "yes"],
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_up(REDIS_PORT)
        wait_up(FR_PORT)

        # Precondition: DEBUG must be enabled on both, else DEBUG PROTOCOL errors
        # identically and the type checks pass trivially.
        for port, who in ((REDIS_PORT, "redis"), (FR_PORT, "fr")):
            dp = raw(port, False, "DEBUG", "PROTOCOL", "true")
            if dp != b":1\r\n":
                print(f"FAIL: DEBUG not enabled on {who} (got {dp!r}); "
                      "gate cannot validate RESP3 types", file=sys.stderr)
                sys.exit(2)

        def check(label, hello3, *cmd):
            o = raw(REDIS_PORT, hello3, *cmd)
            f = raw(FR_PORT, hello3, *cmd)
            if o != f:
                failures.append(f"{label} [{'RESP3' if hello3 else 'RESP2'}] "
                                f"{' '.join(str(c) for c in cmd)}: redis={o!r} fr={f!r}")

        # DEBUG PROTOCOL — every RESP3 type, both protocols. Fully deterministic.
        for ttype in ("string", "integer", "double", "bignum", "null", "array",
                      "set", "map", "attrib", "push", "verbatim", "true", "false"):
            check("debug-protocol", False, "DEBUG", "PROTOCOL", ttype)
            check("debug-protocol", True, "DEBUG", "PROTOCOL", ttype)

        # Real command paths (deterministic). Seed identical state first.
        for port in (REDIS_PORT, FR_PORT):
            raw(port, False, "FLUSHALL")
            raw(port, False, "ZADD", "z", "1.5", "a", "2.5", "b")
            raw(port, False, "HSET", "h", "f1", "v1")
            raw(port, False, "SET", "s", "x")
        for hello3 in (False, True):
            check("zscore", hello3, "ZSCORE", "z", "a")
            check("zrange-withscores", hello3, "ZRANGE", "z", "0", "-1", "WITHSCORES")
            check("zmpop", hello3, "ZMPOP", "1", "z", "MIN")
            raw(REDIS_PORT, False, "ZADD", "z", "1.5", "a")  # reseed after ZMPOP
            raw(FR_PORT, False, "ZADD", "z", "1.5", "a")
            check("hgetall", hello3, "HGETALL", "h")
            check("incrbyfloat", hello3, "INCRBYFLOAT", "fl", "3.0")
            # Lua boolean under setresp(3): #t/#f on RESP3, :1/:0 on RESP2.
            check("lua-false", hello3, "EVAL", "redis.setresp(3); return false", "0")
            check("lua-true", hello3, "EVAL", "redis.setresp(3); return true", "0")

        # MEMORY STATS ratio fields: value-frame TYPE only (allocator values vary).
        def stats_ratio_type(port):
            d = raw(port, True, "MEMORY", "STATS")
            i = d.find(b"dataset.percentage")
            if i < 0:
                return None
            j = d.find(b"\r\n", i) + 2
            return d[j:j + 1]
        ort, frt = stats_ratio_type(REDIS_PORT), stats_ratio_type(FR_PORT)
        if ort != frt or ort != b",":
            failures.append(f"MEMORY STATS dataset.percentage RESP3 type: "
                            f"redis={ort!r} fr={frt!r} (want b',')")
    finally:
        for p in (rproc, fproc):
            if p:
                p.kill()
        try:
            os.remove(conf)
        except OSError:
            pass

    if failures:
        print(f"FAIL: {len(failures)} RESP3 type-fidelity divergence(s):", file=sys.stderr)
        for f in failures:
            print("  " + f, file=sys.stderr)
        sys.exit(1)
    print("OK: RESP3 wire-type fidelity byte-exact vs redis 7.2.4 "
          "(DEBUG PROTOCOL all 13 types x2 protocols + ZSCORE/ZMPOP/WITHSCORES/"
          "HGETALL/INCRBYFLOAT/Lua-bool/MEMORY-STATS)")


if __name__ == "__main__":
    main()
