#!/usr/bin/env python3
"""Generate structured corpus seeds for fuzz_client_tracking.

The fuzz target runs every input through `fuzz_raw_client_tracking`
(parsing argv via `argv_from_raw` and feeding to
`parse_client_tracking_state`) and a structured-arbitrary path. The
documented invariant on accept-class inputs is canonical
round-trip: parse → render canonical argv → reparse must produce
the same `ClientTrackingState`.

Seeds are most valuable as text-form CLIENT TRACKING command lines
covering each option combination + each error branch:

  Accept-class:
    - CLIENT TRACKING OFF
    - CLIENT TRACKING ON                           (plain)
    - CLIENT TRACKING ON BCAST                     (broadcast)
    - CLIENT TRACKING ON OPTIN                     (opt-in mode)
    - CLIENT TRACKING ON OPTOUT                    (opt-out mode)
    - CLIENT TRACKING ON NOLOOP
    - CLIENT TRACKING ON BCAST PREFIX <p>          (PREFIX needs BCAST)
    - CLIENT TRACKING ON BCAST PREFIX a PREFIX b   (multiple prefixes)
    - CLIENT TRACKING ON BCAST NOLOOP
    - CLIENT TRACKING ON OPTIN NOLOOP
    - CLIENT TRACKING ON OPTOUT NOLOOP
    - CLIENT TRACKING ON REDIRECT <id>             (with redirect target)
    - CLIENT TRACKING ON REDIRECT <id> BCAST PREFIX <p>
    - lowercase / mixed-case option tokens
    - CLIENT TRACKING OFF after redirect (state cleared)

  Reject-class:
    - CLIENT TRACKING                              (missing mode arg)
    - CLIENT TRACKING ON BCAST OPTIN               (BCAST+OPTIN conflict)
    - CLIENT TRACKING ON BCAST OPTOUT              (BCAST+OPTOUT conflict)
    - CLIENT TRACKING ON OPTIN OPTOUT              (OPTIN+OPTOUT conflict)
    - CLIENT TRACKING ON PREFIX foo                (PREFIX without BCAST)
    - CLIENT TRACKING ON REDIRECT                  (missing arg)
    - CLIENT TRACKING ON REDIRECT 0                (zero is invalid)
    - CLIENT TRACKING ON PREFIX                    (missing arg)
    - CLIENT TRACKING BOGUS                        (invalid mode)

Run:
    python3 fuzz/scripts/gen_client_tracking_seeds.py
"""
from __future__ import annotations

from pathlib import Path


def seed(label: str, body: bytes) -> tuple[str, bytes]:
    return (label, body)


def main() -> None:
    repo = Path(__file__).resolve().parent.parent.parent
    out_dir = repo / "fuzz" / "corpus" / "fuzz_client_tracking"
    out_dir.mkdir(parents=True, exist_ok=True)

    seeds: list[tuple[str, bytes]] = [
        # ── Accept-class: every documented option combination ──
        seed("tracking_off.txt", b"CLIENT TRACKING OFF\n"),
        seed("tracking_on_plain.txt", b"CLIENT TRACKING ON\n"),
        seed("tracking_on_bcast.txt", b"CLIENT TRACKING ON BCAST\n"),
        seed("tracking_on_optin.txt", b"CLIENT TRACKING ON OPTIN\n"),
        seed("tracking_on_optout.txt", b"CLIENT TRACKING ON OPTOUT\n"),
        seed("tracking_on_noloop.txt", b"CLIENT TRACKING ON NOLOOP\n"),
        seed(
            "tracking_on_bcast_one_prefix.txt",
            b"CLIENT TRACKING ON BCAST PREFIX foo\n",
        ),
        seed(
            "tracking_on_bcast_two_prefixes.txt",
            b"CLIENT TRACKING ON BCAST PREFIX foo PREFIX bar\n",
        ),
        seed(
            "tracking_on_bcast_three_prefixes.txt",
            b"CLIENT TRACKING ON BCAST PREFIX a PREFIX b PREFIX c\n",
        ),
        seed(
            "tracking_on_bcast_noloop.txt",
            b"CLIENT TRACKING ON BCAST NOLOOP\n",
        ),
        seed(
            "tracking_on_optin_noloop.txt",
            b"CLIENT TRACKING ON OPTIN NOLOOP\n",
        ),
        seed(
            "tracking_on_optout_noloop.txt",
            b"CLIENT TRACKING ON OPTOUT NOLOOP\n",
        ),
        seed(
            "tracking_on_redirect.txt",
            b"CLIENT TRACKING ON REDIRECT 12345\n",
        ),
        seed(
            "tracking_on_redirect_bcast_prefix.txt",
            b"CLIENT TRACKING ON REDIRECT 1 BCAST PREFIX foo\n",
        ),
        seed(
            "tracking_on_redirect_optin_noloop.txt",
            b"CLIENT TRACKING ON REDIRECT 99 OPTIN NOLOOP\n",
        ),
        # ── Case sensitivity (option tokens are case-insensitive) ─
        seed("tracking_lowercase.txt", b"client tracking on\n"),
        seed("tracking_mixed_case.txt", b"Client Tracking On Optin Noloop\n"),
        seed(
            "tracking_uppercase_prefix_value.txt",
            b"CLIENT TRACKING ON BCAST PREFIX FOO\n",
        ),
        # ── Edge: prefix with binary-safe content ────────────────
        seed(
            "tracking_on_bcast_prefix_with_colon.txt",
            b"CLIENT TRACKING ON BCAST PREFIX user:\n",
        ),
        # Raw fuzz argv parsing has no quote semantics and cannot
        # encode a zero-length token; the crate contract test covers
        # a real empty PREFIX through an explicit argv vector.
        seed(
            "tracking_on_bcast_empty_prefix.txt",
            b"CLIENT TRACKING ON BCAST PREFIX \"\"\n",
        ),
        # ── Reject-class: each must surface an error ──────────────
        seed("tracking_no_mode_arg.txt", b"CLIENT TRACKING\n"),
        seed("tracking_on_bcast_optin_conflict.txt",
             b"CLIENT TRACKING ON BCAST OPTIN\n"),
        seed("tracking_on_bcast_optout_conflict.txt",
             b"CLIENT TRACKING ON BCAST OPTOUT\n"),
        seed("tracking_on_optin_optout_conflict.txt",
             b"CLIENT TRACKING ON OPTIN OPTOUT\n"),
        seed("tracking_on_prefix_without_bcast.txt",
             b"CLIENT TRACKING ON PREFIX foo\n"),
        seed("tracking_on_redirect_missing_arg.txt",
             b"CLIENT TRACKING ON REDIRECT\n"),
        seed("tracking_on_redirect_zero.txt",
             b"CLIENT TRACKING ON REDIRECT 0\n"),
        seed("tracking_on_prefix_missing_arg.txt",
             b"CLIENT TRACKING ON BCAST PREFIX\n"),
        seed("tracking_bogus_mode.txt", b"CLIENT TRACKING BOGUS\n"),
        seed(
            "tracking_on_redirect_negative_id.txt",
            b"CLIENT TRACKING ON REDIRECT -1\n",
        ),
        seed(
            "tracking_on_redirect_nonnumeric.txt",
            b"CLIENT TRACKING ON REDIRECT abc\n",
        ),
    ]

    for label, payload in seeds:
        path = out_dir / label
        path.write_bytes(payload)
        print(f"wrote {len(payload):4d} bytes to {path.relative_to(repo)}")
    print(f"\ngenerated {len(seeds)} corpus seeds")


if __name__ == "__main__":
    main()
