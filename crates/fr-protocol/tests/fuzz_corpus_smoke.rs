//! Replay every file in `fuzz/corpus/fuzz_resp_parser/` through
//! `parse_frame` and `parse_frame_with_config` to lock in the
//! invariant that none of the seeded inputs panics. Whenever the
//! corpus expands (e.g. RESP3 dialect seeds added in
//! frankenredis-* tickets), this test exercises the new shapes
//! immediately under regular `cargo test` — without needing a 60s
//! cargo-fuzz run.
//!
//! Mirrors the `parse_frame_never_panics` proptest in fr-protocol's
//! inline tests but seeds from the same corpus the libfuzzer harness
//! consumes, so any handcrafted sample stays deterministically
//! covered between fuzzer runs.

use fr_protocol::{ParserConfig, parse_frame, parse_frame_with_config};
use std::fs;
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("../../fuzz/corpus/fuzz_resp_parser")
}

#[test]
fn fuzz_resp_parser_corpus_never_panics() {
    let dir = corpus_dir();
    assert!(
        dir.is_dir(),
        "corpus dir missing: {} — did the workspace move?",
        dir.display()
    );

    let restrictive = ParserConfig {
        max_bulk_len: 512,
        max_array_len: 16,
        max_recursion_depth: 4,
        allow_resp3: false,
    };
    let permissive = ParserConfig {
        max_bulk_len: 64 * 1024 * 1024,
        max_array_len: 1_048_576,
        max_recursion_depth: 64,
        allow_resp3: true,
    };

    let mut count = 0_usize;
    for entry in fs::read_dir(&dir).expect("read corpus dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).unwrap_or_else(|err| {
            panic!("failed to read {}: {err}", path.display());
        });

        // Each call wraps a panic boundary internally via Result —
        // we just need to exercise that Err/Incomplete/Ok all return
        // cleanly without unwinding. Discarding results is the point.
        let _ = parse_frame(&bytes);
        let _ = parse_frame_with_config(&bytes, &restrictive);
        let _ = parse_frame_with_config(&bytes, &permissive);

        count += 1;
    }

    // We seeded ≥ 13 corpus files initially plus the RESP3 dialect
    // additions; bail loudly if the corpus is silently emptied.
    assert!(
        count >= 13,
        "fuzz_resp_parser corpus shrank to {count} files — regressed seed coverage?"
    );
}
