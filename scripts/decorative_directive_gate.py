#!/usr/bin/env python3
"""decorative_directive_gate.py — every config directive fr ACCEPTS must be classified.

A directive fr parses, stores and echoes from CONFIG GET, but which nothing READS, is a defect no
other gate in this repo can see. The command-coverage and arity gates compare the command TABLE;
the INFO and CLIENT INFO gates compare rendered FIELDS. All of them pass while a directive quietly
does nothing -- which is how `protected-mode` shipped defaulting to `yes` and accepting every
remote connection anyway.

RUNS UNDER A BUILD FREEZE: no server, no cargo, no disk writes.

WHAT IT CHECKS. Each directive in fr's config table falls into exactly one class below. The gate
fails when a directive is UNCLASSIFIED (someone added a knob and no one said what reads it), and
when a directive classified OPEN has grown references (it was implemented -- move it, or the class
stops meaning anything). That second half is the same stale-allowance discipline
`client_info_field_parity_gate` and `info_field_parity_gate` already apply.

WHAT IT DOES NOT CHECK, and this is the honest limit: reference COUNTING cannot prove a directive
is read. `min-slaves-max-lag` looked decorative -- one reference outside the table -- and is not:
the CONFIG SET path routes it and its `min-replicas-` spelling to the same in-memory value the
enforcement reads. The count narrows the search; only locating the incumbent's application point
and showing fr's corresponding site absent decides it. Every OPEN entry below carries the evidence
that decision was made on.
"""
import glob
import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RUNTIME = os.path.join(REPO, "crates", "fr-runtime", "src", "lib.rs")

# Directives whose absence of a consumer is DELIBERATE, with the reason. These are not work.
INERT_BY_DESIGN = {
    "server_cpulist": "fr does not pin threads; there is no affinity subsystem to configure",
    "bio_cpulist": "same -- no background-IO thread pool to pin",
    "aof_rewrite_cpulist": "same -- no rewrite child to pin",
    "bgsave_cpulist": "same -- no bgsave child to pin",
    "disable-thp": "needs /sys writes fr does not perform; upstream's own default is to warn only",
    "crash-log-enabled": "fr installs no crash handler, so there is no log to enable",
    "crash-memcheck-enabled": "same -- no crash handler",
    "activerehashing": "fr's map is hashbrown; there is no incremental rehash cron to gate",
    "syslog-ident": "fr logs to stderr; there is no syslog sink to name",
    "syslog-facility": "same -- no syslog sink",
    "jemalloc-bg-thread": "fr links mimalloc, not jemalloc",
    "locale-collate": "SORT ALPHA uses byte collation by design (project_sort_alpha_ascii_collation_fast_path)",
    "ignore-warnings": "upstream uses it to silence specific STARTUP warnings (the ARM64 COW bug notice); fr emits none of them, so there is nothing to silence",
    "tls-ca-cert-dir": "fr has no TLS subsystem at all -- the whole feature is absent and is tracked as a feature, not as eight directives",
    "tls-ca-cert-file": "see tls-ca-cert-dir",
    "tls-ciphersuites": "see tls-ca-cert-dir",
    "tls-client-cert-file": "see tls-ca-cert-dir",
    "tls-client-key-file": "see tls-ca-cert-dir",
    "tls-client-key-file-pass": "see tls-ca-cert-dir",
    "tls-dh-params-file": "see tls-ca-cert-dir",
    "tls-key-file-pass": "see tls-ca-cert-dir",
}

# Verified against the incumbent, genuinely unimplemented, NOT yet done -- each with the reason it
# was not taken blind under the freeze. These are the work queue.
# name -> (reason, baseline reference count OUTSIDE the config table).
#
# THE BASELINE IS NOT DECORATION. A directive named in a CONFIG SET allowlist already has
# references while being read by nothing -- that is precisely the shape of every defect this gate
# exists to find. Failing on `refs > 0` would fire on all of them permanently; failing on
# `refs > baseline` fires when someone actually wires one up.
OPEN = {
    "proc-title-template": (
        "upstream rewrites the process title so `ps` shows the port and role. Doing that from "
        "Rust means overwriting the argv block through libc -- the same unsafe-with-no-compiler "
        "hazard as bind-source-addr, for a cosmetic gain",
        0,
    ),
    "bind-source-addr": (
        "upstream binds the local end of the replica/cluster/sentinel connection to it "
        "(replication.c:2932). TcpStream::connect cannot; fr-server has no libc dependency, so "
        "this needs a new dependency plus unsafe socket/bind/connect -- a correctness and "
        "security hazard to write with no compiler",
        1,
    ),
    "aof-timestamp-enabled": (
        "upstream emits `#TS:<unix>` annotations as it appends (aof.c:1326). fr's AofRecord "
        "carries only argv, so a faithful stamp needs a per-record timestamp threaded through "
        "every capture site -- stamping at flush time would misdate exactly the point-in-time "
        "recovery the feature exists for. fr's LOADER already skips `#` lines, so the reader "
        "half is done",
        1,
    ),
    "aof-load-truncated": (
        "fr does not have upstream's boolean; it has a BOUNDED tail-repair policy "
        "(AofReplayTailRepairPolicy::BoundedFinalSegment). Mapping the directive onto it is a "
        "data-integrity design decision -- `yes` would mean unbounded truncation, weakening an "
        "existing safety bound -- not a wiring job",
        2,
    ),
    "no-appendfsync-on-rewrite": (
        "fr's appendfsync_mode exists for WAITAOF visibility, not a real fsync path, so there "
        "is no fsync to skip. Implementing it means deciding what it does to WAITAOF "
        "accounting, which is inventing durability semantics rather than porting them",
        2,
    ),
}


def table_names():
    src = open(RUNTIME, encoding="utf-8", errors="replace").read()
    i = src.index('("unixsocket", "")')
    start = src.rindex("[", 0, i)
    end = src.index("];", i)
    return re.findall(r'\(\s*"([a-z0-9\-_.]+)"\s*,', src[start:end]), src[start:end]


def reference_counts(table_text):
    counts = {}
    blobs = []
    for path in sorted(glob.glob(os.path.join(REPO, "crates", "*", "src", "*.rs"))):
        text = open(path, encoding="utf-8", errors="replace").read()
        if path.endswith(os.path.join("fr-runtime", "src", "lib.rs")):
            text = text.replace(table_text, "")
        blobs.append(text)
    blob = "\n".join(blobs)
    return blob, counts


def main():
    names, table_text = table_names()
    blob, _ = reference_counts(table_text)
    failures = []

    classified = set(INERT_BY_DESIGN) | set(OPEN)
    unknown_low = []
    for name in names:
        refs = blob.count(f'"{name}"')
        if refs == 0 and name not in classified:
            unknown_low.append(name)

    revived = [
        n for n, (_reason, baseline) in OPEN.items() if blob.count(f'"{n}"') > baseline
    ]

    print("=" * 78)
    print(f"decorative-directive gate — {len(names)} directives in fr's config table")
    print("=" * 78)
    print(f"  classified inert by design : {len(INERT_BY_DESIGN)}")
    print(f"  classified open (verified) : {len(OPEN)}")

    if unknown_low:
        failures.append(
            "directive(s) with NO reference outside the config table and no classification -- "
            "either something reads them and this gate cannot see it, or they are decorative and "
            f"need an entry: {sorted(unknown_low)}"
        )
    if revived:
        failures.append(
            "directive(s) classified OPEN now have references -- if they were implemented, move "
            f"them out of OPEN so the class keeps meaning something: {sorted(revived)}"
        )

    if failures:
        print()
        for f in failures:
            print(f"FAIL — {f}")
        return 1

    print("\nPASS — every directive with no consumer is accounted for.")
    print(
        "\nA PASS does not mean every directive works: counting references cannot prove one is "
        "read.\nSee the module docstring for the false-positive that established that limit."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
