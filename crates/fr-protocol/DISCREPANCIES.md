# fr-protocol Known Conformance Divergences

Scope: RESP2 + RESP3 wire protocol, as produced / consumed by
`fr-protocol/src/lib.rs`. Every entry below is an intentional deviation
from upstream Redis reply semantics, pinned by a test gate so regressions
stay visible.

Format mirrors the `/testing-conformance-harnesses` skill:

- **DISC-NNN** sequential id.
- Resolution: ACCEPTED | INVESTIGATING | WILL-FIX.
- Tests affected: XFAIL markers (not SKIP).
- Review date: the last time the entry was confirmed to still apply.

## DISC-001: Default `parse_frame` rejects inbound RESP3 prefixes unless opted in

- **Reference:** upstream Redis 7.2+ supports RESP3 via `HELLO 3`;
  the parser accepts map (`%`), set (`~`), null (`_`), boolean (`#`),
  double (`,`), big number (`(`), push (`>`), verbatim-string (`=`),
  attribute (`|`), and bulk-error (`!`) prefixes.
- **Our impl:** Outbound RESP3 reply encoding (`Map`, `Set`, `Null`,
  `Boolean`, `Double`, `Push`, `Attribute`, `VerbatimString`) is
  implemented and used by `fr-runtime` / `fr-server` once a client
  negotiates `HELLO 3`. Inbound RESP3 frame parsing is also implemented
  in `parse_frame_with_config` when `ParserConfig::allow_resp3` is true
  (downconverting frames for command dispatch), while the default
  `parse_frame` (`allow_resp3 = false`) still rejects inbound RESP3
  prefixes with `RespParseError::UnsupportedResp3Type(b)`.
- **Impact:** Standard Redis clients negotiating `HELLO 3` receive true
  RESP3 replies and work end-to-end. Direct callers of default
  `parse_frame` must opt in via `ParserConfig::allow_resp3` to parse
  inbound RESP3 prefixes.
- **Resolution:** ACCEPTED (default fail-closed `parse_frame` preserved
  for strict-mode / protocol-negative fixtures; full RESP3 reply
  negotiation active via `HELLO 3`).
- **Tests affected:** `golden_unsupported_resp3_map_prefix`,
  `..._set_prefix`, `..._null_prefix`, `..._boolean_prefix`,
  `..._double_prefix`, `..._big_number_prefix`, `..._push_prefix`.
- **Review date:** 2026-11-25.

## DISC-002: `parse_frame` rejects integer forms with a leading `+`

- **Reference:** upstream `redis-server` accepts `:+5\r\n` as the
  integer 5 (its `string2ll` implementation treats leading `+` as
  optional).
- **Our impl:** `parse_i64_strict` rejects any non-`-` sign byte,
  returning `InvalidInteger` for `:+5\r\n`.
- **Impact:** Observably different for a hand-crafted client that
  emits `:+5`. No real Redis client library does this.
- **Resolution:** ACCEPTED. Stricter canonicalization protects the
  round-trip property; upstream also never produces this form on the
  wire, so our rejection is observable only against adversarial input.
- **Tests affected:** `golden_integer_with_leading_plus_rejected`.
- **Review date:** 2026-04-23.

## DISC-003: `MAX_LINE_LENGTH = 64 KiB` hard cap

- **Reference:** upstream Redis uses a 64 KiB limit on inline-command
  line length; for RESP frames (length-prefixed) the limit is the
  per-reply `proto-max-bulk-len` (512 MiB default).
- **Our impl:** 64 KiB applies to any RESP line that feeds
  `read_line` — i.e., all simple-string, error, integer, bulk-length,
  and multibulk-length lines. The bulk-body itself is governed by
  `ParserConfig::max_bulk_len`.
- **Impact:** Nil. No upstream reply line exceeds 64 KiB in practice.
- **Resolution:** ACCEPTED. Matches the most defensive reading of the
  upstream source.
- **Tests affected:** none directly; see `COVERAGE.md` gap entry.
- **Review date:** 2026-04-23.

---

Last updated: 2026-04-23 (br-frankenredis-0zyf).
