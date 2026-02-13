# Phase-2C Contract Schema Lock (V1)

`topology_lock_v1.json` is the machine-readable lock for `bd-2wb.3`.

Normative points:
- `schema_version` is fixed at `fr_phase2c_packet_v1`.
- Missing any required packet file or required manifest field is `NOT READY`.
- `parity_report.json.readiness` must reflect gate outcome:
  - complete contract -> `READY_FOR_IMPL`
  - missing mandatory data -> `NOT READY`

Validation entrypoint:

```bash
cargo run -p fr-conformance --bin phase2c_schema_gate -- <packet-dir ...>
# or scan artifacts/phase2c automatically:
cargo run -p fr-conformance --bin phase2c_schema_gate
```
