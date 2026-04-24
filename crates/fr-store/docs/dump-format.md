# Upstream Redis DUMP Payload Format

This note documents the upstream Redis `DUMP` / `RESTORE` serialized-value
format that `fr-store` must target for `DUMP`, `RESTORE`, `MIGRATE`, and
cross-server fixture interchange.

Local source references:

- `legacy_redis_code/redis/src/cluster.c`
- `legacy_redis_code/redis/src/rdb.c`
- `legacy_redis_code/redis/src/rdb.h`
- `crates/fr-store/src/lib.rs`
- `crates/fr-persist/src/lib.rs`

Redis 7.4 source references for hash field expiration metadata:

- https://raw.githubusercontent.com/redis/redis/7.4.0/src/rdb.h
- https://raw.githubusercontent.com/redis/redis/7.4.0/src/rdb.c

## Envelope

`DUMP key` returns a bulk string containing one RDB object, not a whole RDB file.
It has no `REDIS000x` file header, no SELECTDB opcode, no key name, and no key
expiry opcode. Whole-key TTL is supplied by `RESTORE key ttl serialized-value`.

The payload is:

| Order | Field | Width | Endianness | Source |
| --- | --- | --- | --- | --- |
| 1 | `type_byte` | 1 byte | n/a | `rdbSaveObjectType` |
| 2 | object body | variable | type-specific | `rdbSaveObject` |
| 3 | RDB version | 2 bytes | little endian | `RDB_VERSION` |
| 4 | CRC64 | 8 bytes | little endian | `crc64(0, payload_without_crc)` |

`RESTORE` verifies the footer first. It rejects payloads shorter than 10 bytes,
rejects payload versions newer than the receiving server's `RDB_VERSION`, and
rejects a CRC mismatch unless checksum validation is disabled. After the footer
check, it reads `type_byte` with `rdbLoadObjectType` and the body with
`rdbLoadObject`.

## Version Field

The version footer is the RDB file-format version, not a DUMP-specific version.
This repository's Redis 7.2 legacy checkout defines `RDB_VERSION` as `11`.
Redis 7.4.0 defines `RDB_VERSION` as `12` and adds hash field expiration RDB
types. Redis accepts old payloads with `version <= RDB_VERSION` and rejects
newer payloads.

## CRC64

Redis stores the CRC64 in little-endian order after computing it over every byte
before the CRC field, including the 2-byte version footer. FrankenRedis already
implements the Redis CRC64 in `fr_persist::crc64_redis` using polynomial
`0xAD93_D235_94C9_35A9`; its reference vector for `123456789` is
`0xe9c6_d914_c4b8_d9ca`.

## Shared Encodings

RDB lengths use Redis variable-length encoding:

| Prefix bits / byte | Meaning | Payload |
| --- | --- | --- |
| `00xxxxxx` | 6-bit length | Low 6 bits are the length |
| `01xxxxxx` | 14-bit length | Low 6 bits plus next byte |
| `0x80` | 32-bit length | Next 4 bytes, big endian |
| `0x81` | 64-bit length | Next 8 bytes, big endian |
| `11xxxxxx` | encoded string | Low 6 bits select the string encoding |

RDB strings are usually `length + bytes`, but `rdbSaveRawString` may write
integer encodings (`INT8`, `INT16`, `INT32`) or LZF-compressed strings. A strict
upstream-compatible decoder must handle both raw and encoded strings anywhere
Redis calls `rdbSaveStringObject` or `rdbSaveRawString`.

## Object Type Tags

The local Redis 7.2 checkout defines the relevant object tags as:

| Tag | Name | Body summary |
| --- | --- | --- |
| `0` | `RDB_TYPE_STRING` | RDB string object |
| `2` | `RDB_TYPE_SET` | Length, then one RDB string per member |
| `4` | `RDB_TYPE_HASH` | Length, then field/value RDB string pairs |
| `5` | `RDB_TYPE_ZSET_2` | Length, then member RDB string plus binary `f64` score |
| `11` | `RDB_TYPE_SET_INTSET` | Raw intset blob as an RDB string |
| `16` | `RDB_TYPE_HASH_LISTPACK` | Raw listpack blob as an RDB string |
| `17` | `RDB_TYPE_ZSET_LISTPACK` | Raw listpack blob as an RDB string |
| `18` | `RDB_TYPE_LIST_QUICKLIST_2` | Quicklist node count, then node container plus blob |
| `20` | `RDB_TYPE_SET_LISTPACK` | Raw listpack blob as an RDB string |
| `21` | `RDB_TYPE_STREAM_LISTPACKS_3` | Stream radix/listpack payload |

Redis 7.4 adds hash field expiration tags:

| Tag | Name | Body summary |
| --- | --- | --- |
| `24` | `RDB_TYPE_HASH_METADATA` | Min field expiry, length, TTL/field/value tuples |
| `25` | `RDB_TYPE_HASH_LISTPACK_EX` | Min field expiry, then raw listpack-ex blob |

Tags `22` and `23` were Redis 7.4 pre-GA variants without the leading
`minExpire` field. They should be recognized only if compatibility with 7.4
release candidates is required.

## Body Layouts

### STRING (`0`)

Body is one RDB string object:

| Order | Field | Encoding |
| --- | --- | --- |
| 1 | value | RDB string, including possible integer or LZF encoding |

### LIST_QUICKLIST_2 (`18`)

Body is a quicklist:

| Order | Field | Encoding | Notes |
| --- | --- | --- | --- |
| 1 | `node_count` | RDB length | Number of quicklist nodes |
| 2 | `container` | RDB length | Repeated per node; `1` = plain, `2` = packed |
| 3 | `node_blob` | RDB string | Plain bytes or listpack bytes, repeated per node |

Redis saves listpack-encoded lists as a fake quicklist with one packed node.
For normal quicklists it may emit multiple packed nodes and may LZF-compress a
node blob through the RDB string encoding.

### SET (`2`)

Body is a hash-table set:

| Order | Field | Encoding |
| --- | --- | --- |
| 1 | `member_count` | RDB length |
| 2 | `member[i]` | RDB string, repeated `member_count` times |

### SET_INTSET (`11`)

Body is a raw intset blob wrapped as an RDB string:

| Order | Field | Encoding | Notes |
| --- | --- | --- | --- |
| 1 | `intset_blob` | RDB string | Intset header and integer storage are little endian |

### SET_LISTPACK (`20`)

Body is one raw listpack blob wrapped as an RDB string. Each listpack element is
a member. The loader validates duplicate members during deep integrity checks.

### HASH (`4`)

Body is a hash-table hash:

| Order | Field | Encoding |
| --- | --- | --- |
| 1 | `field_count` | RDB length |
| 2 | `field[i]` | RDB string |
| 3 | `value[i]` | RDB string |

Fields and values repeat `field_count` times.

### HASH_LISTPACK (`16`)

Body is one raw listpack blob wrapped as an RDB string. The listpack alternates
field, value, field, value.

### HASH_METADATA (`24`, Redis 7.4+)

Body is a hash-table hash with hash field expiration metadata:

| Order | Field | Encoding | Notes |
| --- | --- | --- | --- |
| 1 | `min_expire_ms` | RDB millisecond time | Earliest field expiry, used as the delta base |
| 2 | `field_count` | RDB length | Number of field/value pairs |
| 3 | `ttl_delta[i]` | RDB length | `0` = no field TTL, otherwise `expire_at_ms - min_expire_ms + 1` |
| 4 | `field[i]` | RDB string | Hash field |
| 5 | `value[i]` | RDB string | Hash value |

`ttl_delta`, `field`, and `value` repeat `field_count` times.

### HASH_LISTPACK_EX (`25`, Redis 7.4+)

Body is a listpack-ex hash with hash field expiration metadata:

| Order | Field | Encoding | Notes |
| --- | --- | --- | --- |
| 1 | `min_expire_ms` | RDB millisecond time | `0` if no valid minimum is attached |
| 2 | `listpack_ex_blob` | RDB string | Raw listpack whose tuple length is 3 |

The listpack-ex tuple shape is `field`, `value`, `expire_at_ms`. An expiry of
`0` means the field has no TTL.

### ZSET_2 (`5`)

Body is a skiplist-backed sorted set:

| Order | Field | Encoding |
| --- | --- | --- |
| 1 | `member_count` | RDB length |
| 2 | `member[i]` | RDB string |
| 3 | `score[i]` | Binary `f64`, little endian |

Redis saves skiplist elements from greatest score to smallest score. The loader
does not depend on that order for correctness, but byte-for-byte DUMP parity
does.

### ZSET_LISTPACK (`17`)

Body is one raw listpack blob wrapped as an RDB string. The listpack alternates
member, score, member, score. Scores are string/listpack elements, not binary
`f64` fields.

### STREAM_LISTPACKS_3 (`21`)

Body is the upstream stream radix/listpack format documented in
`crates/fr-persist/docs/rdb-stream-format.md`. At a high level it contains:

| Order | Field | Encoding |
| --- | --- | --- |
| 1 | `listpack_count` | RDB length |
| 2 | `node_key` / `node_listpack` pairs | RDB strings |
| 3 | `length` | RDB length |
| 4 | `last_id`, `first_id`, `max_deleted_entry_id` | RDB length pairs |
| 5 | `entries_added` | RDB length |
| 6 | consumer groups | RDB stream group payload |

## Current FrankenRedis Delta

`Store::dump_key` already writes the Redis footer shape and CRC64, but several
object bodies are still narrower than upstream Redis:

| Area | Upstream expectation | Current `fr-store` behavior |
| --- | --- | --- |
| Envelope | `type_byte + rdbSaveObject + version + crc64` | Matches envelope and footer for supported bodies |
| Version | 7.2 uses `11`, 7.4 uses `12`; reject newer versions | Uses `RDB_DUMP_VERSION = 11` and rejects newer |
| CRC64 | Redis CRC64 over all bytes before CRC | Matches via `fr_persist::crc64_redis` |
| RDB strings | Raw, integer-encoded, or LZF-encoded strings | Writes only raw `length + bytes`; restore does not decode encoded-string prefixes |
| LIST | Multiple quicklist nodes; packed or plain containers | Writes one packed listpack node; restore rejects plain containers |
| SET | `SET`, `SET_INTSET`, or `SET_LISTPACK` | Writes these tags, but listpack element encodings may not be byte-identical to Redis |
| HASH | `HASH`, `HASH_LISTPACK`, or Redis 7.4 metadata variants | Writes `HASH` or `HASH_LISTPACK`; does not include per-field TTL metadata |
| ZSET | `ZSET_2` with descending skiplist order or `ZSET_LISTPACK` | Writes both tags, but `ZSET_2` iteration is ascending |
| STREAM | Current upstream tag is `STREAM_LISTPACKS_3` with radix/listpack body | Writes tag `15` with a private flat little-endian entry layout |
| RESTORE parse boundary | RDB object parser consumes the body before the footer | Checks that its custom cursor exactly reaches `payload.len() - 10` |

For `MIGRATE` interoperability, the blocking deltas are the object body formats,
not the footer. A Redis server receiving a FrankenRedis payload with a private
stream body or missing encoded-string support will fail `RESTORE` even though
the version and CRC footer are shaped correctly.

## Decoder / Encoder Checklist

- Treat DUMP as a single-object RDB payload with a 10-byte footer.
- Compute CRC64 over `type_byte + body + version`, excluding only the CRC field.
- Keep version comparisons as `payload_version <= local_RDB_VERSION`.
- Use full RDB string decoding for all raw strings, including integer and LZF
  encodings.
- Preserve upstream type tag selection before byte-for-byte parity work.
- Encode Redis 7.4 hash field TTLs with `HASH_METADATA` or `HASH_LISTPACK_EX`
  when any field has a TTL.
- Encode streams as `STREAM_LISTPACKS_3`, not the private flat type-15 layout.
- Keep `RESTORE` atomic: validate footer and body fully before replacing an
  existing key.
