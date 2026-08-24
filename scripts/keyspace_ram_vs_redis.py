#!/usr/bin/env python3
"""Keyspace RAM per key, FrankenRedis vs live Redis 7.2.4, in ONE invocation.

    python3 scripts/keyspace_ram_vs_redis.py --fr /tmp/fr_bin \
        --redis legacy_redis_code/redis/src/redis-server --keys 1000000

Why this exists (frankenredis-uhthd): the keyspace index is the only gap that hits
EVERY workload, and every number on the bead so far was taken in a separate
invocation from its incumbent -- 5.4x, then 4.49x, then 2.687x, each a bare fr
figure divided by a redis figure remembered from another run. This starts both
engines from the same process, loads them with the same command, and samples both
at the same instant.

THREE SERVERS, ON PURPOSE. Two independent fr instances are started and loaded
identically; `fr_a / fr_b` is the A/A NULL. RSS is not a deterministic quantity --
it depends on allocator arena growth, page faulting and decommit timing -- so a
vs-redis ratio is only readable against a null taken the same way, in the same
window, on the same host. A null far from 1.000 means the instrument is not
resolving anything and the ratio must not be quoted.

WHY VmRSS AND NOT `used_memory`: fr's `used_memory` is a deliberate
redis-accounting MODEL of logical data; it does not count index overhead, and it
has read ~3.3x BELOW real RSS on this workload. The whole gap being measured lives
in exactly what the model omits. Both are reported so the divergence stays visible,
but the claim is RSS.

WHY THE SAMPLE IS TAKEN IMMEDIATELY AFTER LOAD: mimalloc (fr) and jemalloc (redis)
both decommit pages over the following seconds, at different rates. A delay of a few
seconds moves fr's number materially, so every arm is sampled at the same point in
its own lifecycle -- right after its load returns -- rather than at a wall-clock
moment shared across arms.

Exit 0 always; this is an instrument, not a gate.
"""

import argparse
import hashlib
import json
import os
import platform
import socket
import subprocess
import sys
import tempfile
import time


def enc(*parts):
    """RESP-encode one command."""
    out = b"*%d\r\n" % len(parts)
    for p in parts:
        b = p.encode() if isinstance(p, str) else p
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


def free_port(start, taken):
    for port in range(start, start + 500):
        if port in taken:
            continue
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
        except OSError:
            taken.add(port)
            return port
    raise SystemExit("no free port")


def wait_ready(port, proc, timeout=120):
    for _ in range(timeout * 10):
        if proc.poll() is not None:
            raise SystemExit(f"server exited early rc={proc.returncode}")
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=0.3)
            s.close()
            return
        except OSError:
            time.sleep(0.1)
    raise SystemExit(f"server on {port} never accepted")


def call(port, *parts, timeout=600):
    """Send one command, return the raw reply bytes (enough to detect -ERR)."""
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    s.settimeout(timeout)
    s.sendall(enc(*parts))
    buf = b""
    while b"\r\n" not in buf:
        chunk = s.recv(65536)
        if not chunk:
            break
        buf += chunk
    s.close()
    return buf


def info_field(port, field):
    s = socket.create_connection(("127.0.0.1", port), timeout=30)
    s.settimeout(30)
    s.sendall(enc("INFO", "memory"))
    buf = b""
    deadline = time.time() + 30
    while time.time() < deadline:
        chunk = s.recv(65536)
        if not chunk:
            break
        buf += chunk
        if buf.endswith(b"\r\n") and len(buf) > 64:
            break
    s.close()
    for line in buf.decode("utf-8", "replace").splitlines():
        if line.startswith(field + ":"):
            try:
                return int(line.split(":", 1)[1].strip())
            except ValueError:
                return None
    return None


def vmrss_bytes(pid):
    """Resident set of an EXACT pid, from /proc -- never parsed out of `ps` output."""
    with open(f"/proc/{pid}/status", "r", encoding="utf-8") as fh:
        for line in fh:
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    raise SystemExit(f"no VmRSS for pid {pid}")


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


class Server:
    def __init__(self, label, binary, port, workdir):
        self.label = label
        self.binary = binary
        self.port = port
        # Each server gets its OWN empty directory and a dbfilename that does not
        # exist, so a stray dump.rdb from another run cannot be loaded and counted
        # as this run's keyspace.
        self.dir = workdir
        self.proc = subprocess.Popen(
            [
                binary,
                "--port", str(port),
                "--dir", workdir,
                "--dbfilename", "uhthd-nonexistent.rdb",
                "--save", "",
                "--appendonly", "no",
                "--enable-debug-command", "yes",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        wait_ready(port, self.proc)

    def stop(self):
        try:
            self.proc.kill()
            self.proc.wait(timeout=15)
        except Exception:
            pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fr", required=True)
    ap.add_argument("--redis", required=True)
    ap.add_argument("--keys", type=int, default=1_000_000)
    ap.add_argument("--settle", type=float, default=0.0,
                    help="seconds to wait after load before sampling (default 0)")
    ap.add_argument("--json", default="")
    args = ap.parse_args()

    taken = set()
    tmp = tempfile.mkdtemp(prefix="uhthd_ram_")

    # fr_b is the A/A null partner: same binary, same load, independent process.
    specs = [
        ("fr_a", args.fr),
        ("fr_b", args.fr),
        ("redis", args.redis),
    ]
    servers = []
    try:
        for label, binary in specs:
            d = os.path.join(tmp, label)
            os.makedirs(d, exist_ok=True)
            servers.append(Server(label, binary, free_port(7900, taken), d))

        rows = {}
        for srv in servers:
            empty_rss = vmrss_bytes(srv.proc.pid)
            reply = call(srv.port, "DEBUG", "POPULATE", str(args.keys))
            if not reply.startswith(b"+OK"):
                raise SystemExit(f"{srv.label}: DEBUG POPULATE failed: {reply[:120]!r}")
            if args.settle:
                time.sleep(args.settle)
            loaded_rss = vmrss_bytes(srv.proc.pid)
            dbsize = call(srv.port, "DBSIZE")
            rows[srv.label] = {
                "port": srv.port,
                "pid": srv.proc.pid,
                "empty_rss": empty_rss,
                "loaded_rss": loaded_rss,
                "delta_rss": loaded_rss - empty_rss,
                "bytes_per_key": (loaded_rss - empty_rss) / args.keys,
                "used_memory": info_field(srv.port, "used_memory"),
                "dbsize": dbsize.decode("utf-8", "replace").strip(),
            }

        # Every arm must actually hold the keys, or the comparison is between a
        # loaded server and an empty one.
        expect = f":{args.keys}"
        for label, row in rows.items():
            if row["dbsize"] != expect:
                raise SystemExit(
                    f"{label}: DBSIZE is {row['dbsize']!r}, expected {expect!r} -- "
                    "arms are not holding the same keyspace"
                )

        null = rows["fr_a"]["bytes_per_key"] / rows["fr_b"]["bytes_per_key"]
        ratio = rows["fr_a"]["bytes_per_key"] / rows["redis"]["bytes_per_key"]
        ratio_b = rows["fr_b"]["bytes_per_key"] / rows["redis"]["bytes_per_key"]
        worst = max(ratio, ratio_b)

        with open("/proc/loadavg", "r", encoding="utf-8") as fh:
            loadavg = fh.read().split()[:3]

        prov = {
            "host": platform.node(),
            "keys": args.keys,
            "fr_elf_sha256": sha256_file(args.fr),
            "redis_elf_sha256": sha256_file(args.redis),
            "redis_version": subprocess.run(
                [args.redis, "--version"], capture_output=True, text=True
            ).stdout.strip(),
            "loadavg": loadavg,
            "settle_s": args.settle,
        }

        print(json.dumps({"rows": rows, "provenance": prov}, indent=2))
        print()
        for label in ("fr_a", "fr_b", "redis"):
            r = rows[label]
            print(
                f"  {label:6s} delta RSS {r['delta_rss']/1e6:9.1f} MB   "
                f"{r['bytes_per_key']:8.1f} B/key   "
                f"used_memory {(r['used_memory'] or 0)/1e6:8.1f} MB"
            )
        print()
        print(f"  A/A null (fr_a/fr_b)   {null:.4f}")
        print(f"  fr/redis               {ratio:.4f}  and  {ratio_b:.4f}")
        print(f"  WORST BOUND            {worst:.4f}x  <- quote this")
        if args.json:
            with open(args.json, "w", encoding="utf-8") as fh:
                json.dump(
                    {"rows": rows, "provenance": prov, "null": null, "worst": worst},
                    fh,
                    indent=2,
                )
    finally:
        for srv in servers:
            srv.stop()

    return 0


if __name__ == "__main__":
    sys.exit(main())
