#!/usr/bin/env python3
"""Static parity gate: which INFO fields upstream emits that fr never renders
(frankenredis-infofields).

WHY THIS IS NOT COVERED BY info_stats_differ.py. That differ compares the logical
stat VALUES in `# Stats` after an identical workload, and its docstring excludes
"all memory/cpu/time/version/pid env fields". It checks values in one section; it
cannot see a field that is absent from ANOTHER section, which is what this gate is for.
Absence is the failure mode that breaks monitoring clients: `info['aof_base_size']`
raises KeyError rather than reporting a wrong number.

WHY STATIC, AND THE TRAP THAT MAKES IT NECESSARY. Most of these fields are CONDITIONAL
upstream -- replica-only, AOF-only, during-load-only -- so a live INFO diff on a default
standalone server shows them missing from BOTH engines and agrees. Reaching them needs a
specific server state per field, which is exactly the setup a differ does not do by
accident.

The method matters, because the obvious version of it is wrong. Extracting fr's fields
from a WINDOW of one file produced a 35-field "gap" that was mostly artifact: fr builds
the Replication section in fr-runtime and the rest in fr-command, so `master_host`,
`master_link_status`, `slave_read_only` and `replica_announced` all looked missing and are
not. This gate instead asks whether each upstream field name appears ANYWHERE in crates/
as a rendered `<name>:`, anchored at a quote OR a line start. That is a presence test with
no window to get wrong. It over-approximates presence -- a name mentioned in a comment or
a test counts -- which is the safe direction for a gate whose failure mode is a false
ABSENCE claim.

THE ANCHOR HAS ALREADY BEEN WRONG ONCE, in the direction that matters. Requiring a quote
before the name missed fields rendered mid-way through a multi-line `write!`, and reported
`slave_priority` and `master_link_down_since_seconds` absent while fr renders both. See
`fr_rendered_names` for the full account. If you tighten this matcher, the thing to prove
is that it still finds a field that does not begin its own string literal.

Exit 0 = the absent set matches the declared baseline exactly.
Exit 1 = a field went missing that was not declared (regression), or a declared-absent
         field is now rendered (progress -- update the baseline so the gate keeps meaning
         something).

Usage: info_field_parity_gate.py [--verbose]
Runs with no server, no build, no network and no disk writes.
"""
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
UPSTREAM = os.path.join(ROOT, "legacy_redis_code", "redis", "src", "server.c")

# Every upstream INFO field fr does not render, with the upstream GUARD that decides when
# it appears and what a verifier must do to reach it. Grouped, because the groups have
# very different reachability and therefore very different severity.
DECLARED_ABSENT = {
    # IMPLEMENTED 2026-08-17 in 29048d447 and removed from this baseline, which is the
    # deletion condition this gate was written with. The six AOF sizing fields
    # (aof_current_size, aof_base_size, aof_pending_rewrite, aof_buffer_length,
    # aof_pending_bio_fsync, aof_delayed_fsync) now render under `appendonly yes`, verified
    # live: redis 36 persistence fields and fr 36, zero presence divergences in BOTH the
    # on and off configurations. The gate flagged their arrival itself, via the
    # "declared-absent field is now rendered" arm — which is what that arm is for.
    # NOT LISTED, and the reason is a correction: `slave_priority` and
    # `master_link_down_since_seconds` were in this baseline and are RENDERED. fr emits
    # them in the multi-line `write!` at fr-runtime/src/lib.rs:45698, which the first
    # version of `fr_rendered_names` could not see. See that function for the full
    # account. fr renders all three of upstream's slave_priority / slave_read_only /
    # replica_announced trio.
    # `# Replication`, replica with a sync in progress.
    "master_sync_total_bytes": "replica, sync in progress",
    "master_sync_read_bytes": "replica, sync in progress",
    "master_sync_left_bytes": "replica, sync in progress",
    "master_sync_perc": "replica, sync in progress",
    "master_sync_last_io_seconds_ago": "replica, sync in progress",
    # IMPLEMENTED 2026-08-17 in f464351ec: min_slaves_good_slaves now renders under
    # upstream's two-knob guard, sourced from the same good_replica_write_count fr already
    # uses for its write-quorum gate. Removed per this gate's deletion condition.
    # `# Persistence`, only while an RDB/AOF load is in flight. Narrow window, low impact.
    "loading_start_time": "loading",
    "loading_total_bytes": "loading",
    "loading_rdb_used_mem": "loading",
    "loading_loaded_bytes": "loading",
    "loading_loaded_perc": "loading",
    "loading_eta_seconds": "loading",
    # `# Debug` -- an opt-in section (INFO debug / everything) fr does not emit at all.
    "eventloop_duration_aof_sum": "INFO debug section",
    "eventloop_duration_cron_sum": "INFO debug section",
    "eventloop_duration_max": "INFO debug section",
    "eventloop_cmd_per_cycle_max": "INFO debug section",
    # `# Server`, only when a unix socket is configured.
    "unixsocket": "unixsocket configured",
    # `# Server`, only during a graceful shutdown pause.
    "shutdown_in_milliseconds": "shutdown in progress",
}


def upstream_info_fields():
    """Field names rendered by genRedisInfoString, in source order."""
    src = open(UPSTREAM, encoding="utf-8", errors="replace").read()
    i = src.index("sds genRedisInfoString(")
    m = re.search(r"\nsds \w+\(", src[i + 10:])
    seg = src[i: i + 10 + m.start()] if m else src[i:]
    seen, ordered = set(), []
    for name in re.findall(r'"([a-z][a-z0-9_]{2,})\s*:%', seg):
        if name not in seen:
            seen.add(name)
            ordered.append(name)
    return ordered


def fr_rendered_names():
    """Every `<name>:` rendered anywhere under crates/ — presence, with no window.

    THE ANCHOR IS `(^|")` AND THE FIRST VERSION OF THIS FUNCTION GOT IT WRONG, which is
    worth keeping in the source because the failure was silent and pointed the unsafe way.
    Requiring a literal quote before the name assumes every field begins a string literal.
    It does not: fr renders the replica block as one multi-line `write!` whose continuation
    lines start with the field name and no quote —

        "master_host:{host}\\r\\n\\
        master_port:{port}\\r\\n\\
        ...
        slave_priority:{}\\r\\n\\

    — so `slave_priority` and `master_link_down_since_seconds` were reported ABSENT while
    being rendered a few lines apart from fields the same gate reported present. That is a
    false ABSENCE, the direction this gate must never fail in, and it reached a bead, a
    commit message and the README before a source read caught it. Accepting a line start as
    well as a quote restores the intended over-approximation: a name in a comment or a test
    now counts as present, which can only ever HIDE a gap, never invent one.
    """
    out = subprocess.run(
        ["rg", "-oNI", r'(^|")[a-z][a-z0-9_]{2,}\s*:', os.path.join(ROOT, "crates")],
        capture_output=True, text=True, check=False).stdout
    return set(re.findall(r'([a-z][a-z0-9_]{2,})\s*:', out))


def main():
    verbose = "--verbose" in sys.argv
    upstream = upstream_info_fields()
    present = fr_rendered_names()
    absent = [f for f in upstream if f not in present]

    undeclared = [f for f in absent if f not in DECLARED_ABSENT]
    now_present = [f for f in DECLARED_ABSENT if f in present]

    by_guard = {}
    for f in absent:
        by_guard.setdefault(DECLARED_ABSENT.get(f, "UNDECLARED"), []).append(f)

    print("=" * 78)
    print(f"INFO field parity — {len(upstream)} upstream fields, "
          f"{len(absent)} not rendered anywhere in crates/")
    print("=" * 78)
    for guard in sorted(by_guard):
        print(f"  reachable when: {guard}")
        for f in sorted(by_guard[guard]):
            print(f"      {f}")
    if verbose:
        print(f"\nfr renders {len(present)} distinct `name:` literals under crates/")

    failures = []
    if undeclared:
        failures.append(
            "UPSTREAM FIELD(S) NOT RENDERED AND NOT DECLARED — either implement them or "
            f"declare them with the upstream guard that reaches them: {sorted(undeclared)}")
    if now_present:
        failures.append(
            "declared-absent field(s) are now rendered — good, but remove them from "
            f"DECLARED_ABSENT so this gate keeps meaning something: {sorted(now_present)}")

    if failures:
        print()
        print(f"FAIL — {len(failures)} finding(s):")
        for f in failures:
            print(f"  {f}")
        sys.exit(1)
    print(f"\nPASS — the {len(absent)} absent field(s) match the declared baseline exactly.")


if __name__ == "__main__":
    main()
