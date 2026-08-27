#!/usr/bin/env python3
"""Per-frame callgrind profile of ONE fr arm doing hash RESTOREs.

(frankenredis-33832) Companion to scripts/restore_instr_per_op.py. That one gives
the fr/redis instr/op RATIO; this one keeps the callgrind dump so you can ask WHERE
the instructions go. Both exist because shape_instr_per_op.py cannot express RESTORE
at all -- its SHAPES table is whitespace-split strings and a DUMP payload is
arbitrary bytes.

Load-immune, like every instruction-count instrument here: no quiet window, no core
pinning. Verified while the host sat at loadavg 36.

    restore_profile_frames.py <fr_binary> <members> <ops> [--type=hash|list|set|zset]
    callgrind_annotate --threshold=60 <printed dump path>

WHAT IT ESTABLISHED, 2026-08-16, 128-field hash, 300 ops, ELF 2d5a352c (self cost,
27,083,795 Ir total):

    27.73 pct  fr_store::decode_rdb_string      (LZF decompress + bulk decode)
    18.51 pct  fr_persist::listpack::decode_value_spans
    13.83 pct  __memcpy_avx_unaligned_erms

AND THE POINT OF RECORDING IT: all three are already worked over or are parity work,
which is a negative result worth not rediscovering.

  - decode_rdb_string is dominated by LZF decompression, whose literal runs already
    use extend_from_slice and whose back-references already use chunked
    extend_from_within (frankenredis-5boi9). 33832's own attribution notes that most
    of its memcpys are LZF that Redis pays too -- parity work, not gap.
  - decode_value_spans already pre-sizes from the header element count, has its two
    per-element helpers inlined, and its span type is capped at 32 bytes by a
    compile-time assert.
  - the memcpy is the copy Redis performs as well.

So the remaining hash-RESTORE gap is NOT concentrated in the top three frames, which
is the evidence for 33832's standing conclusion that the structural item (b1o02 --
keep the listpack instead of re-packing it) is the only lever with a multiple in it.
Re-run this after any RESTORE change to check that conclusion still holds rather
than assuming it.
"""
import os
import socket
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def resp(*args):
    out = [b"*%d\r\n" % len(args)]
    for a in args:
        b = a if isinstance(a, bytes) else str(a).encode()
        out.append(b"$%d\r\n%s\r\n" % (len(b), b))
    return b"".join(out)


def read_reply(sock, buf):
    """Bulk-aware: DUMP's payload is binary and contains CRLF."""
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


KIND = "hash"
OP = "restore"
KEYS = 1


VARY_FIELDS = False


def main():
    global VARY_FIELDS
    global KIND, OP, KEYS
    for a in sys.argv[1:]:
        if a.startswith("--type="):
            KIND = a.split("=", 1)[1]
        if a.startswith("--op="):
            OP = a.split("=", 1)[1]
        if a == "--varyfields":
            VARY_FIELDS = True
        if a.startswith("--keys="):
            KEYS = int(a.split("=", 1)[1])
    argv = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(argv) != 3:
        print("usage: restore_profile_frames.py <fr_binary> <members> <ops>", file=sys.stderr)
        return 2
    fr, members, ops = argv[0], int(argv[1]), int(argv[2])
    # Resolve the binary BEFORE anything chdirs: these run the engine with
    # cwd set to a workdir, so a relative path like target/release/frankenredis
    # becomes "command not found" (rc=127) rather than an obvious error.
    fr = os.path.abspath(fr)
    # (cross-project check, 2026-08-16) franken_networkx measured through an
    # INSTALLED package that had drifted twelve days behind its repo and INVERTED a
    # ratio by 5.4x. Checking my own arm found the same class: a scratchpad binary
    # two commits stale, one of them worth -75.6 pct on the shape being discussed.
    # A stale arm yields a clean, reproducible, WRONG number, so warn before, not after.
    subprocess.run([sys.executable,
                    os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                 "assert_fresh_build.py"), fr], check=False)
    # (frankenredis-qj6jn) ONE DIRECTORY PER RUN. This used a single fixed workdir,
    # and fr-server configures an rdb path by default -- so `DEBUG RELOAD` leaves a
    # dump.rdb behind and the NEXT run's server LOADS it at startup. That put a
    # previous run's `src` key of a DIFFERENT TYPE into the keyspace, the seed
    # command then failed with WRONGTYPE (silently: the reply is read and not
    # checked), and the profile that came out described the stale container instead
    # of the requested one. It cost me a --type=set profile whose three largest
    # frames were LIST functions -- a clean, plausible, entirely wrong answer.
    # Nothing is deleted; each run simply gets its own directory.
    work = os.path.join(ROOT, "target", "restore_profile", "run-%d-%s" % (os.getpid(), KIND))
    os.makedirs(work, exist_ok=True)
    out = os.path.join(work, "cg.restore.out")
    port = 47900 + (os.getpid() % 90)

    proc = subprocess.Popen(
        ["valgrind", "--tool=callgrind", "--callgrind-out-file=" + out,
         # CG_EXTRA passes extra valgrind options, e.g. --dump-instr=yes to get
         # per-address attribution inside a frame. Empty by default, so an
         # unset run is byte-identical to before this existed.
         *os.environ.get("CG_EXTRA", "").split(), fr,
         "--port", str(port), "--save", "", "--appendonly", "no", "--dir", work,
         # DEBUG RELOAD needs the debug command admitted (--op=reload).
         "--enable-debug-command", "yes"],
        cwd=work, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sock = None
    try:
        for _ in range(600):
            if proc.poll() is not None:
                raise RuntimeError("server exited rc=%s" % proc.returncode)
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
            raise RuntimeError("server never became ready under callgrind")

        buf = b""
        # (frankenredis-gvm6z) Container type matters: the retained-listpack-span
        # path is reached from the LIST arm of the RESTORE payload walk, not the
        # hash arm. Profiling a hash workload to reason about that lever profiles a
        # path it never executes.
        #
        # (frankenredis-qj6jn) SET and ZSET for the same reason again. KEEP `members`
        # UNDER set-/zset-max-listpack-entries (128) for those two: above it the
        # container is hashtable-encoded and saves as the PLAIN RDB type, so the
        # listpack decode arm never runs and the profile silently describes a
        # different route.
        # (BlackThrush 2026-08-25) `--keys=N` seeds N containers rather than one.
        # DEBUG RELOAD saves and loads the WHOLE db, so a one-key profile charges
        # every per-RELOAD fixed cost against one container and the per-key frames
        # -- which is what the reload gap actually is -- barely register.
        for index in range(KEYS):
            key = "src:%d" % index
            if KIND == "stream":
                # (BlackThrush 2026-08-26) STREAM: `members` XADDs per key with
                # explicit IDs and two fields each -- the same seed
                # restore_instr_per_op.py uses, so this profile describes exactly
                # the workload its ratio was taken on.
                for i in range(members):
                    # `--varyfields`: distinct field NAMES per entry, which turns
                    # upstream's SAMEFIELDS flag off and grows the stream's field
                    # dictionary once per entry.
                    f0, f1 = (("f%04da" % i, "f%04db" % i) if VARY_FIELDS
                              else ("f0", "f1"))
                    sock.sendall(resp("XADD", key, "%d-1" % (i + 1),
                                      f0, "v%04d" % i, f1, "w%04d" % i))
                for _ in range(members):
                    seed_reply, buf = read_reply(sock, buf)
                    if seed_reply.startswith(b"-"):
                        raise RuntimeError("stream seed failed: %r" % seed_reply)
                continue
            if KIND == "list":
                sock.sendall(resp("RPUSH", key, *["v%04d" % i for i in range(members)]))
            elif KIND == "set":
                sock.sendall(resp("SADD", key, *["v%04d" % i for i in range(members)]))
            elif KIND == "zset":
                args = []
                for i in range(members):
                    args += [str(i), "v%04d" % i]
                sock.sendall(resp("ZADD", key, *args))
            else:
                fields = []
                for i in range(members):
                    fields += ["f%04d" % i, "v%04d" % i]
                sock.sendall(resp("HSET", key, *fields))
        # A stream key consumes its own XADD replies above; every other type
        # queued exactly one command per key.
        seed_reply = b"+OK"
        if KIND != "stream":
            for _ in range(KEYS):
                seed_reply, buf = read_reply(sock, buf)
                if seed_reply.startswith(b"-"):
                    break
        # And CHECK it. The swallowed error above is what made the stale-keyspace
        # contamination invisible rather than loud.
        if seed_reply.startswith(b"-"):
            raise RuntimeError("seed command failed: %r" % seed_reply)
        sock.sendall(resp("DUMP", "src:0"))
        payload, buf = read_reply(sock, buf)
        if not payload:
            raise RuntimeError("empty DUMP")

        # (frankenredis-qj6jn) `--op=reload` profiles DEBUG RELOAD (save+load of
        # the whole db), which is a different route from single-key RESTORE and the
        # one the 3.31x figure names.
        if OP == "reload":
            one = resp("DEBUG", "RELOAD")
        else:
            one = resp("RESTORE", "dst", "0", payload, "REPLACE")
        sock.sendall(one * ops)
        done = 0
        while done < ops:
            reply, buf = read_reply(sock, buf)
            if reply.startswith(b"-"):
                raise RuntimeError("RESTORE error: %r" % reply)
            done += 1
        print("ran %d RESTOREs of %d fields (payload %d B)" % (ops, members, len(payload)))
    finally:
        if sock is not None:
            sock.close()
        proc.terminate()
        try:
            proc.wait(timeout=180)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=60)

    # Callgrind writes the dump at process EXIT; anything that reads it earlier sees
    # an empty file and reports a profile of nothing.
    if not os.path.exists(out) or os.path.getsize(out) == 0:
        raise RuntimeError("no callgrind dump at %s -- the arm did not profile" % out)
    print("dump: %s" % out)
    print("next: callgrind_annotate --threshold=60 %s" % out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
