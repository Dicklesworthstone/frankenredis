#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt::{self, Display};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespFrame {
    SimpleString(String),
    Error(String),
    Integer(i64),
    BulkString(Option<Vec<u8>>),
    Array(Option<Vec<RespFrame>>),
}

impl RespFrame {
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::SimpleString(s) => {
                out.extend_from_slice(b"+");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::Error(s) => {
                out.extend_from_slice(b"-");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::Integer(n) => {
                out.extend_from_slice(b":");
                out.extend_from_slice(n.to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::BulkString(None) => out.extend_from_slice(b"$-1\r\n"),
            Self::BulkString(Some(bytes)) => {
                out.extend_from_slice(b"$");
                out.extend_from_slice(bytes.len().to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(bytes);
                out.extend_from_slice(b"\r\n");
            }
            Self::Array(None) => out.extend_from_slice(b"*-1\r\n"),
            Self::Array(Some(frames)) => {
                out.extend_from_slice(b"*");
                out.extend_from_slice(frames.len().to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into(out);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseResult {
    pub frame: RespFrame,
    pub consumed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespParseError {
    Incomplete,
    InvalidPrefix(u8),
    InvalidInteger,
    InvalidBulkLength,
    InvalidMultibulkLength,
    InvalidUtf8,
}

impl Display for RespParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete => write!(f, "incomplete frame"),
            Self::InvalidPrefix(ch) => write!(f, "invalid RESP type prefix: {}", char::from(*ch)),
            Self::InvalidInteger => write!(f, "invalid RESP integer"),
            Self::InvalidBulkLength => write!(f, "invalid bulk length"),
            Self::InvalidMultibulkLength => write!(f, "invalid multibulk length"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 payload"),
        }
    }
}

impl Error for RespParseError {}

pub fn parse_frame(input: &[u8]) -> Result<ParseResult, RespParseError> {
    let (frame, consumed) = parse_frame_internal(input, 0)?;
    Ok(ParseResult { frame, consumed })
}

fn parse_frame_internal(input: &[u8], start: usize) -> Result<(RespFrame, usize), RespParseError> {
    let prefix = *input.get(start).ok_or(RespParseError::Incomplete)?;
    let next = start + 1;
    match prefix {
        b'+' => {
            let (line, consumed) = read_line(input, next)?;
            let text = std::str::from_utf8(line)
                .map(str::to_owned)
                .map_err(|_| RespParseError::InvalidUtf8)?;
            Ok((RespFrame::SimpleString(text), consumed))
        }
        b'-' => {
            let (line, consumed) = read_line(input, next)?;
            let text = std::str::from_utf8(line)
                .map(str::to_owned)
                .map_err(|_| RespParseError::InvalidUtf8)?;
            Ok((RespFrame::Error(text), consumed))
        }
        b':' => {
            let (line, consumed) = read_line(input, next)?;
            let text = std::str::from_utf8(line).map_err(|_| RespParseError::InvalidUtf8)?;
            let n = text
                .parse::<i64>()
                .map_err(|_| RespParseError::InvalidInteger)?;
            Ok((RespFrame::Integer(n), consumed))
        }
        b'$' => parse_bulk(input, next),
        b'*' => parse_array(input, next),
        other => Err(RespParseError::InvalidPrefix(other)),
    }
}

fn parse_bulk(input: &[u8], start: usize) -> Result<(RespFrame, usize), RespParseError> {
    let (line, consumed) = read_line(input, start)?;
    let text = std::str::from_utf8(line).map_err(|_| RespParseError::InvalidUtf8)?;
    let len = text
        .parse::<i64>()
        .map_err(|_| RespParseError::InvalidBulkLength)?;
    if len == -1 {
        return Ok((RespFrame::BulkString(None), consumed));
    }
    if len < -1 {
        return Err(RespParseError::InvalidBulkLength);
    }
    let data_len = usize::try_from(len).map_err(|_| RespParseError::InvalidBulkLength)?;
    let end = consumed
        .checked_add(data_len)
        .and_then(|idx| idx.checked_add(2))
        .ok_or(RespParseError::Incomplete)?;
    if input.len() < end {
        return Err(RespParseError::Incomplete);
    }
    if input[consumed + data_len] != b'\r' || input[consumed + data_len + 1] != b'\n' {
        return Err(RespParseError::Incomplete);
    }
    let bytes = input[consumed..consumed + data_len].to_vec();
    Ok((RespFrame::BulkString(Some(bytes)), end))
}

fn parse_array(input: &[u8], start: usize) -> Result<(RespFrame, usize), RespParseError> {
    let (line, mut cursor) = read_line(input, start)?;
    let text = std::str::from_utf8(line).map_err(|_| RespParseError::InvalidUtf8)?;
    let len = text
        .parse::<i64>()
        .map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if len == -1 {
        return Ok((RespFrame::Array(None), cursor));
    }
    if len < -1 {
        return Err(RespParseError::InvalidMultibulkLength);
    }
    let count = usize::try_from(len).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let (item, consumed) = parse_frame_internal(input, cursor)?;
        items.push(item);
        cursor = consumed;
    }
    Ok((RespFrame::Array(Some(items)), cursor))
}

fn read_line(input: &[u8], start: usize) -> Result<(&[u8], usize), RespParseError> {
    if start >= input.len() {
        return Err(RespParseError::Incomplete);
    }
    let mut i = start;
    while i + 1 < input.len() {
        if input[i] == b'\r' && input[i + 1] == b'\n' {
            return Ok((&input[start..i], i + 2));
        }
        i += 1;
    }
    Err(RespParseError::Incomplete)
}

#[cfg(test)]
mod tests {
    use super::{RespFrame, parse_frame};

    #[test]
    fn round_trip_simple_array() {
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PING".to_vec())),
            RespFrame::BulkString(Some(b"hello".to_vec())),
        ]));
        let encoded = frame.to_bytes();
        let parsed = parse_frame(&encoded).expect("must parse");
        assert_eq!(parsed.frame, frame);
        assert_eq!(parsed.consumed, encoded.len());
    }

    #[test]
    fn parse_nested_array() {
        let input = b"*2\r\n*1\r\n+OK\r\n:42\r\n";
        let parsed = parse_frame(input).expect("must parse nested array");
        let expected = RespFrame::Array(Some(vec![
            RespFrame::Array(Some(vec![RespFrame::SimpleString("OK".to_string())])),
            RespFrame::Integer(42),
        ]));
        assert_eq!(parsed.frame, expected);
    }

    #[test]
    fn parse_incomplete_frame() {
        let input = b"$3\r\nab";
        let err = parse_frame(input).expect_err("must fail");
        assert_eq!(err.to_string(), "incomplete frame");
    }

    #[test]
    fn parse_invalid_lengths() {
        let bulk = parse_frame(b"$-2\r\n").expect_err("must fail");
        assert_eq!(bulk.to_string(), "invalid bulk length");

        let mbulk = parse_frame(b"*-2\r\n").expect_err("must fail");
        assert_eq!(mbulk.to_string(), "invalid multibulk length");
    }
}
