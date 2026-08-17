#!/usr/bin/env python3
"""Differential gate: the CLIENT LIST/INFO `cmd=` field after an UNKNOWN command
(frankenredis-zbiy3).

Upstream carries this field as a POINTER to the command-table entry and prints
`lastcmd->fullname`, or the literal `NULL` when the pointer is null
(networking.c:2843). The pointer is assigned unconditionally, from
`lookupCommand(argv, argc)`, BEFORE the existence and arity checks
(server.c:3865) -- and `lookupCommandLogic` (server.c:3133) yields NULL both for an
unknown NAME and, when the base command has subcommands, for an unknown SUBCOMMAND.

fr builds the field instead: `write_client_info_command_name` lowercases the raw
argv with no table lookup anywhere in it, and only ever emits `NULL` for an EMPTY
argv. So an unknown command is predicted to report as though it ran. This gate is
what turns that source-derived prediction into a measurement; frankenredis-dpu2y is
the fix (store the handle, render on demand), which removes the divergence for free.

WHY THIS CANNOT BE OBSERVED WITH `CLIENT INFO` ON THE SAME CONNECTION, which is the
trap this gate exists to avoid: `CLIENT INFO` is itself a command, and BOTH engines
assign the field before dispatching it, so a same-connection read always reports
`client|info` and both engines agree no matter what came before. The field has to be
read from ANOTHER connection's row in `CLIENT LIST`.

AND THE ROW HAS TO BE LABELLED BEFORE THE CASE RUNS, for the same reason: any command
used to identify the probe connection -- `CLIENT SETNAME`, `CLIENT ID` -- becomes its
last command and overwrites the very field under test. So the name is set FIRST and the
case commands run after it, which is also exactly the overwrite behaviour upstream
specifies.

Usage: client_info_cmd_field_differ.py <oracle_port> <fr_port>
       Exit 0 = the cmd= field agrees on every case, 1 = divergence.
"""
import sys

from _respread import cmd, conn

PROBE_NAME = "zbiy3probe"

# (label, commands to run on the probe connection AFTER it is named, expected-to-agree)
# `agree=True` marks a CONTROL: a case where both engines must report the same thing.
# Without controls a gate that reads the wrong row, or the wrong field, reports every
# case as a divergence and looks like a finding.
CASES = [
    ("known simple",            [["GET", "zbiy3:k"]],                        True),
    ("known container",         [["PUBSUB", "CHANNELS"]],                    True),
    ("container bare argc==1",  [["CONFIG"]],                                True),
    ("known, wrong arity",      [["GET"]],                                   True),
    ("known, mixed case",       [["gEt", "zbiy3:k"]],                        True),
    ("UNKNOWN name",            [["BOGUS"]],                                 False),
    ("UNKNOWN subcommand",      [["CLIENT", "BOGUS"]],                       False),
    ("known then UNKNOWN",      [["GET", "zbiy3:k"], ["BOGUS"]],             False),
]


def cmd_field_of_named_row(reader, name):
    """The `cmd=` value of the CLIENT LIST row whose `name=` matches, or None.

    Returns None rather than raising so the caller can report "row not found" as a
    HARNESS failure distinct from a divergence -- a gate that silently compares
    None to None would pass while measuring nothing.
    """
    raw = cmd(reader, "CLIENT", "LIST")
    text = raw.decode("utf-8", "replace")
    for line in text.splitlines():
        fields = dict(
            piece.split("=", 1) for piece in line.split(" ") if "=" in piece
        )
        if fields.get("name") == name:
            return fields.get("cmd")
    return None


def observe(port, commands):
    """Run `commands` on a freshly-named probe connection; read its cmd= from another."""
    probe = conn(port)
    reader = conn(port)
    try:
        # Name FIRST, so the case commands are what overwrite the field.
        cmd(probe, "CLIENT", "SETNAME", PROBE_NAME)
        for argv in commands:
            cmd(probe, *argv)          # errors are expected and drained
        return cmd_field_of_named_row(reader, PROBE_NAME)
    finally:
        probe.close()
        reader.close()


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400

    rows, divergences, harness_errors = [], [], []
    for label, commands, agree in CASES:
        ro, fv = observe(op, commands), observe(fp, commands)
        if ro is None or fv is None:
            harness_errors.append(
                f"{label}: probe row not found in CLIENT LIST "
                f"(redis={ro!r} fr={fv!r}) — the gate measured nothing"
            )
            continue
        rows.append((label, ro, fv, ro == fv))
        if ro != fv:
            divergences.append((label, commands, ro, fv, agree))

    print("=" * 78)
    print(f"{'case':<26} {'redis 7.2.4':<22} {'fr':<22} verdict")
    print("-" * 78)
    for label, ro, fv, same in rows:
        print(f"{label:<26} {ro:<22} {fv:<22} {'agree' if same else 'DIVERGE'}")
    print("=" * 78)

    if harness_errors:
        print("HARNESS FAILURE — the gate could not read the field it exists to compare:")
        for x in harness_errors:
            print(f"  {x}")
        sys.exit(1)

    # A control that diverges means the harness is wrong, not the engine. Report it
    # separately and loudly, because it invalidates the non-control rows too.
    broken_controls = [d for d in divergences if d[4]]
    if broken_controls:
        print("CONTROL DIVERGED — do not read the other rows; the gate is suspect:")
        for label, commands, ro, fv, _ in broken_controls:
            print(f"  {label}: {commands} redis={ro!r} fr={fv!r}")
        sys.exit(1)

    if divergences:
        print(f"FAIL — {len(divergences)} cmd= divergence(s) vs redis 7.2.4 "
              f"(frankenredis-zbiy3; fix is frankenredis-dpu2y):")
        for label, commands, ro, fv, _ in divergences:
            print(f"  {label}: {commands} redis={ro!r} fr={fv!r}")
        sys.exit(1)

    print(f"PASS — cmd= agrees on all {len(rows)} cases "
          f"({sum(1 for r in CASES if r[2])} controls + "
          f"{sum(1 for r in CASES if not r[2])} unknown-command cases). "
          "If this passes, frankenredis-zbiy3 is refuted and should be closed as such.")


if __name__ == "__main__":
    main()
