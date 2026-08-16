#!/usr/bin/env python3
"""Exact instructions/op for hash RESTORE, fr vs the vendored Redis 7.2.4.

(frankenredis-33832) WHY THIS EXISTS SEPARATELY FROM shape_instr_per_op.py: that
file's SHAPES table is whitespace-split STRINGS, and a DUMP payload is arbitrary
bytes -- NULs, CRLFs, LZF output. RESTORE therefore cannot be expressed there at
all, which is why the RESTORE surface kept being measured from private scratchpad
scripts and why its rows were not reproducible by anyone else. 33832 is the worst
vs-incumbent ratio on the board (hash RESTORE 2.45-2.81x behind depending on
workload); it deserves an instrument in the repo.

METHOD: the same two-point subtraction shape_instr_per_op.py uses. Run the
identical workload at N and 2N ops under callgrind and difference the
whole-process totals, so process startup, seeding and teardown cancel exactly and
what is left is per-op work. Instruction counts are load-immune -- this repo's
instrument audit bounded a 34 pct MHz swing at 0.64 pct on instr/op -- so unlike
the timing harness (collection_reload_headtohead.py) this needs no quiet window
and no core pinning.

WORKLOAD: HSET one hash of M fields, DUMP it to get a REAL payload from the engine
under test, then issue N x `RESTORE k 0 <payload> REPLACE`. REPLACE is what makes
every op identical: the key exists from the first op onward, so the keyspace does
not grow across the run and the slope is decode work rather than insertion growth.

    NOT COMPARABLE TO 33832's BANKED ABSOLUTES, and the reason is this workload.
    Restoring repeatedly onto one existing key makes REPLACE free the previous
    value every op -- a cost BOTH engines pay, which dilutes the ratio: this reads
    2.13x at 40 fields where the bead's 200-distinct-key harness reads 2.81x.
    Same-workload before/after deltas are unaffected and are what this is for.
    Do not quote its absolute ratio against a row taken with a different workload.

    THE fr ARM REPRODUCES TO 0.03 pct here; the redis arm carries the usual
    serverCron contaminant (~3 pct), so quote fr-side deltas with more confidence
    than the ratio.

Usage: restore_instr_per_op.py <fr_bin> <members> <ops>
"""
from __future__ import annotations

import os
import socket
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")


def resp(*args):
    out = [b"*%d\r\n" % len(args)]
    for a in args:
        b = a if isinstance(a, bytes) else str(a).encode()
        out.append(b"$%d\r\n%s\r\n" % (len(b), b))
    return b"".join(out)


def read_reply(sock, buf):
    """One RESP reply. Bulk-aware, because DUMP's payload is binary and contains
    CRLF: splitting on CRLF would truncate it and the RESTORE would be rejected."""
    while b"\r\n" not in buf:
        buf += sock.recv(1 << 20)
    line, rest = buf.split(b"\r\n", 1)
    tag = line[:1]
    if tag in (b"+", b"-", b":"):
        return line, rest
    if tag == b"$":
        n = int(line[1:])
        if n == -1:
            return b"", rest
        while len(rest) < n + 2:
            rest += sock.recv(1 << 20)
        return rest[:n], rest[n + 2:]
    raise RuntimeError("unexpected reply tag %r" % tag)


def total_ir(path):
    total = 0
    directory = os.path.dirname(path)
    for name in os.listdir(directory):
        if not name.startswith(os.path.basename(path)):
            continue
        with open(os.path.join(directory, name), "rb") as fh:
            for raw in fh:
                if raw.startswith(b"summary:") or raw.startswith(b"totals:"):
                    total += int(raw.split(b":", 1)[1].split()[0])
    return total


def run(binary, tag, port, members, ops, workdir):
    out = os.path.join(workdir, "cg.%s.out" % tag)
    argv = ["valgrind", "--tool=callgrind", "--callgrind-out-file=" + out,
            "--collect-systime=no", binary, "--port", str(port), "--save", "",
            "--appendonly", "no", "--dir", workdir]
    proc = subprocess.Popen(argv, cwd=workdir,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sock = None
    try:
        for _ in range(600):
            if proc.poll() is not None:
                raise RuntimeError("%s exited during startup rc=%s" % (tag, proc.returncode))
            try:
                sock = socket.create_connection(("127.0.0.1", port), timeout=5)
                sock.settimeout(600)
                sock.sendall(resp("PING"))
                if b"PONG" in sock.recv(64):
                    break
                sock.close()
                sock = None
            except OSError:
                time.sleep(0.25)
        if sock is None:
            raise RuntimeError("%s never became ready under callgrind" % tag)
        buf = b""

        fields = []
        for i in range(members):
            fields += ["f%04d" % i, "v%04d" % i]
        sock.sendall(resp("HSET", "src", *fields))
        _, buf = read_reply(sock, buf)
        # Take the payload from the engine under test, so each arm restores a
        # payload its own DUMP produced rather than one translated between them.
        sock.sendall(resp("DUMP", "src"))
        payload, buf = read_reply(sock, buf)
        if not payload:
            raise RuntimeError("%s produced an empty DUMP" % tag)

        one = resp("RESTORE", "dst", "0", payload, "REPLACE")
        sock.sendall(one * ops)
        done = 0
        while done < ops:
            reply, buf = read_reply(sock, buf)
            if reply.startswith(b"-"):
                raise RuntimeError("%s RESTORE error: %r" % (tag, reply))
            done += 1
        payload_len = len(payload)
    finally:
        if sock is not None:
            sock.close()
        proc.terminate()
        try:
            proc.wait(timeout=180)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=60)

    # TRAP, and it cost this harness its first run: callgrind writes its output
    # file at process EXIT. Reading the total before terminating the server
    # returns 0 -- and a two-point subtraction of 0 minus 0 is 0 instr/op for
    # EVERY arm, i.e. an instrument that silently reports both engines as free
    # rather than failing. Raise instead of returning a zero that looks like data.
    ir = total_ir(out)
    if ir == 0:
        raise RuntimeError(
            "%s produced no callgrind total -- the arm did not measure. This is "
            "what reading the dump before process exit looks like." % tag)
    return ir, payload_len


def main():
    if len(sys.argv) != 4:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    fr_bin, members, ops = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    port = 47800 + (os.getpid() % 200) * 4
    with tempfile.TemporaryDirectory(dir="/data/tmp") as workdir:
        results = {}
        for name, binary in (("fr", fr_bin), ("redis", REDIS)):
            a, plen = run(binary, name + ".n", port, members, ops, workdir)
            b, _ = run(binary, name + ".2n", port + 1, members, ops * 2, workdir)
            results[name] = (b - a) / ops
            print("  %-6s Ir(N)=%-14d Ir(2N)=%-14d -> %10.1f instr/op  (payload %d B)"
                  % (name, a, b, results[name], plen))
        print("  fr/redis instructions per op: %.4fx" % (results["fr"] / results["redis"]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
