#!/usr/bin/env python3
"""Differential gate: SORT ... ALPHA locale collation (frankenredis-zzt3u).

redis 7.2.4 `SORT key ALPHA` orders elements with glibc `strcoll` (locale
collation), NOT binary memcmp — lowercase sorts before an uppercase first letter,
accents collate next to their base letter, etc. fr matches this via a Rust
collator (frankenredis-jaezc, 1d7320fbf). This gate locks the cases the collator
reproduces byte-exact: pure letters, mixed case, accented Latin (é/è/ê), CJK,
numeric-as-string, alphanumeric, and single space/dash.

KNOWN RESIDUAL (frankenredis-zzt3u — a WONTFIX, intentionally NOT asserted here):
glibc collation gives ignorable characters (whitespace, punctuation, control) ZERO
primary weight, so elements differing only by those chars (e.g. "ab" vs "a-b" vs
"a b") collate as EQUAL and the order is decided by glibc's UNSTABLE qsort tie-break
— non-deterministic on redis AND fr (the same identical SORT can return different
orders run-to-run). Byte-exact parity is therefore impossible there, the same
glibc-qsort-chaos family as the SORT BY missing-* ALPHA tie-order WONTFIX. Only
elements with strictly DISTINCT collation weight (letters/case/accents/CJK/numbers,
no ignorable-char ties) are deterministic; this gate pins exactly those.

Usage: sort_alpha_collation_differ.py <oracle_port> <fr_port>
       Exit 0 = working-set byte-exact, 1 = regression in the supported cases.
"""
import sys

from _respread import assert_seed, cmd, conn

# (label, members) — SORT ALPHA over each must be byte-identical to redis 7.2.4.
CASES = [
    ("lower_words", ["banana", "apple", "cherry", "date", "fig", "grape"]),
    ("mixed_case", ["Apple", "apple", "Banana", "banana", "Cherry", "cherry"]),
    ("case_words", ["Zebra", "apple", "Mango", "banana", "Cherry", "aardvark"]),
    ("accented", ["café", "cafe", "cafz", "caff", "cafè"]),
    ("accent_letters", ["é", "e", "è", "ê", "a", "z", "n", "ñ"]),
    ("numbers_as_str", ["10", "9", "100", "2", "30", "1", "21"]),
    ("alphanumeric", ["a1", "a10", "a2", "ab", "a", "a1b", "a1a"]),
    ("cjk_mixed", ["中", "あ", "a", "z", "1", "Z", "ω"]),
    ("desc_words", ["delta", "alpha", "charlie", "bravo"]),
]

# Each tested under these option suffixes (LIMIT/DESC must compose with collation).
OPTS = [
    ("ALPHA",),
    ("ALPHA", "DESC"),
    ("ALPHA", "LIMIT", "0", "3"),
    ("ALPHA", "DESC", "LIMIT", "1", "3"),
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []
    n = 0
    for label, members in CASES:
        # utf-8, explicitly: the deliberate exception in _respread.encode_arg's
        # docstring. Its latin-1 default exists so a "\xNN" escape means the one
        # byte it names; these accented/CJK members are real TEXT, which is what
        # a real client sends — and "中" has no latin-1 encoding at all.
        members = [m.encode("utf-8") for m in members]
        for opts in OPTS:
            cmd(od, "DEL", "sk")
            cmd(fr, "DEL", "sk")
            assert_seed(cmd(od, "RPUSH", "sk", *members), len(members), f"redis RPUSH {label}")
            assert_seed(cmd(fr, "RPUSH", "sk", *members), len(members), f"fr RPUSH {label}")
            ro, rf = cmd(od, "SORT", "sk", *opts), cmd(fr, "SORT", "sk", *opts)
            n += 1
            if ro != rf:
                fails.append(f"{label} {opts}: redis={ro!r} fr={rf!r}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} SORT ALPHA collation divergence(s) vs redis 7.2.4:")
        for x in fails[:10]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — SORT ALPHA locale collation byte-exact vs redis 7.2.4 "
        f"({n} cases: distinct-weight letters/case/accents/CJK/numbers x ALPHA/DESC/LIMIT) "
        "[ignorable-char qsort-tie residual = zzt3u WONTFIX]"
    )


if __name__ == "__main__":
    main()
