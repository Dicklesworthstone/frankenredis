//! Instruction-level probe for the stream RESTORE decode path.
//!
//! Stream RESTORE is the worst measured arm against live Redis 7.2.4 — 3.4829x at
//! 400 entries — and 45 pct of fr's cost there is decode+rebuild that redis does
//! not do at all (it stores the macro-node listpacks verbatim). Of that,
//! `decode_raw_values` and `UpstreamStreamSkeleton::flat_entries` are ~40 pct of
//! the whole op between them, at roughly 44 and 41 instructions per listpack
//! element.
//!
//! Driving them through a real DUMP payload here makes that path iterable in
//! SECONDS with a deterministic instrument, instead of minutes through a server
//! under callgrind. Build the payload with the crate's own encoder so the bytes
//! are exactly what a DUMP produces.
//!
//!     stream_restore_hot <entries> <fields> <reps>

use std::hint::black_box;

fn main() {
    let mut args = std::env::args().skip(1);
    let entries: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(400);
    let fields: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(2);
    let reps: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(200);

    // Same shape the harness seeds: explicit ids, `f0/f1` names repeated across
    // every entry (so upstream's SAMEFIELDS flag is set, as on any real stream)
    // and short distinct values.
    let owned: Vec<(u64, u64, Vec<(Vec<u8>, Vec<u8>)>)> = (0..entries)
        .map(|i| {
            let pairs = (0..fields)
                .map(|f| {
                    (
                        format!("f{f}").into_bytes(),
                        format!("v{i:04}{f}").into_bytes(),
                    )
                })
                .collect();
            (i + 1, 1, pairs)
        })
        .collect();

    let payload = fr_persist::encode_stream_listpacks3_blob_borrowed(
        &owned,
        Some((entries, 1)),
        &[],
        Some(entries),
        None,
    )
    .expect("the crate's own encoder must produce a payload for this shape");

    // Prove the payload decodes to what went in before timing anything: a probe
    // that silently decoded fewer entries would measure the wrong amount of work.
    let (count, _, _) = fr_persist::decode_upstream_stream_payload_borrowed(
        fr_persist::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3,
        &payload,
        |out| out.len(),
    )
    .expect("payload must decode");
    assert_eq!(
        count, entries as usize,
        "decoded entry count must match what was encoded"
    );

    let mut acc = 0usize;
    for _ in 0..reps {
        let (n, _, _) = fr_persist::decode_upstream_stream_payload_borrowed(
            fr_persist::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3,
            black_box(&payload),
            |out| out.iter().map(|(_, f)| f.len()).sum::<usize>(),
        )
        .expect("decodes");
        acc += black_box(n);
    }
    println!(
        "entries={entries} fields={fields} payload={} B reps={reps} acc={acc}",
        payload.len()
    );
}
