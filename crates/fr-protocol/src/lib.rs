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
    Map(Option<Vec<(RespFrame, RespFrame)>>),
    Push(Vec<RespFrame>),
    Sequence(Vec<RespFrame>),
    /// RESP3 Double type (`,value\r\n`). Stores string representation to allow Eq derive.
    Double(String),
    /// RESP3 Set type (`~count\r\n` followed by elements).
    Set(Option<Vec<RespFrame>>),
    /// RESP3 Verbatim string (`=<len>\r\ntxt:<body>\r\n`). Used for INFO, CLIENT INFO/LIST,
    /// LOLWUT, LATENCY DOCTOR, MEMORY DOCTOR under HELLO 3.
    Verbatim(String),
    /// RESP3 Big Number (`(value\r\n`). Used by the Lua `{big_number=...}`
    /// reply hint; downconverts to a bulk string under RESP2. (frankenredis-h2uga)
    BigNumber(String),
}

/// Sanitize bytes destined for an inline RESP frame body (`SimpleString`
/// or `Error`). RESP inline frames are terminated by the first `\r\n`,
/// so any embedded `\r` or `\n` in the payload would split the frame
/// and let bytes after the split be re-parsed by the peer as a separate
/// reply — a CRLF-injection / frame-smuggling primitive when the body
/// is built from user-controlled input via `format!()`. Mirrors
/// upstream Redis' `_addReplyErrorFormat` which does
/// `sdsmapchars(s, "\r\n", "  ", 2)` for the same reason.
///
/// Used both for encoding (output) and parsing (input) to ensure
/// roundtrip consistency: `parse(encode(frame)) == frame`.
fn sanitize_inline_body(s: &str) -> String {
    if !s.bytes().any(|b| b == b'\r' || b == b'\n') {
        return s.to_owned();
    }
    s.chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect()
}

/// Fast integer-to-bytes without format machinery. Writes decimal representation
/// of `n` directly into `out`. Avoids the allocation overhead of write!().
fn push_i64(out: &mut Vec<u8>, n: i64) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let (neg, mut val) = if n < 0 {
        (true, (n as i128).unsigned_abs() as u64)
    } else {
        (false, n as u64)
    };
    // Max i64 is 19 digits + sign = 20 bytes
    let mut buf = [0u8; 20];
    let mut pos = 20;
    while val > 0 {
        pos -= 1;
        buf[pos] = b'0' + (val % 10) as u8;
        val /= 10;
    }
    if neg {
        pos -= 1;
        buf[pos] = b'-';
    }
    out.extend_from_slice(&buf[pos..]);
}

/// Fast usize-to-bytes for lengths (always non-negative).
fn push_usize(out: &mut Vec<u8>, n: usize) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut val = n;
    // Max usize on 64-bit is 20 digits
    let mut buf = [0u8; 20];
    let mut pos = 20;
    while val > 0 {
        pos -= 1;
        buf[pos] = b'0' + (val % 10) as u8;
        val /= 10;
    }
    out.extend_from_slice(&buf[pos..]);
}

fn decimal_u64_len(mut n: u64) -> usize {
    let mut len = 1;
    while n >= 10 {
        n /= 10;
        len += 1;
    }
    len
}

fn decimal_usize_len(mut n: usize) -> usize {
    let mut len = 1;
    while n >= 10 {
        n /= 10;
        len += 1;
    }
    len
}

fn decimal_i64_len(n: i64) -> usize {
    if n < 0 {
        1 + decimal_u64_len((n as i128).unsigned_abs() as u64)
    } else {
        decimal_u64_len(n as u64)
    }
}

fn push_inline_sanitized(out: &mut Vec<u8>, body: &[u8]) {
    let needs_sanitize = body.iter().any(|&b| b == b'\r' || b == b'\n');
    if !needs_sanitize {
        out.extend_from_slice(body);
        return;
    }
    out.reserve(body.len());
    for &b in body {
        if b == b'\r' || b == b'\n' {
            out.push(b' ');
        } else {
            out.push(b);
        }
    }
}

impl RespFrame {
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len_hint().unwrap_or(0));
        self.encode_into(&mut out);
        out
    }

    fn encoded_len_hint(&self) -> Option<usize> {
        match self {
            Self::SimpleString(s) | Self::Error(s) | Self::Double(s) | Self::BigNumber(s) => {
                1usize.checked_add(s.len())?.checked_add(2)
            }
            Self::Integer(n) => 1usize.checked_add(decimal_i64_len(*n))?.checked_add(2),
            Self::BulkString(None) => Some(5),
            Self::BulkString(Some(bytes)) => 1usize
                .checked_add(decimal_usize_len(bytes.len()))?
                .checked_add(2)?
                .checked_add(bytes.len())?
                .checked_add(2),
            Self::Array(None) => Some(5),
            Self::Array(Some(frames)) | Self::Set(Some(frames)) => {
                let mut len = 1usize
                    .checked_add(decimal_usize_len(frames.len()))?
                    .checked_add(2)?;
                for frame in frames {
                    len = len.checked_add(frame.encoded_len_hint()?)?;
                }
                Some(len)
            }
            Self::Map(None) => Some(5),
            Self::Map(Some(entries)) => {
                let mut len = 1usize
                    .checked_add(decimal_usize_len(entries.len()))?
                    .checked_add(2)?;
                for (key, value) in entries {
                    len = len.checked_add(key.encoded_len_hint()?)?;
                    len = len.checked_add(value.encoded_len_hint()?)?;
                }
                Some(len)
            }
            Self::Push(frames) => {
                let mut len = 1usize
                    .checked_add(decimal_usize_len(frames.len()))?
                    .checked_add(2)?;
                for frame in frames {
                    len = len.checked_add(frame.encoded_len_hint()?)?;
                }
                Some(len)
            }
            Self::Sequence(frames) => {
                let mut len = 0usize;
                for frame in frames {
                    len = len.checked_add(frame.encoded_len_hint()?)?;
                }
                Some(len)
            }
            Self::Set(None) => Some(5),
            Self::Verbatim(s) => {
                let body_len = s.len().checked_add(4)?;
                1usize
                    .checked_add(decimal_usize_len(body_len))?
                    .checked_add(2)?
                    .checked_add(body_len)?
                    .checked_add(2)
            }
        }
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::SimpleString(s) => {
                out.extend_from_slice(b"+");
                push_inline_sanitized(out, s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::Error(s) => {
                out.extend_from_slice(b"-");
                push_inline_sanitized(out, s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::Integer(n) => {
                out.extend_from_slice(b":");
                push_i64(out, *n);
                out.extend_from_slice(b"\r\n");
            }
            Self::BulkString(None) => out.extend_from_slice(b"$-1\r\n"),
            Self::BulkString(Some(bytes)) => {
                out.extend_from_slice(b"$");
                push_usize(out, bytes.len());
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(bytes);
                out.extend_from_slice(b"\r\n");
            }
            Self::Array(None) => out.extend_from_slice(b"*-1\r\n"),
            Self::Array(Some(frames)) => {
                out.extend_from_slice(b"*");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into(out);
                }
            }
            Self::Map(None) => out.extend_from_slice(b"%-1\r\n"),
            Self::Map(Some(entries)) => {
                out.extend_from_slice(b"%");
                push_usize(out, entries.len());
                out.extend_from_slice(b"\r\n");
                for (key, value) in entries {
                    key.encode_into(out);
                    value.encode_into(out);
                }
            }
            Self::Push(frames) => {
                out.extend_from_slice(b">");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into(out);
                }
            }
            Self::Sequence(frames) => {
                for frame in frames {
                    frame.encode_into(out);
                }
            }
            Self::Double(s) => {
                out.extend_from_slice(b",");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::BigNumber(s) => {
                out.extend_from_slice(b"(");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::Set(None) => out.extend_from_slice(b"~-1\r\n"),
            Self::Set(Some(frames)) => {
                out.extend_from_slice(b"~");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into(out);
                }
            }
            Self::Verbatim(s) => {
                // RESP3 verbatim string: =<len>\r\ntxt:<body>\r\n
                // len includes the "txt:" prefix (4 bytes)
                out.extend_from_slice(b"=");
                push_usize(out, s.len() + 4);
                out.extend_from_slice(b"\r\ntxt:");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
        }
    }

    /// Encode this frame to RESP3 wire bytes. Identical to [`encode_into`]
    /// except that every null reply uses the RESP3 null type `_\r\n` — redis
    /// 7.2 emits `_` for all nulls under `HELLO 3`, never the RESP2 `$-1` /
    /// `*-1` / `~-1` / `%-1`. Container frames recurse through this method so
    /// nested nulls (e.g. `MGET k missing`, the XPENDING summary's null
    /// consumers field) are promoted too. Scalar and already-RESP3 types
    /// (Double, Verbatim, populated Set/Map) encode identically to
    /// [`encode_into`], so this only diverges on the null leaves.
    ///
    /// [`encode_into`]: Self::encode_into
    pub fn encode_into_resp3(&self, out: &mut Vec<u8>) {
        match self {
            Self::BulkString(None) | Self::Array(None) | Self::Map(None) | Self::Set(None) => {
                out.extend_from_slice(b"_\r\n");
            }
            Self::Array(Some(frames)) => {
                out.extend_from_slice(b"*");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into_resp3(out);
                }
            }
            Self::Map(Some(entries)) => {
                out.extend_from_slice(b"%");
                push_usize(out, entries.len());
                out.extend_from_slice(b"\r\n");
                for (key, value) in entries {
                    key.encode_into_resp3(out);
                    value.encode_into_resp3(out);
                }
            }
            Self::Set(Some(frames)) => {
                out.extend_from_slice(b"~");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into_resp3(out);
                }
            }
            Self::Push(frames) => {
                out.extend_from_slice(b">");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into_resp3(out);
                }
            }
            Self::Sequence(frames) => {
                for frame in frames {
                    frame.encode_into_resp3(out);
                }
            }
            // Scalars (SimpleString/Error/Integer/Double/Verbatim/populated
            // BulkString) carry no nulls and encode identically.
            _ => self.encode_into(out),
        }
    }

    /// Create a RESP3 Double frame from an f64, formatted exactly as
    /// vendored Redis 7.2.4 `addReplyDouble`/`d2string` would (so RESP3
    /// `,<value>\r\n` is byte-identical to upstream). (frankenredis-sk4ss)
    #[must_use]
    pub fn double_from_f64(v: f64) -> Self {
        Self::Double(format_redis_double(v))
    }
}

/// Format an f64 exactly as vendored Redis 7.2.4 util.c::d2string does,
/// the canonical conversion behind ZSCORE / ZADD INCR / GEODIST and the
/// RESP3 Double type. d2string special-cases nan/inf/±0, fast-paths
/// exact integers (double2ll → ll2string), and otherwise emits the
/// Grisu shortest-roundtrip form via deps/fpconv/fpconv_dtoa.c
/// ::emit_digits. Rust's `{:e}` yields the same shortest significant
/// digits and base-10 exponent that fpconv derives, so we re-lay them
/// out with fpconv's exact fixed-vs-scientific rules — keyed on K (the
/// power of the LAST digit), not on the leading exponent. Verified
/// byte-exact against the oracle across 437 magnitudes. (frankenredis-sk4ss)
#[must_use]
pub fn format_redis_double(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_string();
    }

    // Exact-integer fast path (double2ll → ll2string).
    if value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        let truncated = value as i64;
        if truncated as f64 == value {
            return truncated.to_string();
        }
    }

    // fpconv_dtoa::emit_digits, driven by Rust's shortest-roundtrip {:e}.
    let sci = format!("{value:e}");
    let (mantissa, exp_str) = sci.split_once('e').expect("{:e} always emits 'e'");
    let scientific_exp: i32 = exp_str.parse().expect("{:e} exponent is an integer");
    let neg = mantissa.starts_with('-');
    let digits: String = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(char::from)
        .collect();
    let ndigits = digits.len() as i32;
    let k = scientific_exp - (ndigits - 1);
    let exp_abs = scientific_exp.abs();

    let mut out = String::with_capacity(digits.len() + 8);
    if neg {
        out.push('-');
    }
    if k >= 0 && exp_abs < ndigits + 7 {
        out.push_str(&digits);
        for _ in 0..k {
            out.push('0');
        }
    } else if k < 0 && (k > -7 || exp_abs < 4) {
        let offset = ndigits - k.abs();
        if offset <= 0 {
            out.push_str("0.");
            for _ in 0..(-offset) {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            let o = offset as usize;
            out.push_str(&digits[..o]);
            out.push('.');
            out.push_str(&digits[o..]);
        }
    } else {
        out.push_str(&digits[..1]);
        if ndigits > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if scientific_exp < 0 { '-' } else { '+' });
        out.push_str(&exp_abs.to_string());
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseResult {
    pub frame: RespFrame,
    pub consumed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorrowedCommandParseResult<'a> {
    pub frame: BorrowedCommandFrame<'a>,
    pub consumed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BorrowedCommandFrame<'a> {
    NullArray,
    Arguments(Vec<&'a [u8]>),
    Owned(RespFrame),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorrowedCommandArgsParseResult {
    pub kind: BorrowedCommandArgsKind,
    pub consumed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorrowedCommandArgsKind {
    NullArray,
    Arguments,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParserConfig {
    pub max_bulk_len: usize,
    pub max_array_len: usize,
    pub max_recursion_depth: usize,
    /// Opt-in RESP3 reply parsing. The default (`false`) keeps the
    /// fail-closed posture codified by
    /// `fr_p2c_002_u007_resp3_fail_closed_prefix_matrix`: any RESP3
    /// type prefix on untrusted input is rejected with
    /// `UnsupportedResp3Type`. Trusted callers (e.g. the live-oracle
    /// harness reading replies from the vendored redis-server after
    /// `HELLO 3`) flip this to `true` so RESP3 frames downgrade into
    /// RESP2-equivalent `RespFrame` shapes — map → flat Array of 2N
    /// field/value entries, set/push → Array, bool → Integer, double
    /// / big-number → BulkString of the ASCII form, null → BulkString
    /// None, verbatim → BulkString (prefix stripped), attribute →
    /// peeled before parsing the next real frame, blob-error → Error.
    /// (br-frankenredis-ozcx)
    pub allow_resp3: bool,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            max_bulk_len: 512 * 1024 * 1024, // 512 MiB default (Redis standard)
            max_array_len: 1024 * 1024,      // 1M elements
            max_recursion_depth: 128,
            allow_resp3: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespParseError {
    Incomplete,
    InvalidPrefix(u8),
    UnsupportedResp3Type(u8),
    InvalidInteger,
    InvalidBulkLength,
    InvalidMultibulkLength,
    InvalidUtf8,
    BulkLengthTooLarge,
    MultibulkLengthTooLarge,
    RecursionLimitExceeded,
    LineTooLong,
    /// (frankenredis-5qqv1) A command multibulk element was not a `$` bulk
    /// string. Carries the offending type byte for the upstream wording
    /// "expected '$', got 'X'".
    ExpectedBulk(u8),
}

impl Display for RespParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete => write!(f, "incomplete frame"),
            Self::InvalidPrefix(ch) => write!(f, "invalid RESP type prefix: {}", char::from(*ch)),
            Self::UnsupportedResp3Type(ch) => {
                write!(f, "unsupported RESP3 type prefix: {}", char::from(*ch))
            }
            Self::InvalidInteger => write!(f, "invalid RESP integer"),
            Self::InvalidBulkLength => write!(f, "invalid bulk length"),
            Self::InvalidMultibulkLength => write!(f, "invalid multibulk length"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 payload"),
            // (frankenredis-w7xy8) Upstream networking.c emits the SAME message
            // for malformed and over-limit lengths ("invalid bulk length" /
            // "invalid multibulk length"), so a client sees identical wording
            // whether it sent `$abc` or `$<huge>`.
            Self::BulkLengthTooLarge => write!(f, "invalid bulk length"),
            Self::MultibulkLengthTooLarge => write!(f, "invalid multibulk length"),
            Self::RecursionLimitExceeded => write!(f, "nested array depth limit exceeded"),
            Self::LineTooLong => write!(f, "RESP line too long"),
            Self::ExpectedBulk(got) => write!(f, "expected '$', got '{}'", char::from(*got)),
        }
    }
}

impl Error for RespParseError {}

pub fn parse_frame(input: &[u8]) -> Result<ParseResult, RespParseError> {
    parse_frame_with_config(input, &ParserConfig::default())
}

pub fn parse_frame_with_config(
    input: &[u8],
    config: &ParserConfig,
) -> Result<ParseResult, RespParseError> {
    let (frame, consumed) = parse_frame_internal(input, 0, 0, 0, config)?;
    Ok(ParseResult { frame, consumed })
}

/// Parse a CLIENT → SERVER **command** frame. Unlike [`parse_frame_with_config`]
/// (which accepts any RESP type for array elements — correct for parsing
/// *replies*), every element of a command multibulk must be a non-null bulk
/// string, matching upstream networking.c::processMultibulkBuffer. A non-`$`
/// element yields `ExpectedBulk(byte)` ("expected '$', got 'X'") and a `$-1`
/// null argument yields `InvalidBulkLength`. (frankenredis-5qqv1)
///
/// Non-multibulk input (e.g. an inline command, which never reaches here)
/// falls through to the generic parser.
pub fn parse_command_frame(
    input: &[u8],
    config: &ParserConfig,
) -> Result<ParseResult, RespParseError> {
    if input.first() != Some(&b'*') {
        return parse_frame_with_config(input, config);
    }
    let (line, mut cursor) = read_line(input, 1)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if len == -1 {
        return Ok(ParseResult {
            frame: RespFrame::Array(None),
            consumed: cursor,
        });
    }
    if len < -1 {
        return Err(RespParseError::InvalidMultibulkLength);
    }
    let count = usize::try_from(len).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if count > config.max_array_len {
        return Err(RespParseError::MultibulkLengthTooLarge);
    }
    let mut items = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        match input.get(cursor) {
            None => return Err(RespParseError::Incomplete),
            Some(&b'$') => {}
            Some(&other) => return Err(RespParseError::ExpectedBulk(other)),
        }
        let (item, consumed) = parse_bulk(input, cursor + 1, config)?;
        // A `$-1` null bulk is a valid *reply* but never a command argument.
        if matches!(item, RespFrame::BulkString(None)) {
            return Err(RespParseError::InvalidBulkLength);
        }
        items.push(item);
        cursor = consumed;
    }
    Ok(ParseResult {
        frame: RespFrame::Array(Some(items)),
        consumed: cursor,
    })
}

/// Parse a CLIENT -> SERVER command frame while borrowing multibulk arguments
/// from `input`.
///
/// This mirrors [`parse_command_frame`] for validation and error behavior, but
/// the normal `*N\r\n$...\r\n` command path returns `&[u8]` argument slices
/// instead of allocating one `Vec<u8>` per bulk string. Non-multibulk input
/// falls back to the generic owned parser for the same reason
/// [`parse_command_frame`] does: callers may still be handling a non-command
/// RESP frame on a shared parsing path.
pub fn parse_command_frame_borrowed<'a>(
    input: &'a [u8],
    config: &ParserConfig,
) -> Result<BorrowedCommandParseResult<'a>, RespParseError> {
    if input.first() != Some(&b'*') {
        let parsed = parse_frame_with_config(input, config)?;
        return Ok(BorrowedCommandParseResult {
            frame: BorrowedCommandFrame::Owned(parsed.frame),
            consumed: parsed.consumed,
        });
    }
    let mut args = Vec::new();
    let parsed = parse_command_args_borrowed_into(input, config, &mut args)?;
    let frame = match parsed.kind {
        BorrowedCommandArgsKind::NullArray => BorrowedCommandFrame::NullArray,
        BorrowedCommandArgsKind::Arguments => BorrowedCommandFrame::Arguments(args),
    };
    Ok(BorrowedCommandParseResult {
        frame,
        consumed: parsed.consumed,
    })
}

/// Parse a strict RESP multibulk command into a caller-reused borrowed argv
/// buffer. This is the allocation-minimal primitive for future hot-path
/// runtime wiring; it is multibulk-only, so non-`*` input is rejected instead
/// of using the owned fallback in [`parse_command_frame_borrowed`].
pub fn parse_command_args_borrowed_into<'a>(
    input: &'a [u8],
    config: &ParserConfig,
    args: &mut Vec<&'a [u8]>,
) -> Result<BorrowedCommandArgsParseResult, RespParseError> {
    args.clear();
    match parse_command_args_borrowed_into_inner(input, config, args) {
        Ok(parsed) => Ok(parsed),
        Err(err) => {
            args.clear();
            Err(err)
        }
    }
}

fn parse_command_args_borrowed_into_inner<'a>(
    input: &'a [u8],
    config: &ParserConfig,
    args: &mut Vec<&'a [u8]>,
) -> Result<BorrowedCommandArgsParseResult, RespParseError> {
    match input.first() {
        Some(b'*') => {}
        Some(&other) => return Err(RespParseError::InvalidPrefix(other)),
        None => return Err(RespParseError::Incomplete),
    }
    let (line, mut cursor) = read_line(input, 1)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if len == -1 {
        return Ok(BorrowedCommandArgsParseResult {
            kind: BorrowedCommandArgsKind::NullArray,
            consumed: cursor,
        });
    }
    if len < -1 {
        return Err(RespParseError::InvalidMultibulkLength);
    }
    let count = usize::try_from(len).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if count > config.max_array_len {
        return Err(RespParseError::MultibulkLengthTooLarge);
    }
    let reserve = count.min(1024);
    if args.capacity() < reserve {
        args.reserve(reserve - args.capacity());
    }
    for _ in 0..count {
        match input.get(cursor) {
            None => return Err(RespParseError::Incomplete),
            Some(&b'$') => {}
            Some(&other) => return Err(RespParseError::ExpectedBulk(other)),
        }
        let (arg, consumed) = parse_bulk_slice(input, cursor + 1, config)?;
        let Some(arg) = arg else {
            return Err(RespParseError::InvalidBulkLength);
        };
        args.push(arg);
        cursor = consumed;
    }
    Ok(BorrowedCommandArgsParseResult {
        kind: BorrowedCommandArgsKind::Arguments,
        consumed: cursor,
    })
}

/// Maximum number of consecutive RESP3 attribute prefixes ('|...')
/// the parser will follow before returning RecursionLimitExceeded.
/// Independent of `max_recursion_depth` because the attribute branch
/// is depth-transparent (xfxtd) — without a separate cap an attacker
/// could send '|N\r\n+k\r\n+v\r\n' × M as untrusted input and grow
/// the Rust call stack linearly with M, regardless of the recursion-
/// depth budget. (frankenredis-oafun)
const RESP3_ATTRIBUTE_CHAIN_LIMIT: usize = 8;

fn parse_frame_internal(
    input: &[u8],
    start: usize,
    depth: usize,
    attr_chain_depth: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    if depth > config.max_recursion_depth {
        return Err(RespParseError::RecursionLimitExceeded);
    }
    if attr_chain_depth > RESP3_ATTRIBUTE_CHAIN_LIMIT {
        return Err(RespParseError::RecursionLimitExceeded);
    }
    let prefix = *input.get(start).ok_or(RespParseError::Incomplete)?;
    let next = start + 1;
    match prefix {
        b'+' => {
            let (line, consumed) = read_line(input, next)?;
            let raw = std::str::from_utf8(line).map_err(|_| RespParseError::InvalidUtf8)?;
            let text = sanitize_inline_body(raw);
            Ok((RespFrame::SimpleString(text), consumed))
        }
        b'-' => {
            let (line, consumed) = read_line(input, next)?;
            let raw = std::str::from_utf8(line).map_err(|_| RespParseError::InvalidUtf8)?;
            let text = sanitize_inline_body(raw);
            Ok((RespFrame::Error(text), consumed))
        }
        b':' => {
            let (line, consumed) = read_line(input, next)?;
            let n = parse_i64_strict(line)?;
            Ok((RespFrame::Integer(n), consumed))
        }
        b'$' => parse_bulk(input, next, config),
        b'*' => parse_array(input, next, depth, config),
        // RESP3 type prefixes. Without `config.allow_resp3`, hard-reject
        // every RESP3 prefix to preserve the fail-closed posture
        // codified by fr_p2c_002_u007_resp3_fail_closed_prefix_matrix.
        // With `allow_resp3`, downgrade each type into a RESP2-shaped
        // RespFrame so downstream RESP2-only code can consume the
        // reply. (br-frankenredis-ozcx)
        b'~' | b'%' | b'#' | b',' | b'_' | b'(' | b'=' | b'|' | b'>' | b'!'
            if !config.allow_resp3 =>
        {
            Err(RespParseError::UnsupportedResp3Type(prefix))
        }
        b'%' => parse_resp3_map(input, next, depth, config),
        b'~' | b'>' => parse_array(input, next, depth, config),
        b'#' => parse_resp3_bool(input, next),
        b',' => {
            // RESP3 double: ',<f64>\r\n'. The value must parse as a
            // double (incl. 'inf', '-inf', 'nan'). Empty / non-
            // numeric payloads are malformed — reject same way
            // ny5fu rejects '_payload\r\n'. (frankenredis-u1xg5)
            let (line, consumed) = read_line(input, next)?;
            let s = std::str::from_utf8(line).map_err(|_| RespParseError::InvalidUtf8)?;
            if s.is_empty() || s.parse::<f64>().is_err() {
                return Err(RespParseError::InvalidInteger);
            }
            Ok((RespFrame::BulkString(Some(s.as_bytes().to_vec())), consumed))
        }
        b'(' => {
            // RESP3 big number: '(<base-10-integer>\r\n'. Body must
            // be all decimal digits with optional leading +/- sign.
            // Empty / non-numeric payloads are malformed.
            // (frankenredis-u1xg5)
            let (line, consumed) = read_line(input, next)?;
            if line.is_empty() {
                return Err(RespParseError::InvalidInteger);
            }
            let digits_start = match line[0] {
                b'+' | b'-' => 1,
                _ => 0,
            };
            if digits_start == line.len()
                || line[digits_start..].iter().any(|b| !b.is_ascii_digit())
            {
                return Err(RespParseError::InvalidInteger);
            }
            let s = std::str::from_utf8(line)
                .map_err(|_| RespParseError::InvalidUtf8)?
                .to_string();
            Ok((RespFrame::BulkString(Some(s.into_bytes())), consumed))
        }
        b'_' => {
            let (line, consumed) = read_line(input, next)?;
            if !line.is_empty() {
                return Err(RespParseError::InvalidBulkLength);
            }
            Ok((RespFrame::BulkString(None), consumed))
        }
        b'=' => parse_resp3_verbatim(input, next, config),
        b'|' => {
            // Attribute: parse the attribute map and discard it,
            // then return the next real frame. Upstream uses this
            // to attach per-reply metadata that RESP2 clients don't
            // understand. The attribute is transparent for the
            // depth budget (xfxtd) — but chained attribute prefixes
            // ('|...|...|...frame') must still be capped so an
            // attacker can't grow the Rust call stack at the same
            // depth without bound. (frankenredis-oafun)
            let (_attr, consumed) = parse_resp3_map(input, next, depth, config)?;
            parse_frame_internal(input, consumed, depth, attr_chain_depth + 1, config)
        }
        b'!' => parse_resp3_blob_error(input, next, config),
        other => Err(RespParseError::InvalidPrefix(other)),
    }
}

fn parse_resp3_map(
    input: &[u8],
    start: usize,
    depth: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    let (line, mut cursor) = read_line(input, start)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if len == -1 {
        return Ok((RespFrame::Array(None), cursor));
    }
    if len < 0 {
        return Err(RespParseError::InvalidMultibulkLength);
    }
    let count = usize::try_from(len).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    let pair_count = count
        .checked_mul(2)
        .ok_or(RespParseError::MultibulkLengthTooLarge)?;
    if pair_count > config.max_array_len {
        return Err(RespParseError::MultibulkLengthTooLarge);
    }
    let mut items = Vec::with_capacity(pair_count.min(1024));
    for _ in 0..pair_count {
        // Child frames reset attr_chain_depth: each k/v pair is its
        // own independent frame, not part of the outer attribute
        // chain. (frankenredis-oafun)
        let (item, consumed) = parse_frame_internal(input, cursor, depth + 1, 0, config)?;
        items.push(item);
        cursor = consumed;
    }
    Ok((RespFrame::Array(Some(items)), cursor))
}

fn parse_resp3_bool(input: &[u8], start: usize) -> Result<(RespFrame, usize), RespParseError> {
    let (line, consumed) = read_line(input, start)?;
    let flag = match line {
        b"t" => 1,
        b"f" => 0,
        _ => return Err(RespParseError::InvalidInteger),
    };
    Ok((RespFrame::Integer(flag), consumed))
}

fn parse_resp3_verbatim(
    input: &[u8],
    start: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    let (line, consumed) = read_line(input, start)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidBulkLength)?;
    if len < 0 {
        return Err(RespParseError::InvalidBulkLength);
    }
    let data_len = usize::try_from(len).map_err(|_| RespParseError::InvalidBulkLength)?;
    if data_len > config.max_bulk_len {
        return Err(RespParseError::BulkLengthTooLarge);
    }
    let end = consumed
        .checked_add(data_len)
        .and_then(|idx| idx.checked_add(2))
        .ok_or(RespParseError::Incomplete)?;
    if input.len() < end {
        return Err(RespParseError::Incomplete);
    }
    if input[consumed + data_len] != b'\r' || input[consumed + data_len + 1] != b'\n' {
        return Err(RespParseError::InvalidBulkLength);
    }
    // Verbatim string body is `<3-char-type>:<payload>`. The format
    // tag is mandatory even when the payload is empty.
    let body = &input[consumed..consumed + data_len];
    if body.len() < 4 || body[3] != b':' {
        return Err(RespParseError::InvalidBulkLength);
    }
    Ok((RespFrame::BulkString(Some(body[4..].to_vec())), end))
}

fn parse_resp3_blob_error(
    input: &[u8],
    start: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    let (line, consumed) = read_line(input, start)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidBulkLength)?;
    if len < 0 {
        return Err(RespParseError::InvalidBulkLength);
    }
    let data_len = usize::try_from(len).map_err(|_| RespParseError::InvalidBulkLength)?;
    if data_len > config.max_bulk_len {
        return Err(RespParseError::BulkLengthTooLarge);
    }
    let end = consumed
        .checked_add(data_len)
        .and_then(|idx| idx.checked_add(2))
        .ok_or(RespParseError::Incomplete)?;
    if input.len() < end {
        return Err(RespParseError::Incomplete);
    }
    if input[consumed + data_len] != b'\r' || input[consumed + data_len + 1] != b'\n' {
        return Err(RespParseError::InvalidBulkLength);
    }
    let text = std::str::from_utf8(&input[consumed..consumed + data_len])
        .map(str::to_owned)
        .map_err(|_| RespParseError::InvalidUtf8)?;
    Ok((RespFrame::Error(text), end))
}

fn parse_bulk(
    input: &[u8],
    start: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    let (line, consumed) = read_line(input, start)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidBulkLength)?;
    if len == -1 {
        return Ok((RespFrame::BulkString(None), consumed));
    }
    if len < -1 {
        return Err(RespParseError::InvalidBulkLength);
    }
    let data_len = usize::try_from(len).map_err(|_| RespParseError::InvalidBulkLength)?;
    if data_len > config.max_bulk_len {
        return Err(RespParseError::BulkLengthTooLarge);
    }
    let end = consumed
        .checked_add(data_len)
        .and_then(|idx| idx.checked_add(2))
        .ok_or(RespParseError::Incomplete)?;
    if input.len() < end {
        return Err(RespParseError::Incomplete);
    }
    if input[consumed + data_len] != b'\r' || input[consumed + data_len + 1] != b'\n' {
        return Err(RespParseError::InvalidBulkLength);
    }
    let bytes = input[consumed..consumed + data_len].to_vec();
    Ok((RespFrame::BulkString(Some(bytes)), end))
}

fn parse_bulk_slice<'a>(
    input: &'a [u8],
    start: usize,
    config: &ParserConfig,
) -> Result<(Option<&'a [u8]>, usize), RespParseError> {
    let (line, consumed) = read_line(input, start)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidBulkLength)?;
    if len == -1 {
        return Ok((None, consumed));
    }
    if len < -1 {
        return Err(RespParseError::InvalidBulkLength);
    }
    let data_len = usize::try_from(len).map_err(|_| RespParseError::InvalidBulkLength)?;
    if data_len > config.max_bulk_len {
        return Err(RespParseError::BulkLengthTooLarge);
    }
    let end = consumed
        .checked_add(data_len)
        .and_then(|idx| idx.checked_add(2))
        .ok_or(RespParseError::Incomplete)?;
    if input.len() < end {
        return Err(RespParseError::Incomplete);
    }
    if input[consumed + data_len] != b'\r' || input[consumed + data_len + 1] != b'\n' {
        return Err(RespParseError::InvalidBulkLength);
    }
    Ok((Some(&input[consumed..consumed + data_len]), end))
}

fn parse_array(
    input: &[u8],
    start: usize,
    depth: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    let (line, mut cursor) = read_line(input, start)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if len == -1 {
        return Ok((RespFrame::Array(None), cursor));
    }
    if len < -1 {
        return Err(RespParseError::InvalidMultibulkLength);
    }
    let count = usize::try_from(len).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if count > config.max_array_len {
        return Err(RespParseError::MultibulkLengthTooLarge);
    }
    let mut items = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        // Child frames reset attr_chain_depth — each array element
        // is its own independent frame. (frankenredis-oafun)
        let (item, consumed) = parse_frame_internal(input, cursor, depth + 1, 0, config)?;
        items.push(item);
        cursor = consumed;
    }
    Ok((RespFrame::Array(Some(items)), cursor))
}

const MAX_LINE_LENGTH: usize = 64 * 1024; // 64 KiB

fn parse_i64_strict(input: &[u8]) -> Result<i64, RespParseError> {
    let slen = input.len();
    if slen == 0 || slen > 20 {
        return Err(RespParseError::InvalidInteger);
    }
    if slen == 1 && input[0] == b'0' {
        return Ok(0);
    }

    let mut p = 0;
    let negative = input[0] == b'-';
    if negative {
        p += 1;
        if p == slen {
            return Err(RespParseError::InvalidInteger);
        }
    }

    if input[p] >= b'1' && input[p] <= b'9' {
        let mut v: u64 = (input[p] - b'0') as u64;
        p += 1;
        while p < slen {
            let b = input[p];
            if b.is_ascii_digit() {
                if v > (u64::MAX / 10) {
                    return Err(RespParseError::InvalidInteger);
                }
                v *= 10;
                let digit = (b - b'0') as u64;
                if v > (u64::MAX - digit) {
                    return Err(RespParseError::InvalidInteger);
                }
                v += digit;
                p += 1;
            } else {
                return Err(RespParseError::InvalidInteger);
            }
        }

        if negative {
            let limit = (i64::MIN as u64).wrapping_neg();
            if v > limit {
                return Err(RespParseError::InvalidInteger);
            }
            return Ok(v.wrapping_neg() as i64);
        } else {
            if v > i64::MAX as u64 {
                return Err(RespParseError::InvalidInteger);
            }
            return Ok(v as i64);
        }
    }

    Err(RespParseError::InvalidInteger)
}

fn read_line(input: &[u8], start: usize) -> Result<(&[u8], usize), RespParseError> {
    if start >= input.len() {
        return Err(RespParseError::Incomplete);
    }
    let max_line_end = start.saturating_add(MAX_LINE_LENGTH);
    let mut i = start;
    while i + 1 < input.len() {
        if input[i] == b'\r' && input[i + 1] == b'\n' {
            return Ok((&input[start..i], i + 2));
        }
        i += 1;
        if i > max_line_end {
            return Err(RespParseError::LineTooLong);
        }
    }
    Err(RespParseError::Incomplete)
}

#[cfg(test)]
mod tests {
    use super::{
        BorrowedCommandArgsKind, BorrowedCommandFrame, MAX_LINE_LENGTH, ParserConfig, RespFrame,
        RespParseError, format_redis_double, parse_command_args_borrowed_into, parse_command_frame,
        parse_command_frame_borrowed, parse_frame, parse_frame_with_config,
    };

    #[test]
    fn parse_command_frame_requires_bulk_elements() {
        // (frankenredis-5qqv1) A command multibulk's elements must each be a
        // non-null bulk string, matching upstream processMultibulkBuffer.
        let cfg = ParserConfig::default();

        // Valid command parses normally.
        let ok = parse_command_frame(b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n", &cfg).unwrap();
        assert_eq!(
            ok.frame,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"GET".to_vec())),
                RespFrame::BulkString(Some(b"k".to_vec())),
            ]))
        );

        // Non-`$` element -> ExpectedBulk carrying the offending type byte.
        for (input, got) in [
            (&b"*1\r\n+PING\r\n"[..], b'+'),
            (b"*1\r\n:5\r\n", b':'),
            (b"*1\r\n*0\r\n", b'*'),
        ] {
            assert_eq!(
                parse_command_frame(input, &cfg).unwrap_err(),
                RespParseError::ExpectedBulk(got),
                "input {input:?}"
            );
        }

        // A `$-1` null bulk is a valid reply but never a command argument.
        assert_eq!(
            parse_command_frame(b"*2\r\n$3\r\nGET\r\n$-1\r\n", &cfg).unwrap_err(),
            RespParseError::InvalidBulkLength
        );

        // `expected '$', got 'X'` wording matches upstream.
        assert_eq!(
            RespParseError::ExpectedBulk(b'+').to_string(),
            "expected '$', got '+'"
        );
    }

    #[test]
    fn parse_command_frame_borrowed_matches_owned_command_parser() {
        let cfg = ParserConfig::default();
        let input = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n+tail\r\n";
        let owned = parse_command_frame(input, &cfg).expect("owned command parses");
        let borrowed = parse_command_frame_borrowed(input, &cfg).expect("borrowed command parses");

        assert_eq!(borrowed.consumed, owned.consumed);
        assert_eq!(borrowed.consumed, input.len() - b"+tail\r\n".len());
        assert_eq!(
            borrowed.frame,
            BorrowedCommandFrame::Arguments(vec![
                b"SET".as_slice(),
                b"key".as_slice(),
                b"value".as_slice(),
            ])
        );
    }

    #[test]
    fn parse_command_frame_borrowed_preserves_empty_and_null_multibulks() {
        let cfg = ParserConfig::default();

        let empty = parse_command_frame_borrowed(b"*0\r\n", &cfg).expect("empty array parses");
        assert_eq!(empty.consumed, 4);
        assert_eq!(empty.frame, BorrowedCommandFrame::Arguments(Vec::new()));

        let null = parse_command_frame_borrowed(b"*-1\r\n", &cfg).expect("null array parses");
        assert_eq!(null.consumed, 5);
        assert_eq!(null.frame, BorrowedCommandFrame::NullArray);
    }

    #[test]
    fn parse_command_frame_borrowed_preserves_fallback_and_error_semantics() {
        let cfg = ParserConfig::default();

        let simple = parse_command_frame_borrowed(b"+OK\r\n", &cfg).expect("fallback parses");
        assert_eq!(simple.consumed, 5);
        assert_eq!(
            simple.frame,
            BorrowedCommandFrame::Owned(RespFrame::SimpleString("OK".to_string()))
        );

        for (input, expected) in [
            (&b"*1\r\n+PING\r\n"[..], RespParseError::ExpectedBulk(b'+')),
            (&b"*1\r\n:5\r\n"[..], RespParseError::ExpectedBulk(b':')),
            (&b"*1\r\n*0\r\n"[..], RespParseError::ExpectedBulk(b'*')),
            (
                &b"*2\r\n$3\r\nGET\r\n$-1\r\n"[..],
                RespParseError::InvalidBulkLength,
            ),
            (
                &b"*2\r\n$3\r\nGET\r\n$4\r\nkey\n"[..],
                RespParseError::Incomplete,
            ),
        ] {
            assert_eq!(
                parse_command_frame_borrowed(input, &cfg).unwrap_err(),
                expected,
                "input {input:?}"
            );
            assert_eq!(
                parse_command_frame(input, &cfg).unwrap_err(),
                expected,
                "owned input {input:?}"
            );
        }
    }

    #[test]
    fn parse_command_args_borrowed_into_reuses_buffer_and_clears_on_error() {
        let cfg = ParserConfig::default();
        let mut args = Vec::with_capacity(8);
        let capacity = args.capacity();
        let input = b"*3\r\n$3\r\nSET\r\n$0\r\n\r\n$6\r\n\x00\xff\r\nzz\r\n";

        let parsed =
            parse_command_args_borrowed_into(input, &cfg, &mut args).expect("borrowed parses");
        assert_eq!(parsed.kind, BorrowedCommandArgsKind::Arguments);
        assert_eq!(parsed.consumed, input.len());
        assert_eq!(
            args,
            vec![
                b"SET".as_slice(),
                b"".as_slice(),
                b"\x00\xff\r\nzz".as_slice()
            ]
        );
        assert_eq!(args.capacity(), capacity);

        let err = parse_command_args_borrowed_into(b"*1\r\n+PING\r\n", &cfg, &mut args)
            .expect_err("non-bulk arg must reject");
        assert_eq!(err, RespParseError::ExpectedBulk(b'+'));
        assert!(args.is_empty());
    }

    #[test]
    fn parse_command_args_borrowed_into_preserves_multibulk_error_semantics() {
        let cfg = ParserConfig::default();
        let allow_resp3 = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };
        let cases = [
            &b"*1\r\n+PING\r\n"[..],
            &b"*1\r\n:5\r\n"[..],
            &b"*1\r\n_ \r\n"[..],
            &b"*1\r\n#t\r\n"[..],
            &b"*1\r\n*0\r\n"[..],
            &b"*2\r\n$3\r\nGET\r\n$-1\r\n"[..],
            &b"*-2\r\n"[..],
            &b"*x\r\n"[..],
            &b"*1\r\n$3\r\nfo"[..],
            &b"*1\r\n$3\r\nfoo\rx"[..],
            &b"*1\r\n$01\r\nx\r\n"[..],
            &b"*1\r\n$-0\r\n"[..],
        ];
        let mut args = Vec::new();
        for input in cases {
            assert_eq!(
                parse_command_args_borrowed_into(input, &cfg, &mut args).unwrap_err(),
                parse_command_frame(input, &cfg).unwrap_err(),
                "default config input {input:?}"
            );
            assert!(
                args.is_empty(),
                "default config left stale args for {input:?}"
            );
            assert_eq!(
                parse_command_args_borrowed_into(input, &allow_resp3, &mut args).unwrap_err(),
                parse_command_frame(input, &allow_resp3).unwrap_err(),
                "allow_resp3 input {input:?}"
            );
            assert!(args.is_empty(), "allow_resp3 left stale args for {input:?}");
        }
    }

    #[test]
    fn parse_command_args_borrowed_into_preserves_config_limits() {
        let mut args = Vec::new();
        let bulk_limited = ParserConfig {
            max_bulk_len: 4,
            ..ParserConfig::default()
        };
        let at_limit =
            parse_command_args_borrowed_into(b"*1\r\n$4\r\nxxxx\r\n", &bulk_limited, &mut args)
                .expect("bulk at max_bulk_len parses");
        assert_eq!(at_limit.kind, BorrowedCommandArgsKind::Arguments);
        assert_eq!(args, vec![b"xxxx".as_slice()]);
        assert_eq!(
            parse_command_args_borrowed_into(b"*1\r\n$5\r\nxxxxx\r\n", &bulk_limited, &mut args)
                .unwrap_err(),
            RespParseError::BulkLengthTooLarge
        );

        let array_limited = ParserConfig {
            max_array_len: 1,
            ..ParserConfig::default()
        };
        assert_eq!(
            parse_command_args_borrowed_into(
                b"*2\r\n$1\r\na\r\n$1\r\nb\r\n",
                &array_limited,
                &mut args
            )
            .unwrap_err(),
            RespParseError::MultibulkLengthTooLarge
        );
    }

    #[test]
    fn parse_command_args_borrowed_into_preserves_null_and_empty_array_status() {
        let cfg = ParserConfig::default();
        let mut args = Vec::new();

        let empty = parse_command_args_borrowed_into(b"*0\r\n", &cfg, &mut args)
            .expect("empty array parses");
        assert_eq!(empty.kind, BorrowedCommandArgsKind::Arguments);
        assert_eq!(empty.consumed, 4);
        assert!(args.is_empty());

        let null = parse_command_args_borrowed_into(b"*-1\r\n", &cfg, &mut args)
            .expect("null array parses");
        assert_eq!(null.kind, BorrowedCommandArgsKind::NullArray);
        assert_eq!(null.consumed, 5);
        assert!(args.is_empty());
    }

    const PACKET_ID: &str = "FR-P2C-002";
    const SCHEMA_VERSION: &str = "fr_testlog_v1";
    const ARTIFACT_REFS: [&str; 4] = [
        "TEST_LOG_SCHEMA_V1.md",
        "crates/fr-conformance/fixtures/phase2c/FR-P2C-002/contract_table.md",
        "crates/fr-conformance/fixtures/phase2c/FR-P2C-002/risk_note.md",
        "crates/fr-conformance/fixtures/log_contract_v1/env.json",
    ];

    #[derive(Debug)]
    struct StructuredTestLogEvent {
        schema_version: String,
        ts_utc: String,
        suite_id: String,
        test_or_scenario_id: String,
        packet_id: String,
        mode: String,
        verification_path: String,
        seed: u64,
        input_digest: String,
        output_digest: String,
        duration_ms: u64,
        outcome: String,
        reason_code: String,
        replay_cmd: String,
        artifact_refs: Vec<String>,
        fixture_id: Option<String>,
        env_ref: Option<String>,
    }

    impl StructuredTestLogEvent {
        fn assert_schema_contract(&self) {
            assert_eq!(self.schema_version, SCHEMA_VERSION);
            assert_eq!(self.packet_id, PACKET_ID);
            assert!(!self.ts_utc.is_empty());
            assert!(self.suite_id.starts_with("unit::fr-p2c-002"));
            assert!(self.test_or_scenario_id.starts_with("fr_p2c_002_"));
            assert_eq!(self.mode, "strict");
            assert!(matches!(
                self.verification_path.as_str(),
                "unit" | "property"
            ));
            assert!(self.seed > 0);
            assert!(!self.input_digest.is_empty());
            assert!(!self.output_digest.is_empty());
            assert!(self.duration_ms > 0);
            assert_eq!(self.outcome, "pass");
            assert!(!self.reason_code.is_empty());
            assert!(self.replay_cmd.contains("cargo test -p fr-protocol"));
            assert!(self.replay_cmd.contains(&self.test_or_scenario_id));
            assert!(!self.artifact_refs.is_empty());
            for required in ARTIFACT_REFS {
                assert!(
                    self.artifact_refs
                        .iter()
                        .any(|artifact| artifact == required),
                    "missing required artifact ref: {required}"
                );
            }
            assert_eq!(
                self.fixture_id.as_deref(),
                Some("FR-P2C-002::unit-contract-fixture")
            );
            assert_eq!(
                self.env_ref.as_deref(),
                Some("crates/fr-conformance/fixtures/log_contract_v1/env.json")
            );
        }
    }

    fn stable_digest_hex(bytes: &[u8]) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    }

    fn build_event(
        test_or_scenario_id: &str,
        verification_path: &str,
        seed: u64,
        input_bytes: &[u8],
        output_bytes: &[u8],
        reason_code: &str,
    ) -> StructuredTestLogEvent {
        StructuredTestLogEvent {
            schema_version: SCHEMA_VERSION.to_string(),
            ts_utc: "2026-02-16T00:00:00Z".to_string(),
            suite_id: "unit::fr-p2c-002".to_string(),
            test_or_scenario_id: test_or_scenario_id.to_string(),
            packet_id: PACKET_ID.to_string(),
            mode: "strict".to_string(),
            verification_path: verification_path.to_string(),
            seed,
            input_digest: stable_digest_hex(input_bytes),
            output_digest: stable_digest_hex(output_bytes),
            duration_ms: 1,
            outcome: "pass".to_string(),
            reason_code: reason_code.to_string(),
            replay_cmd: format!(
                "FR_MODE=strict FR_SEED={seed} cargo test -p fr-protocol {test_or_scenario_id} -- --nocapture"
            ),
            artifact_refs: ARTIFACT_REFS.into_iter().map(str::to_string).collect(),
            fixture_id: Some("FR-P2C-002::unit-contract-fixture".to_string()),
            env_ref: Some("crates/fr-conformance/fixtures/log_contract_v1/env.json".to_string()),
        }
    }

    fn nested_singleton_array(depth: usize) -> RespFrame {
        let mut frame = RespFrame::Integer(42);
        for _ in 0..depth {
            frame = RespFrame::Array(Some(vec![frame]));
        }
        frame
    }

    #[test]
    fn fr_p2c_002_u001_scalar_decode_parity() {
        let cases = [
            (
                b"+OK\r\n".as_slice(),
                RespFrame::SimpleString("OK".to_string()),
            ),
            (
                b"-ERR boom\r\n".as_slice(),
                RespFrame::Error("ERR boom".to_string()),
            ),
            (b":-42\r\n".as_slice(), RespFrame::Integer(-42)),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for (input, expected) in cases {
            let parsed = parse_frame(input).expect("scalar frame must parse");
            assert_eq!(parsed.frame, expected);
            assert_eq!(parsed.consumed, input.len());
            input_acc.extend_from_slice(input);
            output_acc.extend_from_slice(parsed.frame.to_bytes().as_slice());
        }
        let event = build_event(
            "fr_p2c_002_u001_scalar_decode_parity",
            "unit",
            17,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "parity_ok",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn resp_inline_encoder_sanitizes_crlf_in_body() {
        // CRLF-injection guard: when SimpleString/Error bodies are
        // built via `format!()` from user-controlled bytes (e.g.
        // `RespFrame::Error(format!("ERR Unrecognized option '{attr}'"))`
        // where `attr` comes from a client argv), any embedded `\r`
        // or `\n` would terminate the inline frame early and let the
        // remaining bytes be re-parsed by the peer as a separate
        // reply. The encoder must replace `\r` and `\n` with spaces,
        // matching upstream Redis' `_addReplyErrorFormat`
        // sanitization.
        let injected = RespFrame::Error("ERR x\r\nINJECTED".to_string());
        let bytes = injected.to_bytes();
        assert_eq!(
            bytes, b"-ERR x  INJECTED\r\n",
            "Error body must have \\r and \\n replaced with spaces; otherwise the frame splits"
        );
        // And re-parsing the encoder output round-trips through our
        // own parser as a single Error frame consuming all bytes —
        // proving the injection is neutralized.
        let parsed = parse_frame(&bytes).expect("sanitized frame must parse");
        assert_eq!(parsed.consumed, bytes.len());
        assert_eq!(
            parsed.frame,
            RespFrame::Error("ERR x  INJECTED".to_string())
        );

        let ss = RespFrame::SimpleString("OK\r\nSMUGGLED".to_string());
        let ss_bytes = ss.to_bytes();
        assert_eq!(ss_bytes, b"+OK  SMUGGLED\r\n");
        let parsed_ss = parse_frame(&ss_bytes).expect("sanitized SimpleString must parse");
        assert_eq!(parsed_ss.consumed, ss_bytes.len());

        // Lone \r and lone \n in isolation must also be sanitized —
        // a peer reading bytes off the wire could see a lone \n as a
        // legacy inline-protocol terminator.
        let lone = RespFrame::Error("ERR a\rb\nc".to_string()).to_bytes();
        assert_eq!(lone, b"-ERR a b c\r\n");
    }

    #[test]
    fn resp_integer_rejects_noncanonical_tokens() {
        assert!(matches!(
            parse_frame(b":+1\r\n"),
            Err(RespParseError::InvalidInteger)
        ));
        assert!(matches!(
            parse_frame(b":01\r\n"),
            Err(RespParseError::InvalidInteger)
        ));
        assert!(matches!(
            parse_frame(b":-0\r\n"),
            Err(RespParseError::InvalidInteger)
        ));
    }

    #[test]
    fn resp_bulk_len_rejects_noncanonical_tokens() {
        assert!(matches!(
            parse_frame(b"$+1\r\nx\r\n"),
            Err(RespParseError::InvalidBulkLength)
        ));
        assert!(matches!(
            parse_frame(b"$01\r\nx\r\n"),
            Err(RespParseError::InvalidBulkLength)
        ));
        assert!(matches!(
            parse_frame(b"$-0\r\n"),
            Err(RespParseError::InvalidBulkLength)
        ));
    }

    #[test]
    fn resp_array_len_rejects_noncanonical_tokens() {
        assert!(matches!(
            parse_frame(b"*+1\r\n$1\r\na\r\n"),
            Err(RespParseError::InvalidMultibulkLength)
        ));
        assert!(matches!(
            parse_frame(b"*01\r\n$1\r\na\r\n"),
            Err(RespParseError::InvalidMultibulkLength)
        ));
        assert!(matches!(
            parse_frame(b"*-0\r\n"),
            Err(RespParseError::InvalidMultibulkLength)
        ));
    }

    #[test]
    fn fr_p2c_002_u002_bulk_decode_parity() {
        let cases = [
            (
                b"$5\r\nhello\r\n".as_slice(),
                RespFrame::BulkString(Some(b"hello".to_vec())),
            ),
            (
                b"$4\r\n\x00\xff\x10z\r\n".as_slice(),
                RespFrame::BulkString(Some(vec![0x00, 0xff, 0x10, b'z'])),
            ),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for (input, expected) in cases {
            let parsed = parse_frame(input).expect("bulk frame must parse");
            assert_eq!(parsed.frame, expected);
            assert_eq!(parsed.consumed, input.len());
            input_acc.extend_from_slice(input);
            output_acc.extend_from_slice(parsed.frame.to_bytes().as_slice());
        }
        let event = build_event(
            "fr_p2c_002_u002_bulk_decode_parity",
            "unit",
            19,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "parity_ok",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u003_array_recursion_parity_property() {
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for depth in 0..=8 {
            let frame = nested_singleton_array(depth);
            let encoded = frame.to_bytes();
            let parsed = parse_frame(encoded.as_slice()).expect("recursive array must parse");
            assert_eq!(parsed.frame, frame);
            assert_eq!(parsed.consumed, encoded.len());
            input_acc.extend_from_slice(encoded.as_slice());
            output_acc.extend_from_slice(parsed.frame.to_bytes().as_slice());
        }
        let event = build_event(
            "fr_p2c_002_u003_array_recursion_parity_property",
            "property",
            23,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "parity_ok",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u004_truncated_frame_rejection() {
        let cases = [
            b"+OK\r".as_slice(),
            b"$3\r\nab".as_slice(),
            b"*2\r\n+OK\r\n".as_slice(),
            b":123".as_slice(),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for input in cases {
            let err = parse_frame(input).expect_err("truncated frame must fail");
            assert_eq!(err, RespParseError::Incomplete);
            input_acc.extend_from_slice(input);
            output_acc.extend_from_slice(err.to_string().as_bytes());
        }
        let event = build_event(
            "fr_p2c_002_u004_truncated_frame_rejection",
            "unit",
            29,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.incomplete_frame_detected",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u005_malformed_length_rejection() {
        let cases = [
            (b"$x\r\n".as_slice(), RespParseError::InvalidBulkLength),
            (b"$-2\r\n".as_slice(), RespParseError::InvalidBulkLength),
            (
                b"$9223372036854775808\r\n".as_slice(),
                RespParseError::InvalidBulkLength,
            ),
            (b"*x\r\n".as_slice(), RespParseError::InvalidMultibulkLength),
            (
                b"*-2\r\n".as_slice(),
                RespParseError::InvalidMultibulkLength,
            ),
            (
                b"*9223372036854775808\r\n".as_slice(),
                RespParseError::InvalidMultibulkLength,
            ),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for (input, expected_err) in cases {
            let err = parse_frame(input).expect_err("malformed length must fail");
            assert_eq!(err, expected_err);
            input_acc.extend_from_slice(input);
            output_acc.extend_from_slice(err.to_string().as_bytes());
        }
        let event = build_event(
            "fr_p2c_002_u005_malformed_length_rejection",
            "unit",
            31,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.invalid_length_rejected",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u005_line_length_limit_is_inclusive() {
        let mut ok = Vec::with_capacity(MAX_LINE_LENGTH + 3);
        ok.push(b'+');
        ok.extend(std::iter::repeat_n(b'a', MAX_LINE_LENGTH));
        ok.extend_from_slice(b"\r\n");
        let parsed = parse_frame(ok.as_slice()).expect("line at limit must parse");
        assert_eq!(
            parsed.frame,
            RespFrame::SimpleString("a".repeat(MAX_LINE_LENGTH))
        );

        let mut too_long = Vec::with_capacity(MAX_LINE_LENGTH + 4);
        too_long.push(b'+');
        too_long.extend(std::iter::repeat_n(b'a', MAX_LINE_LENGTH + 1));
        too_long.extend_from_slice(b"\r\n");
        let err = parse_frame(too_long.as_slice()).expect_err("line beyond limit must fail");
        assert_eq!(err, RespParseError::LineTooLong);
    }

    #[test]
    fn fr_p2c_002_u006_invalid_prefix_rejection() {
        let cases = *b"?@/";
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for prefix in cases {
            let input = [prefix, b'\r', b'\n'];
            let err = parse_frame(input.as_slice()).expect_err("unknown prefix must fail");
            assert_eq!(err, RespParseError::InvalidPrefix(prefix));
            input_acc.extend_from_slice(input.as_slice());
            output_acc.extend_from_slice(err.to_string().as_bytes());
        }
        let event = build_event(
            "fr_p2c_002_u006_invalid_prefix_rejection",
            "unit",
            37,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.invalid_prefix_rejected",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u007_resp3_fail_closed_prefix_matrix() {
        let prefixes = *b"~%#,_(=|>!";
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for prefix in prefixes {
            let input = [prefix, b'1', b'\r', b'\n'];
            let err = parse_frame(input.as_slice()).expect_err("unsupported RESP3 must fail");
            assert_eq!(err, RespParseError::UnsupportedResp3Type(prefix));
            input_acc.extend_from_slice(input.as_slice());
            output_acc.extend_from_slice(err.to_string().as_bytes());
        }
        let event = build_event(
            "fr_p2c_002_u007_resp3_fail_closed_prefix_matrix",
            "unit",
            41,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.resp3_unimplemented_fail_closed",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u008_attribute_cursor_alignment_fail_closed() {
        let attr_wrapped = b"|1\r\n+meta\r\n+value\r\n+OK\r\n";
        let err = parse_frame(attr_wrapped).expect_err("attribute wrapper must fail closed");
        assert_eq!(err, RespParseError::UnsupportedResp3Type(b'|'));

        let follow_up = parse_frame(b"+OK\r\n").expect("independent parse remains deterministic");
        assert_eq!(follow_up.frame, RespFrame::SimpleString("OK".to_string()));
        assert_eq!(follow_up.consumed, 5);

        let event = build_event(
            "fr_p2c_002_u008_attribute_cursor_alignment_fail_closed",
            "unit",
            43,
            attr_wrapped,
            err.to_string().as_bytes(),
            "protocol.attribute_cursor_drift",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u009_consumed_length_exactness_property() {
        let frames = [
            RespFrame::SimpleString("OK".to_string()),
            RespFrame::Integer(7),
            RespFrame::BulkString(Some(b"hello".to_vec())),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"PING".to_vec())),
                RespFrame::BulkString(Some(b"payload".to_vec())),
            ])),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for frame in frames {
            let encoded = frame.to_bytes();
            for tail_len in 0..=4 {
                let mut with_tail = encoded.clone();
                with_tail.extend(std::iter::repeat_n(b'X', tail_len));
                let parsed = parse_frame(with_tail.as_slice()).expect("frame with tail must parse");
                assert_eq!(parsed.frame, frame);
                assert_eq!(parsed.consumed, encoded.len());
                input_acc.extend_from_slice(with_tail.as_slice());
                output_acc.extend_from_slice(parsed.frame.to_bytes().as_slice());
            }
        }
        let event = build_event(
            "fr_p2c_002_u009_consumed_length_exactness_property",
            "property",
            47,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "parity_ok",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u010_null_semantics_parity() {
        let null_bulk = parse_frame(b"$-1\r\n").expect("canonical null bulk");
        assert_eq!(null_bulk.frame, RespFrame::BulkString(None));
        assert_eq!(null_bulk.consumed, 5);

        let null_array = parse_frame(b"*-1\r\n").expect("canonical null array");
        assert_eq!(null_array.frame, RespFrame::Array(None));
        assert_eq!(null_array.consumed, 5);

        let noncanonical_bulk = parse_frame(b"$-01\r\n").expect_err("must reject non-canonical");
        assert_eq!(noncanonical_bulk, RespParseError::InvalidBulkLength);
        let noncanonical_array = parse_frame(b"*-01\r\n").expect_err("must reject non-canonical");
        assert_eq!(noncanonical_array, RespParseError::InvalidMultibulkLength);

        let mut input_acc = Vec::new();
        input_acc.extend_from_slice(b"$-1\r\n");
        input_acc.extend_from_slice(b"*-1\r\n");
        input_acc.extend_from_slice(b"$-01\r\n");
        input_acc.extend_from_slice(b"*-01\r\n");

        let mut output_acc = Vec::new();
        output_acc.extend_from_slice(null_bulk.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(null_array.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(noncanonical_bulk.to_string().as_bytes());
        output_acc.extend_from_slice(noncanonical_array.to_string().as_bytes());

        let event = build_event(
            "fr_p2c_002_u010_null_semantics_parity",
            "unit",
            53,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.null_semantics_drift",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn encode_into_resp3_promotes_nulls_and_recurses() {
        // (frankenredis-pgplm) Redis 7.2 emits the RESP3 null type `_\r\n`
        // for every null under HELLO 3 — bulk, array, map, set — never the
        // RESP2 `$-1` / `*-1`. encode_into_resp3 promotes null leaves and
        // recurses into containers; non-null scalars are byte-identical to
        // encode_into.
        fn r3(frame: &RespFrame) -> Vec<u8> {
            let mut out = Vec::new();
            frame.encode_into_resp3(&mut out);
            out
        }
        assert_eq!(r3(&RespFrame::BulkString(None)), b"_\r\n");
        assert_eq!(r3(&RespFrame::Array(None)), b"_\r\n");
        assert_eq!(r3(&RespFrame::Map(None)), b"_\r\n");
        assert_eq!(r3(&RespFrame::Set(None)), b"_\r\n");

        // Nested nulls (MGET-style array with a hit and two misses).
        let mget = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"1".to_vec())),
            RespFrame::BulkString(None),
            RespFrame::BulkString(None),
        ]));
        assert_eq!(r3(&mget), b"*3\r\n$1\r\n1\r\n_\r\n_\r\n");

        // XPENDING-style summary: count + three nulls (min/max/consumers).
        let xpending = RespFrame::Array(Some(vec![
            RespFrame::Integer(0),
            RespFrame::BulkString(None),
            RespFrame::BulkString(None),
            RespFrame::Array(None),
        ]));
        assert_eq!(r3(&xpending), b"*4\r\n:0\r\n_\r\n_\r\n_\r\n");

        // Non-null scalars and populated containers are identical to RESP2
        // encoding (only the null leaves differ).
        for frame in [
            RespFrame::SimpleString("OK".to_string()),
            RespFrame::Integer(42),
            RespFrame::BulkString(Some(b"hi".to_vec())),
            RespFrame::Double("1.5".to_string()),
            RespFrame::Array(Some(vec![RespFrame::Integer(1)])),
        ] {
            let mut a = Vec::new();
            frame.encode_into(&mut a);
            assert_eq!(r3(&frame), a, "non-null frame must encode identically");
        }
    }

    #[test]
    fn fr_p2c_002_u011_invalid_utf8_consistency() {
        // Test that invalid UTF-8 in scalar frames is rejected appropriately.
        // Simple strings and errors should fail with InvalidUtf8, while integers
        // fail with InvalidInteger (0xFF is not a valid digit).
        let cases: [(&[u8], RespParseError); 3] = [
            (b"+\xff\r\n", RespParseError::InvalidUtf8),
            (b"-\xff\r\n", RespParseError::InvalidUtf8),
            (b":\xff\r\n", RespParseError::InvalidInteger),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for (input, expected_err) in cases {
            let err = parse_frame(input).expect_err("invalid scalar must fail");
            assert_eq!(err, expected_err);
            input_acc.extend_from_slice(input);
            output_acc.extend_from_slice(err.to_string().as_bytes());
        }
        let event = build_event(
            "fr_p2c_002_u011_invalid_utf8_consistency",
            "unit",
            59,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.scalar_decode_mismatch",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u012_depth_or_size_stress_behavior_is_deterministic() {
        let deep_frame = nested_singleton_array(64);
        let encoded = deep_frame.to_bytes();
        let parsed = parse_frame(encoded.as_slice()).expect("deep frame must parse");
        assert_eq!(parsed.frame, deep_frame);
        assert_eq!(parsed.consumed, encoded.len());

        let truncated = &encoded[..encoded.len() - 2];
        let truncated_err = parse_frame(truncated).expect_err("truncated deep frame must fail");
        assert_eq!(truncated_err, RespParseError::Incomplete);

        let oversized = b"$100\r\nabc\r\n";
        let oversized_err = parse_frame(oversized).expect_err("short payload must fail");
        assert_eq!(oversized_err, RespParseError::Incomplete);

        let mut input_acc = Vec::new();
        input_acc.extend_from_slice(encoded.as_slice());
        input_acc.extend_from_slice(truncated);
        input_acc.extend_from_slice(oversized);

        let mut output_acc = Vec::new();
        output_acc.extend_from_slice(parsed.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(truncated_err.to_string().as_bytes());
        output_acc.extend_from_slice(oversized_err.to_string().as_bytes());

        let event = build_event(
            "fr_p2c_002_u012_depth_or_size_stress_behavior_is_deterministic",
            "property",
            61,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.depth_or_size_resource_clamp",
        );
        event.assert_schema_contract();
    }

    /// Opt-in RESP3 parser (br-frankenredis-ozcx). Default config
    /// still hard-rejects every RESP3 prefix; with allow_resp3=true
    /// each RESP3 type downgrades into a RESP2-shaped RespFrame.
    #[test]
    fn fr_p2c_002_u012b_resp3_allow_downgrade() {
        let allow = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };

        // Default config remains fail-closed.
        assert_eq!(
            parse_frame(b"%1\r\n+name\r\n+alice\r\n").expect_err("fail-closed without allow_resp3"),
            RespParseError::UnsupportedResp3Type(b'%')
        );

        // Map → flat Array of 2N entries.
        let parsed = parse_frame_with_config(b"%2\r\n+name\r\n+alice\r\n+age\r\n:30\r\n", &allow)
            .expect("map parses under allow_resp3");
        assert_eq!(
            parsed.frame,
            RespFrame::Array(Some(vec![
                RespFrame::SimpleString("name".to_string()),
                RespFrame::SimpleString("alice".to_string()),
                RespFrame::SimpleString("age".to_string()),
                RespFrame::Integer(30),
            ]))
        );

        // Set → Array.
        let parsed = parse_frame_with_config(b"~2\r\n+a\r\n+b\r\n", &allow).unwrap();
        assert_eq!(
            parsed.frame,
            RespFrame::Array(Some(vec![
                RespFrame::SimpleString("a".to_string()),
                RespFrame::SimpleString("b".to_string()),
            ]))
        );

        // Push → Array.
        let parsed = parse_frame_with_config(b">2\r\n+message\r\n+payload\r\n", &allow).unwrap();
        assert_eq!(
            parsed.frame,
            RespFrame::Array(Some(vec![
                RespFrame::SimpleString("message".to_string()),
                RespFrame::SimpleString("payload".to_string()),
            ]))
        );

        // Bool → Integer 0/1.
        let parsed = parse_frame_with_config(b"#t\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::Integer(1));
        let parsed = parse_frame_with_config(b"#f\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::Integer(0));

        // Null → BulkString(None).
        let parsed = parse_frame_with_config(b"_\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::BulkString(None));

        // Double → BulkString of the ASCII payload.
        let parsed = parse_frame_with_config(b",3.14\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::BulkString(Some(b"3.14".to_vec())));

        // Verbatim → BulkString with 4-byte "txt:" prefix stripped.
        let parsed = parse_frame_with_config(b"=15\r\ntxt:hello world\r\n", &allow).unwrap();
        assert_eq!(
            parsed.frame,
            RespFrame::BulkString(Some(b"hello world".to_vec()))
        );

        // Attribute: peel the attribute map, return the next frame.
        let parsed = parse_frame_with_config(b"|1\r\n+meta\r\n+value\r\n+OK\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::SimpleString("OK".to_string()));

        // Blob error → Error.
        let parsed = parse_frame_with_config(b"!5\r\nWRONG\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::Error("WRONG".to_string()));
    }

    #[test]
    fn resp3_null_rejects_non_empty_payload() {
        let allow = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };

        assert_eq!(
            parse_frame_with_config(b"_payload\r\n", &allow),
            Err(RespParseError::InvalidBulkLength)
        );
        assert_eq!(
            parse_frame_with_config(b"_0\r\n", &allow),
            Err(RespParseError::InvalidBulkLength)
        );
        assert_eq!(
            parse_frame_with_config(b"_\r\n", &allow)
                .expect("canonical RESP3 null must parse")
                .frame,
            RespFrame::BulkString(None)
        );
    }

    #[test]
    fn format_redis_double_matches_vendored_d2string() {
        // Golden values captured from vendored redis 7.2.4 (util.c::d2string
        // -> fpconv_dtoa). Verified byte-exact against the oracle across 437
        // diverse magnitudes; these lock the representative branches and keep
        // the RESP3 Double path in lockstep with RESP2. (frankenredis-sk4ss)
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (-0.0, "-0"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (3.0, "3"),
            (17179869184.0, "17179869184"),
            (-42.0, "-42"),
            (2.75, "2.75"),
            (123.456, "123.456"),
            (-2.5, "-2.5"),
            (0.1, "0.1"),
            (0.000001, "0.000001"),
            (-0.0007, "-0.0007"),
            (123_456_789.123_456_79, "1.2345678912345679e+8"),
            (1.5e300, "1.5e+300"),
            (1e20, "1e+20"),
            (1e308, "1e+308"),
            (-1.5e300, "-1.5e+300"),
            (6.022e23, "6.022e+23"),
            (1e-7, "1e-7"),
            (1e-10, "1e-10"),
            (1.6e-19, "1.6e-19"),
        ];
        for (value, expected) in cases {
            assert_eq!(
                &format_redis_double(*value),
                expected,
                "format_redis_double({value:?})"
            );
            assert_eq!(
                RespFrame::double_from_f64(*value),
                RespFrame::Double((*expected).to_string()),
                "double_from_f64({value:?})"
            );
        }
        assert_eq!(format_redis_double(f64::NAN), "nan");
    }

    #[test]
    fn resp3_double_rejects_empty_and_non_numeric_payloads() {
        // (frankenredis-u1xg5) Continuing ny5fu's RESP3 strictness
        // pass for the ',' (double) prefix. Empty / garbage payloads
        // are malformed and must be rejected with InvalidInteger
        // instead of silently passing as BulkString(Some(...)).
        let allow = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };
        assert_eq!(
            parse_frame_with_config(b",\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "empty double must reject"
        );
        assert_eq!(
            parse_frame_with_config(b",abc\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "non-numeric double must reject"
        );
        assert_eq!(
            parse_frame_with_config(b",1.2.3\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "double-dotted double must reject"
        );

        // Canonical doubles still parse to BulkString of the payload.
        for ok in [
            &b",3.14\r\n"[..],
            &b",-0.5\r\n"[..],
            &b",1e10\r\n"[..],
            &b",inf\r\n"[..],
            &b",-inf\r\n"[..],
            &b",nan\r\n"[..],
        ] {
            assert!(
                matches!(
                    parse_frame_with_config(ok, &allow),
                    Ok(parsed) if matches!(parsed.frame, RespFrame::BulkString(Some(_)))
                ),
                "canonical double {ok:?} should parse"
            );
        }
    }

    #[test]
    fn resp3_big_number_rejects_empty_and_non_numeric_payloads() {
        // (frankenredis-u1xg5) Same strictness for the '(' (big
        // number) prefix. Body must be all decimal digits with an
        // optional leading +/-.
        let allow = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };
        assert_eq!(
            parse_frame_with_config(b"(\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "empty big number must reject"
        );
        assert_eq!(
            parse_frame_with_config(b"(abc\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "non-numeric big number must reject"
        );
        assert_eq!(
            parse_frame_with_config(b"(+\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "sign-only big number must reject"
        );
        assert_eq!(
            parse_frame_with_config(b"(1.5\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "non-integer big number must reject"
        );
        assert_eq!(
            parse_frame_with_config(b"(123abc\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "trailing-garbage big number must reject"
        );

        // Canonical big numbers still parse.
        for ok in [
            &b"(0\r\n"[..],
            &b"(12345\r\n"[..],
            &b"(-42\r\n"[..],
            &b"(+99999999999999999999\r\n"[..],
        ] {
            assert!(
                matches!(
                    parse_frame_with_config(ok, &allow),
                    Ok(parsed) if matches!(parsed.frame, RespFrame::BulkString(Some(_)))
                ),
                "canonical big number {ok:?} should parse"
            );
        }
    }

    #[test]
    fn resp3_verbatim_rejects_missing_format_prefix() -> Result<(), String> {
        // (frankenredis-gg805) RESP3 verbatim bodies are
        // `<3-byte-format>:<payload>`. Accepting bodies without the
        // mandatory format prefix lets malformed replies pass through
        // as ordinary bulk strings once allow_resp3 is enabled.
        let allow = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };

        for (input, context) in [
            (
                &b"=3\r\nabc\r\n"[..],
                "verbatim body shorter than the format prefix",
            ),
            (&b"=4\r\nabcd\r\n"[..], "verbatim body with no prefix colon"),
            (
                &b"=0\r\n\r\n"[..],
                "empty verbatim body cannot carry the mandatory format prefix",
            ),
        ] {
            let actual = parse_frame_with_config(input, &allow);
            if actual != Err(RespParseError::InvalidBulkLength) {
                return Err(format!("{context} should reject; got {actual:?}"));
            }
        }

        let parsed = parse_frame_with_config(b"=4\r\ntxt:\r\n", &allow)
            .map_err(|err| format!("empty verbatim payload with prefix should parse: {err:?}"))?;
        if parsed.frame != RespFrame::BulkString(Some(Vec::new())) {
            return Err(format!(
                "empty verbatim payload stripped incorrectly: {parsed:?}"
            ));
        }
        Ok(())
    }

    #[test]
    fn resp3_attribute_chain_is_capped_independently_of_recursion_depth() {
        // (frankenredis-oafun) The xfxtd fix made the attribute branch
        // depth-transparent, so without a separate chain cap a
        // malicious peer could send '|1\r\n+k\r\n+v\r\n' × N to grow
        // the parser's Rust call stack linearly at the same depth.
        // RESP3_ATTRIBUTE_CHAIN_LIMIT (8) catches that. Generate a
        // 16-deep chain wrapping a single SimpleString and assert
        // the parser rejects it cleanly with RecursionLimitExceeded
        // — well before any real workload would trip the cap.
        let mut payload = Vec::new();
        for _ in 0..16 {
            payload.extend_from_slice(b"|1\r\n+k\r\n+v\r\n");
        }
        payload.extend_from_slice(b"+OK\r\n");

        let cfg = ParserConfig {
            // Very generous depth budget to isolate the chain cap as
            // the actual rejection signal.
            max_recursion_depth: 1024,
            allow_resp3: true,
            ..ParserConfig::default()
        };
        let result = parse_frame_with_config(&payload, &cfg);
        assert_eq!(result, Err(RespParseError::RecursionLimitExceeded));
    }

    #[test]
    fn resp3_attribute_chain_under_cap_still_parses() {
        // (frankenredis-oafun) 7 chained attributes (well under the
        // cap of 8) must still resolve to the wrapped frame. Pins
        // that the cap doesn't accidentally reject legitimately
        // multi-attribute replies.
        let mut payload = Vec::new();
        for _ in 0..7 {
            payload.extend_from_slice(b"|1\r\n+k\r\n+v\r\n");
        }
        payload.extend_from_slice(b"+OK\r\n");

        let cfg = ParserConfig {
            max_recursion_depth: 1024,
            allow_resp3: true,
            ..ParserConfig::default()
        };
        let parsed = parse_frame_with_config(&payload, &cfg).expect("7 attrs must parse");
        assert_eq!(parsed.frame, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn resp3_attribute_wrapper_does_not_spend_extra_recursion_depth() {
        let allow_tight_depth = ParserConfig {
            max_recursion_depth: 1,
            allow_resp3: true,
            ..ParserConfig::default()
        };

        let parsed = parse_frame_with_config(
            b"|1\r\n+meta\r\n+value\r\n*1\r\n+OK\r\n",
            &allow_tight_depth,
        )
        .expect("attribute metadata must not consume the wrapped frame's depth budget");
        assert_eq!(
            parsed.frame,
            RespFrame::Array(Some(vec![RespFrame::SimpleString("OK".to_string())]))
        );
    }

    #[test]
    fn fr_p2c_002_u013_bulk_limit_clamp_holds_at_boundary() {
        let config = ParserConfig {
            max_bulk_len: 5,
            ..ParserConfig::default()
        };
        let accepted =
            parse_frame_with_config(b"$5\r\nhello\r\n", &config).expect("bulk at limit parses");
        assert_eq!(
            accepted.frame,
            RespFrame::BulkString(Some(b"hello".to_vec()))
        );
        assert_eq!(accepted.consumed, b"$5\r\nhello\r\n".len());

        let rejected = parse_frame_with_config(b"$6\r\nhello!\r\n", &config)
            .expect_err("bulk above limit must fail");
        assert_eq!(rejected, RespParseError::BulkLengthTooLarge);

        let mut input_acc = Vec::new();
        input_acc.extend_from_slice(b"$5\r\nhello\r\n");
        input_acc.extend_from_slice(b"$6\r\nhello!\r\n");

        let mut output_acc = Vec::new();
        output_acc.extend_from_slice(accepted.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(rejected.to_string().as_bytes());

        let event = build_event(
            "fr_p2c_002_u013_bulk_limit_clamp_holds_at_boundary",
            "unit",
            67,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.bulk_length_clamp_enforced",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u014_array_limit_clamp_holds_at_boundary() {
        let config = ParserConfig {
            max_array_len: 1,
            ..ParserConfig::default()
        };
        let accepted =
            parse_frame_with_config(b"*1\r\n+OK\r\n", &config).expect("array at limit parses");
        assert_eq!(
            accepted.frame,
            RespFrame::Array(Some(vec![RespFrame::SimpleString("OK".to_string())]))
        );
        assert_eq!(accepted.consumed, b"*1\r\n+OK\r\n".len());

        let rejected = parse_frame_with_config(b"*2\r\n+OK\r\n+OK\r\n", &config)
            .expect_err("array above limit must fail");
        assert_eq!(rejected, RespParseError::MultibulkLengthTooLarge);

        let mut input_acc = Vec::new();
        input_acc.extend_from_slice(b"*1\r\n+OK\r\n");
        input_acc.extend_from_slice(b"*2\r\n+OK\r\n+OK\r\n");

        let mut output_acc = Vec::new();
        output_acc.extend_from_slice(accepted.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(rejected.to_string().as_bytes());

        let event = build_event(
            "fr_p2c_002_u014_array_limit_clamp_holds_at_boundary",
            "unit",
            71,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.array_length_clamp_enforced",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u015_recursion_limit_clamp_holds_at_boundary() {
        let config = ParserConfig {
            max_recursion_depth: 2,
            ..ParserConfig::default()
        };
        let accepted_frame = nested_singleton_array(2);
        let accepted_bytes = accepted_frame.to_bytes();
        let accepted = parse_frame_with_config(accepted_bytes.as_slice(), &config)
            .expect("frame at recursion limit parses");
        assert_eq!(accepted.frame, accepted_frame);
        assert_eq!(accepted.consumed, accepted_bytes.len());

        let rejected_frame = nested_singleton_array(3);
        let rejected_bytes = rejected_frame.to_bytes();
        let rejected = parse_frame_with_config(rejected_bytes.as_slice(), &config)
            .expect_err("frame above recursion limit must fail");
        assert_eq!(rejected, RespParseError::RecursionLimitExceeded);

        let mut input_acc = Vec::new();
        input_acc.extend_from_slice(accepted_bytes.as_slice());
        input_acc.extend_from_slice(rejected_bytes.as_slice());

        let mut output_acc = Vec::new();
        output_acc.extend_from_slice(accepted.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(rejected.to_string().as_bytes());

        let event = build_event(
            "fr_p2c_002_u015_recursion_limit_clamp_holds_at_boundary",
            "unit",
            73,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.recursion_limit_clamp_enforced",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn sequence_frames_encode_as_back_to_back_resp_messages() {
        let frame = RespFrame::Sequence(vec![
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"ch1".to_vec())),
                RespFrame::Integer(1),
            ])),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"ch2".to_vec())),
                RespFrame::Integer(2),
            ])),
        ]);

        assert_eq!(
            frame.to_bytes(),
            b"*3\r\n$9\r\nsubscribe\r\n$3\r\nch1\r\n:1\r\n*3\r\n$9\r\nsubscribe\r\n$3\r\nch2\r\n:2\r\n"
                .to_vec()
        );
    }

    #[test]
    fn resp3_map_frames_encode_with_percent_prefix() {
        let frame = RespFrame::Map(Some(vec![
            (
                RespFrame::BulkString(Some(b"server".to_vec())),
                RespFrame::BulkString(Some(b"redis".to_vec())),
            ),
            (
                RespFrame::BulkString(Some(b"proto".to_vec())),
                RespFrame::Integer(3),
            ),
        ]));

        assert_eq!(
            frame.to_bytes(),
            b"%2\r\n$6\r\nserver\r\n$5\r\nredis\r\n$5\r\nproto\r\n:3\r\n".to_vec()
        );
    }

    #[test]
    fn resp3_push_frames_encode_with_angle_prefix() {
        let frame = RespFrame::Push(vec![
            RespFrame::BulkString(Some(b"message".to_vec())),
            RespFrame::BulkString(Some(b"channel".to_vec())),
            RespFrame::BulkString(Some(b"payload".to_vec())),
        ]);

        assert_eq!(
            frame.to_bytes(),
            b">3\r\n$7\r\nmessage\r\n$7\r\nchannel\r\n$7\r\npayload\r\n".to_vec()
        );
    }

    // ── Proptest fuzz tests ──────────────────────────────────────────

    mod fuzz {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(10_000))]

            #[test]
            fn parse_frame_never_panics(data: Vec<u8>) {
                let _ = parse_frame(&data);
            }

            #[test]
            fn parse_frame_with_config_never_panics(data: Vec<u8>) {
                let config = ParserConfig::default();
                let _ = parse_frame_with_config(&data, &config);
            }

            #[test]
            fn parse_frame_with_tight_limits_never_panics(data: Vec<u8>) {
                let config = ParserConfig {
                    max_bulk_len: 64,
                    max_array_len: 4,
                    max_recursion_depth: 2,
                    ..ParserConfig::default()
                };
                let _ = parse_frame_with_config(&data, &config);
            }

            #[test]
            fn parse_frame_with_resp_prefix_never_panics(
                prefix in prop::sample::select(vec![b'+', b'-', b':', b'$', b'*']),
                payload: Vec<u8>,
            ) {
                let mut data = vec![prefix];
                data.extend_from_slice(&payload);
                let _ = parse_frame(&data);
            }
        }
    }

    /// Golden artifact tests: verify RESP encoding produces exact expected bytes.
    /// These catch accidental encoding format changes that would break wire compatibility.
    mod golden {
        use super::*;

        /// Golden test: SimpleString encoding must produce exact bytes.
        #[test]
        fn golden_simple_string_ok() {
            let frame = RespFrame::SimpleString("OK".to_string());
            let golden = b"+OK\r\n";
            assert_eq!(frame.to_bytes(), golden, "SimpleString encoding changed");
        }

        /// Golden test: SimpleString with spaces and special chars.
        #[test]
        fn golden_simple_string_pong() {
            let frame = RespFrame::SimpleString("PONG".to_string());
            let golden = b"+PONG\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "SimpleString PONG encoding changed"
            );
        }

        /// Golden test: Error encoding must produce exact bytes.
        #[test]
        fn golden_error_generic() {
            let frame = RespFrame::Error("ERR unknown command".to_string());
            let golden = b"-ERR unknown command\r\n";
            assert_eq!(frame.to_bytes(), golden, "Error encoding changed");
        }

        /// Golden test: Error with WRONGTYPE prefix.
        #[test]
        fn golden_error_wrongtype() {
            let frame = RespFrame::Error(
                "WRONGTYPE Operation against a key holding the wrong kind of value".to_string(),
            );
            let golden = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";
            assert_eq!(frame.to_bytes(), golden, "WRONGTYPE error encoding changed");
        }

        /// Golden test: positive integer encoding.
        #[test]
        fn golden_integer_positive() {
            let frame = RespFrame::Integer(42);
            let golden = b":42\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Positive integer encoding changed"
            );
        }

        /// Golden test: negative integer encoding.
        #[test]
        fn golden_integer_negative() {
            let frame = RespFrame::Integer(-1);
            let golden = b":-1\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Negative integer encoding changed"
            );
        }

        /// Golden test: zero integer encoding.
        #[test]
        fn golden_integer_zero() {
            let frame = RespFrame::Integer(0);
            let golden = b":0\r\n";
            assert_eq!(frame.to_bytes(), golden, "Zero integer encoding changed");
        }

        /// Golden test: large integer encoding (Redis INCR max).
        #[test]
        fn golden_integer_large() {
            let frame = RespFrame::Integer(9_223_372_036_854_775_807);
            let golden = b":9223372036854775807\r\n";
            assert_eq!(frame.to_bytes(), golden, "Large integer encoding changed");
        }

        /// Golden test: null bulk string encoding.
        #[test]
        fn golden_bulk_null() {
            let frame = RespFrame::BulkString(None);
            let golden = b"$-1\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Null bulk string encoding changed"
            );
        }

        /// Golden test: empty bulk string encoding.
        #[test]
        fn golden_bulk_empty() {
            let frame = RespFrame::BulkString(Some(vec![]));
            let golden = b"$0\r\n\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Empty bulk string encoding changed"
            );
        }

        /// Golden test: simple bulk string encoding.
        #[test]
        fn golden_bulk_hello() {
            let frame = RespFrame::BulkString(Some(b"hello".to_vec()));
            let golden = b"$5\r\nhello\r\n";
            assert_eq!(frame.to_bytes(), golden, "Bulk string encoding changed");
        }

        /// Golden test: bulk string with binary data (including null bytes).
        #[test]
        fn golden_bulk_binary() {
            let frame = RespFrame::BulkString(Some(vec![0x00, 0xFF, 0x0D, 0x0A]));
            let golden = b"$4\r\n\x00\xFF\x0D\x0A\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Binary bulk string encoding changed"
            );
        }

        #[test]
        fn to_bytes_capacity_hint_preserves_encode_into_bytes() {
            let frames = vec![
                RespFrame::SimpleString("OK\r\nstill-one-frame".to_string()),
                RespFrame::Error("ERR sample".to_string()),
                RespFrame::Integer(i64::MIN),
                RespFrame::BulkString(None),
                RespFrame::BulkString(Some(vec![0x00, 0xFF, b'\r', b'\n'])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"field".to_vec())),
                    RespFrame::BulkString(Some(b"value".to_vec())),
                ])),
                RespFrame::Map(Some(vec![(
                    RespFrame::SimpleString("key".to_string()),
                    RespFrame::Integer(7),
                )])),
                RespFrame::Push(vec![
                    RespFrame::SimpleString("message".to_string()),
                    RespFrame::BulkString(Some(b"payload".to_vec())),
                ]),
                RespFrame::Sequence(vec![RespFrame::Integer(1), RespFrame::Integer(2)]),
                RespFrame::Double("-inf".to_string()),
                RespFrame::Set(None),
                RespFrame::Set(Some(vec![RespFrame::BulkString(Some(b"member".to_vec()))])),
                RespFrame::Verbatim("hello world".to_string()),
            ];

            for frame in frames {
                let mut encode_into_bytes = Vec::new();
                frame.encode_into(&mut encode_into_bytes);
                let to_bytes = frame.to_bytes();
                assert_eq!(to_bytes, encode_into_bytes);
                assert_eq!(frame.encoded_len_hint(), Some(to_bytes.len()));
            }
        }

        /// Golden test: null array encoding.
        #[test]
        fn golden_array_null() {
            let frame = RespFrame::Array(None);
            let golden = b"*-1\r\n";
            assert_eq!(frame.to_bytes(), golden, "Null array encoding changed");
        }

        /// Golden test: empty array encoding.
        #[test]
        fn golden_array_empty() {
            let frame = RespFrame::Array(Some(vec![]));
            let golden = b"*0\r\n";
            assert_eq!(frame.to_bytes(), golden, "Empty array encoding changed");
        }

        /// Golden test: array with single integer.
        #[test]
        fn golden_array_single_int() {
            let frame = RespFrame::Array(Some(vec![RespFrame::Integer(1)]));
            let golden = b"*1\r\n:1\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Single-element array encoding changed"
            );
        }

        /// Golden test: array with mixed types (typical LRANGE response).
        #[test]
        fn golden_array_mixed() {
            let frame = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"first".to_vec())),
                RespFrame::BulkString(Some(b"second".to_vec())),
            ]));
            let golden = b"*2\r\n$5\r\nfirst\r\n$6\r\nsecond\r\n";
            assert_eq!(frame.to_bytes(), golden, "Mixed array encoding changed");
        }

        /// Golden test: nested array (typical XREAD response structure).
        #[test]
        fn golden_array_nested() {
            let frame = RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::Integer(1),
                RespFrame::Integer(2),
            ]))]));
            let golden = b"*1\r\n*2\r\n:1\r\n:2\r\n";
            assert_eq!(frame.to_bytes(), golden, "Nested array encoding changed");
        }

        /// Golden test: typical SET command (client request format).
        #[test]
        fn golden_command_set() {
            let frame = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SET".to_vec())),
                RespFrame::BulkString(Some(b"key".to_vec())),
                RespFrame::BulkString(Some(b"value".to_vec())),
            ]));
            let golden = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n";
            assert_eq!(frame.to_bytes(), golden, "SET command encoding changed");
        }

        /// Golden test: typical GET response (null for missing key).
        #[test]
        fn golden_response_get_miss() {
            let frame = RespFrame::BulkString(None);
            let golden = b"$-1\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "GET miss response encoding changed"
            );
        }

        /// Golden test: SCAN response format (cursor + keys array).
        #[test]
        fn golden_response_scan() {
            let frame = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"0".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"key1".to_vec())),
                    RespFrame::BulkString(Some(b"key2".to_vec())),
                ])),
            ]));
            let golden = b"*2\r\n$1\r\n0\r\n*2\r\n$4\r\nkey1\r\n$4\r\nkey2\r\n";
            assert_eq!(frame.to_bytes(), golden, "SCAN response encoding changed");
        }

        /// Golden test: HGETALL response format (field-value pairs).
        #[test]
        fn golden_response_hgetall() {
            let frame = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"field1".to_vec())),
                RespFrame::BulkString(Some(b"value1".to_vec())),
                RespFrame::BulkString(Some(b"field2".to_vec())),
                RespFrame::BulkString(Some(b"value2".to_vec())),
            ]));
            let golden = b"*4\r\n$6\r\nfield1\r\n$6\r\nvalue1\r\n$6\r\nfield2\r\n$6\r\nvalue2\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "HGETALL response encoding changed"
            );
        }

        /// Golden test: BLPOP response format (key + value).
        #[test]
        fn golden_response_blpop() {
            let frame = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"mylist".to_vec())),
                RespFrame::BulkString(Some(b"element".to_vec())),
            ]));
            let golden = b"*2\r\n$6\r\nmylist\r\n$7\r\nelement\r\n";
            assert_eq!(frame.to_bytes(), golden, "BLPOP response encoding changed");
        }

        /// Golden test: Sequence frame encoding (multiple frames concatenated).
        #[test]
        fn golden_sequence() {
            let frame = RespFrame::Sequence(vec![
                RespFrame::SimpleString("OK".to_string()),
                RespFrame::Integer(1),
            ]);
            let golden = b"+OK\r\n:1\r\n";
            assert_eq!(frame.to_bytes(), golden, "Sequence encoding changed");
        }
    }

    /// Metamorphic tests for RESP encoding/decoding invariants.
    mod metamorphic {
        use super::super::{RespFrame, parse_frame};
        use proptest::prelude::*;

        fn arb_simple_string() -> impl Strategy<Value = RespFrame> {
            "[a-zA-Z0-9 ]{0,50}"
                .prop_filter("no CRLF", |s| !s.contains('\r') && !s.contains('\n'))
                .prop_map(RespFrame::SimpleString)
        }

        fn arb_error() -> impl Strategy<Value = RespFrame> {
            "[A-Z]{3,10} [a-zA-Z0-9 ]{0,40}"
                .prop_filter("no CRLF", |s| !s.contains('\r') && !s.contains('\n'))
                .prop_map(RespFrame::Error)
        }

        fn arb_integer() -> impl Strategy<Value = RespFrame> {
            any::<i64>().prop_map(RespFrame::Integer)
        }

        fn arb_bulk_string() -> impl Strategy<Value = RespFrame> {
            prop_oneof![
                Just(RespFrame::BulkString(None)),
                prop::collection::vec(any::<u8>(), 0..100)
                    .prop_map(|v| RespFrame::BulkString(Some(v))),
            ]
        }

        fn arb_frame_leaf() -> impl Strategy<Value = RespFrame> {
            prop_oneof![
                arb_simple_string(),
                arb_error(),
                arb_integer(),
                arb_bulk_string(),
            ]
        }

        fn arb_frame() -> impl Strategy<Value = RespFrame> {
            arb_frame_leaf().prop_recursive(3, 32, 8, |inner| {
                prop_oneof![
                    Just(RespFrame::Array(None)),
                    prop::collection::vec(inner.clone(), 0..8)
                        .prop_map(|v| RespFrame::Array(Some(v))),
                ]
            })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(500))]

            /// MR1: Encode-decode roundtrip identity
            /// encode(frame) → parse(encoded) == frame
            #[test]
            fn mr_encode_decode_roundtrip(frame in arb_frame()) {
                let encoded = frame.to_bytes();
                let parsed = parse_frame(&encoded).expect("encoded frame must parse");
                prop_assert_eq!(parsed.frame, frame, "roundtrip mismatch");
                prop_assert_eq!(parsed.consumed, encoded.len(), "consumed mismatch");
            }

            /// MR2: Encoding determinism
            /// encode(frame) == encode(clone(frame))
            #[test]
            fn mr_encoding_determinism(frame in arb_frame()) {
                let enc1 = frame.to_bytes();
                let enc2 = frame.clone().to_bytes();
                prop_assert_eq!(enc1, enc2, "encoding not deterministic");
            }

            /// MR3: Encoding length monotonicity for bulk strings
            /// len(encode(bulk(a))) < len(encode(bulk(a ++ b))) when b is non-empty
            #[test]
            fn mr_bulk_length_monotonic(
                base in prop::collection::vec(any::<u8>(), 0..50),
                extra in prop::collection::vec(any::<u8>(), 1..20),
            ) {
                let short_frame = RespFrame::BulkString(Some(base.clone()));
                let mut long_data = base.clone();
                long_data.extend(&extra);
                let long_frame = RespFrame::BulkString(Some(long_data));

                let short_enc = short_frame.to_bytes();
                let long_enc = long_frame.to_bytes();

                prop_assert!(short_enc.len() < long_enc.len(),
                    "adding bytes should increase encoding length: {} vs {}",
                    short_enc.len(), long_enc.len());
            }

            /// MR4: Array length encoding correctness
            /// len(array) == count from encoded header
            #[test]
            fn mr_array_length_encoding(elements in prop::collection::vec(arb_frame_leaf(), 0..20)) {
                let frame = RespFrame::Array(Some(elements.clone()));
                let encoded = frame.to_bytes();

                // Parse the array count from the header
                let header_end = encoded.windows(2)
                    .position(|w| w == b"\r\n")
                    .expect("must have CRLF");
                let count_str = std::str::from_utf8(&encoded[1..header_end])
                    .expect("count must be ASCII");
                let count: usize = count_str.parse().expect("count must be number");

                prop_assert_eq!(count, elements.len(),
                    "array count in header doesn't match element count");
            }

            /// MR5: Concatenated encoding equals sequence encoding
            /// concat(encode(a), encode(b)) == encode(Sequence([a, b]))
            #[test]
            fn mr_sequence_concat_equivalence(
                frame_a in arb_frame_leaf(),
                frame_b in arb_frame_leaf(),
            ) {
                let concat = {
                    let mut v = frame_a.to_bytes();
                    v.extend(frame_b.to_bytes());
                    v
                };
                let seq = RespFrame::Sequence(vec![frame_a.clone(), frame_b.clone()]);
                let seq_encoded = seq.to_bytes();

                prop_assert_eq!(concat, seq_encoded,
                    "sequence encoding differs from concatenation");
            }

            /// MR6: Integer encoding preserves ordering
            /// a < b => encode(:a) lexicographically relates to encode(:b)
            /// (Note: not lex order due to length prefixes, but decoded value order)
            #[test]
            fn mr_integer_order_preservation(a in -10000i64..10000i64, b in -10000i64..10000i64) {
                let frame_a = RespFrame::Integer(a);
                let frame_b = RespFrame::Integer(b);

                let enc_a = frame_a.to_bytes();
                let enc_b = frame_b.to_bytes();

                let parsed_a = parse_frame(&enc_a).expect("must parse").frame;
                let parsed_b = parse_frame(&enc_b).expect("must parse").frame;

                if let (RespFrame::Integer(va), RespFrame::Integer(vb)) = (parsed_a, parsed_b) {
                    if a < b {
                        prop_assert!(va < vb, "order not preserved: {} < {} but {} >= {}", a, b, va, vb);
                    } else if a > b {
                        prop_assert!(va > vb, "order not preserved: {} > {} but {} <= {}", a, b, va, vb);
                    } else {
                        prop_assert_eq!(va, vb, "equal integers should decode equal");
                    }
                } else {
                    prop_assert!(false, "decoded frames are not integers");
                }
            }

            /// MR7: Nested arrays decode to matching depth
            #[test]
            fn mr_nested_array_depth(depth in 1usize..6, value in any::<i64>()) {
                // Build nested array: [[[[value]]]] with `depth` levels
                let mut frame = RespFrame::Integer(value);
                for _ in 0..depth {
                    frame = RespFrame::Array(Some(vec![frame]));
                }

                let encoded = frame.to_bytes();
                let parsed = parse_frame(&encoded).expect("nested array must parse");

                // Unwrap the nesting to verify depth
                let mut current = parsed.frame;
                let mut actual_depth = 0;
                while let RespFrame::Array(Some(inner)) = current {
                    actual_depth += 1;
                    current = inner.into_iter().next().expect("should have one element");
                }

                prop_assert_eq!(actual_depth, depth, "nesting depth mismatch");

                // Verify the inner value
                if let RespFrame::Integer(v) = current {
                    prop_assert_eq!(v, value, "inner value mismatch");
                } else {
                    prop_assert!(false, "inner value is not an integer");
                }
            }
        }
    }
}
