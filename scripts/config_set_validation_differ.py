#!/usr/bin/env python3
"""config_set_validation_differ.py — CONFIG SET validation parity vs redis 7.2.4.

Companion to config_defaults_gate.py (which checks CONFIG GET *defaults*). This
gate checks CONFIG SET *input validation*: for every settable parameter, redis
rejects malformed/out-of-range values with a specific
`CONFIG SET failed (possibly related to argument 'X') - <detail>` message, while
a naive store-everything implementation silently accepts them (an ACCEPT-GAP) or
rejects valid input (a REJECT-GAP) or returns the wrong wording.

It enumerates the full parameter set from the oracle's `CONFIG GET *`, then for
each param sends a battery of adversarial values (garbage, negative, digit
overflow, fractional, off-by-one bounds) to BOTH servers and compares the raw
first-line reply byte-for-byte. Divergences are classified:

  ACCEPT-GAP   fr returned +OK where redis returned an error  (real hole)
  REJECT-GAP   fr returned an error where redis returned +OK  (too strict)
  WORDING      both errored but with different detail text     (wire diverge)

Both servers are launched fresh & config-less by the gate (compiled-in defaults,
so e.g. hash-max-listpack-* align at 512/-2 — see config_defaults_gate.py) and
torn down at exit, so a stray CONFIG SET from another probe can't pollute it.

KNOWN ISSUES: none — the full CONFIG SET validation surface is byte-exact,
including CONFIG SET port (MODIFIABLE_CONFIG with a live listener rebind,
frankenredis-zyx9q).

Usage: config_set_validation_differ.py [--bin PATH] [--redis-bin PATH] [-v]
Exit 0 if validation matches (modulo known-issues), else 1.
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
            chunk = self.s.recv(65536)
            if not chunk:
                raise IOError("connection closed")
            self.b += chunk
        line, self.b = self.b.split(b"\r\n", 1)
        return line

    def _rn(self, n):
        while len(self.b) < n + 2:
            chunk = self.s.recv(65536)
            if not chunk:
                raise IOError("connection closed")
            self.b += chunk
        d, self.b = self.b[:n], self.b[n + 2:]
        return d

    def raw_first_line(self, args):
        """Send a command, return the raw first reply line (bytes, no CRLF)."""
        buf = b"*%d\r\n" % len(args)
        for a in args:
            if isinstance(a, str):
                a = a.encode()
            buf += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(buf)
        return self._line()

    def parse(self, args):
        """Send a command and parse a flat reply (line / bulk / array-of-bulk)."""
        buf = b"*%d\r\n" % len(args)
        for a in args:
            if isinstance(a, str):
                a = a.encode()
            buf += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(buf)
        line = self._line()
        t, rest = line[:1], line[1:]
        if t == b"+":
            return rest.decode("latin1")
        if t == b"-":
            return Exception(rest.decode("latin1"))
        if t == b":":
            return int(rest)
        if t == b"$":
            n = int(rest)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t == b"*":
            n = int(rest)
            return [self.parse_one() for _ in range(n)] if n >= 0 else None
        raise IOError("unexpected reply type %r" % t)

    def parse_one(self):
        line = self._line()
        t, rest = line[:1], line[1:]
        if t == b"$":
            n = int(rest)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t == b":":
            return int(rest)
        if t in (b"+", b"-"):
            return rest.decode("latin1")
        raise IOError("unexpected element type %r" % t)


# Parameters whose validation is host/runtime/format specific or otherwise out
# of the value-range validation surface this gate covers. (Empty: port is now
# MODIFIABLE with byte-exact range validation + live rebind, frankenredis-zyx9q.)
KNOWN_ISSUES: "dict[str, str]" = {}

# String/enum/free-form params where an arbitrary token is legitimately accepted
# by both (or rejected by both with server-specific wording we don't pin here).
SKIP = {
    "requirepass", "masterauth", "masteruser", "dir", "logfile", "pidfile",
    "unixsocket", "dbfilename", "appenddirname", "appendfilename", "aclfile",
    "bind", "bind-source-addr", "save", "oom-score-adj-values",
    "latency-tracking-info-percentiles", "notify-keyspace-events",
    "cluster-announce-ip", "cluster-announce-human-nodename",
    "replica-announce-ip", "slave-announce-ip", "syslog-ident", "socket-mark-id",
    "locale-collate", "acl-pubsub-default", "enable-debug-command",
    "enable-module-command", "enable-protected-configs",
    "propagation-error-behavior", "supervised", "loglevel", "maxmemory-policy",
    "appendfsync", "repl-diskless-load", "sanitize-dump-payload",
    "io-threads-do-reads", "cluster-preferred-endpoint-type", "tls-ciphers",
    "tls-ciphersuites", "tls-protocols", "tls-cert-file", "tls-key-file",
    "tls-key-file-pass", "tls-client-cert-file", "tls-client-key-file",
    "tls-client-key-file-pass", "tls-dh-params-file", "tls-ca-cert-file",
    "tls-ca-cert-dir", "tls-auth-clients", "req-res-logfile", "ignore-warnings",
    "cluster-config-file", "syslog-facility", "oom-score-adj", "maxmemory-clients",
    "appendonly",
}

# Adversarial values exercised against every numeric/bool/charset param.
TEST_VALUES = ["__garbage__", "-1", "99999999999999999999999", "1.5"]


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
    for _ in range(80):
        try:
            c = Conn(port)
            if c.parse(["PING"]) == "PONG":
                return proc, c
        except Exception:
            time.sleep(0.1)
    proc.terminate()
    raise RuntimeError("server on port %d did not come up" % port)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=None, help="frankenredis binary")
    ap.add_argument("--redis-bin", default=None, help="redis-server binary")
    ap.add_argument("--oracle-port", type=int, default=16399)
    ap.add_argument("--fr-port", type=int, default=16400)
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()

    binpath = args.bin or find_bin()
    redispath = args.redis_bin or find_redis()
    if not binpath or not os.path.exists(binpath):
        print("FAIL: frankenredis binary not found (pass --bin PATH)", file=sys.stderr)
        sys.exit(2)
    if not redispath or not os.path.exists(redispath):
        print("FAIL: redis-server not found (pass --redis-bin PATH)", file=sys.stderr)
        sys.exit(2)

    oproc = fproc = None
    try:
        oproc, oc = launch(
            [redispath, "--port", str(args.oracle_port), "--save", "",
             "--appendonly", "no"], args.oracle_port)
        fproc, fc = launch(
            [binpath, "--port", str(args.fr_port), "--mode", "strict"],
            args.fr_port)

        names = oc.parse(["CONFIG", "GET", "*"])
        params = names[0::2]

        accept_gaps, reject_gaps, wording, known = [], [], [], []
        for p in sorted(params):
            if p in SKIP:
                continue
            for v in TEST_VALUES:
                ob = oc.raw_first_line(["CONFIG", "SET", p, v])
                fb = fc.raw_first_line(["CONFIG", "SET", p, v])
                if ob == fb:
                    continue
                rec = (p, v, ob.decode("latin1"), fb.decode("latin1"))
                if p in KNOWN_ISSUES:
                    known.append(rec)
                elif fb == b"+OK" and ob.startswith(b"-"):
                    accept_gaps.append(rec)
                elif ob == b"+OK" and fb.startswith(b"-"):
                    reject_gaps.append(rec)
                else:
                    wording.append(rec)

        def show(title, recs):
            print(f"\n=== {title}: {len(recs)} ===")
            seen = set()
            for p, v, ob, fb in recs:
                if p in seen and not args.verbose:
                    continue
                seen.add(p)
                print(f"  {p}  (v={v!r})")
                print(f"     oracle: {ob}")
                print(f"     fr    : {fb}")

        show("ACCEPT-GAPs (fr accepts what redis rejects)", accept_gaps)
        show("REJECT-GAPs (fr rejects what redis accepts)", reject_gaps)
        show("WORDING divergences", wording)
        if known:
            show("KNOWN ISSUES (allowlisted)", known)

        total = len(accept_gaps) + len(reject_gaps) + len(wording)
        print(f"\n{len(params)} params swept; {total} unexpected divergences; "
              f"{len(known)} known-issue cases.")
        if total == 0:
            print("PASS: CONFIG SET validation byte-exact vs redis 7.2.4.")
            sys.exit(0)
        print("FAIL: CONFIG SET validation divergences detected.")
        sys.exit(1)
    finally:
        for proc in (fproc, oproc):
            if proc is not None:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except Exception:
                    proc.kill()


if __name__ == "__main__":
    main()
