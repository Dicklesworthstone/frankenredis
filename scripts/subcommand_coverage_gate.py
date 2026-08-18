#!/usr/bin/env python3
"""subcommand_coverage_gate.py — REDUNDANT. Use scripts/command_arity_gate.py instead.

SUPERSEDED BEFORE IT WAS USEFUL, and this notice is the correction. `command_arity_gate.py`
(9392e5d25) landed roughly twenty minutes before this file and is STRICTLY STRONGER on the same
129 subcommands: it reads fr's declared `SUBCOMMAND_TABLE` rather than grepping for a literal, it
reports entries present in the incumbent and absent from that table, AND it compares arity. Its
current verdict is 242/242 top-level and 129/129 subcommands agreeing.

I wrote this without checking whether the existing arity gate already covered subcommands. It did
-- its `fr_subcommand_arities` half was wired from the start. Two checks were also considered for
salvage and both come back empty: subcommands declared in the table but referenced nowhere else
(0 of 129), and any coverage axis this has that the other lacks (none).

Kept only because deleting a file needs explicit permission. Nothing should depend on it, and
removing it is the right cleanup.

--- original description, retained for the record ---

Static SUBCOMMAND coverage vs vendored redis 7.2.4.

The existing command-arity gate compares the 242 TOP-LEVEL commands. Redis 7.2.4 ships 392 command
definitions, so 150 of them are subcommands of container commands (CLUSTER 28, SENTINEL 21,
CLIENT 18, ACL 13, FUNCTION 9, LATENCY 7, COMMAND 7, XGROUP 6, ...). Nothing checked those, and a
container command can be present and wired while an individual subcommand of it is simply absent --
which a top-level gate cannot see by construction.

RUNS UNDER A BUILD FREEZE: no server, no cargo, no disk writes. It reads the incumbent's own
command JSON and greps fr's sources.

WHAT IT PROVES, AND WHAT IT DOES NOT. This checks that each subcommand NAME occurs as a quoted
literal somewhere in `crates/*/src/*.rs`. That is a genuine tripwire -- if a refactor drops a
subcommand entirely, the literal disappears and this fails -- but it is NOT proof the subcommand is
reachable, correctly parsed, or behaviourally right. A name occurring only in a comment or an
unrelated string would satisfy it. Treat a PASS as "nothing has gone missing", never as "coverage is
correct"; behaviour is the live differs' job.

SENTINEL is excluded: it is a separate server mode with its own crate and its own parity surface,
not part of the command table this gate is about.

Exit 0 if every checked subcommand is present, 1 otherwise.
"""
import glob
import json
import os
import re
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMMANDS_DIR = os.path.join(REPO, "legacy_redis_code", "redis", "src", "commands")
EXCLUDED_CONTAINERS = {"SENTINEL"}


def incumbent_subcommands():
    """(container, subcommand) for every non-excluded subcommand the incumbent defines."""
    pairs = []
    for path in sorted(glob.glob(os.path.join(COMMANDS_DIR, "*.json"))):
        try:
            with open(path) as fh:
                spec = json.load(fh)
        except (OSError, ValueError) as exc:
            print(f"FAIL — cannot read {os.path.basename(path)}: {exc}")
            raise SystemExit(1)
        for name, body in spec.items():
            if not isinstance(body, dict):
                continue
            container = body.get("container")
            if not container:
                continue
            if container.upper() in EXCLUDED_CONTAINERS:
                continue
            pairs.append((container.upper(), name.upper()))
    return sorted(set(pairs))


def fr_source_upper():
    files = sorted(glob.glob(os.path.join(REPO, "crates", "*", "src", "*.rs")))
    if not files:
        print("FAIL — no crate sources found; is this the repo root?")
        raise SystemExit(1)
    blobs = []
    for path in files:
        with open(path, "rb") as fh:
            blobs.append(fh.read().decode("utf-8", "replace"))
    return "\n".join(blobs).upper()


def main():
    print(
        "NOTE: this gate is REDUNDANT -- scripts/command_arity_gate.py checks the same 129\n"
        "      subcommands against fr's declared SUBCOMMAND_TABLE and compares arity too.\n"
        "      Run that instead; this remains only because deleting a file needs permission.\n"
    )
    pairs = incumbent_subcommands()
    if not pairs:
        print("FAIL — parsed zero subcommands; the incumbent tree is missing or changed shape")
        return 1
    src = fr_source_upper()

    missing = []
    for container, sub in pairs:
        # A quoted literal in any of the forms Rust sources use for a command token.
        if not re.search(r'(?:B?"%s"|\'%s\')' % (re.escape(sub), re.escape(sub)), src):
            missing.append((container, sub))

    print("=" * 78)
    print(
        f"SUBCOMMAND coverage vs vendored 7.2.4 — {len(pairs)} checked "
        f"({', '.join(sorted(EXCLUDED_CONTAINERS))} excluded)"
    )
    print("=" * 78)

    if missing:
        by_container = {}
        for container, sub in missing:
            by_container.setdefault(container, []).append(sub)
        print(f"FAIL — {len(missing)} subcommand(s) absent from crates/*/src/*.rs:\n")
        for container in sorted(by_container):
            print(f"  {container}: {' '.join(sorted(by_container[container]))}")
        print(
            "\nA name absent here is absent from the source entirely. Check it is genuinely "
            "unimplemented before filing:\n  git grep -in '<name>' -- crates/"
        )
        return 1

    print(
        f"PASS — every one of the {len(pairs)} subcommands appears as a literal in fr's sources.\n"
        "\nThis is a tripwire against a subcommand going MISSING, not evidence that any of them "
        "behaves correctly: a name in a comment would satisfy it. Behaviour belongs to the live "
        "differs."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
