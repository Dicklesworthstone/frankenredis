//! Replay every file in fr-persist's fuzz corpora through their
//! respective parsers and lock in the invariant that none of the
//! seeded inputs panics. Mirrors the fr-protocol fuzz_corpus_smoke
//! test added in cwx6 — exercises the same handcrafted samples that
//! libfuzzer consumes, so corner-case coverage stays deterministic
//! between cargo-fuzz runs.
//!
//! (frankenredis-ohm5 + frankenredis-fek0y)

use fr_persist::{decode_aof_stream, parse_aof_manifest};
use std::fs;
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("../../fuzz/corpus/fuzz_aof_decoder")
}

fn manifest_corpus_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("../../fuzz/corpus/fuzz_aof_manifest_parser")
}

#[test]
fn fuzz_aof_decoder_corpus_never_panics() {
    let dir = corpus_dir();
    assert!(
        dir.is_dir(),
        "corpus dir missing: {} — did the workspace move?",
        dir.display()
    );

    let mut count = 0_usize;
    for entry in fs::read_dir(&dir).expect("read corpus dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));

        // Decoder must surface every malformed input as Err, never
        // unwind. We don't care whether it returns Ok or Err — only
        // that it returns at all.
        let _ = decode_aof_stream(&bytes);

        count += 1;
    }

    // 14 original happy-path seeds + 8 hostile-input seeds added in
    // ohm5; bail loudly if the corpus is silently emptied.
    assert!(
        count >= 14,
        "fuzz_aof_decoder corpus shrank to {count} files — regressed seed coverage?"
    );
}

/// Confirm the hostile-input seeds added in ohm5 actually surface as
/// Err rather than tripping a never-panic-but-silently-Ok path.
/// Catches a future refactor that loosens the array/bulk-length caps
/// or drops the recursion guard without explicit thought.
#[test]
fn fuzz_aof_decoder_rejects_hostile_seeds() {
    let dir = corpus_dir();
    for name in [
        "malformed_array_len_overflow",
        "malformed_bulk_len_overflow",
        "truncated_array_header",
        "truncated_bulk_header",
        "truncated_bulk_body",
        "missing_crlf_after_bulk_len",
        "non_resp_prefix",
        "deeply_nested_arrays",
    ] {
        let bytes =
            fs::read(dir.join(name)).unwrap_or_else(|err| panic!("missing seed {name}: {err}"));
        let result = decode_aof_stream(&bytes);
        assert!(
            result.is_err(),
            "hostile seed {name} should fail decoding, got Ok: {result:?}"
        );
    }
}

#[test]
fn fuzz_aof_manifest_parser_corpus_never_panics() {
    // (frankenredis-fek0y) Replay every fuzz_aof_manifest_parser corpus
    // file through parse_aof_manifest under regular cargo test, mirroring
    // the libfuzzer harness's UTF-8-then-parse pipeline. Catches corpus
    // shrinkage and any panic regression in the manifest parser without
    // needing a 60s cargo-fuzz run.
    let dir = manifest_corpus_dir();
    assert!(
        dir.is_dir(),
        "corpus dir missing: {} — did the workspace move?",
        dir.display()
    );

    let mut count = 0_usize;
    for entry in fs::read_dir(&dir).expect("read manifest corpus dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        if let Ok(text) = std::str::from_utf8(&bytes) {
            let _ = parse_aof_manifest(text);
        }
        count += 1;
    }

    assert!(
        count >= 12,
        "fuzz_aof_manifest_parser corpus shrank to {count} files — regressed seed coverage?"
    );
}

#[test]
fn fuzz_aof_manifest_parser_classifies_valid_vs_hostile_seeds() {
    // (frankenredis-fek0y) Catches a future refactor that loosens the
    // manifest parser (e.g. accepting duplicate base entries or
    // non-monotonic incremental sequences) without explicit thought.
    let dir = manifest_corpus_dir();
    let load = |name: &str| -> String {
        let bytes =
            fs::read(dir.join(name)).unwrap_or_else(|err| panic!("missing seed {name}: {err}"));
        String::from_utf8(bytes).unwrap_or_else(|err| panic!("non-utf8 seed {name}: {err}"))
    };

    for name in [
        "valid_base_history_incremental.manifest",
        "valid_comments_blank.manifest",
        "valid_incremental_only.manifest",
        "valid_quoted_spaces.manifest",
    ] {
        let text = load(name);
        let result = parse_aof_manifest(&text);
        assert!(
            result.is_ok(),
            "valid seed {name} should parse to Ok, got {result:?}"
        );
    }

    for name in [
        "duplicate_base.manifest",
        "duplicate_field.manifest",
        "invalid_quoting.manifest",
        "leading_zero_seq.manifest",
        "missing_type.manifest",
        "non_monotonic_incremental.manifest",
        "path_filename.manifest",
        "unknown_type.manifest",
    ] {
        let text = load(name);
        let result = parse_aof_manifest(&text);
        assert!(
            result.is_err(),
            "hostile seed {name} should reject, got Ok: {result:?}"
        );
    }
}
