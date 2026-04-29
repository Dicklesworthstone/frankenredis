#!/usr/bin/env python3
"""Generate structured corpus seeds for fuzz_function_restore.

The fuzz target dispatches the first byte three ways
(`mode_byte % 3`):

    0 → raw_function_source(body, mode_byte)
        body is fed straight into Store::function_load. The
        `mode_byte & 0b1000` bit selects the `replace_existing`
        flag, so seeds use byte 0x00 for replace=false and 0x08 for
        replace=true.

    1 → fuzz_valid_function_library(body)
        body is fed to `arbitrary::Unstructured`. The arbitrary
        format is intentionally not version-stable, so we leave that
        path for libfuzzer's mutator to discover and only seed modes
        0 and 2 here.

    2 → fuzz_raw_function_restore(body)
        body[0] = policy_selector (`% 4`: APPEND/REPLACE/FLUSH/BOGUS)
        body[1..] = serialized FUNCTION DUMP payload, validated by
        Store::function_restore against the upstream envelope:
            [ RDB_OPCODE_FUNCTION2 (0xF5)
              + encode_rdb_string(library_code) ]*
            + version (u16 LE; current = 11)
            + crc64_redis (u64 LE) over preceding bytes

Seeds aim at the **error-precedence boundaries** the recent
br-frankenredis-r85v / r83v / r84v bead chain hardened:

    Mode 0 (function_load):
      - missing #! header (Missing library metadata)
      - empty header line (Missing library metadata)
      - non-LUA engine (Engine 'X' not found)
      - LUA engine, no name= (Library name was not given)
      - LUA + name= but empty name (same)
      - name with hyphen / dot (charset rejection)
      - valid library, single call-form registration
      - valid library, table-form registration with description
      - valid library, mixed call+table forms
      - register_function with empty name (function name charset)
      - register_function with non-[A-Za-z0-9_] (function name charset)
      - 1-char library name (boundary)
      - replace=true on empty store

    Mode 2 (function_restore):
      - empty payload (footer-only) under each policy
      - pre-GA opcode 0xF6 (Pre-GA function format not supported)
      - non-FUNCTION2 opcode (given type is not a function)
      - corrupted CRC (DUMP payload version or checksum are wrong)
      - future RDB version (DUMP payload version or checksum are wrong)
      - truncated below FOOTER_LEN (Invalid dump data)
      - valid single-library payload, REPLACE / FLUSH / BOGUS policy
      - valid two-library payload, APPEND policy

Run:
    python3 fuzz/scripts/gen_function_restore_seeds.py
"""
from __future__ import annotations

import struct
from pathlib import Path

# ── RDB length / string encoding (mirrors fr-store::encode_length) ─────

def encode_length(length: int) -> bytes:
    if length < 64:
        return bytes([length])
    if length < 16384:
        return bytes([0x40 | (length >> 8), length & 0xFF])
    if length <= 0xFFFF_FFFF:
        return bytes([0x80]) + struct.pack(">I", length)
    return bytes([0x81]) + struct.pack(">Q", length)


def encode_rdb_string(data: bytes) -> bytes:
    # The store's encoder also has an integer-form fast path
    # (encode_integer_rdb_string) for short ASCII integers, but a
    # length-prefixed raw-string encoding is always accepted by the
    # decoder, which is what these seeds need.
    return encode_length(len(data)) + data


# ── CRC64 with Redis polynomial (0xAD93D23594C935A9) ───────────────────
# Identical impl to gen_compact_rdb_seeds.crc64_redis. Duplicated
# here so this script stays self-contained.

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


# ── FUNCTION DUMP envelope ─────────────────────────────────────────────

RDB_OPCODE_FUNCTION2 = 0xF5
RDB_OPCODE_FUNCTION_PRE_GA = 0xF6  # = 246, gated by "Pre-GA" rejection
FUNCTION_DUMP_RDB_VERSION = 11


def function_dump_envelope(library_codes: list[bytes], version: int = FUNCTION_DUMP_RDB_VERSION) -> bytes:
    body = bytearray()
    for code in library_codes:
        body.append(RDB_OPCODE_FUNCTION2)
        body.extend(encode_rdb_string(code))
    body.extend(struct.pack("<H", version))
    crc = crc64_redis(bytes(body))
    body.extend(struct.pack("<Q", crc))
    return bytes(body)


# ── Mode-byte builders ─────────────────────────────────────────────────

# mode_byte where (mode_byte % 3) == 0 → raw_function_source.
# `mode_byte & 0b1000` selects the `replace` argument.
SOURCE_NO_REPLACE = 0x00       # 0 % 3 == 0, & 0b1000 == 0
SOURCE_WITH_REPLACE = 0x09     # 9 % 3 == 0, & 0b1000 != 0

# mode_byte where (mode_byte % 3) == 2 → raw_function_restore.
# body[0] = policy_selector (% 4): 0=APPEND, 1=REPLACE, 2=FLUSH, 3=BOGUS.
RESTORE_DISPATCH = 0x02
POLICY_APPEND = 0
POLICY_REPLACE = 1
POLICY_FLUSH = 2
POLICY_BOGUS = 3


def source_seed(body: bytes, replace: bool = False) -> bytes:
    return bytes([SOURCE_WITH_REPLACE if replace else SOURCE_NO_REPLACE]) + body


def restore_seed(payload: bytes, policy: int) -> bytes:
    return bytes([RESTORE_DISPATCH, policy]) + payload


# ── Seed catalogue ─────────────────────────────────────────────────────

def main() -> None:
    repo = Path(__file__).resolve().parent.parent.parent
    out_dir = repo / "fuzz" / "corpus" / "fuzz_function_restore"
    out_dir.mkdir(parents=True, exist_ok=True)

    seeds: list[tuple[str, bytes]] = []

    # ── Mode 0: function_load error-precedence corpus ──────────────────
    seeds.append((
        "source_no_shebang.lua",
        source_seed(b"redis.register_function('a', function() return 1 end)\n"),
    ))
    seeds.append((
        "source_empty.lua",
        source_seed(b""),
    ))
    seeds.append((
        "source_shebang_only.lua",
        source_seed(b"#!\n"),
    ))
    seeds.append((
        "source_unknown_engine.lua",
        source_seed(b"#!python name=foo\nredis.register_function('a', function() return 1 end)\n"),
    ))
    seeds.append((
        "source_lua_no_name_token.lua",
        source_seed(b"#!lua\nredis.register_function('a', function() return 1 end)\n"),
    ))
    seeds.append((
        "source_lua_name_empty.lua",
        source_seed(b"#!lua name=\nredis.register_function('a', function() return 1 end)\n"),
    ))
    seeds.append((
        "source_lib_name_with_hyphen.lua",
        source_seed(b"#!lua name=my-lib\nredis.register_function('a', function() return 1 end)\n"),
    ))
    seeds.append((
        "source_lib_name_with_dot.lua",
        source_seed(b"#!lua name=my.lib\nredis.register_function('a', function() return 1 end)\n"),
    ))
    seeds.append((
        "source_lib_name_one_char.lua",
        source_seed(b"#!lua name=A\nredis.register_function('first', function() return 1 end)\n"),
    ))
    seeds.append((
        "source_call_form_simple.lua",
        source_seed(b"#!lua name=callsimple\nredis.register_function('first', function() return 1 end)\n"),
    ))
    seeds.append((
        "source_table_form_with_description.lua",
        source_seed(
            b"#!lua name=tabledesc\n"
            b"redis.register_function{function_name='first', "
            b"callback=function() return 1 end, "
            b"description='structured seed'}\n"
        ),
    ))
    seeds.append((
        "source_mixed_call_and_table.lua",
        source_seed(
            b"#!lua name=mixed\n"
            b"redis.register_function('alpha', function() return 1 end)\n"
            b"redis.register_function{function_name='beta', callback=function() return 2 end}\n"
        ),
    ))
    seeds.append((
        "source_register_empty_function_name.lua",
        source_seed(b"#!lua name=funcempty\nredis.register_function('', function() return 1 end)\n"),
    ))
    seeds.append((
        "source_register_function_name_with_hyphen.lua",
        source_seed(b"#!lua name=fhyphen\nredis.register_function('a-b', function() return 1 end)\n"),
    ))
    seeds.append((
        "source_replace_into_empty_store.lua",
        source_seed(
            b"#!lua name=replaceempty\nredis.register_function('first', function() return 1 end)\n",
            replace=True,
        ),
    ))

    # ── Mode 2: function_restore envelope corpus ────────────────────────

    # Round-trippable single-library payload.
    single_lib_code = b"#!lua name=restore_one\nredis.register_function('one', function() return 1 end)\n"
    single_lib_envelope = function_dump_envelope([single_lib_code])

    seeds.append((
        "restore_valid_single_lib_replace.dump",
        restore_seed(single_lib_envelope, POLICY_REPLACE),
    ))
    seeds.append((
        "restore_valid_single_lib_flush.dump",
        restore_seed(single_lib_envelope, POLICY_FLUSH),
    ))
    seeds.append((
        "restore_valid_single_lib_bogus_policy.dump",
        restore_seed(single_lib_envelope, POLICY_BOGUS),
    ))

    two_lib_envelope = function_dump_envelope([
        b"#!lua name=restore_a\nredis.register_function('a', function() return 'a' end)\n",
        b"#!lua name=restore_b\nredis.register_function('b', function() return 'b' end)\n",
    ])
    seeds.append((
        "restore_valid_two_libs_append.dump",
        restore_seed(two_lib_envelope, POLICY_APPEND),
    ))

    # Empty libraries marker (just version + CRC) — round-trippable.
    empty_envelope = function_dump_envelope([])
    seeds.append((
        "restore_empty_libraries_marker_append.dump",
        restore_seed(empty_envelope, POLICY_APPEND),
    ))

    # Pre-GA opcode 0xF6 must surface "Pre-GA function format not
    # supported" rather than parsing as a normal record.
    pre_ga_body = bytearray([RDB_OPCODE_FUNCTION_PRE_GA])
    pre_ga_body.extend(encode_rdb_string(single_lib_code))
    pre_ga_body.extend(struct.pack("<H", FUNCTION_DUMP_RDB_VERSION))
    pre_ga_body.extend(struct.pack("<Q", crc64_redis(bytes(pre_ga_body))))
    seeds.append((
        "restore_pre_ga_opcode.dump",
        restore_seed(bytes(pre_ga_body), POLICY_APPEND),
    ))

    # Non-FUNCTION2 opcode (use 0x00) must be rejected with
    # "given type is not a function".
    bogus_op_body = bytearray([0x00])
    bogus_op_body.extend(encode_rdb_string(single_lib_code))
    bogus_op_body.extend(struct.pack("<H", FUNCTION_DUMP_RDB_VERSION))
    bogus_op_body.extend(struct.pack("<Q", crc64_redis(bytes(bogus_op_body))))
    seeds.append((
        "restore_unknown_opcode.dump",
        restore_seed(bytes(bogus_op_body), POLICY_APPEND),
    ))

    # Future RDB version (current+1) must trip the version gate.
    future_envelope = function_dump_envelope([single_lib_code], version=FUNCTION_DUMP_RDB_VERSION + 1)
    seeds.append((
        "restore_future_rdb_version.dump",
        restore_seed(future_envelope, POLICY_APPEND),
    ))

    # Corrupt the CRC by flipping a low-order byte. The CRC check must
    # reject this with "DUMP payload version or checksum are wrong".
    corrupted = bytearray(single_lib_envelope)
    corrupted[-1] ^= 0xFF
    seeds.append((
        "restore_corrupted_crc.dump",
        restore_seed(bytes(corrupted), POLICY_APPEND),
    ))

    # Truncated below the 10-byte footer must surface "Invalid dump
    # data" without panicking.
    seeds.append((
        "restore_truncated_below_footer.dump",
        restore_seed(b"\x00" * 5, POLICY_APPEND),
    ))

    # Body with a valid envelope but the embedded library code itself
    # fails function_load (no `#!` header). The restore must surface
    # that nested error rather than corrupting the existing libraries.
    bad_inner = function_dump_envelope([b"redis.register_function('a', function() return 1 end)\n"])
    seeds.append((
        "restore_inner_load_fails_missing_header.dump",
        restore_seed(bad_inner, POLICY_APPEND),
    ))

    for label, payload in seeds:
        path = out_dir / label
        path.write_bytes(payload)
        print(f"wrote {len(payload):4d} bytes to {path.relative_to(repo)}")
    print(f"\ngenerated {len(seeds)} corpus seeds")


if __name__ == "__main__":
    main()
