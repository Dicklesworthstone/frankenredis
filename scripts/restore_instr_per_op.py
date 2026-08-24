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

Usage: restore_instr_per_op.py <fr_bin> <members> <ops> [--type=hash|list]
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

# (frankenredis-gvm6z) A/A NOISE FLOOR FOR THIS HARNESS'S ARMS, in ABSOLUTE instr/op.
#
# This file had no noise guidance at all, which matters more here than elsewhere: 33832 has
# already adjudicated five micro-levers on this surface, and a reader reaching for the sibling
# harness's percentage constant (0.067 pct) would compute 46 instr/op of noise on a ~69,000
# instr/op RESTORE arm. The measured floor is ~4.5, so that fallback overstates it TENFOLD and
# would bury any real effect under ~30 instr/op.
#
# MEASURED (shape_instr_per_op.py, six --fr-only draws each on one ELF, three sizes spanning
# 83x): sigma is 3.12 / 6.52 / 3.73 instr at 1,305 / 7,290 / 108,610 instr/op -- a 2.09x
# spread against an 83x size range, i.e. NOT proportional to the arm. Three models were
# predicted in advance and all three failed: flat percentage (0.04x), sqrt (0.14x) and a
# fitted power law (0.18x). What survives is a small constant number of instructions, which
# is what a two-point subtraction should leave once the work cancels.
#
# CARRIED, NOT RE-MEASURED HERE. The figure comes from the sibling harness's shapes, and this
# one differs in payload and in booting a fresh engine per arm. Treat it as an order-of-
# magnitude floor rather than a calibrated gate until someone repeats the six-draw procedure
# on a RESTORE shape; the honest use is "is my delta ~10 instr or ~1,000", which is the
# question the five micro-levers on this surface actually turned on.
NULL_SIGMA_INSTR = 4.46

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _incumbent import require_incumbent  # noqa: E402  (path set above)


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


def seed_command(kind, members):
    """The command that builds the source container, by container type.

    (frankenredis-gvm6z) LIST was added because the retained-listpack-span lever
    decodes RDB QUICKLIST_2 nodes -- `decode_retained_listpack_spans` is reached
    from the LIST arm of the RESTORE payload walk, NOT from the hash arm this
    harness originally drove. Certifying that lever against a hash workload would
    have measured a path it does not touch and reported "no change" as if it were
    a verdict on the lever.
    """
    if kind == "hash":
        fields = []
        for i in range(members):
            fields += ["f%04d" % i, "v%04d" % i]
        return resp("HSET", "src", *fields)
    if kind == "list":
        # Short string elements: this is the listpack (QUICKLIST_2 type 2) regime,
        # which is the one the span lever decodes. Letter-leading on purpose --
        # a digit-leading payload takes the derivation-guard path and measures a
        # different frame (frankenredis-qj6jn).
        return resp("RPUSH", "src", *["v%04d" % i for i in range(members)])
    raise SystemExit("unknown container type %r (want hash|list)" % kind)


def run(binary, tag, port, members, ops, workdir, kind="hash"):
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

        sock.sendall(seed_command(kind, members))
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
    # (cross-project check) This harness divides every ratio by the vendored binary;
    # verify it IS its source before printing a denominator.
    require_incumbent(REDIS, os.path.join(ROOT, "legacy_redis_code/redis"))
    argv = [a for a in sys.argv[1:] if not a.startswith("--")]
    kind = "hash"
    for a in sys.argv[1:]:
        if a.startswith("--type="):
            kind = a.split("=", 1)[1]
    if len(argv) != 3:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    fr_bin, members, ops = argv[0], int(argv[1]), int(argv[2])
    # Resolve the binary BEFORE anything chdirs: these run the engine with
    # cwd set to a workdir, so a relative path like target/release/frankenredis
    # becomes "command not found" (rc=127) rather than an obvious error.
    fr_bin = os.path.abspath(fr_bin)
    # (cross-project check, 2026-08-16) franken_networkx measured through an
    # INSTALLED package that had drifted twelve days behind its repo and INVERTED a
    # ratio by 5.4x. Checking my own arm found the same class: a scratchpad binary
    # two commits stale, one of them worth -75.6 pct on the shape being discussed.
    # A stale arm yields a clean, reproducible, WRONG number, so warn before, not after.
    subprocess.run([sys.executable,
                    os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                 "assert_fresh_build.py"), fr_bin], check=False)
    port = 47800 + (os.getpid() % 200) * 4
    with tempfile.TemporaryDirectory(dir="/data/tmp") as workdir:
        results = {}
        for name, binary in (("fr", fr_bin), ("redis", REDIS)):
            a, plen = run(binary, name + ".n", port, members, ops, workdir, kind)
            b, _ = run(binary, name + ".2n", port + 1, members, ops * 2, workdir, kind)
            results[name] = (b - a) / ops
            print("  %-6s Ir(N)=%-14d Ir(2N)=%-14d -> %10.1f instr/op  (payload %d B)"
                  % (name, a, b, results[name], plen))
        print("  fr/redis instructions per op: %.4fx" % (results["fr"] / results["redis"]))
        # (frankenredis-gvm6z) Print the NOISE FLOOR next to the number, because this harness
        # had none and the natural fallback is wrong here by an order of magnitude.
        print("  A/A noise floor: sigma ~%.2f instr/op ABSOLUTE (size-independent); a delta "
              "needs ~%.0f instr/op to clear 2 sigma on one arm" % (NULL_SIGMA_INSTR,
                                                                   2 * NULL_SIGMA_INSTR))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
