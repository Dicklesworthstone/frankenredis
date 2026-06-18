#!/usr/bin/env python3
"""quicklist_dump_boundary_differ.py — DUMP node-boundary parity probe vs redis 7.2.4.

Targets the quicklist node-split predicate (frankenredis-s36di /
frankenredis-quicklist-node-overhead). Redis `_quicklistNodeAllowInsert` sizes a
trial node with `new_sz = node->sz + RAW_value_len + SIZE_ESTIMATE_OVERHEAD` (the
overhead is 8 in vendored 7.2.4), NOT the precise listpack-encoded entry length.
Using the encoded length packs a few extra bytes per node before splitting, so a
large list's DUMP node boundaries (and DEBUG OBJECT serializedlength) drift from
redis even though the values round-trip fine.

This probe builds random mixed integer/string lists large enough to span multiple
quicklist nodes and compares, byte-for-byte:
  * the raw DUMP payload, and
  * DEBUG OBJECT serializedlength (== rdbSavedObjectLen, the node-structure tell).

Element sizes are capped below the ~8192-byte plain-node threshold so any
divergence is the packed-node boundary predicate (the s36di bug), not the
separate large-element plain-vs-packed question.

Usage:
    legacy_redis_code/redis/src/redis-server --port 16399 --save '' --appendonly no \
        --daemonize yes --enable-debug-command yes
    frankenredis --port 16400 --mode strict --enable-debug-command yes
    scripts/quicklist_dump_boundary_differ.py --oracle 16399 --fr 16400 [--seed N] [--trials N] [--diagnostic]
    scripts/quicklist_dump_boundary_differ.py 16399 16400 [seed] [trials]   # positional form

By default this is a hard regression gate: frankenredis-s36di is closed, so any
DUMP or serializedlength divergence exits 1 and fails the auto-discovering
scripts/run_parity_differs.sh suite. Pass --diagnostic for exploratory sweeps
that should print divergences without failing the process.
"""
import argparse
import re
import socket
import subprocess
import sys

ORACLE_DEFAULT = 16399
FR_DEFAULT = 16400
RC = "legacy_redis_code/redis/src/redis-cli"


def cli(port, *args):
    return subprocess.run(
        [RC, "-p", str(port), *[str(a) for a in args]],
        capture_output=True,
    ).stdout


def dump_raw(port, key):
    """One DUMP reply, raw bulk bytes (None for a nil reply)."""
    s = socket.socket()
    s.connect(("127.0.0.1", port))
    s.sendall(b"*2\r\n$4\r\nDUMP\r\n$%d\r\n%s\r\n" % (len(key), key.encode()))
    buf = b""
    while b"\r\n" not in buf:
        chunk = s.recv(4096)
        if not chunk:
            break
        buf += chunk
    hdr, rest = buf.split(b"\r\n", 1)
    if hdr[:1] != b"$" or int(hdr[1:]) < 0:
        s.close()
        return None
    n = int(hdr[1:])
    while len(rest) < n + 2:
        chunk = s.recv(4096)
        if not chunk:
            break
        rest += chunk
    s.close()
    return rest[:n]


def serializedlength(port, key):
    out = cli(port, "debug", "object", key).decode(errors="replace")
    m = re.search(r"serializedlength:(\d+)", out)
    return m.group(1) if m else None


def rng(seed):
    """Tiny deterministic LCG so runs are reproducible without importing state."""
    state = (seed * 2862933555777941757 + 3037000493) & ((1 << 64) - 1)

    def nxt(bound):
        nonlocal state
        state = (state * 2862933555777941757 + 3037000493) & ((1 << 64) - 1)
        return (state >> 17) % bound

    return nxt


def build(port, key, elems):
    cli(port, "del", key)
    for i in range(0, len(elems), 50):
        cli(port, "rpush", key, *elems[i : i + 50])


def parse_args(argv):
    """Accept both the flag form (--oracle/--fr, used by run_parity_differs.sh)
    and the positional form (<oracle> <fr> [seed] [trials])."""
    ap = argparse.ArgumentParser(add_help=True)
    ap.add_argument("--oracle", type=int, default=None)
    ap.add_argument("--fr", type=int, default=None)
    ap.add_argument("--seed", type=int, default=None)
    ap.add_argument("--trials", type=int, default=None)
    ap.add_argument("--diagnostic", action="store_true",
                    help="exit 0 on divergence for exploratory sweeps; default is a hard gate")
    # Backward-compatible no-op for old command lines. The default is already
    # strict, but accepting this avoids turning stale docs into infra failures.
    ap.add_argument("--strict", action="store_true", help=argparse.SUPPRESS)
    ap.add_argument("rest", nargs="*", type=int)
    a = ap.parse_args(argv)
    pos = a.rest
    oracle = a.oracle if a.oracle is not None else (pos[0] if len(pos) > 0 else ORACLE_DEFAULT)
    fr = a.fr if a.fr is not None else (pos[1] if len(pos) > 1 else FR_DEFAULT)
    seed = a.seed if a.seed is not None else (pos[2] if len(pos) > 2 else 2026)
    trials = a.trials if a.trials is not None else (pos[3] if len(pos) > 3 else 600)
    hard_gate = a.strict or not a.diagnostic
    return oracle, fr, seed, trials, hard_gate


def main():
    oracle, fr, seed, trials, hard_gate = parse_args(sys.argv[1:])

    nxt = rng(seed)
    alphabet = "abcXYZ0123456789"
    size_buckets = [5, 40, 120, 500, 2000]
    len_buckets = [3, 10, 70, 130, 260, 500, 900]

    dump_div = 0
    slen_div = 0
    checked = 0
    for trial in range(trials):
        n = len_buckets[nxt(len(len_buckets))]
        elems = []
        for _ in range(n):
            if nxt(100) < 45:
                width = 1 + nxt(18)
                val = nxt(10 ** width)
                if nxt(2):
                    val = -val
                elems.append(str(val))
            else:
                ln = 1 + nxt(size_buckets[nxt(len(size_buckets))])
                elems.append("".join(alphabet[nxt(len(alphabet))] for _ in range(ln)))
        key = f"qlb{trial}"
        build(oracle, key, elems)
        build(fr, key, elems)
        do, df = dump_raw(oracle, key), dump_raw(fr, key)
        so, sf = serializedlength(oracle, key), serializedlength(fr, key)
        checked += 1
        if do != df:
            dump_div += 1
            if dump_div <= 5:
                eo = cli(oracle, "object", "encoding", key).strip().decode()
                ef = cli(fr, "object", "encoding", key).strip().decode()
                print(
                    f"DUMP DIVERGE trial={trial} n={n} enc={eo}/{ef} "
                    f"len={len(do) if do else None}/{len(df) if df else None}"
                )
        # serializedlength only meaningful when both report it (fr may omit on a
        # non-quicklist encoding); a mismatch is the node-structure tell.
        if so is not None and sf is not None and so != sf:
            slen_div += 1
            if slen_div <= 5:
                print(f"SERIALIZEDLENGTH DIVERGE trial={trial} n={n} oracle={so} fr={sf}")

    print("-" * 60)
    print(
        f"checked={checked} DUMP divergences={dump_div} "
        f"serializedlength divergences={slen_div}"
    )
    if dump_div or slen_div:
        if hard_gate:
            print("FAIL — quicklist DUMP node boundaries differ from redis 7.2.4 (see frankenredis-s36di)")
            return 1
        print("WARNING — quicklist DUMP node boundaries differ from redis 7.2.4 "
              "(diagnostic mode; frankenredis-s36di is closed, omit --diagnostic to gate)")
        return 0
    print("PASS — quicklist DUMP node boundaries byte-exact vs redis 7.2.4")
    return 0


if __name__ == "__main__":
    sys.exit(main())
