use fr_protocol::{ParserConfig, parse_frame, parse_frame_with_config};
use std::fs;
use std::path::Path;

fn assert_golden(test_name: &str, actual: &str) {
    let golden_path = Path::new("tests/golden").join(format!("{}.golden", test_name));

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
        fs::write(&golden_path, actual).unwrap();
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = fs::read_to_string(&golden_path).unwrap_or_else(|_| {
        panic!(
            "Golden file missing: {}\n\
             Run with UPDATE_GOLDENS=1 to create it",
            golden_path.display()
        )
    });

    if actual != expected {
        let actual_path = golden_path.with_extension("actual");
        fs::write(&actual_path, actual).unwrap();

        panic!(
            "GOLDEN MISMATCH: {}\n\
             To update: UPDATE_GOLDENS=1 cargo test --test golden\n\
             To review: diff {} {}",
            test_name,
            golden_path.display(),
            actual_path.display(),
        );
    }
}

fn parse_and_snapshot(test_name: &str, input: &[u8]) {
    match parse_frame(input) {
        Ok(result) => {
            let actual = format!("{:#?}", result);
            assert_golden(test_name, &actual);
        }
        Err(e) => {
            let actual = format!("Error: {:#?}", e);
            assert_golden(test_name, &actual);
        }
    }
}

fn parse_and_snapshot_with_config(test_name: &str, input: &[u8], config: &ParserConfig) {
    match parse_frame_with_config(input, config) {
        Ok(result) => {
            let actual = format!("{:#?}", result);
            assert_golden(test_name, &actual);
        }
        Err(e) => {
            let actual = format!("Error: {:#?}", e);
            assert_golden(test_name, &actual);
        }
    }
}

#[test]
fn golden_simple_string() {
    parse_and_snapshot("simple_string", b"+OK\r\n");
}

#[test]
fn golden_error_string() {
    parse_and_snapshot("error_string", b"-ERR unknown command 'foobar'\r\n");
}

#[test]
fn golden_integer() {
    parse_and_snapshot("integer", b":1000\r\n");
}

#[test]
fn golden_bulk_string() {
    parse_and_snapshot("bulk_string", b"$6\r\nfoobar\r\n");
}

#[test]
fn golden_null_bulk_string() {
    parse_and_snapshot("null_bulk_string", b"$-1\r\n");
}

#[test]
fn golden_array() {
    parse_and_snapshot("array", b"*2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n");
}

#[test]
fn golden_null_array() {
    parse_and_snapshot("null_array", b"*-1\r\n");
}

#[test]
fn golden_nested_array() {
    parse_and_snapshot(
        "nested_array",
        b"*2\r\n*3\r\n:1\r\n:2\r\n:3\r\n*2\r\n+Foo\r\n-Bar\r\n",
    );
}

#[test]
fn golden_invalid_prefix() {
    parse_and_snapshot("invalid_prefix", b"x1000\r\n");
}

#[test]
fn golden_invalid_bulk_length() {
    parse_and_snapshot("invalid_bulk_length", b"$abc\r\n");
}

// ── Happy-path boundary fixtures ─────────────────────────────────────

#[test]
fn golden_empty_simple_string() {
    parse_and_snapshot("empty_simple_string", b"+\r\n");
}

#[test]
fn golden_empty_error_string() {
    parse_and_snapshot("empty_error_string", b"-\r\n");
}

#[test]
fn golden_empty_bulk_string() {
    parse_and_snapshot("empty_bulk_string", b"$0\r\n\r\n");
}

#[test]
fn golden_empty_array() {
    parse_and_snapshot("empty_array", b"*0\r\n");
}

#[test]
fn golden_zero_integer() {
    parse_and_snapshot("zero_integer", b":0\r\n");
}

#[test]
fn golden_negative_integer() {
    parse_and_snapshot("negative_integer", b":-1234\r\n");
}

#[test]
fn golden_i64_max_integer() {
    parse_and_snapshot("i64_max_integer", b":9223372036854775807\r\n");
}

#[test]
fn golden_i64_min_integer() {
    parse_and_snapshot("i64_min_integer", b":-9223372036854775808\r\n");
}

#[test]
fn golden_bulk_string_with_binary() {
    // Bulk body of length 4 carrying two control bytes followed by CRLF.
    // Upstream Redis allows arbitrary bytes inside a bulk body; the framing
    // CRLF after the body is what terminates it, not any CRLF in the body.
    parse_and_snapshot("bulk_string_with_binary", b"$4\r\n\x00\xff\r\n\r\n");
}

#[test]
fn golden_pipelined_sequence_takes_first_frame() {
    // Two +OK frames back to back — parse_frame consumes exactly the first.
    parse_and_snapshot(
        "pipelined_sequence_takes_first_frame",
        b"+PING\r\n+OK\r\n:7\r\n",
    );
}

// ── Incomplete / partial fixtures ────────────────────────────────────

#[test]
fn golden_incomplete_simple_string() {
    parse_and_snapshot("incomplete_simple_string", b"+PING");
}

#[test]
fn golden_incomplete_bulk_body() {
    // Length declares 6 bytes, only 3 delivered.
    parse_and_snapshot("incomplete_bulk_body", b"$6\r\nfoo");
}

#[test]
fn golden_incomplete_bulk_terminator() {
    // Full body present but trailing CRLF missing.
    parse_and_snapshot("incomplete_bulk_terminator", b"$3\r\nfoo");
}

#[test]
fn golden_incomplete_array_child() {
    // Header says 2 children, only one full child delivered.
    parse_and_snapshot("incomplete_array_child", b"*2\r\n:1\r\n");
}

// ── Adversarial / error-path fixtures ────────────────────────────────

#[test]
fn golden_invalid_integer_payload() {
    parse_and_snapshot("invalid_integer_payload", b":abc\r\n");
}

#[test]
fn golden_integer_with_leading_plus_rejected() {
    // parse_i64_strict rejects leading '+'; fixture locks that choice.
    parse_and_snapshot("integer_with_leading_plus_rejected", b":+5\r\n");
}

#[test]
fn golden_bulk_length_minus_two_invalid() {
    // Only -1 is the valid negative bulk length (for nil).
    parse_and_snapshot("bulk_length_minus_two_invalid", b"$-2\r\n");
}

#[test]
fn golden_array_length_minus_two_invalid() {
    parse_and_snapshot("array_length_minus_two_invalid", b"*-2\r\n");
}

#[test]
fn golden_bulk_length_exceeds_limit() {
    // Use a tiny max_bulk_len so the fixture stays small and deterministic.
    let config = ParserConfig {
        max_bulk_len: 16,
        ..ParserConfig::default()
    };
    parse_and_snapshot_with_config("bulk_length_exceeds_limit", b"$32\r\n", &config);
}

#[test]
fn golden_multibulk_length_exceeds_limit() {
    let config = ParserConfig {
        max_array_len: 4,
        ..ParserConfig::default()
    };
    parse_and_snapshot_with_config("multibulk_length_exceeds_limit", b"*10\r\n", &config);
}

#[test]
fn golden_recursion_limit_exceeded() {
    // Three-level nested array under a max_recursion_depth of 2 → reject.
    let config = ParserConfig {
        max_recursion_depth: 2,
        ..ParserConfig::default()
    };
    parse_and_snapshot_with_config(
        "recursion_limit_exceeded",
        b"*1\r\n*1\r\n*1\r\n:1\r\n",
        &config,
    );
}

// ── RESP3 prefixes — frozen as XFAIL markers for frankenredis-0zyf ──

#[test]
fn golden_unsupported_resp3_map_prefix() {
    parse_and_snapshot("unsupported_resp3_map_prefix", b"%2\r\n");
}

#[test]
fn golden_unsupported_resp3_set_prefix() {
    parse_and_snapshot("unsupported_resp3_set_prefix", b"~2\r\n");
}

#[test]
fn golden_unsupported_resp3_null_prefix() {
    parse_and_snapshot("unsupported_resp3_null_prefix", b"_\r\n");
}

#[test]
fn golden_unsupported_resp3_boolean_prefix() {
    parse_and_snapshot("unsupported_resp3_boolean_prefix", b"#t\r\n");
}

#[test]
fn golden_unsupported_resp3_double_prefix() {
    parse_and_snapshot("unsupported_resp3_double_prefix", b",3.14\r\n");
}

#[test]
fn golden_unsupported_resp3_big_number_prefix() {
    parse_and_snapshot("unsupported_resp3_big_number_prefix", b"(12345\r\n");
}

#[test]
fn golden_unsupported_resp3_push_prefix() {
    parse_and_snapshot("unsupported_resp3_push_prefix", b">1\r\n");
}
