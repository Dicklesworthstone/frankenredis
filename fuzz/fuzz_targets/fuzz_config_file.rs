#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_config::{parse_redis_config_bytes, split_config_line_args_bytes};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 8_192;
const MAX_RAW_LEN: usize = 4_096;
const MAX_LINES: usize = 32;
const MAX_ARGS: usize = 8;
const MAX_TOKEN_LEN: usize = 32;

#[derive(Debug, Arbitrary)]
enum ConfigFuzzInput {
    Raw(Vec<u8>),
    Structured(StructuredConfigFile),
}

#[derive(Debug, Arbitrary)]
struct StructuredConfigFile {
    lines: Vec<StructuredConfigLine>,
    trailing_newline: bool,
}

#[derive(Debug, Arbitrary)]
enum StructuredConfigLine {
    Blank,
    Comment(Vec<u8>),
    Directive {
        name: Vec<u8>,
        uppercase_name: bool,
        args: Vec<StructuredToken>,
    },
}

#[derive(Debug, Arbitrary)]
enum StructuredToken {
    Bare(Vec<u8>),
    SingleQuoted(Vec<u8>),
    DoubleQuoted(Vec<u8>),
}

#[derive(Debug, PartialEq, Eq)]
struct ExpectedDirective {
    line_number: usize,
    name: Vec<u8>,
    args: Vec<Vec<u8>>,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    fuzz_raw_config(data);

    let mut unstructured = Unstructured::new(data);
    let Ok(input) = ConfigFuzzInput::arbitrary(&mut unstructured) else {
        return;
    };

    match input {
        ConfigFuzzInput::Raw(raw) => fuzz_raw_config(&raw),
        ConfigFuzzInput::Structured(case) => fuzz_structured_config(case),
    }
});

fn fuzz_raw_config(data: &[u8]) {
    let mut raw = data.to_vec();
    raw.truncate(MAX_RAW_LEN);
    if let Ok(parsed) = parse_redis_config_bytes(&raw) {
        for directive in parsed.directives {
            assert!(!directive.name.is_empty());
            assert!(
                directive.name.iter().all(|byte| !byte.is_ascii_uppercase()),
                "directive names must be ASCII-lowercased like Redis sdstolower",
            );
        }
    }

    let _ = split_config_line_args_bytes(&raw);
}

fn fuzz_structured_config(case: StructuredConfigFile) {
    let (rendered, expected) = render_config(case);
    let parsed =
        parse_redis_config_bytes(rendered.as_bytes()).expect("structured config must parse");
    let actual: Vec<ExpectedDirective> = parsed
        .directives
        .into_iter()
        .map(|directive| ExpectedDirective {
            line_number: directive.line_number,
            name: directive.name,
            args: directive.args,
        })
        .collect();
    assert_eq!(
        actual, expected,
        "structure-aware config rendering must preserve directive tokens",
    );
}

fn render_config(case: StructuredConfigFile) -> (String, Vec<ExpectedDirective>) {
    let mut text = String::new();
    let mut expected = Vec::new();
    let mut lines = case.lines;
    lines.truncate(MAX_LINES);

    if lines.is_empty() {
        lines.push(StructuredConfigLine::Directive {
            name: b"port".to_vec(),
            uppercase_name: false,
            args: vec![StructuredToken::Bare(b"6379".to_vec())],
        });
    }

    for (idx, line) in lines.into_iter().enumerate() {
        let line_number = idx + 1;
        match line {
            StructuredConfigLine::Blank => text.push_str(" \t\r"),
            StructuredConfigLine::Comment(raw) => {
                text.push('#');
                text.push_str(&sanitize_comment(raw));
            }
            StructuredConfigLine::Directive {
                name,
                uppercase_name,
                args,
            } => {
                let mut directive_name = sanitize_name(name);
                if uppercase_name {
                    directive_name.make_ascii_uppercase();
                }
                text.push_str(&String::from_utf8_lossy(&directive_name));

                let mut expected_args = Vec::new();
                let mut args = args;
                args.truncate(MAX_ARGS);
                for arg in args {
                    let (rendered, expected_arg) = render_token(arg);
                    text.push(' ');
                    text.push_str(&rendered);
                    expected_args.push(expected_arg);
                }

                let mut expected_name = directive_name;
                expected_name.make_ascii_lowercase();
                expected.push(ExpectedDirective {
                    line_number,
                    name: expected_name,
                    args: expected_args,
                });
            }
        }
        text.push('\n');
    }

    if !case.trailing_newline {
        text.pop();
    }

    (text, expected)
}

fn render_token(token: StructuredToken) -> (String, Vec<u8>) {
    match token {
        StructuredToken::Bare(raw) => {
            let bytes = sanitize_bare_token(raw, b"value");
            (String::from_utf8_lossy(&bytes).into_owned(), bytes)
        }
        StructuredToken::SingleQuoted(raw) => {
            let bytes = sanitize_single_quoted_token(raw);
            let mut rendered = String::from("'");
            for byte in &bytes {
                if *byte == b'\'' {
                    rendered.push_str("\\'");
                } else {
                    rendered.push(char::from(*byte));
                }
            }
            rendered.push('\'');
            (rendered, bytes)
        }
        StructuredToken::DoubleQuoted(raw) => {
            let mut bytes = raw;
            bytes.truncate(MAX_TOKEN_LEN);
            if bytes.is_empty() {
                bytes.extend_from_slice(b"value");
            }

            let mut rendered = String::from("\"");
            for byte in &bytes {
                match *byte {
                    b'\n' => rendered.push_str("\\n"),
                    b'\r' => rendered.push_str("\\r"),
                    b'\t' => rendered.push_str("\\t"),
                    0x08 => rendered.push_str("\\b"),
                    0x07 => rendered.push_str("\\a"),
                    b'"' => rendered.push_str("\\\""),
                    b'\\' => rendered.push_str("\\\\"),
                    byte if byte.is_ascii_graphic() || byte == b' ' => {
                        rendered.push(char::from(byte));
                    }
                    byte => rendered.push_str(&format!("\\x{byte:02x}")),
                }
            }
            rendered.push('"');
            (rendered, bytes)
        }
    }
}

fn sanitize_name(raw: Vec<u8>) -> Vec<u8> {
    sanitize_with_allowed(raw, b"port", |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
    })
}

fn sanitize_bare_token(raw: Vec<u8>, fallback: &[u8]) -> Vec<u8> {
    sanitize_with_allowed(raw, fallback, |byte| {
        byte.is_ascii_graphic() && !matches!(byte, b'\'' | b'"' | b'\\')
    })
}

fn sanitize_single_quoted_token(raw: Vec<u8>) -> Vec<u8> {
    let mut out: Vec<u8> = raw
        .into_iter()
        .filter(|byte| byte.is_ascii_graphic() || *byte == b' ')
        .take(MAX_TOKEN_LEN)
        .collect();
    if out.is_empty() {
        out.extend_from_slice(b"value");
    }
    out
}

fn sanitize_comment(raw: Vec<u8>) -> String {
    let bytes = sanitize_with_allowed(raw, b" fuzz seed", |byte| {
        byte.is_ascii_graphic() || byte == b' '
    });
    String::from_utf8_lossy(&bytes).into_owned()
}

fn sanitize_with_allowed(raw: Vec<u8>, fallback: &[u8], allowed: impl Fn(u8) -> bool) -> Vec<u8> {
    let mut out: Vec<u8> = raw
        .into_iter()
        .filter(|byte| allowed(*byte) && *byte != 0)
        .take(MAX_TOKEN_LEN)
        .collect();
    if out.is_empty() {
        out.extend_from_slice(fallback);
    }
    out
}
