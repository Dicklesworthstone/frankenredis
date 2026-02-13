# Evidence Ledger Contract (Round 1)

Implemented surface: `fr-runtime::EvidenceLedger` and `fr-runtime::EvidenceEvent`.

Minimum event schema:
- `ts_ms`: deterministic timestamp for test replayability.
- `subsystem`: protocol/router/compatibility gate.
- `action`: stable action code.
- `reason`: human-readable explanation.
- `confidence`: optional bounded confidence score.

Round 1 events implemented:
- protocol parse failure
- command-frame rejection
- compatibility gate fail-closed events

Planned expansion:
- include loss-matrix IDs, posterior terms, and Bayes factor snippets for adaptive decisions.
