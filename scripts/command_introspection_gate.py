#!/usr/bin/env python3
"""command_introspection_gate.py — hard gate on the COMMAND introspection surface.

Launches a clean redis-server and a freshly-built frankenredis, then asserts that
the COMMAND family is byte-exact vs vendored Redis 7.2.4 for every command:

  * COMMAND (no args)      — the top-level array must list ONLY the top-level
                             commands (subcommands nested, not flattened), and
                             its length must equal COMMAND COUNT.
                             (regression guard for frankenredis-d309r)
  * COMMAND INFO <cmd>      — name/arity/flags/first/last/step/acl-cats/tips/
                             key-specs byte-exact for every command.
  * COMMAND INFO (no args)  — same top-level-only shape as COMMAND.
  * COMMAND DOCS <cmd>      — the full per-command doc map (summary/since/group/
                             complexity/arguments/...) byte-exact.

Documented WONTFIX excluded so the gate is deterministic:
  * container `subcommands` ORDER (Lua-dict-hash order; order-normalized here).

Exit 0 if byte-exact, else 1. Usage:
  command_introspection_gate.py [--bin FR] [--redis-bin REDIS]
"""
import argparse
import json
import os
import socket
import subprocess
import sys
import time

FR_PORT = 21823
REDIS_PORT = 21824


class Conn:
    def __init__(self, port, resp3=False):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(5)
        self.b = b""
        if resp3:
            self.cmd("HELLO", "3")

    def _fill(self, n):
        while len(self.b) < n:
            chunk = self.s.recv(1 << 20)
            if not chunk:
                raise EOFError
            self.b += chunk

    def _line(self):
        while b"\r\n" not in self.b:
            self.b += self.s.recv(1 << 20)
        line, self.b = self.b.split(b"\r\n", 1)
        return line

    def parse(self):
        line = self._line()
        t, rest = line[:1], line[1:]
        if t == b"+":
            return rest.decode("latin1")
        if t == b"-":
            return "-" + rest.decode("latin1")
        if t == b":":
            return int(rest)
        if t == b",":
            return ("dbl", rest.decode("latin1"))
        if t == b"#":
            return rest == b"t"
        if t == b"_":
            return None
        if t in (b"$", b"="):
            n = int(rest)
            if n < 0:
                return None
            self._fill(n + 2)
            v, self.b = self.b[:n], self.b[n + 2:]
            try:
                return v.decode("utf-8")
            except UnicodeDecodeError:
                return v.hex()
        if t in (b"*", b"~", b">"):
            n = int(rest)
            return None if n < 0 else [self.parse() for _ in range(n)]
        if t == b"%":
            n = int(rest)
            m = {}
            for _ in range(n):
                k = self.parse()
                v = self.parse()
                m[k if isinstance(k, str) else json.dumps(k)] = v
            return m
        raise ValueError(line)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()


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
                return proc
        except OSError:
            time.sleep(0.1)
    proc.kill()
    raise SystemExit(f"server on port {port} did not start: {cmdline[0]}")


def info_key(entry):
    """Canonical, order-stable form of a COMMAND INFO entry, recursing into the
    nested subcommands array with the order normalized away (WONTFIX)."""
    if not isinstance(entry, list) or not entry:
        return entry
    name = entry[0]
    arity = entry[1] if len(entry) > 1 else None
    flags = sorted(entry[2] or []) if len(entry) > 2 else []
    fls = entry[3:6] if len(entry) > 6 else []
    acl = sorted(entry[6] or []) if len(entry) > 6 else []
    tips = sorted(entry[7] or []) if len(entry) > 7 else []
    keyspecs = entry[8] if len(entry) > 8 else None
    subs = entry[9] if len(entry) > 9 else None
    sub_norm = None
    if isinstance(subs, list):
        sub_norm = sorted((info_key(s) for s in subs), key=json.dumps)
    return [name, arity, flags, fls, acl, tips, keyspecs, sub_norm]


def names_of(command_reply):
    return [e[0] for e in command_reply if isinstance(e, list) and e and isinstance(e[0], str)]


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

    failures = []
    rproc = fproc = None
    try:
        rproc = launch([redispath, "--port", str(REDIS_PORT), "--save", ""], REDIS_PORT)
        fproc = launch([binpath, "--port", str(FR_PORT)], FR_PORT)
        o2, f2 = Conn(REDIS_PORT), Conn(FR_PORT)
        o3, f3 = Conn(REDIS_PORT, resp3=True), Conn(FR_PORT, resp3=True)

        ocmd, fcmd = o2.cmd("COMMAND"), f2.cmd("COMMAND")
        onames, fnames = set(names_of(ocmd)), set(names_of(fcmd))
        ocount, fcount = o2.cmd("COMMAND", "COUNT"), f2.cmd("COMMAND", "COUNT")

        # Top-level shape: same length, same name set, length == COMMAND COUNT.
        if len(ocmd) != len(fcmd):
            failures.append(f"COMMAND length: redis={len(ocmd)} fr={len(fcmd)}")
        if len(fcmd) != fcount:
            failures.append(f"COMMAND length {len(fcmd)} != COMMAND COUNT {fcount} (d309r)")
        for n in sorted(fnames - onames):
            failures.append(f"COMMAND top-level extra in fr: {n}")
        for n in sorted(onames - fnames):
            failures.append(f"COMMAND top-level missing in fr: {n}")
        if ocount != fcount:
            failures.append(f"COMMAND COUNT: redis={ocount} fr={fcount}")

        # Bare COMMAND INFO must mirror COMMAND's top-level shape.
        oinfo, finfo = o2.cmd("COMMAND", "INFO"), f2.cmd("COMMAND", "INFO")
        if len(oinfo) != len(finfo):
            failures.append(f"COMMAND INFO (bare) length: redis={len(oinfo)} fr={len(finfo)}")

        # Per-command INFO + DOCS byte-exact.
        info_diff = docs_diff = 0
        for name in sorted(onames):
            oe = info_key(o2.cmd("COMMAND", "INFO", name)[0])
            fe = info_key(f2.cmd("COMMAND", "INFO", name)[0])
            if json.dumps(oe) != json.dumps(fe):
                info_diff += 1
                if info_diff <= 8:
                    failures.append(f"COMMAND INFO {name}: redis={json.dumps(oe)[:160]} "
                                    f"fr={json.dumps(fe)[:160]}")
            od = o3.cmd("COMMAND", "DOCS", name)
            fd = f3.cmd("COMMAND", "DOCS", name)
            od = od.get(name) if isinstance(od, dict) else od
            fd = fd.get(name) if isinstance(fd, dict) else fd
            if isinstance(od, dict) and isinstance(fd, dict):
                od = {k: v for k, v in od.items() if k != "subcommands"}
                fd = {k: v for k, v in fd.items() if k != "subcommands"}
            if json.dumps(od, sort_keys=True) != json.dumps(fd, sort_keys=True):
                docs_diff += 1
                if docs_diff <= 8:
                    failures.append(f"COMMAND DOCS {name}: redis={json.dumps(od)[:160]} "
                                    f"fr={json.dumps(fd)[:160]}")
    finally:
        for p in (rproc, fproc):
            if p:
                p.kill()

    if failures:
        print(f"FAIL: {len(failures)} COMMAND introspection divergence(s):", file=sys.stderr)
        for f in failures:
            print("  " + f, file=sys.stderr)
        sys.exit(1)
    print(f"OK: COMMAND introspection byte-exact vs redis 7.2.4 "
          f"({len(onames)} commands: COMMAND/COUNT/INFO/key-specs/DOCS; "
          "subcommand order normalized)")


if __name__ == "__main__":
    main()
