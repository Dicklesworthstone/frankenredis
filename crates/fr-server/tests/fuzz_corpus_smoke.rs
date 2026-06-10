//! Replay every file in `fuzz/corpus/fuzz_inline_parser/` through
//! `try_parse_inline` + `split_inline_args` + `should_try_inline_parsing`
//! to lock in the invariant that none of the seeded inputs panics.
//! Mirrors the fr-protocol/fr-persist fuzz_corpus_smoke pattern: every
//! handcrafted seed gets exercised under regular `cargo test` so corner
//! cases stay deterministically covered between cargo-fuzz runs.
//!
//! (frankenredis-c85oq)

use fr_server::{should_try_inline_parsing, split_inline_args, try_parse_inline};
use std::fs;
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("../../fuzz/corpus/fuzz_inline_parser")
}

#[test]
fn fuzz_inline_parser_corpus_never_panics() {
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

        // The libfuzzer harness reads the trailing byte as the variant
        // tag (0x00 = Raw, 0x01 = Structured); we strip it to get back
        // to the underlying inline byte stream the parser sees.
        let payload = if bytes.is_empty() {
            &bytes[..]
        } else {
            &bytes[..bytes.len() - 1]
        };

        let _ = try_parse_inline(payload);
        if let Some(nl) = payload.iter().position(|&b| b == b'\n') {
            let line = if nl > 0 && payload[nl - 1] == b'\r' {
                &payload[..nl - 1]
            } else {
                &payload[..nl]
            };
            let _ = split_inline_args(line);
        }
        if !payload.is_empty() {
            let _ = should_try_inline_parsing(payload[0]);
        }

        count += 1;
    }

    // Initial seed batch is 42 files (happy paths, quoted-string escapes,
    // hex escapes, single-quoting, mixed quoting, whitespace edges,
    // unbalanced/embedded-NUL hostile inputs, RESP-prefix bytes,
    // long-token + many-args, and incomplete inputs). Bail loudly if the
    // corpus is silently emptied.
    assert!(
        count >= 30,
        "fuzz_inline_parser corpus shrank to {count} files — regressed seed coverage?"
    );
}

/// Confirm only the multibulk prefix `*` is routed to the RESP parser;
/// every other first byte — including the RESP2 reply prefixes `$`/`+`
/// (and the RESP3 type prefixes) — is treated as an inline command,
/// exactly like upstream `processInputBuffer`. Catches a regression that
/// re-adds RESP prefixes to an inline-gate denylist, which would route
/// `>`/`~`/`%`/... through the RESP parser and DROP the client connection
/// with "unsupported RESP3 type prefix" instead of replying "unknown
/// command '<token>'". (frankenredis-c6vt7)
#[test]
fn fuzz_inline_parser_routes_only_star_to_resp() {
    let dir = corpus_dir();
    // `*` (multibulk) is the sole prefix that stays on the RESP parser path.
    let star = fs::read(dir.join("resp_prefix_star"))
        .unwrap_or_else(|err| panic!("missing seed resp_prefix_star: {err}"));
    let star_payload = &star[..star.len() - 1]; // strip variant tag
    assert!(
        !star_payload.is_empty() && !should_try_inline_parsing(star_payload[0]),
        "`*` multibulk prefix must stay on the RESP parser path"
    );
    // RESP2 reply prefixes a client never legitimately sends as a command
    // are treated as inline (→ "unknown command '<token>'"), matching redis.
    for name in ["resp_prefix_dollar", "resp_prefix_plus"] {
        let bytes =
            fs::read(dir.join(name)).unwrap_or_else(|err| panic!("missing seed {name}: {err}"));
        let payload = &bytes[..bytes.len() - 1]; // strip variant tag
        assert!(
            !payload.is_empty() && should_try_inline_parsing(payload[0]),
            "RESP-prefix seed {name} (first byte 0x{:02x}) must be treated as inline like redis",
            payload[0]
        );
    }
}

/// Confirm the unbalanced-quote seeds surface as the UnbalancedInlineQuotes
/// protocol error rather than panicking or silently parsing as if balanced.
/// Like upstream processInlineBuffer's setProtocolError, an inline protocol
/// error is an Err so the caller replies and closes the connection. Catches a
/// future refactor that loosens the unbalanced-quote check in split_inline_args.
#[test]
fn fuzz_inline_parser_unbalanced_quote_seeds_yield_protocol_error() {
    let dir = corpus_dir();
    for name in ["unbalanced_double_quote", "unbalanced_single_quote"] {
        let bytes =
            fs::read(dir.join(name)).unwrap_or_else(|err| panic!("missing seed {name}: {err}"));
        let payload = &bytes[..bytes.len() - 1];
        assert_eq!(
            try_parse_inline(payload),
            Err(fr_protocol::RespParseError::UnbalancedInlineQuotes),
            "unbalanced-quote seed {name} should yield the unbalanced-quotes protocol error"
        );
    }
}
