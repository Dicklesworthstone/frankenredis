#!/usr/bin/env python3
"""Generate structured corpus seeds for fuzz_psync_reply.

The fuzz target dispatches the first byte two ways (`mode % 2`):

    0 → fuzz_raw_psync_reply(body)
        body is fed straight into `fr_repl::parse_psync_reply` after
        a UTF-8-lossy conversion. Accepted replies are required to
        round-trip through their canonical form.

    1 → fuzz_structured_psync_reply(body)
        body is fed to `arbitrary::Unstructured`. The arbitrary
        format is intentionally not version-stable, so we only seed
        mode 0 here — libfuzzer's mutator will discover the
        structured-mode shapes from the mode-0 seeds.

The seeds target the **shape boundaries** of `parse_psync_reply`
(crates/fr-repl/src/lib.rs):

  - empty input + whitespace-only → rejection
  - canonical `CONTINUE` and `CONTINUE <replid>` (PSYNC2)
  - canonical `FULLRESYNC <replid> <offset>`
  - lower / mixed case (must reject — kind is case-sensitive)
  - leading / trailing whitespace around accepted CONTINUE shapes
  - missing replid / missing offset
  - non-numeric / negative / overflow offsets
  - extra token after the offset
  - non-CONTINUE / non-FULLRESYNC first token
  - exactly 40-byte FULLRESYNC replids (the Redis run ID width)
  - very-short / very-long FULLRESYNC replid rejection boundaries

Each seed is `<mode-byte><body>`. mode_byte is 0 (so `mode % 2 == 0`
selects the raw path).

Run:
    python3 fuzz/scripts/gen_psync_reply_seeds.py
"""
from __future__ import annotations

from pathlib import Path

MODE_RAW = b"\x00"


def seed(label: str, body: bytes) -> tuple[str, bytes]:
    return (label, MODE_RAW + body)


def main() -> None:
    repo = Path(__file__).resolve().parent.parent.parent
    out_dir = repo / "fuzz" / "corpus" / "fuzz_psync_reply"
    out_dir.mkdir(parents=True, exist_ok=True)

    seeds: list[tuple[str, bytes]] = [
        # ── Empty / whitespace-only — rejection paths ────────────
        seed("empty.txt", b""),
        seed("whitespace_only_space.txt", b"   "),
        seed("whitespace_only_tab.txt", b"\t\t"),
        seed("whitespace_only_crlf.txt", b"\r\n\r\n"),
        # ── Canonical accepted shapes ────────────────────────────
        seed("continue_canonical.txt", b"CONTINUE"),
        seed("continue_with_psync2_replid.txt",
             b"CONTINUE 1234567890abcdef1234567890abcdef12345678"),
        seed("fullresync_canonical_offset_zero.txt",
             b"FULLRESYNC 1234567890abcdef1234567890abcdef12345678 0"),
        seed("fullresync_canonical_offset_typical.txt",
             b"FULLRESYNC 1234567890abcdef1234567890abcdef12345678 4096"),
        seed("fullresync_canonical_offset_max_u64.txt",
             b"FULLRESYNC 1234567890abcdef1234567890abcdef12345678 18446744073709551615"),
        # ── Whitespace tolerance under split_ascii_whitespace ────
        seed("continue_leading_tab.txt", b"\tCONTINUE"),
        seed("continue_trailing_crlf.txt", b"CONTINUE\r\n"),
        # These two keep the whitespace shape in the raw corpus, but
        # must now reject because FULLRESYNC replids are fixed at 40 bytes.
        seed("fullresync_multi_whitespace_separators.txt",
             b"FULLRESYNC \t  abc\t\t100\r\n"),
        seed("fullresync_with_newlines_between_tokens.txt",
             b"FULLRESYNC\nabcd\n42\n"),
        # ── Case sensitivity (must reject lowercase / mixed) ─────
        seed("continue_lowercase.txt", b"continue"),
        seed("continue_mixedcase.txt", b"Continue"),
        seed("fullresync_lowercase.txt", b"fullresync abc 0"),
        # ── Malformed FULLRESYNC variants ────────────────────────
        seed("fullresync_missing_replid_and_offset.txt", b"FULLRESYNC"),
        seed("fullresync_missing_offset.txt", b"FULLRESYNC abc"),
        seed("fullresync_offset_negative.txt", b"FULLRESYNC abc -1"),
        seed("fullresync_offset_overflow_u64.txt",
             b"FULLRESYNC abc 18446744073709551616"),
        seed("fullresync_offset_nonnumeric.txt", b"FULLRESYNC abc xyz"),
        seed("fullresync_extra_trailing_token.txt",
             b"FULLRESYNC abc 100 extra"),
        seed("fullresync_extra_double_trailing_tokens.txt",
             b"FULLRESYNC abc 100 extra1 extra2"),
        # ── Malformed CONTINUE — 3 tokens triggers the
        #    "Some, Some" rejection branch.
        seed("continue_with_extra_token.txt",
             b"CONTINUE replid extra-token"),
        seed("continue_with_three_extras.txt",
             b"CONTINUE replid extra1 extra2"),
        # ── Wrong first token ─────────────────────────────────────
        seed("first_token_unknown.txt", b"GREETINGS abc 100"),
        seed("first_token_empty_after_resp_prefix.txt", b"+CONTINUE"),
        # ── Replid edge cases: all reject unless exactly 40 bytes ─
        seed("fullresync_one_char_replid.txt", b"FULLRESYNC a 0"),
        seed("fullresync_long_replid.txt",
             b"FULLRESYNC " + b"a" * 200 + b" 0"),
        seed("fullresync_numeric_replid.txt",
             b"FULLRESYNC 12345 100"),
        seed("fullresync_replid_with_punctuation.txt",
             b"FULLRESYNC repl-id_with.dashes 100"),
    ]

    for label, payload in seeds:
        path = out_dir / label
        path.write_bytes(payload)
        print(f"wrote {len(payload):4d} bytes to {path.relative_to(repo)}")
    print(f"\ngenerated {len(seeds)} corpus seeds")


if __name__ == "__main__":
    main()
