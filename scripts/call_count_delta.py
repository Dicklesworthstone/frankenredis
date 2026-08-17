#!/usr/bin/env python3
"""Two-point CALL COUNTS per op from a pair of callgrind dumps.

`frame_delta.py` differences the two dumps for INSTRUCTIONS per op. This differences
them for CALLS per op, which is a different question and, for at least one banked
row, the decisive one: `AzureMouse`'s Timespec REJECT closed four hypotheses and its
retry predicate is stated as a count -- "re-open only if its uprobe count exceeds
1.10 calls/op" -- precisely because a share, a rate, or a cycles/op figure could not
separate "called twice" from "expensive once".

Same two-point subtraction as everywhere else in this campaign: startup, seeding and
teardown appear identically in the N-op and 2N-op dumps and cancel exactly, so
(calls_2N - calls_N) / N is the marginal call count of one operation.

In callgrind's format a call site is `cfn=(id) name` followed by `calls=<n> <line>`
and then the inclusive cost line. Name definitions are interned: `cfn=(id) name`
declares, and a later bare `cfn=(id)` refers back. Both forms are resolved here --
missing the bare form would undercount every callee after its first appearance.

    call_count_delta.py <dump_dir> <ops> <substring> [<substring>...]

    # straight from a harness run:
    #   callgrind dumps: /data/tmp/fr_instr_mfbx7k4w
    call_count_delta.py /data/tmp/fr_instr_mfbx7k4w 2000 Timespec sub_timespec

WHAT IT HAS ALREADY SETTLED, so the next reader knows what a count buys:

  * `AzureMouse`'s Timespec REJECT, whose retry predicate is "re-open only if its
    uprobe count exceeds 1.10 calls/op". Measured on a fresh ELF and a DIFFERENT
    command than that row used (ZCARD, not INCR): `Timespec::now`, `sub_timespec`
    and `clock_gettime` are ALL exactly 1.0000 calls/op. The lever stays closed --
    the ~120 instr/op is one vDSO call, not a duplicated one.

  * The write-gate campaign's MECHANISM, which several rows asserted from
    instruction deltas alone. A converted floor arm calls
    `plain_borrowed_default_key_write_allows` 0.0000 times/op (once per buffered
    pass, which rounds to zero at 2000 ops per pass) and an unconverted one calls
    it 1.0000 times/op. That is the amortisation, counted rather than inferred.
"""
import os
import re
import sys

CFN_RE = re.compile(r"^(c?fn)=\((\d+)\)(?:\s+(.*))?$")


def counts(path, wanted):
    names = {}
    totals = {w: 0 for w in wanted}
    current = None
    with open(path, "rb") as handle:
        for raw in handle:
            line = raw.decode("utf-8", "replace").rstrip("\n")
            match = CFN_RE.match(line)
            if match:
                kind, num, name = match.group(1), match.group(2), match.group(3)
                if name:
                    names[num] = name
                # A bare `cfn=(id)` refers to an already-declared name.
                current = names.get(num, "") if kind == "cfn" else None
                continue
            if current and line.startswith("calls="):
                n = int(line.split("=", 1)[1].split()[0])
                for w in wanted:
                    if w in current:
                        totals[w] += n
                current = None
    return totals


def main():
    directory, ops = sys.argv[1], int(sys.argv[2])
    wanted = sys.argv[3:]
    dumps = sorted(
        os.path.join(directory, f) for f in os.listdir(directory)
        if "callgrind" in f or f.startswith("cg")
    )
    if len(dumps) != 2:
        print(f"expected 2 dumps in {directory}, found {len(dumps)}: "
              f"{[os.path.basename(d) for d in dumps]}")
        return 1
    per = [counts(d, wanted) for d in dumps]
    print(f"dumps: {[os.path.basename(d) for d in dumps]}   ops={ops}")
    for w in wanted:
        lo, hi = sorted(v[w] for v in per)
        print(f"  {w:30s} {lo:>9} -> {hi:>9}   delta {hi - lo:>8}"
              f"   CALLS/OP {(hi - lo) / ops:.4f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
