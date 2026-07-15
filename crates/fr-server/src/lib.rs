#![forbid(unsafe_code)]

use std::io;

use fr_protocol::{ParserConfig, RespFrame, RespParseError};

/// Result of inline command parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum InlineParseResult {
    /// Successfully parsed a command with (frame, bytes_consumed).
    Command(RespFrame, usize),
    /// Empty line that should be silently consumed.
    EmptyLine(usize),
}

/// Check if the first byte suggests inline command parsing should be attempted.
///
/// Upstream `networking.c::processInputBuffer` treats ANY first byte that is
/// not `*` (the multibulk prefix) as the start of an inline command — including
/// the RESP2 reply prefixes (`+ - : $`) and RESP3 type prefixes
/// (`~ % # , _ ( = | > !`), none of which a client ever legitimately sends as
/// a command. So e.g. `>3\r\nfoo\r\n` yields `unknown command '>3'` then
/// `unknown command 'foo'`, with the connection kept open — NOT a protocol
/// error that drops the connection. Only `*` stays on the RESP multibulk
/// parser path. (frankenredis-c6vt7)
#[must_use]
pub fn should_try_inline_parsing(first_byte: u8) -> bool {
    first_byte != b'*'
}

/// Upstream `server.h::PROTO_INLINE_MAX_SIZE` — the largest inline request the
/// server will buffer before a `\n` arrives. A never-terminated inline line
/// must not grow the query buffer without bound.
const PROTO_INLINE_MAX_SIZE: usize = 64 * 1024;

/// Try to parse an inline command (non-RESP). Inline commands are
/// space-separated tokens terminated by \r\n or \n.
/// Returns the parse result on success, or Incomplete if more data is needed.
pub fn try_parse_inline(buf: &[u8]) -> Result<InlineParseResult, fr_protocol::RespParseError> {
    let newline_pos = buf.iter().position(|&b| b == b'\n');
    let Some(nl) = newline_pos else {
        // Upstream networking.c::processInlineBuffer (line 2146): with no `\n`
        // yet, an unconsumed buffer already past PROTO_INLINE_MAX_SIZE is the
        // "too big inline request" protocol error. Returning it as an Err routes
        // through the caller's handle_parse_error, which replies
        // "ERR Protocol error: too big inline request" and closes the
        // connection — mirroring upstream's addReplyError + setProtocolError.
        // Below the cap, keep waiting for more data.
        if buf.len() > PROTO_INLINE_MAX_SIZE {
            return Err(fr_protocol::RespParseError::InlineRequestTooBig);
        }
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
        Err(_) => {
            // Unbalanced quotes is an inline protocol error. Like the too-big
            // case (and every multibulk protocol error), surface it as an Err
            // so the caller's handle_parse_error replies
            // "ERR Protocol error: unbalanced quotes in request" and closes the
            // connection — matching upstream processInlineBuffer's
            // setProtocolError. (split_inline_args keeps its &str message for
            // its own unit tests; the wire wording comes from the Display.)
            return Err(fr_protocol::RespParseError::UnbalancedInlineQuotes);
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
    // preserved. Separators match upstream sds.c::sdssplitargs exactly —
    // ' ' / '\t' / '\r' / '\n'. try_parse_inline strips the TRAILING CR/LF, but
    // an EMBEDDED '\r' (e.g. `SET\rk v`) must still split the token: upstream
    // treats it as whitespace anywhere in the line, so `SET\rk v` parses as
    // `SET k v`; fr previously kept "\r" inside the token and rejected it as an
    // unknown command. (Quotes still suppress separation inside a quoted run.)
    const UNBALANCED: &str = "ERR Protocol error: unbalanced quotes in request";
    // Upstream sds.c::sdssplitargs scans with `while(*p)`, so a NUL byte is a
    // hard end-of-input: in the unquoted state it ends the current token and
    // returns the args gathered so far (`case '\0': done=1` then the outer
    // `if(*p)` is false); inside quotes it is the unterminated-quote error
    // (`else if (!*p) goto err`). fr previously scanned the whole line length
    // and treated `\0` as an ordinary byte, so e.g. inline `SET\0x y` parsed to
    // [`SET\0x`, `y`] (unknown command) instead of upstream's [`SET`] (arity
    // error), and `"a\0b"` returned a token instead of "unbalanced quotes".
    // Bounding `n` at the first NUL reproduces `while(*p)` exactly: every index
    // access below is already `< n`-guarded, so the unquoted path stops the
    // scan there and the quoted path falls through to the unterminated-quote
    // branch. (frankenredis: sdssplitargs NUL termination)
    let n = line.iter().position(|&b| b == b'\0').unwrap_or(line.len());
    let is_sep = |b: u8| b == b' ' || b == b'\t' || b == b'\r' || b == b'\n';

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
                    b' ' | b'\t' | b'\r' | b'\n' => done = true,
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

/// Removes and returns the largest complete RESP-frame prefix in a replica read buffer.
///
/// Any incomplete trailing frame remains in `read_buf` for the next socket read.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidData`] when the complete prefix contains malformed RESP.
#[cfg_attr(feature = "bench-reference", inline(never))]
pub fn consume_complete_replication_prefix(
    read_buf: &mut Vec<u8>,
    parser_config: &ParserConfig,
) -> io::Result<Vec<u8>> {
    let mut consumed_total = 0usize;
    loop {
        if consumed_total >= read_buf.len() {
            break;
        }
        let unparsed = &read_buf[consumed_total..];
        match fr_protocol::parse_frame_with_config(unparsed, parser_config) {
            Ok(parsed) => {
                consumed_total = consumed_total.saturating_add(parsed.consumed);
            }
            Err(RespParseError::Incomplete) => break,
            Err(err) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid replication backlog from primary: {err}"),
                ));
            }
        }
    }
    if consumed_total == 0 {
        return Ok(Vec::new());
    }
    let tail = read_buf.split_off(consumed_total);
    Ok(std::mem::replace(read_buf, tail))
}

/// Frozen pre-optimization replica-prefix extraction for same-binary benchmarks.
#[cfg(feature = "bench-reference")]
#[doc(hidden)]
#[inline(never)]
pub fn bench_consume_complete_replication_prefix_reference(
    read_buf: &mut Vec<u8>,
    parser_config: &ParserConfig,
) -> io::Result<Vec<u8>> {
    let mut consumed_total = 0usize;
    loop {
        if consumed_total >= read_buf.len() {
            break;
        }
        let unparsed = &read_buf[consumed_total..];
        match fr_protocol::parse_frame_with_config(unparsed, parser_config) {
            Ok(parsed) => {
                consumed_total = consumed_total.saturating_add(parsed.consumed);
            }
            Err(RespParseError::Incomplete) => break,
            Err(err) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid replication backlog from primary: {err}"),
                ));
            }
        }
    }
    if consumed_total == 0 {
        return Ok(Vec::new());
    }
    let payload = read_buf[..consumed_total].to_vec();
    read_buf.drain(..consumed_total);
    Ok(payload)
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
    fn inline_no_newline_below_cap_is_incomplete() {
        // No `\n` yet and at/under PROTO_INLINE_MAX_SIZE: wait for more data.
        let buf = vec![b'a'; PROTO_INLINE_MAX_SIZE];
        assert_eq!(
            try_parse_inline(&buf),
            Err(fr_protocol::RespParseError::Incomplete)
        );
    }

    #[test]
    fn inline_too_big_request_is_protocol_error() {
        // Upstream networking.c:2146 — no `\n` and unconsumed buffer exceeds
        // PROTO_INLINE_MAX_SIZE is the "too big inline request" protocol error.
        // It surfaces as an Err so the caller (handle_parse_error) replies and
        // closes the connection like setProtocolError.
        let buf = vec![b'a'; PROTO_INLINE_MAX_SIZE + 1];
        assert_eq!(
            try_parse_inline(&buf),
            Err(fr_protocol::RespParseError::InlineRequestTooBig)
        );
        // The Display wording matches upstream's reply body after "ERR ".
        assert_eq!(
            fr_protocol::RespParseError::InlineRequestTooBig.to_string(),
            "too big inline request"
        );
        // A newline within the cap is still a normal command even when long.
        let mut ok = vec![b'a'; 8];
        ok.extend_from_slice(b"\r\n");
        assert!(matches!(
            try_parse_inline(&ok).unwrap(),
            InlineParseResult::Command(_, _)
        ));
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

    #[test]
    fn inline_nul_terminates_scan_like_sdssplitargs() {
        // Upstream sds.c::sdssplitargs scans with `while(*p)`, so an embedded
        // NUL is a hard end-of-input. fr previously treated `\0` as an ordinary
        // byte and kept parsing past it.

        // Unquoted NUL ends the current token and returns the args so far
        // (`case '\0': done=1`, then the outer `if(*p)` is false) — the bytes
        // after the NUL are dropped, NOT folded into the token or split off.
        assert_eq!(
            split_inline_args(b"SET\0x y").unwrap(),
            vec![b"SET".to_vec()]
        );
        assert_eq!(
            split_inline_args(b"GET foo\0bar baz").unwrap(),
            vec![b"GET".to_vec(), b"foo".to_vec()]
        );
        // A NUL right after a separator returns the prior tokens with no empty
        // trailing arg (outer skip-blanks stops at the NUL, then `if(*p)` fails).
        assert_eq!(
            split_inline_args(b"PING \0ignored").unwrap(),
            vec![b"PING".to_vec()]
        );
        // A leading/only NUL yields no args (an inline empty line).
        assert!(split_inline_args(b"\0whatever").unwrap().is_empty());

        // A NUL INSIDE quotes is the unterminated-quote error
        // (`else if (!*p) goto err`), matching an unclosed quote at end-of-line.
        assert!(split_inline_args(b"SET \"a\0b\"").is_err());
        assert!(split_inline_args(b"SET 'a\0b'").is_err());

        // A closing quote BEFORE the NUL is still a complete, valid token (the
        // NUL only ends the scan, like a trailing newline would).
        assert_eq!(
            split_inline_args(b"SET k \"v\"\0junk").unwrap(),
            vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]
        );
    }
}
