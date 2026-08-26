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

`--aa` adds a THIRD arm: the same fr ELF measured a second time as an independent
pair of processes. Its ratio against the fr arm is this invocation's own null, and
a fr/redis figure printed from a run whose null missed the band is not quotable.
Every arm also prints the sha256 of `/proc/<pid>/exe` of the server that served it,
and the run aborts if an arm's N and 2N launches did not run the same ELF.

Usage: restore_instr_per_op.py <fr_bin> <members> <ops> [--type=hash|list|set|zset|stream] [--op=restore|reload] [--aa] [--keys=N]
"""
from __future__ import annotations

import hashlib
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
    """Whole-process Ir, counted ONCE per callgrind dump.

    (BlackThrush 2026-08-25) This summed every line matching `summary:` OR
    `totals:`, and a callgrind dump carries BOTH with the same value -- so every
    arm was reported at exactly 2x its real instruction count. The RATIO is
    unaffected (both arms doubled identically, which is why this survived every
    ratio row taken with it), but every ABSOLUTE instr/op figure this harness has
    printed is twice the truth, and its stated A/A noise floor of ~4.5 instr/op
    was being compared against doubled arms. Verified against the sibling
    profiler on the identical workload: 62,600 instr/op here vs 31,175 there,
    the same 2x.

    `totals:` is the grand total when present; `summary:` is the per-part one.
    Take totals if the file has it, else summary, and never both.
    """
    total = 0
    directory = os.path.dirname(path)
    for name in os.listdir(directory):
        if not name.startswith(os.path.basename(path)):
            continue
        summary = None
        totals = None
        with open(os.path.join(directory, name), "rb") as fh:
            for raw in fh:
                if raw.startswith(b"summary:"):
                    summary = int(raw.split(b":", 1)[1].split()[0])
                elif raw.startswith(b"totals:"):
                    totals = int(raw.split(b":", 1)[1].split()[0])
        if totals is not None:
            total += totals
        elif summary is not None:
            total += summary
    return total


def seed_command(kind, members, key="src"):
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
        return resp("HSET", key, *fields)
    if kind == "list":
        # Short string elements: this is the listpack (QUICKLIST_2 type 2) regime,
        # which is the one the span lever decodes. Letter-leading on purpose --
        # a digit-leading payload takes the derivation-guard path and measures a
        # different frame (frankenredis-qj6jn).
        return resp("RPUSH", key, *["v%04d" % i for i in range(members)])
    if kind == "set":
        # (frankenredis-qj6jn) SET and ZSET were added for the same reason LIST was:
        # their RDB-FILE load arms materialise one owned Vec<u8> per member out of a
        # listpack the store is about to copy anyway, while their RESTORE arms already
        # walk borrowed spans. Measuring that on a hash or list workload would measure
        # a path it does not touch.
        #
        # KEEP `members` UNDER set-max-listpack-entries (128) OR THIS MEASURES THE
        # WRONG ARM. Above it the set is hashtable-encoded and saves as the plain
        # RDB_TYPE_SET, which never reaches the listpack decode arm at all -- the run
        # still completes and still prints a ratio, so the mistake looks like data.
        # Letter-leading members on purpose: all-integer members would save as
        # RDB_TYPE_SET_INTSET, a third arm again.
        return resp("SADD", key, *["v%04d" % i for i in range(members)])
    if kind == "stream":
        # (BlackThrush 2026-08-26) STREAM was added because it was the only
        # collection type with NO ratio at all -- every other type had one and this
        # one was assumed rather than measured. Its RDB save arm clones every
        # field/value pair per ENTRY (`fields.to_pairs()`), which is the shape that
        # cost the other types 5-10 pct each, so leaving it unmeasured risked
        # optimising a 2.5x arm while a worse one sat unlooked-at.
        #
        # Explicit IDs so the seed is deterministic and the save side has a dense
        # (ms, seq) range to encode; two fields per entry, letter-leading, matching
        # the other types' element shape.
        args = []
        for i in range(members):
            args += ["XADD", key, "%d-1" % (i + 1), "f0", "v%04d" % i, "f1", "w%04d" % i]
        out = []
        for i in range(members):
            base = i * 7
            out.append(resp(*args[base:base + 7]))
        return b"".join(out)
    if kind == "zset":
        # Same threshold trap as `set`, against zset-max-listpack-entries (128).
        # Integer scores on purpose -- a listpack integer score takes the
        # allocation-free `n as f64` shortcut, so what is left in the frame is the
        # MEMBER materialisation this arm exists to measure.
        args = []
        for i in range(members):
            args += [str(i), "v%04d" % i]
        return resp("ZADD", key, *args)
    raise SystemExit("unknown container type %r (want hash|list|set|zset|stream)" % kind)


def elf_identity_from_maps(pid, binary):
    """sha256 of the ELF the SERVER IS RUNNING, proven to be that one via /proc.

    `/proc/<pid>/exe` is the obvious call and it is WRONG here: every arm runs
    under valgrind, which loads the guest itself, so /proc/<pid>/exe resolves to
    /usr/libexec/valgrind/callgrind-amd64-linux for fr and for redis alike. The
    first version of this function printed one identical hash for all three arms
    -- an identity check that could not tell the two engines apart, which is
    worse than none because it reads as provenance.

    The guest is in /proc/<pid>/maps with its dev:inode. `/proc/<pid>/map_files`
    would hash that inode directly but needs CAP_SYS_ADMIN and is unreadable
    here, so we hash the PATH and admit the hash only when the path still
    resolves to the inode the process actually mapped. `target/release/<bin>` is
    SHARED across agents in this checkout; when a peer's `cargo build` replaces
    it mid-run the path gets a new inode, the comparison fails, and we refuse
    rather than print a hash of bytes nothing executed.
    """
    binary = os.path.realpath(binary)
    mapped = None
    with open("/proc/%d/maps" % pid, "r") as fh:
        for line in fh:
            parts = line.split(None, 5)
            if len(parts) < 6:
                continue
            path = parts[5].strip()
            if "x" in parts[1] and os.path.realpath(path) == binary:
                mapped = parts[4]  # inode
                break
    if mapped is None:
        raise RuntimeError("no executable mapping of %s in pid %d" % (binary, pid))
    on_disk = str(os.stat(binary).st_ino)
    if mapped != on_disk:
        raise RuntimeError(
            "%s was REPLACED while this arm was running (process maps inode %s, path is now "
            "inode %s) -- the file we could hash is not the ELF that served this arm"
            % (binary, mapped, on_disk))
    digest = hashlib.sha256()
    with open(binary, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def run(binary, tag, port, members, ops, workdir, kind="hash", op="restore", keys=1):
    # ONE DIRECTORY PER LAUNCH, and it is load-bearing for `--op=reload`.
    #
    # (BlackThrush 2026-08-25) Every launch used to share one workdir. That is
    # harmless for `--op=restore`, which writes no rdb, and it silently destroys
    # `--op=reload`: DEBUG RELOAD SAVES the whole db to `<dir>/dump.rdb`, so the
    # NEXT server started with the same `--dir` LOADS the previous launch's dump
    # at startup -- across arms, and across ENGINES. It cost me a full battery.
    # The tell was not the ratios, which looked plausible, but the A/A null
    # (0.66x-1.40x on four shapes) and the list arm's two same-ELF runs reporting
    # DIFFERENT DUMP payload sizes, 191 B against 212 B.
    #
    # Same trap `restore_profile_frames.py` documents for a fixed profile dir.
    workdir = os.path.join(workdir, tag)
    os.makedirs(workdir, exist_ok=True)
    out = os.path.join(workdir, "cg.%s.out" % tag)
    argv = ["valgrind", "--tool=callgrind", "--callgrind-out-file=" + out,
            "--collect-systime=no", binary, "--port", str(port), "--save", "",
            "--appendonly", "no", "--dir", workdir,
            # DEBUG RELOAD needs the debug command admitted on both engines.
            "--enable-debug-command", "yes"]
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
        exe_sha = elf_identity_from_maps(proc.pid, binary)
        buf = b""

        # (BlackThrush 2026-08-25) SEED `keys` CONTAINERS, NOT ONE. This harness
        # only ever built a single key, which is correct for `--op=restore` (the
        # op is per-key) and MISLEADING for `--op=reload`: DEBUG RELOAD saves and
        # loads the WHOLE db, so a one-key db charges every per-RELOAD fixed cost
        # -- rdb header, aux fields, opcode framing, file round-trip -- against
        # one container and buries the per-key term the ratio is supposed to
        # expose. Reload read 1.02-1.40x per type at one key; that number is a
        # statement about fixed cost, not about reload.
        # A stream seed is `members` separate XADDs, every other type is one
        # command, so count the replies the builder actually produced rather than
        # assuming one per key.
        replies_per_key = members if kind == "stream" else 1
        for index in range(keys):
            sock.sendall(seed_command(kind, members, "src:%d" % index))
        for _ in range(keys * replies_per_key):
            reply, buf = read_reply(sock, buf)
            if reply.startswith(b"-"):
                raise RuntimeError("%s seed failed: %r" % (tag, reply))
        # Take the payload from the engine under test, so each arm restores a
        # payload its own DUMP produced rather than one translated between them.
        sock.sendall(resp("DUMP", "src:0"))
        payload, buf = read_reply(sock, buf)
        if not payload:
            raise RuntimeError("%s produced an empty DUMP" % tag)
        payload_len = len(payload)

        if op == "reload":
            # (frankenredis-qj6jn) DEBUG RELOAD is save+load of the WHOLE db, which
            # is a different route from single-key RESTORE: it additionally pays RDB
            # WRITE, the file round-trip, and one keyspace INSERT per key. The bead's
            # 3.31x is this route, so it cannot be evaluated with the RESTORE mode.
            #
            # Timed by instruction count rather than by the wall-clock harness
            # (collection_reload_headtohead.py) on purpose: that one needs a quiet
            # host and states so, and this host has been under a degraded fleet all
            # session. Instruction counts are load-immune.
            one = resp("DEBUG", "RELOAD")
            expect_err = b"-"
        else:
            one = resp("RESTORE", "dst", "0", payload, "REPLACE")
            expect_err = b"-"
        sock.sendall(one * ops)
        done = 0
        while done < ops:
            reply, buf = read_reply(sock, buf)
            if reply.startswith(expect_err):
                raise RuntimeError("%s %s error: %r" % (tag, op, reply))
            done += 1
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
    return ir, payload_len, exe_sha


def main():
    # (cross-project check) This harness divides every ratio by the vendored binary;
    # verify it IS its source before printing a denominator.
    require_incumbent(REDIS, os.path.join(ROOT, "legacy_redis_code/redis"))
    argv = [a for a in sys.argv[1:] if not a.startswith("--")]
    kind = "hash"
    op = "restore"
    for a in sys.argv[1:]:
        if a.startswith("--type="):
            kind = a.split("=", 1)[1]
        if a.startswith("--op="):
            op = a.split("=", 1)[1]
    if op not in ("restore", "reload"):
        raise SystemExit("unknown --op=%r (want restore|reload)" % op)
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
    # (frankenredis-33832) A/A NULL, in this same invocation. The ratio this
    # harness prints is a CROSS-PROCESS, cross-binary quantity, so the control
    # that authenticates it has to be the same shape: a SECOND, independent
    # two-point subtraction of the fr binary against itself, launched from the
    # same loop, on the same host, in the same window. Without it a printed
    # fr/redis figure and "I ran two arms that happened to differ" are
    # indistinguishable from inside the run.
    aa = "--aa" in sys.argv
    # `--keys=N`, not `--keys N`: the positional filter below is
    # `[a for a in sys.argv[1:] if not a.startswith("--")]`, so a separated value
    # survives as a fourth positional and the run dies on the usage line.
    keys = 1
    for a in sys.argv[1:]:
        if a.startswith("--keys="):
            keys = int(a.split("=", 1)[1])
    AA_BAND = 0.005  # 0.5 pct; the doc above measures this harness's fr arm at 0.03 pct
    port = 47800 + (os.getpid() % 200) * 4
    with tempfile.TemporaryDirectory(dir="/data/tmp") as workdir:
        results = {}
        shas = {}
        arms = [("fr", fr_bin), ("redis", REDIS)]
        if aa:
            # Same ELF, independent processes and ports: this arm's ONLY difference
            # from "fr" is that it is a different pair of processes.
            arms.append(("fr_aa", fr_bin))
        for idx, (name, binary) in enumerate(arms):
            base = port + idx * 2
            a, plen, sha_a = run(binary, name + ".n", base, members, ops, workdir, kind, op, keys)
            b, _, sha_b = run(binary, name + ".2n", base + 1, members, ops * 2, workdir, kind, op, keys)
            # A two-point subtraction across two different ELFs is not a
            # measurement of either one (`feedback_a_failed_build_leaves_a_stale_elf`).
            if sha_a != sha_b:
                raise SystemExit(
                    "%s: the N and 2N launches ran DIFFERENT binaries (%s vs %s) -- a peer "
                    "rebuilt under this run and the subtraction is void" % (name, sha_a, sha_b))
            results[name] = (b - a) / ops
            shas[name] = sha_a
            print("  %-6s Ir(N)=%-14d Ir(2N)=%-14d -> %10.1f instr/op  (payload %d B)"
                  % (name, a, b, results[name], plen))
        for name in shas:
            print("  %-6s in-process ELF sha256 %s" % (name, shas[name]))
        if aa:
            if shas["fr_aa"] != shas["fr"]:
                raise SystemExit("A/A arms ran different binaries -- null is void")
            null = results["fr_aa"] / results["fr"]
            verdict = "PASS" if abs(null - 1.0) <= AA_BAND else "FAIL"
            print("  A/A null (fr_aa/fr, same ELF, independent processes): %.6fx  [%s, band %.3f]"
                  % (null, verdict, AA_BAND))
            if verdict == "FAIL":
                print("  A/A OUTSIDE BAND -- do not quote the fr/redis ratio from this run.")
        print("  fr/redis instructions per op: %.4fx" % (results["fr"] / results["redis"]))
        # (frankenredis-gvm6z) Print the NOISE FLOOR next to the number, because this harness
        # had none and the natural fallback is wrong here by an order of magnitude.
        print("  A/A noise floor: sigma ~%.2f instr/op ABSOLUTE (size-independent); a delta "
              "needs ~%.0f instr/op to clear 2 sigma on one arm" % (NULL_SIGMA_INSTR,
                                                                   2 * NULL_SIGMA_INSTR))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
