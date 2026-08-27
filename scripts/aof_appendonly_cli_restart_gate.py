#!/usr/bin/env python3
"""`appendonly yes` must survive a restart however it was ENABLED, not just via a file.

WHY THIS EXISTS SEPARATELY FROM `aof_roundtrip_digest_fuzz.py`. That gate proves fr's
AOF round-trips across a restart -- and it passed while `frankenredis --appendonly
yes` lost every key, because it starts fr with fr's own `--aof <path>` flag. The AOF
ENGINE was never the problem; the CONFIG SURFACE was. A gate that only ever enables a
feature one way cannot see a second way that silently does not work.

The failure it locks down (fixed 2026-08-26): `appendonly yes` passed as a
COMMAND-LINE ARGUMENT enabled AOF writing but never set the LOAD path, because
`configured_aof_path()` was consulted only inside the config-file branch. fr wrote a
correct redis-shaped appendonlydir, reported `aof_enabled:1`, then came up EMPTY on
restart and TRUNCATED the AOF it had just failed to read:

    fr --appendonly yes (CLI)   200 keys -> 0   incr.aof 6,380 B -> 0 B

Every arm is compared against LIVE redis on the identical sequence, and the gate
FAILS CLOSED: an engine that will not start, a digest that moves across the restart,
or a digest that differs from redis's is a failure, never a skip.

    aof_appendonly_cli_restart_gate.py <fr-bin> [redis-bin] [base-port]
"""
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FR = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else os.path.join(ROOT, "target/release/frankenredis")
REDIS = os.path.abspath(sys.argv[2]) if len(sys.argv) > 2 else os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")
BASE = int(sys.argv[3]) if len(sys.argv) > 3 else 47301
KEYS = 200


def resp(*args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        if isinstance(a, str):
            a = a.encode()
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


def read_reply(sock, buf):
    while True:
        if b"\r\n" in buf:
            line, rest = buf.split(b"\r\n", 1)
            if line[:1] in (b"+", b"-", b":", b"*"):
                return line, rest
            if line[:1] == b"$":
                n = int(line[1:])
                if n == -1:
                    return b"(nil)", rest
                if len(rest) >= n + 2:
                    return rest[:n], rest[n + 2:]
        chunk = sock.recv(1 << 20)
        if not chunk:
            raise RuntimeError("server closed the connection")
        buf += chunk


def boot(binary, port, argv_tail, workdir):
    proc = subprocess.Popen(
        [binary, *argv_tail, "--port", str(port), "--enable-debug-command", "yes"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=workdir,
    )
    for _ in range(100):
        if proc.poll() is not None:
            raise RuntimeError("exited at startup rc=%s" % proc.returncode)
        try:
            sock = socket.create_connection(("127.0.0.1", port), timeout=2)
            sock.sendall(resp("PING"))
            reply, _ = read_reply(sock, b"")
            if reply.startswith(b"+PONG"):
                return proc, sock
        except OSError:
            time.sleep(0.4)
    proc.kill()
    raise RuntimeError("did not become ready")


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


def incr_bytes(workdir, dirname="appendonlydir"):
    d = os.path.join(workdir, dirname)
    if not os.path.isdir(d):
        return 0
    return sum(os.path.getsize(os.path.join(d, f))
               for f in os.listdir(d) if f.endswith(".incr.aof"))


def arm(label, binary, argv_tail, workdir, port, conf=None, positional_conf=False):
    """Seed, restart, and report (digest_before, digest_after, dbsize_after, incr)."""
    os.makedirs(workdir, exist_ok=True)
    tail = list(argv_tail)
    if conf is not None:
        path = os.path.join(workdir, "server.conf")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(conf.replace("__WD__", workdir))
        tail = [path] if positional_conf else ["--config", path]
    proc, sock = boot(binary, port, tail, workdir)
    buf = b""
    for i in range(KEYS):
        sock.sendall(resp("SET", "k%d" % i, "v%d" % i))
    for _ in range(KEYS):
        _, buf = read_reply(sock, buf)
    sock.sendall(resp("DEBUG", "DIGEST"))
    before, buf = read_reply(sock, buf)
    stop(proc, sock)
    on_disk = incr_bytes(workdir, "customdir" if "--appenddirname" in tail else "appendonlydir")
    proc2, sock2 = boot(binary, port + 1, tail, workdir)
    buf2 = b""
    sock2.sendall(resp("DBSIZE"))
    dbsize, buf2 = read_reply(sock2, buf2)
    sock2.sendall(resp("DEBUG", "DIGEST"))
    after, buf2 = read_reply(sock2, buf2)
    stop(proc2, sock2)
    return (before.decode("latin1"), after.decode("latin1"),
            dbsize.decode("latin1"), on_disk)


CONF = 'appendonly yes\nsave ""\ndir __WD__\nenable-debug-command yes\n'
FAILURES = []
work = tempfile.mkdtemp(prefix="aof_cli_gate_", dir=os.environ.get("TMPDIR", "/tmp"))
try:
    # Redis first: it defines what "survives" means for this sequence.
    try:
        r_before, r_after, r_db, _ = arm(
            "redis", REDIS, [], os.path.join(work, "redis"), BASE,
            conf=CONF, positional_conf=True)
    except Exception as exc:
        print("FAIL: redis oracle arm did not run: %s" % exc)
        sys.exit(1)
    if r_before != r_after or r_db != ":%d" % KEYS:
        print("FAIL: redis oracle did not survive its own restart -- the gate cannot "
              "judge fr against it (before=%s after=%s dbsize=%s)" % (r_before, r_after, r_db))
        sys.exit(1)

    ARMS = [
        ("fr  --appendonly yes (CLI)", ["--appendonly", "yes", "--save", "", "--dir", "."], None, False),
        ("fr  --config <file>", [], CONF, False),
        ("fr  --appendonly + --appenddirname",
         ["--appendonly", "yes", "--save", "", "--dir", ".", "--appenddirname", "customdir"], None, False),
    ]
    port = BASE + 10
    for label, tail, conf, positional in ARMS:
        wd = os.path.join(work, label.split()[1].strip("-") + str(port))
        try:
            before, after, dbsize, on_disk = arm(label, FR, tail, wd, port, conf=conf,
                                                 positional_conf=positional)
        except Exception as exc:
            FAILURES.append("%s: did not run: %s" % (label, exc))
            port += 10
            continue
        ok = before == after and after == r_after and dbsize == ":%d" % KEYS
        print("  %-38s dbsize %-6s digest %s -> %s  incr=%dB  %s"
              % (label, dbsize, before[1:9], after[1:9], on_disk, "ok" if ok else "FAIL"))
        if not ok:
            FAILURES.append(
                "%s: dbsize=%s digest %s -> %s (redis %s), incr.aof %d B was on disk"
                % (label, dbsize, before, after, r_after, on_disk))
        port += 10
finally:
    shutil.rmtree(work, ignore_errors=True)

if FAILURES:
    print("\nFAIL: %d arm(s) lost data across a restart:" % len(FAILURES))
    for f in FAILURES:
        print("  - %s" % f)
    sys.exit(1)
print("OK: appendonly yes survives a restart via CLI flag, config file, and custom appenddirname")
