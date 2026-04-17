#![forbid(unsafe_code)]

use fr_protocol::RespFrame;

/// Result of inline command parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum InlineParseResult {
    /// Successfully parsed a command with (frame, bytes_consumed).
    Command(RespFrame, usize),
    /// Empty line that should be silently consumed.
    EmptyLine(usize),
    /// Protocol error (e.g., unbalanced quotes) — send error reply directly.
    ProtocolError(RespFrame, usize),
}

/// Check if the first byte suggests inline command parsing should be attempted.
/// Returns false for bytes that are RESP protocol prefixes.
#[must_use]
pub fn should_try_inline_parsing(first_byte: u8) -> bool {
    !matches!(
        first_byte,
        b'+' | b'-'
            | b':'
            | b'$'
            | b'*'
            | b'~'
            | b'%'
            | b'#'
            | b','
            | b'_'
            | b'('
            | b'='
            | b'|'
            | b'>'
            | b'!'
    )
}

/// Try to parse an inline command (non-RESP). Inline commands are
/// space-separated tokens terminated by \r\n or \n.
/// Returns the parse result on success, or Incomplete if more data is needed.
pub fn try_parse_inline(buf: &[u8]) -> Result<InlineParseResult, fr_protocol::RespParseError> {
    let newline_pos = buf.iter().position(|&b| b == b'\n');
    let Some(nl) = newline_pos else {
        return Err(fr_protocol::RespParseError::Incomplete);
    };
    let consumed = nl + 1;
    let line_end = if nl > 0 && buf[nl - 1] == b'\r' {
        nl - 1
    } else {
        nl
    };
    let line = &buf[..line_end];

    let argv = match split_inline_args(line) {
        Ok(v) => v,
        Err(msg) => {
            let err_frame = RespFrame::Error(msg.to_string());
            return Ok(InlineParseResult::ProtocolError(err_frame, consumed));
        }
    };
    if argv.is_empty() {
        return Ok(InlineParseResult::EmptyLine(consumed));
    }

    let frame = RespFrame::Array(Some(
        argv.into_iter()
            .map(|a| RespFrame::BulkString(Some(a)))
            .collect(),
    ));
    Ok(InlineParseResult::Command(frame, consumed))
}

/// Split inline command arguments, supporting quoted strings.
/// Returns Err if quotes are unbalanced (matching Redis behavior).
pub fn split_inline_args(line: &[u8]) -> Result<Vec<Vec<u8>>, &'static str> {
    let mut args = Vec::new();
    let mut i = 0;
    while i < line.len() {
        if line[i] == b' ' || line[i] == b'\t' {
            i += 1;
            continue;
        }

        if line[i] == b'"' {
            i += 1;
            let mut arg = Vec::new();
            while i < line.len() && line[i] != b'"' {
                if line[i] == b'\\' && i + 1 < line.len() {
                    i += 1;
                    if line[i] == b'x'
                        && i + 2 < line.len()
                        && let Some(byte) = parse_hex_escape(line[i + 1], line[i + 2])
                    {
                        arg.push(byte);
                        i += 3;
                        continue;
                    }
                    match line[i] {
                        b'n' => arg.push(b'\n'),
                        b'r' => arg.push(b'\r'),
                        b't' => arg.push(b'\t'),
                        b'b' => arg.push(b'\x08'),
                        b'a' => arg.push(b'\x07'),
                        b'"' => arg.push(b'"'),
                        b'\\' => arg.push(b'\\'),
                        other => {
                            arg.push(other);
                        }
                    }
                } else {
                    arg.push(line[i]);
                }
                i += 1;
            }
            if i >= line.len() {
                return Err("ERR Protocol error: unbalanced quotes in request");
            }
            i += 1;
            args.push(arg);
        } else if line[i] == b'\'' {
            i += 1;
            let mut arg = Vec::new();
            while i < line.len() && line[i] != b'\'' {
                if line[i] == b'\\' && i + 1 < line.len() && line[i + 1] == b'\'' {
                    i += 1;
                    arg.push(b'\'');
                } else {
                    arg.push(line[i]);
                }
                i += 1;
            }
            if i >= line.len() {
                return Err("ERR Protocol error: unbalanced quotes in request");
            }
            args.push(arg);
            i += 1;
        } else {
            let start = i;
            while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
                i += 1;
            }
            args.push(line[start..i].to_vec());
        }
    }
    Ok(args)
}

fn parse_hex_escape(h1: u8, h2: u8) -> Option<u8> {
    let parse_hex = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let b1 = parse_hex(h1)?;
    let b2 = parse_hex(h2)?;
    Some((b1 << 4) | b2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_simple() {
        let result = try_parse_inline(b"SET key value\r\n").unwrap();
        assert!(matches!(result, InlineParseResult::Command(_, 15)));
    }

    #[test]
    fn inline_quoted() {
        let args = split_inline_args(b"SET key \"hello world\"").unwrap();
        assert_eq!(args.len(), 3);
        assert_eq!(args[2], b"hello world");
    }

    #[test]
    fn inline_unbalanced_quotes() {
        let result = split_inline_args(b"SET key \"unclosed");
        assert!(result.is_err());
    }

    #[test]
    fn inline_escape_sequences() {
        let args = split_inline_args(b"SET key \"hello\\nworld\"").unwrap();
        assert_eq!(args[2], b"hello\nworld");
    }

    #[test]
    fn inline_hex_escape() {
        let args = split_inline_args(b"SET key \"\\x41\\x42\"").unwrap();
        assert_eq!(args[2], b"AB");
    }

    #[test]
    fn inline_single_quotes() {
        let args = split_inline_args(b"SET key 'hello world'").unwrap();
        assert_eq!(args[2], b"hello world");
    }
}
