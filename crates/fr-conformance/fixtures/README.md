# Conformance Fixtures

This folder stores normalized oracle-vs-target fixtures for fr-conformance.

- core_strings.json: first deterministic compatibility suite for `PING`, `SET`, `GET`, `DEL`, `INCR`, `EXPIRE`, and `PTTL`.
- core_errors.json: Redis-style error normalization suite for unknown command, arity, syntax, integer parse, and overflow paths.
- protocol_negative.json: malformed RESP corpus for parser fail-closed behavior and protocol error string contract.
- persist_replay.json: replay-oriented fixtures that execute AOF-shaped records and assert post-replay key state.
- smoke_case.json: legacy bootstrap fixture retained for backwards compatibility.
