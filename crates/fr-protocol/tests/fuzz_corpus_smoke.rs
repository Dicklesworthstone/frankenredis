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
        pre_auth: None,
    };
    let permissive = ParserConfig {
        max_bulk_len: 64 * 1024 * 1024,
        max_array_len: 1_048_576,
        max_recursion_depth: 64,
        allow_resp3: true,
        pre_auth: None,
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

/// Checked structural assertions for the hand-picked RESP3 dialect
/// seeds: confirms each sample parses to the variant the fuzz target
/// actually exercises, rather than silently degrading to BulkString
/// (None) on a missed-parser regression. A "no panic" run alone would
/// not catch e.g. someone deleting the `%` arm of parse_frame_internal.
#[test]
fn fuzz_resp_parser_resp3_seeds_parse_to_expected_variants() {
    use fr_protocol::RespFrame;

    let allow = ParserConfig {
        max_bulk_len: 64 * 1024 * 1024,
        max_array_len: 1_048_576,
        max_recursion_depth: 64,
        allow_resp3: true,
        pre_auth: None,
    };

    let dir = corpus_dir();
    let load = |name: &str| -> Vec<u8> {
        let path = dir.join(name);
        fs::read(&path).unwrap_or_else(|err| panic!("read {} failed: {err}", path.display()))
    };

    // %2\r\n+a\r\n:1\r\n+b\r\n:2\r\n → fr's parser flattens RESP3 Map
    // into Array(2*N) of alternating key/value frames; the dedicated
    // RespFrame::Map variant is only used by the *encoder* path.
    let result = parse_frame_with_config(&load("resp3_map_2_pairs"), &allow).expect("map parse");
    let RespFrame::Array(Some(items)) = result.frame else {
        panic!("resp3_map_2_pairs should parse to Array(alternating)");
    };
    assert_eq!(items.len(), 4);

    // %0\r\n → empty Array
    let result = parse_frame_with_config(&load("resp3_empty_map"), &allow).expect("empty map");
    let RespFrame::Array(Some(items)) = result.frame else {
        panic!("resp3_empty_map should parse to Array");
    };
    assert!(items.is_empty());

    // ~3\r\n+a\r\n+b\r\n+c\r\n → Set folds onto Array(3) (parse_array)
    let result = parse_frame_with_config(&load("resp3_set_3_strings"), &allow).expect("set parse");
    let RespFrame::Array(Some(items)) = result.frame else {
        panic!("resp3_set_3_strings should parse to Array");
    };
    assert_eq!(items.len(), 3);

    // >3\r\n+pubsub\r\n+message\r\n+ch\r\n → Push folds onto Array(3)
    let result = parse_frame_with_config(&load("resp3_push_pubsub"), &allow).expect("push parse");
    let RespFrame::Array(Some(items)) = result.frame else {
        panic!("resp3_push_pubsub should parse to Array");
    };
    assert_eq!(items.len(), 3);

    // #t / #f → Integer(1) / Integer(0)
    let result = parse_frame_with_config(&load("resp3_bool_true"), &allow).expect("bool true");
    assert_eq!(result.frame, RespFrame::Integer(1));
    let result = parse_frame_with_config(&load("resp3_bool_false"), &allow).expect("bool false");
    assert_eq!(result.frame, RespFrame::Integer(0));

    // _\r\n → Null bulk
    let result = parse_frame_with_config(&load("resp3_null"), &allow).expect("null");
    assert_eq!(result.frame, RespFrame::BulkString(None));

    // ,3.14\r\n → Double folded onto BulkString carrying the literal
    let result = parse_frame_with_config(&load("resp3_double"), &allow).expect("double");
    let RespFrame::BulkString(Some(body)) = result.frame else {
        panic!("resp3_double should parse to BulkString");
    };
    assert_eq!(body.as_slice(), b"3.14");

    // ,inf and ,-inf and ,nan all reach the same BulkString path —
    // pin the textual passthrough so a future numeric-parse refactor
    // doesn't accidentally drop infinities.
    for (name, expected) in [
        ("resp3_double_inf", b"inf".as_slice()),
        ("resp3_double_neg_inf", b"-inf".as_slice()),
        ("resp3_double_nan", b"nan".as_slice()),
    ] {
        let result = parse_frame_with_config(&load(name), &allow).expect(name);
        let RespFrame::BulkString(Some(body)) = result.frame else {
            panic!("{name} should parse to BulkString");
        };
        assert_eq!(body.as_slice(), expected, "{name}");
    }

    // = verbatim — fr strips the 4-byte `txt:` prefix, leaving body.
    let result = parse_frame_with_config(&load("resp3_verbatim_text"), &allow).expect("verbatim");
    let RespFrame::BulkString(Some(body)) = result.frame else {
        panic!("resp3_verbatim_text should parse to BulkString");
    };
    assert_eq!(body.as_slice(), b"Some string");

    // ( BigNumber → BulkString carrying the digit run.
    let result = parse_frame_with_config(&load("resp3_bignumber"), &allow).expect("bignumber");
    let RespFrame::BulkString(Some(body)) = result.frame else {
        panic!("resp3_bignumber should parse to BulkString");
    };
    assert!(body.iter().all(|b| b.is_ascii_digit() || *b == b'-'));
    assert!(body.len() >= 30);

    // | Attribute: the parser consumes the attribute map and either
    // recurses into the next frame OR surfaces the attribute itself,
    // depending on the implementation. This corpus seed wraps an
    // attribute followed by SimpleString "OK". Just confirm the
    // bytes parse without error; the exact returned variant is
    // implementation-defined and not part of the parity contract.
    let _ = parse_frame_with_config(&load("resp3_attribute_then_string"), &allow)
        .expect("attribute frame parses");
}
