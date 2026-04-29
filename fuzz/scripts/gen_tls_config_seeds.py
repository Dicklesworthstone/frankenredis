#!/usr/bin/env python3
"""Generate structured corpus seeds for fuzz_tls_config.

The fuzz target runs every input through `fuzz_raw_protocols`
(parsing as a tls-protocols directive value via
`parse_tls_protocols`) AND a structured-arbitrary path (which
exercises the directive validation matrix + config plan path).
Seeds are most valuable as text-form tls-protocols values that
cover each branch of `parse_tls_protocols` + `TlsProtocol::parse`.

`parse_tls_protocols` semantics (per fr-config):

  - splits on literal spaces, filters empty tokens
  - each token: TlsProtocol::parse → TlsV1 / TlsV1_1 /
    TlsV1_2 / TlsV1_3 by Redis protocol token, case-insensitive
  - first-seen order preserved; duplicates dropped silently
  - result must be non-empty (empty list rejected)

The harness's invariant: parse_tls_protocols result must agree
with validate_tls_directive_value(TlsDirective::TlsProtocols, …)
on accept/reject, and accepted protocols must canonical-round-trip.

  Accept-class:
    - canonical: "TLSv1 TLSv1.1 TLSv1.2 TLSv1.3"
    - lowercase: "tlsv1.2 tlsv1.3"
    - mixed-case: "TlSv1.2 tLsV1.3"
    - single protocol: "TLSv1" / "TLSv1.1" / "TLSv1.2" / "TLSv1.3"
    - both protocols, dedup: "TLSv1.2 TLSv1.2 TLSv1.3"
    - multiple-space separators
    - leading + trailing whitespace

  Reject-class:
    - empty input
    - whitespace-only
    - commas-only (all empties)
    - Redis-unsupported aliases (tls1.2 / tlsv1_1 / tlsv1_2 / tls1.3 / tlsv1_3)
    - comma/tab/CRLF separators
    - unsupported version (TLSv1.0 / TLSv1.4 / SSL)
    - mixed valid + invalid
    - non-ASCII (emoji)

Run:
    python3 fuzz/scripts/gen_tls_config_seeds.py
"""
from __future__ import annotations

from pathlib import Path


def seed(label: str, body: bytes) -> tuple[str, bytes]:
    return (label, body)


def main() -> None:
    repo = Path(__file__).resolve().parent.parent.parent
    out_dir = repo / "fuzz" / "corpus" / "fuzz_tls_config"
    out_dir.mkdir(parents=True, exist_ok=True)

    seeds: list[tuple[str, bytes]] = [
        # ── Accept-class ─────────────────────────────────────────
        seed("canonical_all_protocols.txt", b"TLSv1 TLSv1.1 TLSv1.2 TLSv1.3"),
        seed("canonical_both_protocols.txt", b"TLSv1.2 TLSv1.3"),
        seed("canonical_legacy_protocols.txt", b"TLSv1 TLSv1.1"),
        seed("lowercase_both.txt", b"tlsv1.2 tlsv1.3"),
        seed("lowercase_legacy.txt", b"tlsv1 tlsv1.1"),
        seed("mixed_case.txt", b"TlSv1.2 tLsV1.3"),
        seed("mixed_case_legacy.txt", b"TlSv1 tLsV1.1"),
        seed("only_tlsv1.txt", b"TLSv1"),
        seed("only_tlsv1_1.txt", b"TLSv1.1"),
        seed("only_tlsv1_2.txt", b"TLSv1.2"),
        seed("only_tlsv1_3.txt", b"TLSv1.3"),
        seed("dedup_repeated.txt", b"TLSv1.2 TLSv1.2 TLSv1.3"),
        seed("dedup_legacy_repeated.txt", b"TLSv1 TLSv1 TLSv1.1"),
        seed("multiple_spaces.txt", b"TLSv1.2    TLSv1.3"),
        seed("leading_whitespace.txt", b"   TLSv1.2 TLSv1.3"),
        seed("trailing_whitespace.txt", b"TLSv1.2 TLSv1.3   "),
        # ── Reject-class ──────────────────────────────────────────
        seed("empty.txt", b""),
        seed("whitespace_only.txt", b"   \t\n"),
        seed("commas_only.txt", b",,,,"),
        seed("canonical_comma_separated.txt", b"TLSv1.2,TLSv1.3"),
        seed("alias_tls1_2.txt", b"tls1.2"),
        seed("alias_tlsv1_2_underscore.txt", b"tlsv1_2"),
        seed("alias_tls1_3.txt", b"tls1.3"),
        seed("alias_tlsv1_3_underscore.txt", b"tlsv1_3"),
        seed("dedup_mixed_aliases.txt", b"TLSv1.2 tls1.2 tlsv1_2 TLSv1.3"),
        seed("tab_separator.txt", b"TLSv1.2\tTLSv1.3"),
        seed("multi_separator.txt", b"TLSv1.2 ,\tTLSv1.3"),
        seed("leading_comma.txt", b",TLSv1.2"),
        seed("trailing_comma.txt", b"TLSv1.2,"),
        seed("multiple_commas.txt", b"TLSv1.2,,,,TLSv1.3"),
        seed("crlf_separators.txt", b"TLSv1.2\r\nTLSv1.3"),
        seed("unsupported_tls_v1_0.txt", b"TLSv1.0"),
        seed("unsupported_tls_v1_1.txt", b"tlsv1_1"),
        seed("unsupported_tls_v1_4.txt", b"TLSv1.4"),
        seed("unsupported_ssl.txt", b"SSLv3"),
        seed("mixed_valid_and_invalid.txt", b"TLSv1.2 TLSv1.0"),
        seed("nonsense_token.txt", b"banana"),
        seed("numeric_only.txt", b"1.2"),
        seed("almost_match.txt", b"TLSv2.0"),
        seed("unicode_emoji.txt", "TLSv1.2 \U0001F4A9".encode("utf-8")),
        seed("just_dot.txt", b"."),
        seed("just_v.txt", b"v"),
    ]

    for label, payload in seeds:
        path = out_dir / label
        path.write_bytes(payload)
        print(f"wrote {len(payload):4d} bytes to {path.relative_to(repo)}")
    print(f"\ngenerated {len(seeds)} corpus seeds")


if __name__ == "__main__":
    main()
