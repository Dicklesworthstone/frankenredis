#!/usr/bin/env python3
"""Per-frame, per-op callgrind attribution for one fr command shape.

WHY THIS EXISTS AND WHY IT IS NOT restore_profile_frames.py: that script profiles a
SINGLE run, so every frame it reports carries the server's startup cost. That is fine
when you only want the top-3 ranking of an expensive command, but it cannot answer
"how many instructions does frame X cost PER OP" -- startup contributes to the
numerator and never cancels.

This applies the house two-point subtraction (scripts/shape_instr_per_op.py) at the
FRAME level: run N ops and 2N ops in two separate callgrind sessions, then subtract
per function. Startup, seeding and teardown appear identically in both dumps and
cancel exactly, so what is left is the marginal cost of one op, attributed by frame.

Load-immune, like every instruction-count instrument here: instruction counts do not
move with loadavg or CPU MHz, so this needs no quiet window and no core pinning.

    command_profile_frames.py <fr_binary> <ops> [--seed 'CMD ARG ARG'] -- CMD ARG...

    command_profile_frames.py ./fr 2000 -- CONFIG GET maxmemory

ONE FRAME CAN APPEAR AS SEVERAL ROWS. callgrind_annotate keys a row by `file:function`,
and the file is where the INSTRUCTIONS live rather than where the function is written, so
a function that inlined a callee from another crate is split across one row per
contributing file. On a real profile `CollationElements::next` arrived as 16,254
(elements.rs) + 15,372 (smallvec/lib.rs) + smaller pieces, so reading the largest row as
"the frame" under-reports it by 2.4x. `frame_delta.py` aggregates by function and is the
better reader when a frame's TOTAL cost is the question; this script keeps the file split
because knowing which inlined body costs what is often the point of running it.

READ THE OUTPUT THIS WAY: a frame's delta is what ONE op adds to it. A frame with a
large single-run cost but a ~zero delta is startup and is not your problem. Negative
deltas are noise around zero -- callgrind is deterministic, but inlining decisions can
attribute a fixed cost to different frames between runs, so treat anything under about
1 percent of the total delta as unresolved rather than real.
"""
import os
import re
import shutil
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
    while b"\r\n" not in buf:
        chunk = sock.recv(1 << 20)
        if not chunk:
            raise RuntimeError("server closed mid-reply")
        buf += chunk
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
    if tag == b"*":
        n = int(line[1:])
        # (frankenredis-e6c9t) CONFIG GET replies are ARRAYS. A reply reader that
        # counts CRLFs instead of parsing them terminates the send loop early on any
        # payload-carrying reply and silently profiles fewer ops than it claims.
        if n == -1:
            return b"", rest
        for _ in range(n):
            _, rest = read_reply(sock, rest)
        return line, rest
    raise RuntimeError("unexpected reply tag %r" % tag)


# "12,345,678 ( 1.23%)  file:function" -- callgrind_annotate pads the percentage, so it can
# arrive as one token or as "(" plus "1.23%)". Match the whole prefix rather than splitting,
# and keep everything after it: a demangled Rust name contains spaces and colons.
FRAME_RE = re.compile(r"^\s*([\d,]+)\s+(?:\(\s*[\d.]+%\)\s+)?(\S.*?)\s*$")


# A row that is not a frame but matches FRAME_RE anyway: `some_frame (16,052x)` is a
# CALL COUNT printed under the frame it belongs to, carrying that frame's cost a second
# time, and `... events annotated` closes the auto-annotation block.
NON_FRAME_RE = re.compile(r"\(\s*[\d,]+x\)$|events annotated$")


def annotate(dump):
    """func -> self Ir, from callgrind_annotate. Excludes the PROGRAM TOTALS pseudo-row.

    `--auto=no` IS LOAD-BEARING (frankenredis-cgeq5). Without it callgrind_annotate
    appends the SOURCE of every hot file with each line's Ir count in the left column,
    and a line of C matches FRAME_RE exactly as well as a real frame does -- e.g.
    `return __builtin___memcpy_chk (__dest, __src, __len,`. Measured on two real dumps
    from `shape_instr_per_op.py`, that fed in 3,222,800 and 3,222,697 phantom
    instructions (0.57% and 1.57% of the profile), the bulk of it one
    `__memcpy_avx_unaligned_erms (16,052x)` call-count row worth ~760 Ir/op of fake
    frame -- enough to rank inside the top ten of a SORT profile. The shares printed by
    this script divide by PROGRAM TOTALS and were therefore always honest; the FRAME
    TABLE was not, and nothing in the output said so. The reconciliation check in main()
    is what makes a recurrence loud."""
    proc = subprocess.run(
        ["callgrind_annotate", "--auto=no", "--threshold=100", dump],
        capture_output=True, text=True, check=True)
    costs, total = {}, None
    for line in proc.stdout.splitlines():
        m = FRAME_RE.match(line)
        if not m:
            continue
        ir, name = int(m.group(1).replace(",", "")), m.group(2)
        if NON_FRAME_RE.search(name):
            continue
        # PROGRAM TOTALS is the SUM of every frame below it. Counting it as a frame
        # double-counts the whole profile and makes every share exactly half of what it
        # should be, which is how the first run of this script reported a 45 pct "frame".
        if name.startswith("PROGRAM TOTALS"):
            total = ir
            continue
        costs[name] = costs.get(name, 0) + ir
    if not costs:
        raise RuntimeError("callgrind_annotate produced no frames for %s" % dump)
    return costs, total


def run(fr, ops, seed, cmd, work, tag):
    out = os.path.join(work, "cg.%s.out" % tag)
    if os.path.exists(out):
        os.remove(out)
    # (frankenredis-h9h8m / frankenredis-pcio8, found by CrimsonHawk) ONE FRESH DIRECTORY
    # PER POINT. Both points used to run in the shared `work` dir, as both `cwd` and
    # `--dir`. A slope's two points must differ in NOTHING but the repeated operation, and
    # that includes the filesystem state they start from: any profiled workload that
    # persists — DEBUG RELOAD, SAVE/BGSAVE, BGREWRITEAOF, or a `--seed` that writes —
    # leaves state the 2N point then starts by LOADING, so the 2N run does one extra
    # startup load the N run did not, and it lands in the slope as if it were per-op cost.
    #
    # `--save ""` and `--appendonly no` stop only AUTOMATIC persistence, not a command the
    # caller explicitly profiles, and this tool takes an arbitrary command.
    #
    # WHY THE RECONCILIATION CHECK BELOW CANNOT CATCH IT: that check asserts the frame
    # table sums to PROGRAM TOTALS, and under this contamination it still does — the extra
    # load is real work that really happened, counted honestly on both sides. A re-run does
    # not disagree either, because callgrind Ir is deterministic; `qj6jn` records the wrong
    # number reproducing perfectly. What caught it there was an ARITHMETIC IMPOSSIBILITY in
    # a by-product: a frame marginal (10,583) exceeding the whole-program marginal (8,756).
    # That assertion is now in `frame_delta.py` as a second, strictly-different guard.
    point = os.path.join(work, "point.%s" % tag)
    shutil.rmtree(point, ignore_errors=True)
    os.makedirs(point, exist_ok=True)
    port = 47000 + (os.getpid() % 900) + (0 if tag == "n" else 1)
    proc = subprocess.Popen(
        ["valgrind", "--tool=callgrind", "--callgrind-out-file=" + out, fr,
         "--port", str(port), "--save", "", "--appendonly", "no", "--dir", point],
        cwd=point, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sock = None
    try:
        for _ in range(600):
            if proc.poll() is not None:
                raise RuntimeError("server exited rc=%s" % proc.returncode)
            try:
                sock = socket.create_connection(("127.0.0.1", port), timeout=5)
                sock.settimeout(900)
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
        if seed:
            sock.sendall(resp(*seed))
            reply, buf = read_reply(sock, buf)
            if reply.startswith(b"-"):
                raise RuntimeError("seed error: %r" % reply)

        one = resp(*cmd)
        sock.sendall(one * ops)
        for _ in range(ops):
            reply, buf = read_reply(sock, buf)
            if reply.startswith(b"-"):
                raise RuntimeError("command error: %r" % reply)
    finally:
        if sock is not None:
            sock.close()
        proc.terminate()
        try:
            proc.wait(timeout=300)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=60)
    # Callgrind writes its dump at process EXIT; reading earlier profiles nothing.
    if not os.path.exists(out) or os.path.getsize(out) == 0:
        raise RuntimeError("no callgrind dump at %s -- the arm did not profile" % out)
    return annotate(out)


def main():
    argv = sys.argv[1:]
    if "--" not in argv:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        print("usage: command_profile_frames.py <fr_binary> <ops> [--seed 'CMD ...'] "
              "-- CMD ARG...", file=sys.stderr)
        return 2
    split = argv.index("--")
    head, cmd = argv[:split], argv[split + 1:]
    if len(cmd) == 0 or len(head) < 2:
        print("usage: command_profile_frames.py <fr_binary> <ops> [--seed 'CMD ...'] "
              "-- CMD ARG...", file=sys.stderr)
        return 2
    fr, ops = os.path.abspath(head[0]), int(head[1])
    seed = None
    if "--seed" in head:
        seed = head[head.index("--seed") + 1].split()

    subprocess.run([sys.executable, os.path.join(ROOT, "scripts", "assert_fresh_build.py"), fr],
                   check=False)

    work = os.path.join(ROOT, "target", "cmd_profile")
    os.makedirs(work, exist_ok=True)

    print("profiling %r at N=%d and 2N=%d ..." % (" ".join(cmd), ops, 2 * ops), flush=True)
    a, a_total = run(fr, ops, seed, cmd, work, "n")
    b, b_total = run(fr, 2 * ops, seed, cmd, work, "2n")

    deltas = {}
    for func in set(a) | set(b):
        deltas[func] = (b.get(func, 0) - a.get(func, 0)) / ops
    # The authority for the denominator is PROGRAM TOTALS, not the sum of positive frame
    # deltas: a frame whose cost MOVED (inlining attributing it elsewhere) shows up as a
    # negative delta, and dropping those from the denominator inflates every share.
    total = (b_total - a_total) / ops
    # The frames must ADD UP to the process. They did not for months: annotated source
    # lines were entering the table as frames (see annotate()), and every individual row
    # still looked plausible. A sum that overshoots PROGRAM TOTALS is the only signal
    # that would have caught it, so it is now checked on every run rather than trusted.
    attributed = sum(deltas.values())
    if abs(attributed - total) > max(1.0, 0.005 * abs(total)):
        print("\nWARNING: frames sum to %.1f Ir/op but PROGRAM TOTALS moved %.1f — the "
              "frame table is not the process; read neither until that is explained."
              % (attributed, total))
    print("\nTOTAL marginal cost: %10.1f Ir/op   (2N PROGRAM TOTALS minus N, over N)" % total)
    print("%10s  %7s  %s" % ("Ir/op", "share", "frame (self cost)"))
    print("-" * 100)
    shown = 0.0
    for func, ir in sorted(deltas.items(), key=lambda kv: -kv[1]):
        if ir < total * 0.005:
            break
        shown += ir
        print("%10.1f  %6.2f%%  %s" % (ir, 100.0 * ir / total, func[:78]))
    print("-" * 100)
    print("%10.1f  %6.2f%%  shown above; the remainder is spread below the 0.5%% cut"
          % (shown, 100.0 * shown / total))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
