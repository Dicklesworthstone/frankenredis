#!/usr/bin/env python3
"""Generate structured corpus seeds for fuzz_migrate_request.

The fuzz target's `raw_argv` pre-pends `MIGRATE` to the seed body,
then splits the body on `\\n`/`\\r`/`\\0` into argv tokens. Each
seed therefore encodes the trailing argv (everything after MIGRATE)
as newline-separated tokens.

Canonical MIGRATE shape:
    MIGRATE host port key destination-db timeout [COPY] [REPLACE]
            [AUTH password] [AUTH2 username password]
            [KEYS key [key ...]]

The accept-class invariant is canonical round-trip: the parsed
`MigrateRequest` must re-serialize back through
`canonical_migrate_argv` and reparse to the same struct.

  Accept-class (14+ seeds):
    - Minimal: host port key db timeout
    - With COPY / REPLACE / both
    - With AUTH password
    - With AUTH2 username password
    - With AUTH + REPLACE
    - KEYS mode (key arg empty, then KEYS k1 [k2 ...])
    - KEYS mode with AUTH
    - Various host shapes: hostname, IPv4, IPv6
    - Negative / zero timeout (clamps to 1000ms per upstream)
    - Various DB indices (0, 15)
    - Long KEYS list

  Reject-class:
    - Too few arity (< 5 args after MIGRATE)
    - Non-numeric port / db / timeout
    - AUTH missing password arg
    - AUTH2 missing username
    - AUTH2 missing password (single trailing token)
    - KEYS with non-empty key arg (must be empty string per spec)
    - Unknown option token

Run:
    python3 fuzz/scripts/gen_migrate_request_seeds.py
"""
from __future__ import annotations

from pathlib import Path


def seed(label: str, argv_after_migrate: list[bytes]) -> tuple[str, bytes]:
    return (label, b"\n".join(argv_after_migrate) + b"\n")


def main() -> None:
    repo = Path(__file__).resolve().parent.parent.parent
    out_dir = repo / "fuzz" / "corpus" / "fuzz_migrate_request"
    out_dir.mkdir(parents=True, exist_ok=True)

    seeds: list[tuple[str, bytes]] = [
        # ── Accept-class ─────────────────────────────────────────
        seed("minimal_no_options",
             [b"localhost", b"6379", b"key", b"0", b"5000"]),
        seed("with_copy",
             [b"localhost", b"6379", b"key", b"0", b"5000", b"COPY"]),
        seed("with_replace",
             [b"localhost", b"6379", b"key", b"0", b"5000", b"REPLACE"]),
        seed("with_copy_and_replace",
             [b"localhost", b"6379", b"key", b"0", b"5000",
              b"COPY", b"REPLACE"]),
        seed("with_auth",
             [b"localhost", b"6379", b"key", b"0", b"5000",
              b"AUTH", b"secret"]),
        seed("with_auth2",
             [b"localhost", b"6379", b"key", b"0", b"5000",
              b"AUTH2", b"user", b"password"]),
        seed("with_auth_and_replace",
             [b"localhost", b"6379", b"key", b"0", b"5000",
              b"REPLACE", b"AUTH", b"secret"]),
        seed("with_auth2_and_copy",
             [b"localhost", b"6379", b"key", b"0", b"5000",
              b"COPY", b"AUTH2", b"u", b"p"]),
        # KEYS mode requires the key arg to be the empty string.
        seed("keys_mode_two_keys",
             [b"localhost", b"6379", b"", b"0", b"5000",
              b"KEYS", b"k1", b"k2"]),
        seed("keys_mode_single_key",
             [b"localhost", b"6379", b"", b"0", b"5000",
              b"KEYS", b"only"]),
        seed("keys_mode_with_auth",
             [b"localhost", b"6379", b"", b"0", b"5000",
              b"AUTH", b"secret", b"KEYS", b"k1", b"k2", b"k3"]),
        seed("keys_mode_with_auth2_and_replace",
             [b"localhost", b"6379", b"", b"0", b"5000",
              b"REPLACE", b"AUTH2", b"u", b"p",
              b"KEYS", b"k1", b"k2"]),
        seed("ipv4_host",
             [b"127.0.0.1", b"6379", b"key", b"0", b"5000"]),
        seed("ipv6_loopback_host",
             [b"::1", b"6379", b"key", b"0", b"5000"]),
        seed("hostname_with_dots",
             [b"redis.example.com", b"6379", b"key", b"0", b"5000"]),
        seed("db_index_max",
             [b"localhost", b"6379", b"key", b"15", b"5000"]),
        seed("negative_timeout_clamps",
             [b"localhost", b"6379", b"key", b"0", b"-100"]),
        seed("zero_timeout_clamps",
             [b"localhost", b"6379", b"key", b"0", b"0"]),
        seed("large_timeout",
             [b"localhost", b"6379", b"key", b"0", b"60000"]),
        seed("auth_password_with_special_chars",
             [b"localhost", b"6379", b"key", b"0", b"5000",
              b"AUTH", b"p@s$w0rd!#"]),
        seed(
            "keys_mode_long_list",
            [b"localhost", b"6379", b"", b"0", b"5000", b"KEYS"]
            + [bytes(f"k{i}", "ascii") for i in range(8)],
        ),
        # ── Reject-class ──────────────────────────────────────────
        seed("too_few_args",
             [b"localhost", b"6379", b"key", b"0"]),
        seed("invalid_port",
             [b"localhost", b"abc", b"key", b"0", b"5000"]),
        seed("invalid_db_index",
             [b"localhost", b"6379", b"key", b"xyz", b"5000"]),
        seed("invalid_timeout",
             [b"localhost", b"6379", b"key", b"0", b"five"]),
        seed("auth_missing_password",
             [b"localhost", b"6379", b"key", b"0", b"5000", b"AUTH"]),
        seed("auth2_missing_username_and_password",
             [b"localhost", b"6379", b"key", b"0", b"5000", b"AUTH2"]),
        seed("auth2_missing_password",
             [b"localhost", b"6379", b"key", b"0", b"5000",
              b"AUTH2", b"username"]),
        seed("keys_with_non_empty_key_arg",
             [b"localhost", b"6379", b"nonempty", b"0", b"5000",
              b"KEYS", b"k1"]),
        seed("unknown_option",
             [b"localhost", b"6379", b"key", b"0", b"5000", b"BOGUS"]),
        seed("unknown_option_after_keys",
             [b"localhost", b"6379", b"", b"0", b"5000",
              b"KEYS", b"k1", b"BOGUS"]),
    ]

    for label, payload in seeds:
        path = out_dir / label
        path.write_bytes(payload)
        print(f"wrote {len(payload):4d} bytes to {path.relative_to(repo)}")
    print(f"\ngenerated {len(seeds)} corpus seeds")


if __name__ == "__main__":
    main()
