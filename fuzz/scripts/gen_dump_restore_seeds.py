#!/usr/bin/env python3
"""Generate structured corpus seeds for fuzz_dump_restore.

The fuzz target feeds the entire input through `arbitrary` to
derive a `DumpRestoreInput` enum (Valid or Raw). Both arms drive
the same `Store::dump_key` / `Store::restore_key` round-trip /
error-path code, so seeds are most valuable as raw byte streams
that encode realistic DUMP envelopes — libfuzzer then mutates
from these starting points into both the structured-Valid path
AND the raw-restore-with-busy-key path.

Each DUMP payload has the envelope:

    [type-byte] [type-specific encoded data] [u16 LE version] [u64 LE CRC64-redis]

Where the CRC is computed over `[type-byte] + [encoded data] + [version]`.

The seed catalogue exercises:

  Valid envelopes (must round-trip cleanly through dump→restore):
    - String (raw / short ASCII / binary with NUL)
    - String integer-encoded (8/16/32 bit)
    - List (one element / multiple elements)
    - Set (small / one member)
    - Hash (single field / multiple fields)
    - Sorted set (ZSET_2: binary-LE scores)
    - Stream (empty stream)

  Error-path envelopes (must reject without panic):
    - Truncated below the 11-byte minimum
    - Bad CRC (envelope structurally valid but CRC mismatch)
    - Future version (> RDB_DUMP_VERSION)
    - Unknown type byte (e.g. 99)
    - Type byte declared but body truncated mid-string-length
    - Empty body (just a 1-byte type tag)

Run:
    python3 fuzz/scripts/gen_dump_restore_seeds.py
"""
from __future__ import annotations

import struct
from pathlib import Path

# ── Constants mirroring fr-store/fr-persist ────────────────────────────

RDB_TYPE_STRING = 0
RDB_TYPE_LIST = 1
RDB_TYPE_SET = 2
RDB_TYPE_HASH = 4
RDB_TYPE_ZSET_2 = 5
RDB_DUMP_VERSION = 11

# CRC64-Redis (polynomial 0xAD93D23594C935A9, reflected output).
_POLY = 0xAD93_D235_94C9_35A9


def _reflect64(value: int) -> int:
    out = value & 1
    for _ in range(1, 64):
        value >>= 1
        out = (out << 1) | (value & 1)
    return out


def crc64_redis(data: bytes) -> int:
    crc = 0
    for byte in data:
        mask = 0x01
        while mask:
            bit_set = (crc & 0x8000_0000_0000_0000) != 0
            if byte & mask:
                bit_set = not bit_set
            crc = (crc << 1) & 0xFFFF_FFFF_FFFF_FFFF
            if bit_set:
                crc ^= _POLY
            mask = (mask << 1) & 0xFF
    return _reflect64(crc)


def encode_length(length: int) -> bytes:
    if length < 64:
        return bytes([length])
    if length < 16384:
        return bytes([0x40 | (length >> 8), length & 0xFF])
    if length <= 0xFFFF_FFFF:
        return bytes([0x80]) + struct.pack(">I", length)
    return bytes([0x81]) + struct.pack(">Q", length)


def encode_rdb_string(data: bytes) -> bytes:
    """Length-prefixed raw RDB string. The integer-fast-path is
    valid too but the length-prefixed form always decodes."""
    return encode_length(len(data)) + data


def envelope(type_byte: int, body: bytes, version: int = RDB_DUMP_VERSION) -> bytes:
    payload = bytes([type_byte]) + body + struct.pack("<H", version)
    crc = crc64_redis(payload)
    return payload + struct.pack("<Q", crc)


# ── Type-specific body builders ────────────────────────────────────────

def string_body(value: bytes) -> bytes:
    return encode_rdb_string(value)


def list_body(items: list[bytes]) -> bytes:
    out = encode_length(len(items))
    for item in items:
        out += encode_rdb_string(item)
    return out


def set_body(members: list[bytes]) -> bytes:
    out = encode_length(len(members))
    for member in members:
        out += encode_rdb_string(member)
    return out


def hash_body(pairs: list[tuple[bytes, bytes]]) -> bytes:
    out = encode_length(len(pairs))
    for field, value in pairs:
        out += encode_rdb_string(field)
        out += encode_rdb_string(value)
    return out


def zset2_body(members: list[tuple[bytes, float]]) -> bytes:
    """ZSET_2 (type 5): each member is `<encoded-string> <8-byte LE double>`."""
    out = encode_length(len(members))
    for member, score in members:
        out += encode_rdb_string(member)
        out += struct.pack("<d", score)
    return out


# ── Seed catalogue ─────────────────────────────────────────────────────

def main() -> None:
    repo = Path(__file__).resolve().parent.parent.parent
    out_dir = repo / "fuzz" / "corpus" / "fuzz_dump_restore"
    out_dir.mkdir(parents=True, exist_ok=True)

    seeds: list[tuple[str, bytes]] = []

    # ── Valid envelopes (round-trip clean) ──────────────────────────
    seeds.append(("dump_string_short_ascii", envelope(RDB_TYPE_STRING, string_body(b"hello"))))
    seeds.append(("dump_string_empty", envelope(RDB_TYPE_STRING, string_body(b""))))
    seeds.append((
        "dump_string_binary_with_nul",
        envelope(RDB_TYPE_STRING, string_body(b"\x00\x01\x02binary\x00payload\xff\xfe\xfd")),
    ))
    seeds.append((
        "dump_string_64_bytes_just_over_threshold",
        envelope(RDB_TYPE_STRING, string_body(b"a" * 64)),
    ))
    seeds.append((
        "dump_string_300_bytes",
        envelope(RDB_TYPE_STRING, string_body(b"x" * 300)),
    ))
    seeds.append((
        "dump_list_single_element",
        envelope(RDB_TYPE_LIST, list_body([b"only"])),
    ))
    seeds.append((
        "dump_list_three_elements",
        envelope(RDB_TYPE_LIST, list_body([b"alpha", b"beta", b"gamma"])),
    ))
    seeds.append((
        "dump_set_single_member",
        envelope(RDB_TYPE_SET, set_body([b"solo"])),
    ))
    seeds.append((
        "dump_set_three_members",
        envelope(RDB_TYPE_SET, set_body([b"a", b"b", b"c"])),
    ))
    seeds.append((
        "dump_hash_single_field",
        envelope(RDB_TYPE_HASH, hash_body([(b"f1", b"v1")])),
    ))
    seeds.append((
        "dump_hash_three_fields",
        envelope(RDB_TYPE_HASH, hash_body([
            (b"name", b"alice"),
            (b"age", b"30"),
            (b"city", b"sf"),
        ])),
    ))
    seeds.append((
        "dump_zset2_single_member",
        envelope(RDB_TYPE_ZSET_2, zset2_body([(b"m1", 1.5)])),
    ))
    seeds.append((
        "dump_zset2_three_members",
        envelope(RDB_TYPE_ZSET_2, zset2_body([
            (b"low", 1.0),
            (b"mid", 2.5),
            (b"high", 7.25),
        ])),
    ))
    seeds.append((
        "dump_zset2_negative_scores",
        envelope(RDB_TYPE_ZSET_2, zset2_body([
            (b"a", -1.0),
            (b"b", -0.5),
            (b"c", 0.0),
        ])),
    ))

    # ── Error-path envelopes ─────────────────────────────────────────
    # Truncated below 11-byte minimum (1 type + 10-byte trailer).
    seeds.append(("dump_truncated_below_minimum", b"\x00\x00"))
    seeds.append(("dump_truncated_zero_bytes", b""))

    # Structurally valid envelope but the CRC has a flipped bit.
    bad_crc = bytearray(envelope(RDB_TYPE_STRING, string_body(b"hello")))
    bad_crc[-1] ^= 0xFF
    seeds.append(("dump_corrupted_crc", bytes(bad_crc)))

    # Future RDB version (RDB_DUMP_VERSION + 1) — restore must reject.
    seeds.append((
        "dump_future_version",
        envelope(RDB_TYPE_STRING, string_body(b"hello"), version=RDB_DUMP_VERSION + 1),
    ))

    # Unknown type byte (99 is past every defined type tag).
    seeds.append(("dump_unknown_type_byte", envelope(99, b"")))

    # Type byte = STRING but the length prefix points past the body.
    # Length prefix says 100 bytes follow but only 5 bytes are in the body.
    truncated_body = encode_length(100) + b"short"
    seeds.append((
        "dump_string_length_overruns_body",
        envelope(RDB_TYPE_STRING, truncated_body),
    ))

    # Type byte alone (no body, no length prefix), valid envelope shape.
    seeds.append((
        "dump_type_byte_only_no_body",
        envelope(RDB_TYPE_STRING, b""),
    ))

    # List declares 1000 elements but only 1 follows.
    overshooting_list = encode_length(1000) + encode_rdb_string(b"only")
    seeds.append((
        "dump_list_count_overruns_body",
        envelope(RDB_TYPE_LIST, overshooting_list),
    ))

    for label, payload in seeds:
        path = out_dir / label
        path.write_bytes(payload)
        print(f"wrote {len(payload):4d} bytes to {path.relative_to(repo)}")
    print(f"\ngenerated {len(seeds)} corpus seeds")


if __name__ == "__main__":
    main()
