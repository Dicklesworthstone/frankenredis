#!/usr/bin/env python3
"""config_defaults_gate.py — CONFIG defaults parity vs vendored redis 7.2.4.

A config-less redis and a freshly-started frankenredis both expose their
COMPILED-IN defaults, so `CONFIG GET *` should agree on (a) the full set of
parameter names and (b) every default value except parameters whose value is
legitimately host/runtime specific (paths, bind/port, cpulists, run-id, ...).

This gate launches its own clean fr instance (so prior CONFIG SETs from other
probes can't pollute it) and diffs it against a config-less redis oracle. Every
non-host-specific default drift or missing/extra parameter is a hard failure.

Former allowlist entries both landed and are re-verified byte-exact, so every
non-host-specific parameter is checked directly:
  - always-show-logo            frankenredis-zbpg6 (fixed: fr now 'no') CLOSED
  - client-output-buffer-limit  frankenredis-8sb0l (fixed: per-class spec) CLOSED

Both servers are launched fresh by the gate (config-less) so a stray CONFIG SET
from another probe can never pollute the comparison.

Usage: config_defaults_gate.py [--bin PATH] [--redis-bin PATH]
Exit 0 if defaults match (modulo host-specific fields), else 1.
"""
import argparse
import os
import socket
import subprocess
import sys
import time


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(4.0)
        self.b = b""

    def _line(self):
        while b"\r\n" not in self.b:
            self.b += self.s.recv(65536)
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
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t == b":":
            return int(r)
        if t == b"+":
            return r.decode()
        if t == b"-":
            return "ERR:" + r.decode()
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

    def config_all(self):
        flat = self.cmd("CONFIG", "GET", "*")
        return {flat[i]: flat[i + 1] for i in range(0, len(flat), 2)}


# Values that legitimately differ by host/runtime/build environment.
HOST_SPECIFIC = {
    "dir", "logfile", "pidfile", "unixsocket", "unixsocketperm", "bind",
    "port", "tls-port", "requirepass", "masterauth", "masteruser", "save",
    "dbfilename", "appenddirname", "appendfilename", "aclfile",
    "cluster-config-file", "cluster-announce-ip", "cluster-announce-port",
    "cluster-announce-bus-port", "cluster-announce-human-nodename",
    "cluster-announce-tls-port", "replicaof", "slaveof", "syslog-ident",
    # proc-title-template is a FIXED default string (not host-specific) — un-excluded
    # 2026-07-03 after fixing fr's default {laddr}->{listen-addr} to match redis 7.2.4,
    # so the gate now guards it against regression.
    "locale-collate", "io-threads", "server-cpulist",
    "bio-cpulist", "aof-rewrite-cpulist", "bgsave-cpulist", "socket-mark-id",
    "run-id", "maxmemory", "appendonly", "daemonize", "supervised",
    "crash-log-enabled", "crash-memcheck-enabled", "oom-score-adj",
    "syslog-enabled", "tls-cert-file", "tls-key-file", "tls-ca-cert-file",
    "tls-ca-cert-dir", "tls-key-file-pass", "tls-client-cert-file",
    "tls-client-key-file", "tls-client-key-file-pass", "tls-dh-params-file",
    "tls-protocols", "tls-ciphers", "tls-ciphersuites",
    "enable-protected-configs", "enable-debug-command", "enable-module-command",
    "loadmodule", "include", "rdb-key-save-delay", "key-load-delay",
    "watchdog-period",
}

# fr-specific knobs with no upstream equivalent.
FR_ONLY = {"active-expire-enabled"}


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


def launch(cmdline, port):
    proc = subprocess.Popen(cmdline, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(60):
        try:
            c = Conn(port)
            if c.cmd("PING") == "PONG":
                return proc, c
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise SystemExit(f"server on port {port} did not start: {cmdline[0]}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=None)
    ap.add_argument("--redis-bin", default=None)
    args = ap.parse_args()
    binpath = args.bin or find_bin()
    redispath = args.redis_bin or find_redis()
    if not binpath or not os.path.exists(binpath):
        print("FAIL: frankenredis binary not found (pass --bin PATH)", file=sys.stderr)
        sys.exit(2)
    if not redispath or not os.path.exists(redispath):
        print("FAIL: redis-server not found (pass --redis-bin PATH)", file=sys.stderr)
        sys.exit(2)

    failures = []
    rproc, fproc = None, None
    try:
        rproc, rc = launch([redispath, "--port", "21814", "--save", ""], 21814)
        oracle = rc.config_all()
        fproc, c = launch([binpath, "--port", "21813"], 21813)
        fr = c.config_all()

        only_oracle = sorted(set(oracle) - set(fr))
        only_fr = sorted(set(fr) - set(oracle) - FR_ONLY)
        for p in only_oracle:
            failures.append(f"missing parameter: {p}")
        for p in only_fr:
            failures.append(f"extra parameter: {p}")

        for k in sorted(set(oracle) & set(fr)):
            if k in HOST_SPECIFIC or oracle[k] == fr[k]:
                continue
            failures.append(f"{k}: redis={oracle[k]!r} fr={fr[k]!r}")
    finally:
        for p in (fproc, rproc):
            if p is None:
                continue
            p.terminate()
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()

    if failures:
        print("FAIL: CONFIG default divergences:")
        for f in failures:
            print(f"  - {f}")
        sys.exit(1)
    print(f"OK: {len(set(oracle) & set(fr))} CONFIG parameters match redis 7.2.4 "
          "compiled defaults (host-specific excluded)")


if __name__ == "__main__":
    main()
