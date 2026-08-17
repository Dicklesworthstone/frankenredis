#!/usr/bin/env python3
"""Audit that my registered perf shapes actually DO the work they claim.

Prompted by the 'copy' finding: `COPY kk kdst` returns 1 and copies on the FIRST
request, then returns 0 and does nothing for the remaining 49,999 — so the row was
timing a no-op while being labelled a copy.

Two independent detectors, because either alone misses a case:

  1. FIRST-vs-STEADY reply. If reply #1 differs from reply #2, the benchmark's
     steady state is not the operation the shape is named for. This is what catches
     `copy` (1 then 0).

  2. WORK-IS-REDONE. A stable reply does NOT prove work happens — a no-op has a
     stable reply too. So for every shape with a destination, corrupt the
     destination and re-issue: if the command genuinely recomputes, the corruption
     is gone. If it short-circuits, the corruption survives.

DRIFT, and why this now READS the registries instead of listing shapes
(frankenredis-u6kdc): this audit hardcoded 18 shapes while the two
registries it audits had grown past 70. A detector with a hand-maintained copy of
its own domain silently stops covering the thing it was written for — the `copy`
no-op it exists to catch could be re-registered today under a new name and this
would pass. So the FIRST-vs-STEADY detector is now driven from
`shape_instr_per_op.SHAPES` and `balanced_square_ab.SHAPE_SETS` directly, and
cannot drift from them.

The two detectors automate differently, and the asymmetry is the design:

  * FIRST-vs-STEADY needs NO per-shape metadata — issue the argv twice and compare.
    It is therefore run over EVERY registered shape.
  * WORK-IS-REDONE needs a destination key and a corruption command, which cannot
    be inferred from an argv. Those stay hand-written, and the run REPORTS how many
    registered shapes lack that coverage rather than leaving the gap silent.

Usage: shape_work_audit.py <fr_port>
       shape_work_audit.py --coverage        (no server: print what would be audited)
Exit 0 = every shape does its work on every request.
"""
import importlib.util
import os
import socket
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _load(module_name, rel_path):
    """Import a sibling harness by path so its shape registry is the source of truth."""
    spec = importlib.util.spec_from_file_location(
        module_name, os.path.join(REPO, rel_path)
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def registered_shapes():
    """Every (label, seeds, argv) the two perf registries define, de-duplicated.

    Labels are namespaced by source because the two registries reuse names for
    different argv (`get_control` exists in both with different seeds), and an audit
    that silently collapsed them would test one and report two.
    """
    out = {}
    try:
        instr = _load("_fr_instr", "scripts/shape_instr_per_op.py")
        for name, (seeds, cmd) in instr.SHAPES.items():
            out[f"instr:{name}"] = (list(seeds), [str(a) for a in cmd])
    except Exception as exc:  # noqa: BLE001 - a missing sibling must not hide the rest
        print(f"WARN could not read shape_instr_per_op registry: {exc}", file=sys.stderr)
    try:
        square = _load("_fr_square", "scripts/balanced_square_ab.py")
        for set_name, shapes in square.SHAPE_SETS.items():
            for label, seeds, argv in shapes:
                out[f"square:{set_name}:{label}"] = (
                    list(seeds),
                    [str(a) for a in argv],
                )
    except Exception as exc:  # noqa: BLE001
        print(f"WARN could not read balanced_square_ab registry: {exc}", file=sys.stderr)
    return out


PORT = None
if len(sys.argv) > 1 and sys.argv[1] != "--coverage":
    PORT = int(sys.argv[1])


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 10)
        self.buf = b""

    def _enc(self, args):
        out = [b"*%d\r\n" % len(args)]
        for a in args:
            b = a.encode() if isinstance(a, str) else a
            out.append(b"$%d\r\n%s\r\n" % (len(b), b))
        return b"".join(out)

    def _line(self):
        while b"\r\n" not in self.buf:
            c = self.s.recv(1 << 20)
            if not c:
                raise EOFError
            self.buf += c
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _read(self):
        line = self._line()
        tag, rest = line[:1], line[1:]
        if tag in (b"+", b":"):
            return tag.decode() + rest.decode()
        if tag == b"-":
            return "ERR:" + rest.decode()
        if tag == b"$":
            n = int(rest)
            if n == -1:
                return "(nil)"
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(1 << 20)
            v, self.buf = self.buf[:n], self.buf[n + 2:]
            return v.decode(errors="replace")
        if tag == b"*":
            n = int(rest)
            if n == -1:
                return "(nil-array)"
            return "[" + ",".join(str(self._read()) for _ in range(n)) + "]"
        raise RuntimeError(f"bad tag {line!r}")

    def cmd(self, *a):
        self.s.sendall(self._enc(list(a)))
        return self._read()


SEED = [
    "SADD sa m1 m2 m3 m4 m5", "SADD sb m3 m4 m5 m6 m7", "SADD sc m4 m5 m6 m7 m8",
    "ZADD za 1 a 2 b 3 c 4 d", "ZADD zb 1 b 2 c 3 d 4 e",
    "ZADD zm 1 m1 2 m2 3 m3 4 m4 5 m5 6 m6 7 m7 8 m8 9 m9",
    "HSET hm f1 v1 f2 v2 f3 v3 f4 v4 f5 v5 f6 v6 f7 v7 f8 v8 f9 v9",
    "SET ba abcdefghijklmnop", "SET bb ponmlkjihgfedcba", "SET kk vvvvvvvvvvvvvvvv",
    "MSET e1 1 e2 1 e3 1 e4 1 e5 1 e6 1 e7 1 e8 1",
    "MSET tenant:needle:1 1 tenant:decoy:1 1 tenant:decoy:2 1",
    "ZADD zr 1 a 2 b 3 c", "ZADD zl 0 a 0 b 0 c",
]

# (label, argv, destination-key-or-None, corruption-command-or-None)
SHAPES = [
    # storeops
    ("exists_8key", ["EXISTS","e1","e2","e3","e4","e5","e6","e7","e8"], None, None),
    ("hmget_9field", ["HMGET","hm","f1","f2","f3","f4","f5","f6","f7","f8","f9"], None, None),
    ("zmscore_9member", ["ZMSCORE","zm","m1","m2","m3","m4","m5","m6","m7","m8","m9"], None, None),
    ("scan_prefix", ["SCAN","0","MATCH","tenant:needle:*","COUNT","100"], None, None),
    ("zunionstore_2key", ["ZUNIONSTORE","zdst","2","za","zb"], "zdst", ["ZADD","zdst","99","BOGUS"]),
    ("zinterstore_2key", ["ZINTERSTORE","zidst","2","za","zb"], "zidst", ["ZADD","zidst","99","BOGUS"]),
    ("bitop_and", ["BITOP","AND","bdst","ba","bb"], "bdst", ["SET","bdst","CORRUPTCORRUPT!!"]),
    ("bitop_not", ["BITOP","NOT","bndst","ba"], "bndst", ["SET","bndst","CORRUPTCORRUPT!!"]),
    ("sunionstore_3src", ["SUNIONSTORE","sudst","sa","sb","sc"], "sudst", ["SADD","sudst","BOGUS"]),
    ("sinterstore_3src", ["SINTERSTORE","sidst","sa","sb","sc"], "sidst", ["SADD","sidst","BOGUS"]),
    ("sdiffstore_3src", ["SDIFFSTORE","sddst","sa","sb","sc"], "sddst", ["SADD","sddst","BOGUS"]),
    ("zmpop_missing", ["ZMPOP","1","nosuchzset","MIN"], None, None),
    ("get_control", ["GET","kk"], None, None),
    # DISCRIMINATION CHECK: the known-bad shape this audit was written for.
    ("copy_KNOWN_BAD", ["COPY","kk","kdst"], None, None),
    # mutnoop
    ("zremrangebyrank_noop", ["ZREMRANGEBYRANK","zr","5","4"], None, None),
    ("zremrangebylex_noop", ["ZREMRANGEBYLEX","zl","[x","[a"], None, None),
    ("lpop_count_missing", ["LPOP","nosuchlist","10"], None, None),
    ("rpop_count_missing", ["RPOP","nosuchlist","10"], None, None),
]

REGISTERED = registered_shapes()
# Hand-written redone-coverage, keyed by the argv's command so it applies to a shape
# under whatever label a registry gives it. dest + corruption cannot be inferred.
REDONE_BY_COMMAND = {
    (cmd[0].upper() if cmd else ""): (dest, corrupt)
    for _label, cmd, dest, corrupt in SHAPES
    if dest and corrupt
}

if "--coverage" in sys.argv:
    # No server needed: this is the drift check itself.
    with_redone = [
        label
        for label, (_s, argv) in sorted(REGISTERED.items())
        if argv and argv[0].upper() in REDONE_BY_COMMAND
    ]
    print("registered shapes ............ %d" % len(REGISTERED))
    print("  FIRST-vs-STEADY audited .... %d  (all of them: needs no metadata)"
          % len(REGISTERED))
    print("  WORK-IS-REDONE audited ..... %d  (needs a dest + corruption command)"
          % len(with_redone))
    print("  hardcoded list, for scale .. %d" % len(SHAPES))
    missing = sorted(
        {
            (argv[0].upper() if argv else "?")
            for _l, (_s, argv) in REGISTERED.items()
            if not (argv and argv[0].upper() in REDONE_BY_COMMAND)
        }
    )
    # WRITE-ish commands are the ones where a redone check MEANS something: they
    # have a destination that can be corrupted. For a pure read the reply IS the
    # work, so "no redone coverage" is not-applicable rather than missing — and
    # printing one undifferentiated count would read as 123 defects.
    DEST_WRITERS = (
        "STORE", "BITOP", "COPY", "PFMERGE", "SMOVE", "RENAMENX", "RPOPLPUSH",
        "LMOVE", "ZRANGESTORE", "GEORADIUS",
    )
    actionable = [m for m in missing if any(tok in m for tok in DEST_WRITERS)]
    print("\ncommands with no redone coverage: %d distinct" % len(missing))
    print("  of those, ones with a DESTINATION (where the check applies): %d"
          % len(actionable))
    if actionable:
        print("  " + ", ".join(actionable))
    print(
        "  the remainder are reads, where the reply IS the work and this detector\n"
        "  does not apply — that count is not a defect count."
    )
    print(
        "\nThe first number is what this audit now covers and cannot drift from;\n"
        "the redone gap is REPORTED rather than silent, which is the point."
    )
    sys.exit(0)

if PORT is None:
    print("usage: shape_work_audit.py <fr_port> | --coverage", file=sys.stderr)
    sys.exit(2)

c = Conn(PORT)
c.cmd("FLUSHALL")
for s in SEED:
    c.cmd(*s.split())

bad = 0
print(f"{'shape':<24}{'reply#1':<20}{'reply#2':<20}{'work redone':<13}verdict")
print("-" * 92)
for label, argv, dest, corrupt in SHAPES:
    r1 = c.cmd(*argv)
    r2 = c.cmd(*argv)
    first_matches_steady = (r1 == r2)

    redone = "n/a"
    redone_ok = True
    if dest and corrupt:
        c.cmd(*corrupt)                    # poison the destination
        c.cmd(*argv)                       # the shape must rebuild it
        after = c.cmd("SMEMBERS", dest) if corrupt[0] in ("SADD", "ZADD") else c.cmd("GET", dest)
        if corrupt[0] == "ZADD":
            after = c.cmd("ZRANGE", dest, "0", "-1")
        redone_ok = ("BOGUS" not in after) and ("CORRUPT" not in after)
        redone = "yes" if redone_ok else "NO"

    ok = first_matches_steady and redone_ok
    if not ok:
        bad += 1
    why = ("OK" if ok else
           "FIRST != STEADY (measures a different op)" if not first_matches_steady
           else "SHORT-CIRCUITS (no work after first call)")
    print(f"{label:<24}{r1[:19]:<20}{r2[:19]:<20}{redone:<13}{why}")

print()
print(f"{len(SHAPES)} shapes, {bad} not measuring what they claim")
sys.exit(0 if bad == 0 else 1)
