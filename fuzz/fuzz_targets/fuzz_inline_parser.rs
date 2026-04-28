#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use fr_server::{should_try_inline_parsing, split_inline_args, try_parse_inline};

#[derive(Debug, Arbitrary)]
enum FuzzInput {
    Raw(Vec<u8>),
    Structured(StructuredInline),
}

#[derive(Debug, Arbitrary)]
struct StructuredInline {
    args: Vec<InlineArg>,
    terminator: Terminator,
}

#[derive(Debug, Arbitrary)]
enum InlineArg {
    Unquoted(Vec<u8>),
    DoubleQuoted(QuotedString),
    SingleQuoted(Vec<u8>),
}

#[derive(Debug, Arbitrary)]
struct QuotedString {
    segments: Vec<QuotedSegment>,
}

#[derive(Debug, Arbitrary)]
enum QuotedSegment {
    Literal(Vec<u8>),
    EscapeN,
    EscapeR,
    EscapeT,
    EscapeB,
    EscapeA,
    EscapeQuote,
    EscapeBackslash,
    HexEscape(u8),
    RawEscape(u8),
}

#[derive(Debug, Arbitrary)]
enum Terminator {
    Crlf,
    Lf,
    None,
}

impl StructuredInline {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                out.push(b' ');
            }
            match arg {
                InlineArg::Unquoted(bytes) => {
                    for &b in bytes {
                        if b != b' '
                            && b != b'\t'
                            && b != b'"'
                            && b != b'\''
                            && b != b'\r'
                            && b != b'\n'
                        {
                            out.push(b);
                        }
                    }
                }
                InlineArg::DoubleQuoted(qs) => {
                    out.push(b'"');
                    for seg in &qs.segments {
                        match seg {
                            QuotedSegment::Literal(bytes) => {
                                for &b in bytes {
                                    if b == b'"' || b == b'\\' {
                                        out.push(b'\\');
                                    }
                                    out.push(b);
                                }
                            }
                            QuotedSegment::EscapeN => out.extend_from_slice(b"\\n"),
                            QuotedSegment::EscapeR => out.extend_from_slice(b"\\r"),
                            QuotedSegment::EscapeT => out.extend_from_slice(b"\\t"),
                            QuotedSegment::EscapeB => out.extend_from_slice(b"\\b"),
                            QuotedSegment::EscapeA => out.extend_from_slice(b"\\a"),
                            QuotedSegment::EscapeQuote => out.extend_from_slice(b"\\\""),
                            QuotedSegment::EscapeBackslash => out.extend_from_slice(b"\\\\"),
                            QuotedSegment::HexEscape(b) => {
                                out.extend_from_slice(b"\\x");
                                out.push(b"0123456789abcdef"[(b >> 4) as usize]);
                                out.push(b"0123456789abcdef"[(b & 0xf) as usize]);
                            }
                            QuotedSegment::RawEscape(b) => {
                                out.push(b'\\');
                                out.push(*b);
                            }
                        }
                    }
                    out.push(b'"');
                }
                InlineArg::SingleQuoted(bytes) => {
                    out.push(b'\'');
                    for &b in bytes {
                        if b == b'\'' {
                            out.extend_from_slice(b"\\'");
                        } else {
                            out.push(b);
                        }
                    }
                    out.push(b'\'');
                }
            }
        }
        match self.terminator {
            Terminator::Crlf => out.extend_from_slice(b"\r\n"),
            Terminator::Lf => out.push(b'\n'),
            Terminator::None => {}
        }
        out
    }
}

fuzz_target!(|input: FuzzInput| {
    match input {
        FuzzInput::Raw(data) => {
            if data.len() > 64 * 1024 {
                return;
            }
            let _ = try_parse_inline(&data);
            if let Some(nl) = data.iter().position(|&b| b == b'\n') {
                let line = if nl > 0 && data[nl - 1] == b'\r' {
                    &data[..nl - 1]
                } else {
                    &data[..nl]
                };
                let _ = split_inline_args(line);
            }
            if !data.is_empty() {
                let _ = should_try_inline_parsing(data[0]);
            }
        }
        FuzzInput::Structured(inline) => {
            if inline.args.len() > 100 {
                return;
            }
            let data = inline.to_bytes();
            if data.len() > 64 * 1024 {
                return;
            }
            let result = try_parse_inline(&data);

            match &inline.terminator {
                Terminator::None => {
                    assert!(
                        result.is_err(),
                        "unterminated input should return Incomplete"
                    );
                }
                Terminator::Crlf | Terminator::Lf => {
                    if result.is_ok()
                        && let Some(nl) = data.iter().position(|&b| b == b'\n')
                    {
                        let line = if nl > 0 && data[nl - 1] == b'\r' {
                            &data[..nl - 1]
                        } else {
                            &data[..nl]
                        };
                        let _ = split_inline_args(line);
                    }
                }
            }
        }
    }
});
