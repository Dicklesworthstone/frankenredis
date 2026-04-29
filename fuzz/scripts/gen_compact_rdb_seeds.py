#!/usr/bin/env python3
"""Generate RDB corpus seeds for the compact-encoding type tags
(11/16/17/18/20) that `fr_persist::decode_rdb` started accepting in
br-frankenredis-aqgx (commit ec6a274).

The fuzz_rdb_decoder corpus previously had 13 seeds covering the canonical
type tags (0/1/2/4/5/15/19/21) but ZERO coverage of the compact tags. That
made libfuzzer slow to discover the new code paths even though the decoder
machinery for those tags has shipped — the corpus was the bottleneck.

This script writes a fresh batch of well-formed and adversarial seeds to
fuzz/corpus/fuzz_rdb_decoder/. It is idempotent and safe to re-run after
each round of decoder changes. The seeds are kept small (≤256 bytes each)
to fit libfuzzer's preference for short inputs that mutate quickly.

Run:
    python3 fuzz/scripts/gen_compact_rdb_seeds.py

Verification (run from repo root):
    cargo test -p fr-persist compact_corpus_seeds_decode_or_reject_cleanly
"""

from __future__ import annotations

import struct
import zlib
from pathlib import Path


# ── RDB primitives (mirrors fr_persist::rdb_encode_*) ──────────────────

def encode_length(length: int) -> bytes:
    """Variable-length RDB length encoding (mirrors rdb_encode_length)."""
    if length < 64:
        return bytes([length])
    if length < 16384:
        return bytes([0x40 | (length >> 8), length & 0xFF])
    if length <= 0xFFFF_FFFF:
        return bytes([0x80]) + struct.pack(">I", length)
    return bytes([0x81]) + struct.pack(">Q", length)


def encode_string(data: bytes) -> bytes:
    return encode_length(len(data)) + data


def append_select_db(buf: bytearray, db: int, n_keys: int = 1, n_expires: int = 0) -> None:
    buf.append(0xFE)  # SELECTDB
    buf.extend(encode_length(db))
    buf.append(0xFB)  # RESIZEDB
    buf.extend(encode_length(n_keys))
    buf.extend(encode_length(n_expires))


def finalize(buf: bytearray) -> bytes:
    """Append EOF + CRC64-Redis trailer (matching upstream)."""
    buf.append(0xFF)  # EOF
    crc = crc64_redis(bytes(buf))
    buf.extend(struct.pack("<Q", crc))
    return bytes(buf)


# ── CRC64 with Redis polynomial (0xAD93D23594C935A9) ──────────────────

_POLY = 0xAD93_D235_94C9_35A9


def _reflect64(value: int) -> int:
    """Bit-reflect a 64-bit value (mirrors fr_persist::crc64_redis::reflect).
    Upstream `_crc64` returns `crc_reflect(crc, 64)` as the final step;
    omitting this was the bug that made the original Python port emit
    seeds with bogus trailers that decode_rdb rejected with InvalidFrame.
    """
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


# ── Listpack encoder (mirrors fr-store::encode_listpack_strings) ──────

def lp_entry(value: bytes) -> bytes:
    """Encode a single listpack entry as a 6-bit-tag literal string.

    Only supports values < 64 bytes — sufficient for fuzz corpus seeds.
    """
    if len(value) >= 64:
        raise ValueError("seed listpack entries must be <64 bytes")
    body = bytearray()
    body.append(0x80 | len(value))  # 6-bit tag
    body.extend(value)
    # Backlen is the length of `body` as a backwards-decoded varint.
    data_len = len(body)
    if data_len <= 127:
        body.append(data_len)
    else:
        body.append((data_len >> 7) & 0x7F)
        body.append(((data_len & 0x7F) | 0x80))
    return bytes(body)


def listpack(entries: list[bytes]) -> bytes:
    body = b"".join(lp_entry(e) for e in entries)
    total_bytes = 6 + len(body) + 1
    out = bytearray()
    out.extend(struct.pack("<I", total_bytes))  # total bytes (LE u32)
    out.extend(struct.pack("<H", min(len(entries), 0xFFFF)))  # entry count
    out.extend(body)
    out.append(0xFF)  # listpack EOF
    return bytes(out)


# ── Intset encoder (mirrors fr-persist::decode_intset_members shape) ─

def intset(values: list[int]) -> bytes:
    """Build an upstream-compatible intset binary.

    Picks the narrowest encoding that fits all values, sorts them
    ascending (upstream emits intsets sorted), and emits 8-byte header
    plus elements.
    """
    if not values:
        encoding = 2
    elif all(-32768 <= v <= 32767 for v in values):
        encoding = 2
    elif all(-2_147_483_648 <= v <= 2_147_483_647 for v in values):
        encoding = 4
    else:
        encoding = 8

    sorted_values = sorted(values)
    out = bytearray()
    out.extend(struct.pack("<I", encoding))
    out.extend(struct.pack("<I", len(sorted_values)))
    width_struct = {2: "<h", 4: "<i", 8: "<q"}[encoding]
    for v in sorted_values:
        out.extend(struct.pack(width_struct, v))
    return bytes(out)


# ── RDB header ────────────────────────────────────────────────────────

def rdb_header() -> bytearray:
    return bytearray(b"REDIS0011")


# ── Compact-type seed builders ────────────────────────────────────────

def build_intset_seed(values: list[int], key: bytes = b"si") -> bytes:
    """RDB_TYPE_SET_INTSET (11) — string-wrapped intset blob."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(11)  # RDB_TYPE_SET_INTSET
    buf.extend(encode_string(key))
    buf.extend(encode_string(intset(values)))
    return finalize(buf)


def build_set_listpack_seed(members: list[bytes], key: bytes = b"slp") -> bytes:
    """RDB_TYPE_SET_LISTPACK (20) — string-wrapped listpack of members."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(20)  # RDB_TYPE_SET_LISTPACK
    buf.extend(encode_string(key))
    buf.extend(encode_string(listpack(members)))
    return finalize(buf)


def build_hash_listpack_seed(pairs: list[tuple[bytes, bytes]], key: bytes = b"hlp") -> bytes:
    """RDB_TYPE_HASH_LISTPACK (16) — listpack of (field, value) pairs."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(16)
    buf.extend(encode_string(key))
    flat: list[bytes] = []
    for field, value in pairs:
        flat.append(field)
        flat.append(value)
    buf.extend(encode_string(listpack(flat)))
    return finalize(buf)


def build_zset_listpack_seed(pairs: list[tuple[bytes, bytes]], key: bytes = b"zlp") -> bytes:
    """RDB_TYPE_ZSET_LISTPACK (17) — listpack of (member, score-as-string)."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(17)
    buf.extend(encode_string(key))
    flat: list[bytes] = []
    for member, score_str in pairs:
        flat.append(member)
        flat.append(score_str)
    buf.extend(encode_string(listpack(flat)))
    return finalize(buf)


def build_quicklist2_packed_seed(items: list[bytes], key: bytes = b"lq") -> bytes:
    """RDB_TYPE_LIST_QUICKLIST_2 (18) with one PACKED node containing a
    listpack of all items."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(18)
    buf.extend(encode_string(key))
    buf.extend(encode_length(1))  # node count
    buf.extend(encode_length(2))  # container = PACKED
    buf.extend(encode_string(listpack(items)))
    return finalize(buf)


def build_quicklist2_plain_seed(item: bytes, key: bytes = b"lq_plain") -> bytes:
    """RDB_TYPE_LIST_QUICKLIST_2 (18) with one PLAIN node carrying the
    element bytes raw (used when an element exceeds list-max-listpack-size)."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(18)
    buf.extend(encode_string(key))
    buf.extend(encode_length(1))  # node count
    buf.extend(encode_length(1))  # container = PLAIN
    buf.extend(encode_string(item))
    return finalize(buf)


def build_quicklist2_mixed_seed(packed_items: list[bytes], plain_item: bytes, key: bytes = b"lq_mix") -> bytes:
    """Two-node quicklist: one PACKED listpack node followed by one PLAIN node."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(18)
    buf.extend(encode_string(key))
    buf.extend(encode_length(2))  # node count
    buf.extend(encode_length(2))  # node 1: PACKED
    buf.extend(encode_string(listpack(packed_items)))
    buf.extend(encode_length(1))  # node 2: PLAIN
    buf.extend(encode_string(plain_item))
    return finalize(buf)


# ── Adversarial seed builders (must be rejected without panic) ────────

def build_adversarial_truncated_intset() -> bytes:
    """Header claims width=2 / len=10 but only one element follows."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(11)
    buf.extend(encode_string(b"si_short"))
    blob = bytearray()
    blob.extend(struct.pack("<I", 2))   # width=2
    blob.extend(struct.pack("<I", 10))  # claimed length=10
    blob.extend(struct.pack("<h", 7))   # only 1 actual element
    buf.extend(encode_string(bytes(blob)))
    return finalize(buf)


def build_adversarial_intset_invalid_encoding() -> bytes:
    """Encoding byte = 3 (must be 2/4/8)."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(11)
    buf.extend(encode_string(b"si_bad_enc"))
    blob = bytearray()
    blob.extend(struct.pack("<I", 3))   # invalid: must be 2/4/8
    blob.extend(struct.pack("<I", 0))
    buf.extend(encode_string(bytes(blob)))
    return finalize(buf)


def build_adversarial_hash_listpack_odd_count() -> bytes:
    """Hash listpack with an odd number of entries — must trip the
    pair-mismatch rejection in decode_rdb."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(16)
    buf.extend(encode_string(b"hlp_odd"))
    buf.extend(encode_string(listpack([b"f1", b"v1", b"orphan"])))
    return finalize(buf)


def build_adversarial_zset_listpack_non_numeric_score() -> bytes:
    """Zset listpack with a non-numeric score — must trip the f64-parse
    rejection in decode_rdb."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(17)
    buf.extend(encode_string(b"zlp_nan"))
    buf.extend(encode_string(listpack([b"member", b"not_a_number"])))
    return finalize(buf)


def build_adversarial_quicklist2_unknown_container() -> bytes:
    """Quicklist node with container=99 (only 1 PLAIN and 2 PACKED valid)."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(18)
    buf.extend(encode_string(b"lq_bad"))
    buf.extend(encode_length(1))
    buf.extend(encode_length(99))  # not 1 or 2
    buf.extend(encode_string(listpack([b"x"])))
    return finalize(buf)


def build_adversarial_listpack_truncated() -> bytes:
    """Listpack header claims 100 bytes but body is short — exercises
    the listpack decoder's bounds checks."""
    buf = rdb_header()
    append_select_db(buf, 0)
    buf.append(20)
    buf.extend(encode_string(b"slp_short"))
    fake_lp = bytearray()
    fake_lp.extend(struct.pack("<I", 100))  # claim 100 bytes
    fake_lp.extend(struct.pack("<H", 5))    # claim 5 entries
    fake_lp.extend(b"\x80hi\x03")           # 4 bytes body
    fake_lp.append(0xFF)                    # EOF
    buf.extend(encode_string(bytes(fake_lp)))
    return finalize(buf)


# ── Driver ────────────────────────────────────────────────────────────

def main() -> None:
    repo = Path(__file__).resolve().parent.parent.parent
    out_dir = repo / "fuzz" / "corpus" / "fuzz_rdb_decoder"
    out_dir.mkdir(parents=True, exist_ok=True)

    seeds: list[tuple[str, bytes]] = [
        # Well-formed compact-encoding seeds
        ("compact_intset_small_16bit", build_intset_seed([1, 2, 3, 5, 7])),
        ("compact_intset_32bit", build_intset_seed([-100_000, 0, 100_000])),
        ("compact_intset_64bit", build_intset_seed([-(1 << 40), 0, (1 << 40)])),
        ("compact_set_listpack", build_set_listpack_seed([b"alpha", b"beta", b"gamma"])),
        ("compact_set_listpack_single", build_set_listpack_seed([b"only"])),
        (
            "compact_hash_listpack",
            build_hash_listpack_seed([(b"f1", b"v1"), (b"f2", b"v2"), (b"f3", b"v3")]),
        ),
        (
            "compact_zset_listpack",
            build_zset_listpack_seed(
                [(b"a", b"1"), (b"b", b"2.5"), (b"c", b"7.25"), (b"d", b"-3.14159")]
            ),
        ),
        (
            "compact_quicklist2_packed",
            build_quicklist2_packed_seed([b"a", b"b", b"c", b"d", b"e"]),
        ),
        ("compact_quicklist2_plain", build_quicklist2_plain_seed(b"single_plain_element_43_bytes")),
        (
            "compact_quicklist2_mixed",
            build_quicklist2_mixed_seed([b"x", b"y"], b"z_plain_node_payload"),
        ),
        # Adversarial / malformed seeds (must be rejected, not panic)
        ("compact_intset_truncated", build_adversarial_truncated_intset()),
        ("compact_intset_invalid_encoding", build_adversarial_intset_invalid_encoding()),
        ("compact_hash_listpack_odd", build_adversarial_hash_listpack_odd_count()),
        (
            "compact_zset_listpack_non_numeric",
            build_adversarial_zset_listpack_non_numeric_score(),
        ),
        (
            "compact_quicklist2_unknown_container",
            build_adversarial_quicklist2_unknown_container(),
        ),
        ("compact_listpack_truncated", build_adversarial_listpack_truncated()),
    ]

    for name, payload in seeds:
        path = out_dir / name
        path.write_bytes(payload)
        print(f"wrote {len(payload):4d} bytes to {path.relative_to(repo)}")
    print(f"\ngenerated {len(seeds)} corpus seeds")


if __name__ == "__main__":
    main()
