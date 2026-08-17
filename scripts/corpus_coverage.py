#!/usr/bin/env python3
"""Which commands are BOTH unclassified at the dispatch floor AND unmeasured by any shape.

(frankenredis-ozrro) This exists because the corpus, not the engine, has twice been the
limiting instrument — and both times it was found by accident rather than by looking.

  * A 102-shape screen concluded "the front-classification surface is ~ONE command, not a
    campaign". It ranked the shapes the harness HAD.
  * Adding six shapes for commands that were unclassified AND unshaped surfaced THREE
    routes at 1.43x-1.48x — SMOVE, RPOPLPUSH and LTRIM — one of them (SMOVE, 4,438 instr/op
    of dispatch) larger than anything the screen had ever ranked. All three then crossed to
    0.51x-0.62x with one floor entry each.

A command in this report's BLIND SPOT list is not "fine"; it is UNKNOWN. That distinction
is the whole point, and it is the one a ranked screen silently erases.

The three inputs are read from source rather than assumed:
  fr-command/src/lib.rs   COMMAND_TABLE          -> every command fr implements
  fr-server/src/main.rs   borrowed_dispatch_floor_command -> the floor-classified set
  scripts/shape_instr_per_op.py  SHAPES          -> the commands any shape actually issues

Usage:
    corpus_coverage.py            # report
    corpus_coverage.py --self-test
"""

from __future__ import annotations

import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from shared_executor_map import covered_commands  # noqa: E402  (sys.path set above)

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CMD_TABLE = os.path.join(ROOT, "crates", "fr-command", "src", "lib.rs")
FLOOR = os.path.join(ROOT, "crates", "fr-server", "src", "main.rs")
SHAPES = os.path.join(ROOT, "scripts", "shape_instr_per_op.py")
RUNTIME = os.path.join(ROOT, "crates", "fr-runtime", "src", "lib.rs")

# A container command's own name is dispatched, but its work lives in subcommands, so a
# shape for the bare name would measure the wrong thing. Excluded from the blind spot with
# the reason stated rather than silently dropped.
CONTAINERS = {
    "acl", "client", "cluster", "command", "config", "debug", "function", "latency",
    "memory", "object", "pubsub", "script", "slowlog", "xgroup", "xinfo",
}


def command_table(text: str) -> set[str]:
    """Every command name in COMMAND_TABLE."""
    start = text.index("const COMMAND_TABLE:")
    body = text[start : text.index("];", start)]
    return {m.group(1).lower() for m in re.finditer(r'^\s*\("([a-z0-9_|-]+)"\s*,', body, re.M)}


def floor_classified(text: str) -> set[str]:
    """Every command `borrowed_dispatch_floor_command` can classify.

    The table matches byte arrays, not string literals, so the names are reassembled from
    `[b'S', b'M', b'O', b'V', b'E']` patterns. A regex over string literals finds NOTHING
    here — an earlier version of this analysis returned three bogus names that way.

    THE PATTERN MUST TOLERATE RUSTFMT'S EXPLODED FORM, and the first version did not.
    rustfmt keeps a short array on one line with no trailing comma, but breaks a long one
    across lines AND adds a trailing comma:

        [b'S', b'M', b'O', b'V', b'E'] => ...        <- matched
        [
            b'Z', b'R', b'A', b'N', b'G', b'E',
            b'B', b'Y', b'L', b'E', b'X',            <- trailing comma, then a newline
        ] => ...                                     <- was NOT matched

    So every command name long enough to be exploded was invisible, and the tool reported
    17 CLASSIFIED commands as unclassified: bitfield_ro, hincrbyfloat, incrbyfloat,
    pexpiretime, sinterstore, srandmember, sunionstore, zinterstore, zrandmember,
    zrangebylex, zrangebyscore, zremrangebylex, zremrangebyrank, zremrangebyscore,
    zrevrangebylex, zrevrangebyscore, zunionstore — 121 counted against 138 real.
    That is not a cosmetic undercount: this report's whole job is to send someone to add a
    missing floor entry, and a false positive sends them to add one that already exists.
    Observed live — ZRANGEBYLEX was picked off this report as a stranded route and it has
    had a `(4, Zrangebylex)` floor entry the entire time.
    """
    start = text.index("fn borrowed_dispatch_floor_command(")
    body = text[start : text.index("\nfn ", start + 10)]
    out = set()
    for m in re.finditer(r"\[\s*((?:b'[A-Z_]'\s*,\s*)*b'[A-Z_]'\s*,?)\s*\]", body):
        out.add("".join(re.findall(r"b'([A-Z_])'", m.group(1))).lower())
    return out


def borrowed_trio(server_text: str, runtime_text: str) -> dict[str, set[str]]:
    """Which commands already have borrowed machinery, split by piece.

    This is the single best predictor of a CHEAP lever, and it is why the report ranks on
    it: of the six routes this campaign moved from behind to ahead, FIVE needed nothing but
    a floor-table entry, because parser, executor, gate and metrics all existed already.
    Only LTRIM needed an executor written, and that was much the most expensive of the six.

    A [C] command is not "harder by a little" — it is a different job, and mislabelling one
    as [A] would send someone to add a floor class whose arm cannot serve the shape, which
    sends it to GENERIC and is a REGRESSION rather than a no-op.
    """
    return {
        "parser": {
            m.group(1).lower()
            for m in re.finditer(r"fn parse_borrowed_plain_([a-z0-9_]+?)_packet", server_text)
        },
        # (frankenredis-gvm6z) UNION with the SHARED-executor map, because the per-command
        # scan below CANNOT see a command dispatched through a shared executor that takes a
        # discriminant -- `execute_plain_keymeta_borrowed(PlainKeyMetaCmd::Pexpiretime, ..)`
        # matches no `fn execute_plain_pexpiretime_borrowed`. There are EIGHT such enums
        # covering 32 commands.
        #
        # This is not hypothetical: `scripts/shared_executor_map.py` exists because acting on
        # one of those false [C] labels produced a DUPLICATE PEXPIRETIME executor that
        # nothing calls (ledger b2df577ba). That tool was written and then never wired into
        # this report, so the report kept emitting the label that caused the mistake.
        #
        # The error is ONE-DIRECTIONAL: the per-command scan UNDERSTATES machinery, so it can
        # only produce false [C]/[B], never a false [A]. The union therefore moves commands
        # OUT of [C] and never into it.
        "executor": {
            m.group(1).lower()
            for m in re.finditer(r"fn execute_plain_([a-z0-9_]+?)_borrowed", runtime_text)
        }
        | covered_commands(runtime_text),
    }


def shaped(text: str) -> set[str]:
    """Every command any shape ISSUES (the measured command, not the seed commands)."""
    start = text.index("SHAPES")
    body = text[start:]
    return {m.group(1).lower() for m in re.finditer(r'\[\s*"([A-Z_]+)"', body)}


def report() -> int:
    table = command_table(open(CMD_TABLE).read())
    classified = floor_classified(open(FLOOR).read())
    have_shape = shaped(open(SHAPES).read())

    blind = sorted(c for c in table - classified - have_shape if c not in CONTAINERS)
    measured_unclassified = sorted(c for c in (table - classified) & have_shape)

    print(f"COMMAND_TABLE            {len(table)}")
    print(f"floor-classified         {len(classified)}")
    print(f"issued by some shape     {len(have_shape)}")
    print()
    trio = borrowed_trio(open(FLOOR).read(), open(RUNTIME).read())
    ready = [c for c in blind if c in trio["parser"] and c in trio["executor"]]
    partial = [c for c in blind if (c in trio["parser"]) != (c in trio["executor"])]
    bare = [c for c in blind if c not in trio["parser"] and c not in trio["executor"]]

    def block(items):
        for i in range(0, len(items), 6):
            print("      " + "  ".join(f"{c:<16}" for c in items[i : i + 6]))

    print(f"UNCLASSIFIED **and** UNMEASURED — the blind spot ({len(blind)}):")
    print()
    print(f"  [A] fast path EXISTS, only the floor entry missing ({len(ready)}) "
          f"— one table entry each:")
    block(ready)
    print()
    print(f"  [B] PARTIAL, parser or executor but not both ({len(partial)}):")
    block(partial)
    print()
    print(f"  [C] NO borrowed machinery, one must be written first ({len(bare)}):")
    block(bare)
    print()
    print(
        f"unclassified but MEASURED ({len(measured_unclassified)}) — known, and not all are "
        f"behind:\n   " + "  ".join(measured_unclassified[:16])
    )
    print()
    print(
        "A blind-spot command is UNKNOWN, not fine. The last six shapes added from this\n"
        "list produced three routes at 1.43x-1.48x, one of them the largest dispatch cost\n"
        "ever measured in this campaign."
    )
    return 0


def _self_test() -> int:
    """The parsers, on the shapes that actually broke earlier hand-rolled versions."""
    # COMMAND_TABLE: tuple entries only, not the doc comments around them.
    tbl = '''const COMMAND_TABLE: &[(&str, i64, &str, i64, i64, i64)] = &[
    // ("notacommand", 1, "x", 0, 0, 0) in a comment must not count
    ("ping", -1, "fast", 0, 0, 0),
    ("get", 2, "readonly fast", 1, 1, 1),
    ("georadius_ro", -6, "readonly", 1, 1, 1),
];
'''
    got = command_table(tbl)
    assert got == {"ping", "get", "georadius_ro"}, got

    # The floor table matches BYTE ARRAYS. A string-literal regex finds nothing here,
    # which is the bug this case exists for.
    floor = '''fn borrowed_dispatch_floor_command(token: &[u8]) -> Option<X> {
    match token.len() {
        3 => match uppercase_ascii_token::<3>(token)? {
            [b'T', b'T', b'L'] => Some(X::Ttl),
            _ => None,
        },
        5 => match uppercase_ascii_token::<5>(token)? {
            [b'S', b'M', b'O', b'V', b'E'] => Some(X::Smove),
            _ => None,
        },
        11 => match uppercase_ascii_token::<11>(token)? {
            [
                b'Z',
                b'R',
                b'A',
                b'N',
                b'G',
                b'E',
                b'B',
                b'Y',
                b'L',
                b'E',
                b'X',
            ] => Some(X::Zrangebylex),
            _ => None,
        },
    }
}
fn next_function() {}
'''
    got = floor_classified(floor)
    # The exploded entry is the one that broke this: rustfmt puts a TRAILING COMMA after
    # its last byte and a newline before the `]`, and the original pattern required
    # neither. Dropping `zrangebylex` here is the exact live failure — the tool reported
    # 121 classified commands against 138 real and sent someone to re-add a floor entry
    # that already existed.
    assert got == {"ttl", "smove", "zrangebylex"}, got
    assert re.findall(r'b"([A-Z]+)"', floor) == [], "string-literal scan must find nothing"

    # SHAPES: the ISSUED command, not the seed commands. `SADD` here is setup.
    shapes = '''SHAPES = {
    "smove_missing": (["SADD smsrc a b"], ["SMOVE", "smsrc", "smdst", "nosuch"]),
    "get_control": ([], ["GET", "k"]),
}
'''
    got = shaped(shapes)
    assert got == {"smove", "get"}, got
    assert "sadd" not in got, "seed commands must not count as measured"

    # The trio scan must not confuse a command with one that merely CONTAINS its name.
    # hincrby vs hincrbyfloat is the live case: a greedy affix strip would report
    # hincrbyfloat as [A] ready when nothing executes it, sending someone to add a floor
    # class whose arm cannot serve the shape — a REGRESSION, not a no-op.
    srv = "\n".join([
        "fn parse_borrowed_plain_hincrby_packet(input: u8) {}",
        "fn parse_borrowed_plain_hincrbyfloat_packet(input: u8) {}",
        "fn parse_borrowed_plain_smove_packet(input: u8) {}",
    ])
    rt = "\n".join([
        "pub fn execute_plain_hincrby_borrowed(x: u8) {}",
        "pub fn execute_plain_smove_borrowed(x: u8) {}",
    ])
    trio = borrowed_trio(srv, rt)
    assert trio["parser"] == {"hincrby", "hincrbyfloat", "smove"}, trio["parser"]
    assert trio["executor"] == {"hincrby", "smove"}, trio["executor"]
    assert "hincrbyfloat" in trio["parser"] and "hincrbyfloat" not in trio["executor"], (
        "hincrbyfloat must land in [B] PARTIAL, not [A] ready"
    )

    # (frankenredis-gvm6z) A command served ONLY by a SHARED executor must still count as
    # having one. The fixture is the POST-REMOVAL state of PEXPIRETIME, which is scheduled:
    # b2df577ba landed a duplicate per-command PEXPIRETIME executor, found it REDUNDANT, and
    # pinned it "removable rather than removed". The moment it goes, the per-command scan
    # stops seeing pexpiretime, and without the shared-executor union this report emits the
    # same false [C] that caused the duplicate to be written in the first place -- a loop.
    #
    # Fixture rather than live source ON PURPOSE: wiring the map in changes NOTHING about
    # today's report (measured: [A]=1 [B]=2 [C]=46 both ways) precisely because the
    # redundant duplicate is still present. A test asserting against live source would
    # therefore pass for the wrong reason and stop testing anything the day it matters.
    srv_only_parser = "fn parse_borrowed_plain_pexpiretime_packet(i: u8) {}"
    rt_shared_only = "\n".join([
        "pub enum PlainKeyMetaCmd {",
        "    Ttl,",
        "    Pexpiretime,",
        "}",
        "pub fn execute_plain_keymeta_borrowed(cmd: PlainKeyMetaCmd) {}",
    ])
    # The per-command scan alone is blind to it -- that is the defect being guarded.
    per_command_only = {
        m.group(1).lower()
        for m in re.finditer(r"fn execute_plain_([a-z0-9_]+?)_borrowed", rt_shared_only)
    }
    assert "pexpiretime" not in per_command_only, per_command_only
    assert "keymeta" in per_command_only, "the shared fn itself is what the scan sees instead"
    # With the union it is found, so pexpiretime lands in [A]/[B], never [C].
    trio_shared = borrowed_trio(srv_only_parser, rt_shared_only)
    assert "pexpiretime" in trio_shared["executor"], trio_shared["executor"]
    assert "pexpiretime" in trio_shared["parser"], trio_shared["parser"]

    print("self-test ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(_self_test() if "--self-test" in sys.argv else report())
