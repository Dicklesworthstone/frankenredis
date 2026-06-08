# frankenredis-6tsou.1 runtime key-demand lever

Status: rejected, production hunk removed.

## Profile target

Fresh GETSET hit profile for this child kept the command metadata lane visible:
`Runtime::execute_frame_internal`, `process_buffered_frames`,
`parse_command_args_borrowed_into`, `command_table_index`,
`fr_command::command_key_indexes`, `classify_command`,
`acl_command_selectors_for_argv`, and `canonical_command_fullname`.

Lever attempted: defer `command_keys()` extraction in the successful
non-transactional runtime path until one of the consumers is active:
blocked-client ready keys, client tracking invalidations, keyspace
notifications, or read tracking.

## Behavior proof

Raw RESP golden comparator:

- baseline sha256: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`
- candidate sha256: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`
- exact transcript equality: true

Focused runtime tests passed under `rch`:

- `cargo test -p fr-runtime client_tracking -- --nocapture`: 9 passed
- `cargo test -p fr-runtime keyspace -- --nocapture`: 5 passed across unit/integration filters
- `cargo test -p fr-runtime write_records_ready_key_only_when_client_is_blocked -- --nocapture`: 1 passed

Isomorphism notes: the lever did not alter command ordering, command reply
encoding, floating-point behavior, RNG behavior, AOF/replication argv capture,
ACL checks, or commandstats naming. The only intended state change was avoiding
key extraction when no runtime metadata consumer existed.

## Benchmarks

Clean baseline checkout: `ee8096263`

GETSET hit P16/300k paired hyperfine:

- baseline: `2.159s +/- 0.015`
- candidate: `2.159s +/- 0.020`
- result: neutral within noise

SET P16/300k paired hyperfine:

- baseline: `2.127s +/- 0.023`
- candidate: `2.168s +/- 0.026`
- result: baseline `1.02x +/- 0.02` faster

Score: `0.0 = Impact 0 x Confidence 2 / Effort 1`. Failed Score >= 2.0.

## Route

Do not retry this micro-family. The next child should replace the separated
parse/classify/key-index/name/ACL metadata checks with a parser-emitted command
metadata packet: borrowed command token, command table index, canonical name,
arity class, key layout descriptor, command flags, and ACL selector seed computed
once, then threaded through runtime/fr-command consumers. Target ratio: at least
`1.05x` on GETSET/SET P16/300k and a measurable drop in metadata rows on the next
profile.
