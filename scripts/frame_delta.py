#!/usr/bin/env python3
"""Per-frame instructions/op for a LADDER row that has ALREADY been measured.

`shape_instr_per_op.py` prints one number for the whole command plus a dispatch
share, then prints the path of the two callgrind dumps it produced and throws the
rest away. That leaves the interesting half on the floor: WHICH FRAMES the number
is made of. This differences those two existing dumps -- no re-run, no build, no
server -- and prints the marginal cost of one op attributed by function.

    frame_delta.py <dump_dir> [ops] [--top N] [--all]
    frame_delta.py <cg.n.out> <cg.2n.out> <ops> [--top N]

    # straight from a harness run:
    #   callgrind dumps: /data/tmp/fr_instr_rceg6q90
    frame_delta.py /data/tmp/fr_instr_rceg6q90

WHY IT SUBTRACTS TWO DUMPS. Startup, seeding and teardown appear identically in the
N-op and 2N-op dumps, so they cancel exactly and what is left is one op. A frame
with a large single-run cost and a ~zero delta is startup and is not your problem.
This is the same two-point method `shape_instr_per_op.py` uses for the whole-process
number, applied per frame, so the two reconcile -- and this script CHECKS that they do,
on every run, rather than asking you to trust it. That check is not decoration: it is the
only thing that caught annotated SOURCE LINES entering the frame table (see `annotate`),
a defect under which every individual frame still read correctly.

THIS IS THE INSTRUMENT THAT PRODUCED THE MECHANISM PROOF ON `frankenredis-cgeq5`,
and it is here because that row is not reproducible without it. On `sort_ro_alpha_64`
it printed, before -> after:

    CollationElements::next        37,044.0 -> 0
    CollatorBorrowed::compare      29,259.0 -> 0
    CollationElements::iter_next   10,836.0 -> 0
    CollationElements::init         5,418.0 -> 0
    core::str::converts::from_utf8  7,511.5 -> 0
    sort_alpha_compare              6,174.0 -> 7,071.0

90,068 removed, 897 added. A whole-command number cannot tell you that; it cannot
even tell you the change was the one you made.

DO NOT HAND-ROLL THE DUMP PARSER, which is the trap this script exists to keep
solved. Callgrind compresses names as `fn=(7) name` once and `fn=(7)` thereafter --
but a name's DEFINING occurrence can be a `cfn=` (called-function) line, and a later
`fn=(7)` then references it. A parser that reads names only from `fn=` lines leaves
most frames unresolved as `?7`, and an unresolved id is NOT comparable between two
dumps because the id numbering is per file: the same `?7` is two different functions
on the two sides, so the subtraction silently produces garbage rather than failing.
`callgrind_annotate` already handles all of that, so this shells out to it exactly
like `command_profile_frames.py` does.

Related: `command_profile_frames.py` does the same attribution but RUNS the workload
itself for an arbitrary command; use that when you have no dumps yet, and this when a
harness run already made them.
"""
import os
import re
import subprocess
import sys
import tempfile

# "12,345,678 ( 1.23%)  file:function" -- callgrind_annotate pads the percentage, so it
# can arrive as one token or as "(" plus "1.23%)". Match the whole prefix rather than
# splitting on whitespace: a demangled Rust name contains both spaces and colons.
FRAME_RE = re.compile(r"^\s*([\d,]+)\s+(?:\(\s*[\d.]+%\)\s+)?(\S.*?)\s*$")

# `callgrind_annotate` keys a row by `file:function`, and the file is where the
# INSTRUCTIONS live, not where the function is written -- so a function that inlined a
# callee from another crate is reported as SEVERAL rows, one per contributing file, and
# an `[/path/to/object]` suffix appears on some of them and not others. On a real
# profile `CollationElements::next` arrived as 16,254 (elements.rs) + 15,372
# (smallvec/lib.rs) + smaller pieces: reading the largest row as "the frame" would
# UNDER-REPORT it by 2.4x and would rank it below a function that happens not to inline.
# So rows are aggregated by FUNCTION. `--by-file` keeps the split when the question is
# actually about which inlined body costs what.
#
# The file part never contains `::` while a demangled Rust name always does, so the
# separator is the first single colon.
FUNC_RE = re.compile(r"^(?:[^:]|:(?=:))*?:(?P<fn>[^:].*)$")
OBJECT_SUFFIX_RE = re.compile(r"\s*\[[^\]]*\]\s*$")
CALL_COUNT_RE = re.compile(r"\(\s*[\d,]+x\)$")


def function_of(frame):
    """`file.rs:fr_command::foo [/path/elf]` -> `fr_command::foo`."""
    match = FUNC_RE.match(frame)
    name = match.group("fn") if match else frame
    return OBJECT_SUFFIX_RE.sub("", name)


def annotate(dump):
    """(frame -> self Ir, PROGRAM TOTALS). Self cost, not inclusive: a cost line that
    follows `calls=` is the callee's inclusive cost charged to the call SITE, and
    callgrind_annotate already excludes it from the caller's self figure."""
    # `--auto=no` IS LOAD-BEARING, and its absence is a silent wrong answer rather than
    # an error. With auto-annotation on, callgrind_annotate appends the SOURCE of every
    # hot file with each line's Ir count in the left column -- and a line of C
    # (`return __builtin___memcpy_chk (__dest, __src, __len,`) matches a frame regex
    # exactly as well as a real frame does. Measured on one real dump: 3,053,485 phantom
    # instructions from a `__memcpy_avx_unaligned_erms (16,052x)` call-count row, 84,594
    # from the `events annotated` summary and 82,332 from annotated source lines. The
    # frames then out-summed PROGRAM TOTALS by 430 instr/op while every individual frame
    # still read correctly -- which is why the reconciliation check below exists.
    proc = subprocess.run(
        ["callgrind_annotate", "--auto=no", "--threshold=100", dump],
        capture_output=True, text=True, check=True)
    costs, total = {}, None
    for line in proc.stdout.splitlines():
        match = FRAME_RE.match(line)
        if not match:
            continue
        ir, name = int(match.group(1).replace(",", "")), match.group(2)
        # PROGRAM TOTALS is the SUM of every frame below it. Counting it as a frame
        # double-counts the whole profile and halves every share.
        if name.startswith("PROGRAM TOTALS"):
            total = ir
            continue
        # Summary and call-count rows are not frames. `events annotated` closes the
        # auto-annotation block, and `some_frame (16,052x)` is a CALL COUNT for the
        # frame above it, carrying that frame's cost a second time.
        if (name.startswith("Ir ") or name.startswith("file:function")
                or name.endswith("annotated") or CALL_COUNT_RE.search(name)):
            continue
        costs[name] = costs.get(name, 0) + ir
    if not costs:
        raise RuntimeError("callgrind_annotate produced no frames for %s" % dump)
    return costs, total


def aggregate_by_function(costs):
    merged = {}
    for frame, ir in costs.items():
        name = function_of(frame)
        merged[name] = merged.get(name, 0) + ir
    return merged


def frame_deltas(dump_n, dump_2n, ops, by_file=False):
    """[(instr_per_op, frame)], plus the reconciling whole-process instr/op."""
    small, total_n = annotate(dump_n)
    large, total_2n = annotate(dump_2n)
    if not by_file:
        small, large = aggregate_by_function(small), aggregate_by_function(large)
    rows = [((large.get(name, 0) - small.get(name, 0)) / ops, name)
            for name in set(small) | set(large)]
    rows.sort(reverse=True)
    process = None
    if total_n is not None and total_2n is not None:
        process = (total_2n - total_n) / ops
    return rows, process


def resolve_inputs(args):
    """Accept either a harness dump DIRECTORY or two explicit dump files."""
    positional = [a for a in args if not a.startswith("--")]
    if positional and os.path.isdir(positional[0]):
        work = positional[0]
        pair = [os.path.join(work, "cg.fr.n.out"), os.path.join(work, "cg.fr.2n.out")]
        if not all(os.path.exists(p) for p in pair):
            # command_profile_frames.py names them differently; accept either layout.
            found = sorted(f for f in os.listdir(work) if f.endswith(".out"))
            if len(found) != 2:
                raise SystemExit(
                    "%s holds %d .out dumps; this needs exactly the N and 2N pair"
                    % (work, len(found)))
            pair = [os.path.join(work, f) for f in found]
        ops = int(positional[1]) if len(positional) > 1 else 2000
        return pair[0], pair[1], ops
    if len(positional) < 3:
        raise SystemExit(__doc__.strip().splitlines()[0] + "\n\n" + USAGE)
    return positional[0], positional[1], int(positional[2])


USAGE = ("Usage: frame_delta.py <dump_dir> [ops] [--top N] [--all] [--by-file]\n"
         "       frame_delta.py <cg.n.out> <cg.2n.out> <ops> [--top N]\n"
         "       frame_delta.py <dump_dir> --dispatch\n"
         "       frame_delta.py --self-test")

# Frames that are "getting to the command" rather than doing it. The first block is
# VERBATIM from `shape_instr_per_op.py`'s DISPATCH_FRAMES (frankenredis-rzdi8); if that
# list moves, move this one with it.
#
# THE SECOND BLOCK IS THIS FILE'S ADDITION, and the first entry in it is the reason to
# distrust any hand-maintained detector: `classify_borrowed_dispatch_floor_packet_impl`
# IS the floor classifier -- the central function of the entire front-classification
# campaign -- and the campaign's own metric was not counting it. Measured per-op on the
# cgeq5 dumps, the three missing frames are 219.0 instr/op on `SORT_RO ... ALPHA` (7.6%)
# and 174.0 on `get_control` (58%).
#
# THE ASYMMETRY IS THE DANGEROUS PART. These screens RANK commands against each other,
# and a 58% under-count on a cheap classified route against 7.6% on an expensive generic
# one systematically EXAGGERATES the gap between them -- which is exactly the quantity
# the "classified routes sit on a 14-28 pct floor" claim is made of.
#
# This list is still hand-maintained, so any number it produces is a FLOOR, not a value.
DISPATCH_FRAMES = (
    "process_buffered_frames", "execute_frame_internal", "command_table_index",
    "dispatch_with_client_context", "classify_command", "push_ascii_lowercase_lossy",
    "check_full_command_arity", "execute_dispatch", "parse_command_args_borrowed_into",
    "try_dispatch_floor_classified_action", "parse_borrowed_plain_",
    "effective_command_flags", "canonical_command_fullname",
    "dispatch_argv", "acl_permission_error_for_argv", "borrowed_fast_route_key",
    "Utf8Chunks", "resolve_command_spec", "lookup_command",
    # --- additions (frankenredis-7so0e), each a routing decision made BEFORE any of the
    # command's semantic work. Post-execution bookkeeping (`record_*_metrics`,
    # `CommandHistogramTracker`) and background cron (`run_active_expire_cycle`,
    # `drain_pending_pubsub`) are deliberately NOT here: they are neither dispatch nor
    # the command, and folding them in would make dispatch absorb the elapsed-time
    # residue that makes a small shape's A/A wide in the first place.
    "classify_borrowed_dispatch_floor_packet",   # 157.0 SORT_RO / 112.0 GET
    "parse_borrowed_dispatch_floor_",            # 51.0, constant across both
    "parser_config",                             # 11.0, constant across both
)


def dispatch_cost(rows):
    """(instr/op of dispatch, the frames that make it up), as a TWO-POINT delta.

    WHY THIS EXISTS RATHER THAN `shape_instr_per_op.py`'s `dispatch share`
    (frankenredis-cgeq5): that function takes the share from the **2N dump alone** --
    startup, seeding and teardown included -- and the caller multiplies it by a clean
    two-point instructions/op. Multiplying a share of one population by a rate from
    another is not a per-op quantity, and it moves with anything that changes how big
    the per-op part of the dump is. Its regex also requires a trailing ` [object]` on
    the row, dropping every frame without one from BOTH numerator and denominator.

    MEASURED, on the cgeq5 dump pairs: dispatch for `SORT_RO ... ALPHA` is
    **3,116.0 instr/op, bit-identical across n=3 and n=64 and across the before/after
    ELFs** (2,897.0 of it on the harness's own frame list, plus the 219.0 that list
    misses) -- which is what a per-call constant should look like, and the individual
    frames are identical too (execute_frame_internal 457.0, command_table_index 350.0,
    dispatch_with_client_context 330.0, classify_command 304.0, process_buffered_frames
    280.0, parse_command_args_borrowed_into 250.0, classify_borrowed_dispatch_floor_packet
    157.0). The share method reported 2,535.3 /
    2,048.3 / 3,358.5 / 2,517.9 for those same four arms: wrong by -12.5%, -29.3%,
    +15.9% and -13.1%, in BOTH directions, and it manufactured a 487 instr/op
    "reduction" from a change that never touched dispatch. `get_control` reads 299.0
    here against 206.5-239.7 there.

    Any front-classification target list ranked on the share figure is ranked on a
    number that is not per-op. Re-rank on this one before spending a build.
    """
    total = 0.0
    frames = [(ipo, name) for ipo, name in rows
              if any(frame in name for frame in DISPATCH_FRAMES)]
    for ipo, _ in frames:
        total += ipo
    return total, frames


# A frame whose delta is under this many instr/op is inlining noise, not a finding:
# callgrind is deterministic, but the optimizer can attribute one fixed cost to
# different frames between two builds, and the two-point subtraction of two large
# numbers leaves a small residue either way.
NOISE_FLOOR = 0.5


def self_test():
    """Synthetic dumps, so the arithmetic and the self-vs-inclusive split are pinned
    without needing a server, a build or a real profile.

    `alpha` calls `beta`, and the dump charges beta's 4,000 inclusive instructions to
    alpha's call site. alpha's SELF cost is 1,000. A reader that counted the call line
    would report 5,000 and would then attribute beta's whole cost twice."""
    def write(path, alpha, beta, startup):
        # `alpha` is split across TWO files, as a function that inlined a callee from
        # another crate really is: half its cost is filed under `inlined.rs`. Aggregating
        # by function has to put it back together, which is the defect this pins -- on a
        # real profile the split under-reported one frame by 2.4x.
        with open(path, "w") as out:
            out.write("version: 1\ncreator: callgrind-synthetic\npid: 4242\n"
                      "cmd: /synthetic/fr\npart: 1\n\npositions: line\nevents: Ir\n\n"
                      "fl=(1) synthetic.rs\nfn=(1) startup_only\n16 %d\n\n"
                      "fn=(2) alpha\n20 %d\ncfn=(3) beta\ncalls=1 24\n20 %d\n\n"
                      "fl=(2) inlined.rs\nfn=(2)\n30 %d\n\n"
                      "fl=(1) synthetic.rs\nfn=(3) beta\n24 %d\n\ntotals: %d\n"
                      % (startup, alpha - alpha // 2, beta, alpha // 2, beta,
                         startup + alpha + beta))

    work = tempfile.mkdtemp(prefix="frame_delta_selftest_")
    small = os.path.join(work, "cg.fr.n.out")
    large = os.path.join(work, "cg.fr.2n.out")
    # startup is IDENTICAL in both, which is the whole premise of the subtraction.
    write(small, alpha=1000, beta=4000, startup=7000)
    write(large, alpha=2000, beta=8000, startup=7000)

    rows, process = frame_deltas(small, large, 1000)
    got = {name: ipo for ipo, name in rows}
    failures = []
    # Aggregated by FUNCTION, so the `synthetic.rs:` file part is gone by here.
    for name, want in (("alpha", 1.0), ("beta", 4.0), ("startup_only", 0.0)):
        if name not in got:
            failures.append("%s: missing from the attribution entirely" % name)
        elif abs(got[name] - want) > 1e-9:
            failures.append("%s: want %.1f instr/op, got %r" % (name, want, got[name]))
    if abs(process - 5.0) > 1e-9:
        failures.append("process delta: want 5.0 instr/op, got %r" % process)
    # The frames must reconcile with the whole-process figure, which is the check that
    # catches a parser that drops or double-counts a frame.
    attributed = sum(ipo for ipo, _ in rows)
    if abs(attributed - process) > 1e-9:
        failures.append("frames sum to %.6f but the process moved %.6f" % (attributed, process))
    # Directory form must find the pair on its own.
    if resolve_inputs([work, "1000"])[2] != 1000:
        failures.append("directory form did not resolve the ops count")

    for line in failures:
        print("FAIL " + line)
    if failures:
        return 1
    print("PASS — self cost excludes the inclusive call line, startup cancels, "
          "frames reconcile with the process total")
    return 0


def main():
    args = sys.argv[1:]
    if "--self-test" in args:
        raise SystemExit(self_test())
    if not args or "--help" in args or "-h" in args:
        raise SystemExit(USAGE)
    top = None if "--all" in args else 40
    if "--top" in args:
        # Drop the flag AND its value by INDEX. Filtering by string value would eat a
        # positional that happened to be the same number -- `--top 2000` with 2000 ops.
        index = args.index("--top")
        top = int(args[index + 1])
        args = args[:index] + args[index + 2:]
    by_file = "--by-file" in args
    dump_n, dump_2n, ops = resolve_inputs(args)

    rows, process = frame_deltas(dump_n, dump_2n, ops, by_file=by_file)
    attributed = sum(ipo for ipo, _ in rows)
    if "--dispatch" in args:
        total, frames = dispatch_cost(rows)
        print("dispatch:      %10.1f instr/op   %.1f%% of %.1f  (TWO-POINT, not a share "
              "of one dump)" % (total, 100.0 * total / process, process))
        for ipo, name in sorted(frames, reverse=True):
            if abs(ipo) >= NOISE_FLOOR:
                print("%12.1f  %s" % (ipo, name))
        return
    print("whole process: %10.1f instr/op   (%s ops)"
          % (process if process is not None else float("nan"), ops))
    print("attributed:    %10.1f instr/op   across %d frames" % (attributed, len(rows)))
    if process is not None and abs(attributed - process) > max(1.0, 0.01 * abs(process)):
        print("WARNING: frames and PROGRAM TOTALS disagree by %.1f instr/op — read neither"
              % (attributed - process))
    print()
    shown = rows if top is None else rows[:top]
    for ipo, name in shown:
        if abs(ipo) < NOISE_FLOOR and top is not None:
            continue
        print("%12.1f  %s" % (ipo, name))
    hidden = len(rows) - len(shown)
    if hidden > 0:
        print("\n... %d further frames below the cut (--all to see them)" % hidden)


if __name__ == "__main__":
    main()
