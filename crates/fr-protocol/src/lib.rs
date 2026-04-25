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
    Sequence(Vec<RespFrame>),
}

impl RespFrame {
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        use std::io::Write;
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
                let _ = write!(out, "{}", n);
                out.extend_from_slice(b"\r\n");
            }
            Self::BulkString(None) => out.extend_from_slice(b"$-1\r\n"),
            Self::BulkString(Some(bytes)) => {
                out.extend_from_slice(b"$");
                let _ = write!(out, "{}", bytes.len());
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(bytes);
                out.extend_from_slice(b"\r\n");
            }
            Self::Array(None) => out.extend_from_slice(b"*-1\r\n"),
            Self::Array(Some(frames)) => {
                out.extend_from_slice(b"*");
                let _ = write!(out, "{}", frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into(out);
                }
            }
            Self::Map(None) => out.extend_from_slice(b"%-1\r\n"),
            Self::Map(Some(entries)) => {
                out.extend_from_slice(b"%");
                let _ = write!(out, "{}", entries.len());
                out.extend_from_slice(b"\r\n");
                for (key, value) in entries {
                    key.encode_into(out);
                    value.encode_into(out);
                }
            }
            Self::Sequence(frames) => {
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
            Self::BulkLengthTooLarge => write!(f, "bulk length exceeds limit"),
            Self::MultibulkLengthTooLarge => write!(f, "multibulk length exceeds limit"),
            Self::RecursionLimitExceeded => write!(f, "nested array depth limit exceeded"),
            Self::LineTooLong => write!(f, "RESP line too long"),
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
    let (frame, consumed) = parse_frame_internal(input, 0, 0, config)?;
    Ok(ParseResult { frame, consumed })
}

fn parse_frame_internal(
    input: &[u8],
    start: usize,
    depth: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    if depth > config.max_recursion_depth {
        return Err(RespParseError::RecursionLimitExceeded);
    }
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
        b',' | b'(' => {
            let (line, consumed) = read_line(input, next)?;
            let s = std::str::from_utf8(line)
                .map_err(|_| RespParseError::InvalidUtf8)?
                .to_string();
            Ok((RespFrame::BulkString(Some(s.into_bytes())), consumed))
        }
        b'_' => {
            let (_line, consumed) = read_line(input, next)?;
            Ok((RespFrame::BulkString(None), consumed))
        }
        b'=' => parse_resp3_verbatim(input, next, config),
        b'|' => {
            // Attribute: parse the attribute map and discard it,
            // then return the next real frame. Upstream uses this
            // to attach per-reply metadata that RESP2 clients don't
            // understand.
            let (_attr, consumed) = parse_resp3_map(input, next, depth + 1, config)?;
            parse_frame_internal(input, consumed, depth + 1, config)
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
        let (item, consumed) = parse_frame_internal(input, cursor, depth + 1, config)?;
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
    // Verbatim string body is `<3-char-type>:<payload>` — strip the
    // 4-byte prefix when present.
    let mut bytes = input[consumed..consumed + data_len].to_vec();
    if bytes.len() >= 4 && bytes[3] == b':' {
        bytes.drain(..4);
    }
    Ok((RespFrame::BulkString(Some(bytes)), end))
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
        let (item, consumed) = parse_frame_internal(input, cursor, depth + 1, config)?;
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
        MAX_LINE_LENGTH, ParserConfig, RespFrame, RespParseError, parse_frame,
        parse_frame_with_config,
    };

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
        let cases = [b'?', b'@', b'/'];
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
        let prefixes = [b'~', b'%', b'#', b',', b'_', b'(', b'=', b'|', b'>', b'!'];
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
