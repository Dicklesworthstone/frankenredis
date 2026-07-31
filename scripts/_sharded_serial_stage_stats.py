#!/usr/bin/env python3
"""Reduce per-thread perf counts into a serial-stage census.

Input is the TSV emitted by scripts/sharded_serial_stage_census.sh --
one row per (worker count, round): W, round, elapsed_s, perf_csv_path, fr_pid.

The headline is INSTRUCTIONS PER OPERATION ON THE EVENT-LOOP THREAD. The event
loop is FrankenRedis's serial stage in both modes: sharded execution moves the
store hit onto workers but leaves reading, parsing, routing, reply ordering and
writing exactly where they were. Amdahl's law then bounds the whole path by that
one thread, so the ceiling for ANY worker count is

    ceiling = evloop_instr_per_op(W=0) / evloop_instr_per_op(W=N)

A ceiling below 1.0 says the serial stage grew and the design cannot win at any
W. That is a structural verdict, not a tuning observation.

Every figure is a ratio of counts over an EXACT denominator (redis-benchmark
-t set,get -n N issues exactly 2N operations), so none of it moves with host
load, client speed or core identity. The median over rounds is reported with the
observed min/max so the reader can see the instrument's own precision.
"""

import sys
from collections import defaultdict
from pathlib import Path

# perf -x, --per-thread columns: comm-tid, value, unit, event, runtime, pct, ...
COMM_TID, VALUE, _UNIT, EVENT, RUNTIME, PCT = 0, 1, 2, 3, 4, 5

SHARD_PREFIX = "fr-set-get-shar"
WRITER_PREFIX = "fr-writer"


def parse_perf(path, fr_pid):
    """Return {group: {event: total}} plus notes on any low-confidence counter."""
    groups = defaultdict(lambda: defaultdict(float))
    notes = []
    for line in Path(path).read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        f = line.split(",")
        if len(f) <= EVENT:
            continue
        comm_tid, raw, event = f[COMM_TID], f[VALUE], f[EVENT]
        if "-" not in comm_tid:
            continue
        comm, _, tid = comm_tid.rpartition("-")
        if not tid.isdigit():
            continue
        # tid == pid identifies the process main thread EXACTLY. comm is
        # truncated to 15 characters by the kernel and cannot be trusted here.
        if int(tid) == fr_pid:
            group = "evloop"
        elif comm.startswith(SHARD_PREFIX):
            group = "shard"
        elif comm.startswith(WRITER_PREFIX):
            group = "writer"
        else:
            group = "other"
        if raw.startswith("<"):
            # A thread that never got scheduled in the window. Zero is the
            # honest reading; it is recorded so a silently-missing counter
            # cannot masquerade as a low per-op cost.
            value = 0.0
            groups[group]["_uncounted"] += 1
        else:
            try:
                value = float(raw)
            except ValueError:
                continue
            try:
                if len(f) > PCT and f[PCT] and float(f[PCT]) < 99.0:
                    notes.append(f"{comm_tid} {event} enabled {f[PCT]}%")
            except ValueError:
                pass
        groups[group][event] += value
        groups[group]["_threads_" + event] += 1
    return groups, notes


def median(xs):
    s = sorted(xs)
    n = len(s)
    if n == 0:
        return 0.0
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2


def main():
    if len(sys.argv) < 3:
        print("usage: _sharded_serial_stage_stats.py <census.tsv> <ops_per_round>")
        return 2
    tsv, ops = Path(sys.argv[1]), float(sys.argv[2])

    # The first column is a worker count for the census and an arm name for the
    # discriminator. Both reduce identically; only the ordering differs.
    rounds = defaultdict(list)
    order = []
    for line in tsv.read_text().splitlines():
        if not line.strip():
            continue
        w, r, elapsed, perf_csv, fr_pid = line.split("\t")
        groups, notes = parse_perf(perf_csv, int(fr_pid))
        if w not in rounds:
            order.append(w)
        rounds[w].append((float(elapsed), groups, notes))

    if not rounds:
        print("no rounds parsed")
        return 3

    numeric = all(k.lstrip("-").isdigit() for k in rounds)

    def labels():
        return sorted(rounds, key=int) if numeric else order

    def per_op(groups, group, event):
        return groups[group][event] / ops if ops else 0.0

    print("== per-operation counts, by thread group ==")
    print("   ops denominator is EXACT: redis-benchmark -t set,get -n N issues 2N operations.")
    print()
    hdr = (
        f"{'W':>4} {'evloop instr/op':>16} {'shard instr/op':>15} {'total instr/op':>15} "
        f"{'evloop ctxsw/op':>16} {'shard ctxsw/op':>15} {'futex/op':>10} {'write/op':>9} "
        f"{'evloop cpu%':>12} {'shard cpu%':>11} {'ops/s':>10}"
    )
    print(hdr)
    print("-" * len(hdr))

    summary = {}
    for w in labels():
        obs = rounds[w]
        ev_i = [per_op(g, "evloop", "instructions:u") for _, g, _ in obs]
        sh_i = [per_op(g, "shard", "instructions:u") for _, g, _ in obs]
        wr_i = [per_op(g, "writer", "instructions:u") + per_op(g, "other", "instructions:u")
                for _, g, _ in obs]
        tot_i = [a + b + c for a, b, c in zip(ev_i, sh_i, wr_i)]
        ev_c = [per_op(g, "evloop", "context-switches") for _, g, _ in obs]
        sh_c = [per_op(g, "shard", "context-switches") for _, g, _ in obs]
        fx = [
            sum(per_op(g, k, "syscalls:sys_enter_futex") for k in ("evloop", "shard", "writer", "other"))
            for _, g, _ in obs
        ]
        wsys = [
            sum(per_op(g, k, "syscalls:sys_enter_write") for k in ("evloop", "shard", "writer", "other"))
            for _, g, _ in obs
        ]
        # perf -x, reports task-clock in NANOSECONDS (the human-readable form
        # prints msec); elapsed is seconds. 100% == one saturated core.
        ev_cpu = [
            (g["evloop"]["task-clock"] / 1e9) / e * 100.0 if e > 0 else 0.0
            for e, g, _ in obs
        ]
        sh_cpu = [
            (g["shard"]["task-clock"] / 1e9) / e * 100.0 if e > 0 else 0.0
            for e, g, _ in obs
        ]
        rate = [ops / e if e > 0 else 0.0 for e, _, _ in obs]

        summary[w] = {
            "evloop_instr": median(ev_i),
            "shard_instr": median(sh_i),
            "other_instr": median(wr_i),
            "total_instr": median(tot_i),
            "evloop_ctxsw": median(ev_c),
            "shard_ctxsw": median(sh_c),
            "futex": median(fx),
            "write": median(wsys),
            "evloop_cpu": median(ev_cpu),
            "shard_cpu": median(sh_cpu),
            "rate": median(rate),
            "evloop_instr_span": (min(ev_i), max(ev_i)),
            "rate_span": (min(rate), max(rate)),
        }
        s = summary[w]
        print(
            f"{w:>4} {s['evloop_instr']:>16.1f} {s['shard_instr']:>15.1f} {s['total_instr']:>15.1f} "
            f"{s['evloop_ctxsw']:>16.4f} {s['shard_ctxsw']:>15.4f} {s['futex']:>10.4f} {s['write']:>9.4f} "
            f"{s['evloop_cpu']:>11.1f}% {s['shard_cpu']:>10.1f}% {s['rate']:>10.0f}"
        )

    print()
    print("== instrument precision: event-loop instructions/op across rounds ==")
    for w in labels():
        lo, hi = summary[w]["evloop_instr_span"]
        med = summary[w]["evloop_instr"]
        spread = (hi - lo) / med * 100.0 if med else 0.0
        rlo, rhi = summary[w]["rate_span"]
        rate_spread = (rhi - rlo) / rlo * 100.0 if rlo else 0.0
        print(
            f"  W={w:<4} evloop instr/op {lo:.1f}..{hi:.1f}  spread {spread:.2f}%"
            f"   (ops/s {rlo:.0f}..{rhi:.0f}, spread {rate_spread:.1f}%)"
        )

    baseline = "0" if "0" in summary else (labels()[0] if summary else None)
    if baseline is not None:
        base = summary[baseline]["evloop_instr"]
        print()
        print("== AMDAHL CEILING on the serial stage ==")
        print("   ceiling = evloop instr/op at the BASELINE arm / evloop instr/op at this arm")
        print("   A ceiling below 1.00 means the serial stage got MORE expensive, so no")
        print("   worker count -- not even an infinitely fast, free worker -- can win.")
        print()
        for w in labels():
            if w == baseline:
                continue
            s = summary[w]
            ceiling = base / s["evloop_instr"] if s["evloop_instr"] else float("inf")
            added = s["evloop_instr"] - base
            offloaded = s["shard_instr"]
            verdict = "CANNOT WIN" if ceiling < 1.0 else f"headroom {ceiling:.2f}x"
            print(
                f"  W={w:<4} ceiling {ceiling:>5.3f}x   serial stage {added:+.1f} instr/op "
                f"({(s['evloop_instr'] / base - 1) * 100:+.1f}%)   offloaded to workers "
                f"{offloaded:.1f} instr/op   {verdict}"
            )
        print()
        print("   The two columns are the whole argument: we ADD the left number to a")
        print("   thread that stays serial in order to REMOVE the right number from it.")
        print()
        print("== whole-process work multiplier (all threads) ==")
        total_base = summary[baseline]["total_instr"]
        for w in labels():
            s = summary[w]
            mult = s["total_instr"] / total_base if total_base else 0.0
            print(
                f"  W={w:<4} total {s['total_instr']:>8.1f} instr/op   {mult:>5.2f}x the "
                f"instructions the baseline arm executes for the same operation"
            )

    # WHICH thread pays the syscall decides whether the handoff is a background
    # cost or a serial-stage cost. A futex issued by the event loop is a syscall
    # added to the one thread that cannot be parallelized.
    print()
    print("== syscall attribution: who pays the handoff? ==")
    hdr2 = (
        f"{'W':>4} {'evloop futex/op':>16} {'shard futex/op':>15} "
        f"{'evloop write/op':>16} {'shard write/op':>15} {'shard ctxsw/op':>15}"
    )
    print(hdr2)
    print("-" * len(hdr2))
    for w in labels():
        obs = rounds[w]
        ev_f = median([per_op(g, "evloop", "syscalls:sys_enter_futex") for _, g, _ in obs])
        sh_f = median([per_op(g, "shard", "syscalls:sys_enter_futex") for _, g, _ in obs])
        ev_w = median([per_op(g, "evloop", "syscalls:sys_enter_write") for _, g, _ in obs])
        sh_w = median([per_op(g, "shard", "syscalls:sys_enter_write") for _, g, _ in obs])
        sh_c = median([per_op(g, "shard", "context-switches") for _, g, _ in obs])
        print(f"{w:>4} {ev_f:>16.4f} {sh_f:>15.4f} {ev_w:>16.4f} {sh_w:>15.4f} {sh_c:>15.4f}")

    # When labels are "W<n>-<arm>", the interesting comparison is not against a
    # single baseline but WITHIN each arm: normal mode versus sharded mode on the
    # identical key pattern. That holds key length, client structure and store
    # behaviour fixed, so the only difference left is the execution path.
    import re

    parsed = {}
    for lab in summary:
        m = re.fullmatch(r"W(\d+)-(.+)", lab)
        if m:
            parsed[lab] = (int(m.group(1)), m.group(2))
    if len(parsed) == len(summary) and len({w for w, _ in parsed.values()}) > 1:
        print()
        print("== normal vs sharded, PAIRED on key pattern ==")
        hdr3 = (
            f"{'arm':>10} {'evloop instr/op':>26} {'ceiling':>9} "
            f"{'ops/s normal':>13} {'ops/s sharded':>14} {'measured':>9}"
        )
        print(hdr3)
        print("-" * len(hdr3))
        arms = []
        for lab, (w, arm) in parsed.items():
            if arm not in arms:
                arms.append(arm)
        for arm in arms:
            ws = sorted(w for w, a in parsed.values() if a == arm)
            if len(ws) < 2:
                continue
            base_lab = f"W{ws[0]}-{arm}"
            for w in ws[1:]:
                lab = f"W{w}-{arm}"
                b, c = summary[base_lab], summary[lab]
                ceiling = b["evloop_instr"] / c["evloop_instr"] if c["evloop_instr"] else 0.0
                got = c["rate"] / b["rate"] if b["rate"] else 0.0
                print(
                    f"{arm:>10} {b['evloop_instr']:>10.1f} -> {c['evloop_instr']:<11.1f} "
                    f"{ceiling:>8.3f}x {b['rate']:>13.0f} {c['rate']:>14.0f} {got:>8.3f}x"
                )
        print()
        print("   ceiling  is what the serial stage ALLOWS at any worker count.")
        print("   measured is what the path actually delivered. measured < ceiling is the")
        print("   handoff's latency and cross-core stall cost, on top of the instruction count.")

    uncounted = []
    for w in labels():
        for _, g, _ in rounds[w]:
            for grp in g:
                if g[grp].get("_uncounted"):
                    uncounted.append(f"W={w} {grp}: {int(g[grp]['_uncounted'])} counter(s) unscheduled")
    if uncounted:
        print()
        print("!! threads perf could not schedule a counter on (counted as zero):")
        for u in sorted(set(uncounted)):
            print(f"   {u}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
