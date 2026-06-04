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
    // (frankenredis-5qqv1) Faithful port of sds.c::sdssplitargs: a token is
    // built char-by-char with double/single-quote state that can flip MID
    // token. fr previously only recognised a quote at a token's START, so
    // `PING"` was a literal token and `SET a"b c` split into 3 args, where
    // upstream errors "unbalanced quotes" (any quote that isn't closed and
    // followed by whitespace/end is an error). The `\x`/escape handling and
    // the closing-quote-must-be-followed-by-whitespace rule (z1h45) are
    // preserved. Separators are ' ' / '\t' only — try_parse_inline already
    // strips the trailing CR/LF.
    const UNBALANCED: &str = "ERR Protocol error: unbalanced quotes in request";
    let n = line.len();
    let is_sep = |b: u8| b == b' ' || b == b'\t';

    let mut args = Vec::new();
    let mut i = 0;
    loop {
        while i < n && is_sep(line[i]) {
            i += 1;
        }
        if i >= n {
            break;
        }

        let mut arg = Vec::new();
        let mut inq = false; // inside double quotes
        let mut insq = false; // inside single quotes
        let mut done = false;
        while !done {
            if i >= n {
                // Ran off the end. An open quote is unterminated.
                if inq || insq {
                    return Err(UNBALANCED);
                }
                break;
            }
            if inq {
                if line[i] == b'\\'
                    && i + 3 < n
                    && line[i + 1] == b'x'
                    && let Some(byte) = parse_hex_escape(line[i + 2], line[i + 3])
                {
                    arg.push(byte);
                    i += 4;
                } else if line[i] == b'\\' && i + 1 < n {
                    i += 1;
                    arg.push(match line[i] {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'b' => b'\x08',
                        b'a' => b'\x07',
                        other => other,
                    });
                    i += 1;
                } else if line[i] == b'"' {
                    // Closing quote must be followed by whitespace or end.
                    if i + 1 < n && !is_sep(line[i + 1]) {
                        return Err(UNBALANCED);
                    }
                    i += 1;
                    done = true;
                } else {
                    arg.push(line[i]);
                    i += 1;
                }
            } else if insq {
                if line[i] == b'\\' && i + 1 < n && line[i + 1] == b'\'' {
                    arg.push(b'\'');
                    i += 2;
                } else if line[i] == b'\'' {
                    if i + 1 < n && !is_sep(line[i + 1]) {
                        return Err(UNBALANCED);
                    }
                    i += 1;
                    done = true;
                } else {
                    arg.push(line[i]);
                    i += 1;
                }
            } else {
                match line[i] {
                    b' ' | b'\t' => done = true,
                    b'"' => {
                        inq = true;
                        i += 1;
                    }
                    b'\'' => {
                        insq = true;
                        i += 1;
                    }
                    c => {
                        arg.push(c);
                        i += 1;
                    }
                }
            }
        }
        args.push(arg);
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
    fn inline_mid_token_quote_matches_upstream() {
        // (frankenredis-5qqv1) A quote can begin mid-token; if it isn't closed
        // and followed by whitespace/end it's an "unbalanced quotes" error,
        // matching sds.c::sdssplitargs. fr previously only honoured a quote at
        // a token's start, so these silently parsed instead of erroring.
        assert!(split_inline_args(b"PING\"").is_err());
        assert!(split_inline_args(b"SET a\"b c").is_err());
        assert!(split_inline_args(b"SET key \"hello\"world").is_err());
        // A mid-token quote that IS closed-then-space is valid and concatenates
        // the unquoted prefix with the quoted body.
        let args = split_inline_args(b"SET ab\"c d\" e").unwrap();
        assert_eq!(
            args,
            vec![b"SET".to_vec(), b"abc d".to_vec(), b"e".to_vec()]
        );
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

    #[test]
    fn inline_closing_double_quote_must_be_followed_by_whitespace() {
        // (frankenredis-z1h45) Upstream sds.c::sdssplitargs lines
        // 1049-1053 require the closing `"` to be followed by space
        // or end-of-line. fr was silently splitting `SET key "hello"world`
        // into ["SET", "key", "hello", "world"]; now rejects.
        let result = split_inline_args(b"SET key \"hello\"world");
        assert!(result.is_err());

        // No trailer after closing quote → still ok.
        let args = split_inline_args(b"SET key \"hello\"").unwrap();
        assert_eq!(args[2], b"hello");

        // Whitespace after closing quote → ok.
        let args = split_inline_args(b"SET key \"hello\" extra").unwrap();
        assert_eq!(
            args,
            vec![
                b"SET".to_vec(),
                b"key".to_vec(),
                b"hello".to_vec(),
                b"extra".to_vec()
            ]
        );

        // Tab after closing quote → ok.
        let args = split_inline_args(b"SET key \"hello\"\textra").unwrap();
        assert_eq!(args[2], b"hello");
        assert_eq!(args[3], b"extra");
    }

    #[test]
    fn inline_closing_single_quote_must_be_followed_by_whitespace() {
        // (frankenredis-z1h45) Same rule for single-quoted tokens —
        // upstream sds.c lines 1064-1068.
        let result = split_inline_args(b"SET key 'hello'world");
        assert!(result.is_err());

        // No trailer → ok.
        let args = split_inline_args(b"SET key 'hello'").unwrap();
        assert_eq!(args[2], b"hello");

        // Whitespace trailer → ok.
        let args = split_inline_args(b"SET key 'hello' world").unwrap();
        assert_eq!(
            args,
            vec![
                b"SET".to_vec(),
                b"key".to_vec(),
                b"hello".to_vec(),
                b"world".to_vec()
            ]
        );
    }
}
