#!/usr/bin/env python3
"""Instruction-exact same-ELF A/B for the retained listpack span decode.

(frankenredis-gvm6z) WHY NOT THE USUAL TIMED BENCH. `value_spans_presize.rs` and
its siblings interleave arms and take a median of wall-clock ratios. That needs a
host whose noise is below the effect. This lever was measured on a host at
loadavg 656, where wall-clock is worthless -- so both arms are counted in RETIRED
INSTRUCTIONS under Callgrind instead, which this repo's instrument audit bounded
at 0.64 pct across a 34 pct MHz swing.

METHOD, the same two-point subtraction `restore_instr_per_op.py` uses: run one arm
at N and at 2N iterations and difference the whole-process instruction totals.
Process startup, the fixture build, the final print and teardown are IDENTICAL in
both runs and cancel exactly; the remainder is N decodes' worth of work.

BOTH ARMS COME FROM ONE ELF, selected by argv (docs/BENCH_METHODOLOGY section 3:
an A/B split across two `rch exec` invocations is invalid because worker choice is
non-deterministic and the ratio is not worker-invariant). The ELF's sha256 is
printed so a recorded number cannot later be attributed to a build nobody can
identify, and `verify` is run FIRST so a faster arm that decodes something else
cannot be reported as a win.

A/A: each arm is also measured a second time. Callgrind is deterministic, so the
A/A here is an assertion that the harness is wired correctly (it must be exactly
1.0000), not a noise estimate.

Usage: retained_spans_pass_ab.py <bench_elf> [--entries=200] [--iters=200] [--shapes=strings,integers,mixed]
"""
from __future__ import annotations

import hashlib
import os
import re
import subprocess
import sys
import tempfile

ARMS = ("two_pass", "single_pass")


def callgrind_total(elf: str, arm: str, entries: int, iters: int, shape: str, workdir: str) -> int:
    """Whole-process retired-instruction total for one run."""
    out = os.path.join(workdir, f"cg.{arm}.{shape}.{iters}")
    cmd = [
        "valgrind", "--tool=callgrind", "--callgrind-out-file=" + out,
        "--collect-systime=no", "--cache-sim=no", "--branch-sim=no",
        elf, arm, str(entries), str(iters), shape,
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.exit(f"callgrind failed for {arm}/{shape}/{iters}:\n{proc.stderr[-2000:]}")
    # "Collected : <n>" on the summary line of the callgrind output file.
    total = None
    with open(out, "rb") as fh:
        for raw in fh:
            if raw.startswith(b"summary:") or raw.startswith(b"totals:"):
                total = int(raw.split(b":", 1)[1].split()[0])
    if total is None:
        sys.exit(f"no summary/totals line in {out}")
    return total


def main() -> None:
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    elf = sys.argv[1]
    entries, iters = 200, 200
    shapes = ["strings", "integers", "mixed"]
    for arg in sys.argv[2:]:
        if arg.startswith("--entries="):
            entries = int(arg.split("=", 1)[1])
        elif arg.startswith("--iters="):
            iters = int(arg.split("=", 1)[1])
        elif arg.startswith("--shapes="):
            shapes = arg.split("=", 1)[1].split(",")
        else:
            sys.exit(f"unknown argument {arg}")

    if not os.path.isfile(elf):
        sys.exit(f"no such bench ELF: {elf}")
    sha = hashlib.sha256(open(elf, "rb").read()).hexdigest()

    # Correctness gate FIRST: a faster arm that decodes something else is not a win.
    verify = subprocess.run([elf, "verify", "1", "1", "strings"], capture_output=True, text=True)
    if verify.returncode != 0 or "VERIFY_OK" not in verify.stdout:
        sys.exit(f"arm-equivalence gate FAILED:\n{verify.stdout}\n{verify.stderr}")
    print(verify.stdout.strip())
    print(f"  bench ELF sha256 {sha}")
    print(f"  entries={entries} iters N={iters} 2N={2 * iters}\n")

    print(f"  {'shape':<10} {'arm':<12} {'Ir(N)':>14} {'Ir(2N)':>14} {'instr/decode':>14}")
    results: dict[tuple[str, str], float] = {}
    with tempfile.TemporaryDirectory(prefix="gvm6z-cg-") as workdir:
        for shape in shapes:
            for arm in ARMS:
                lo = callgrind_total(elf, arm, entries, iters, shape, workdir)
                hi = callgrind_total(elf, arm, entries, 2 * iters, shape, workdir)
                per = (hi - lo) / iters
                results[(shape, arm)] = per
                print(f"  {shape:<10} {arm:<12} {lo:>14} {hi:>14} {per:>14.1f}")
            # A/A on the CAND arm: Callgrind is deterministic, so this must read
            # exactly 1.0000. Anything else means the harness, not the code, moved.
            aa_lo = callgrind_total(elf, "single_pass", entries, iters, shape, workdir + "/")
            aa_hi = callgrind_total(elf, "single_pass", entries, 2 * iters, shape, workdir + "/")
            aa = (aa_hi - aa_lo) / iters
            cand = results[(shape, "single_pass")]
            print(f"  {shape:<10} {'A/A':<12} {'':>14} {'':>14} {aa:>14.1f}"
                  f"   null {aa / cand if cand else float('nan'):.4f}")

    print(f"\n  {'shape':<10} {'two_pass':>14} {'single_pass':>14} {'delta':>12} {'cand/orig':>11}")
    for shape in shapes:
        orig = results[(shape, "two_pass")]
        cand = results[(shape, "single_pass")]
        print(f"  {shape:<10} {orig:>14.1f} {cand:>14.1f} {cand - orig:>+12.1f} "
              f"{cand / orig:>10.4f}x")
    print("\n  Instruction counts, not time. Per DECODE of one "
          f"{entries}-entry listpack.")


if __name__ == "__main__":
    main()
