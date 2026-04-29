#![no_main]

use arbitrary::{Arbitrary, Result as ArbitraryResult, Unstructured};
use fr_protocol::{ParserConfig, RespFrame, RespParseError, parse_frame_with_config};
use libfuzzer_sys::fuzz_target;

const MAX_SEQUENCE_FRAMES: usize = 6;
const MAX_TEXT_LEN: usize = 32;
const MAX_BULK_LEN: usize = 96;
const MAX_ARRAY_ITEMS: usize = 4;
const MAX_DEPTH: usize = 4;
const MAX_GARBAGE_LEN: usize = 16;
const MAX_WIRE_LEN: usize = 4_096;

#[derive(Debug)]
struct FuzzInput {
    first: FrameSeed,
    trailing: Vec<FrameSeed>,
    limit_mode: LimitMode,
    mutation: Mutation,
    garbage: Vec<u8>,
}

#[derive(Debug, Clone)]
enum FrameSeed {
    SimpleString(String),
    Error(String),
    Integer(i64),
    BulkString(Option<Vec<u8>>),
    Array(Option<Vec<FrameSeed>>),
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum LimitMode {
    Exact,
    TightBulk,
    TightArray,
    TightDepth,
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum Mutation {
    None,
    Truncate,
    AppendGarbage,
    CorruptLineEnding,
    CorruptDeclaredLength,
    ReplacePrefixWithResp3,
}

#[derive(Debug, Clone, Copy, Default)]
struct FrameProfile {
    max_bulk_len: usize,
    max_array_len: usize,
    max_depth: usize,
}

impl<'a> Arbitrary<'a> for FuzzInput {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbitraryResult<Self> {
        let first = arbitrary_frame(u, 0)?;
        let trailing_len = u.int_in_range(0..=MAX_SEQUENCE_FRAMES)?;
        let mut trailing = Vec::with_capacity(trailing_len);
        for _ in 0..trailing_len {
            trailing.push(arbitrary_frame(u, 0)?);
        }
        Ok(Self {
            first,
            trailing,
            limit_mode: LimitMode::arbitrary(u)?,
            mutation: Mutation::arbitrary(u)?,
            garbage: bounded_bytes(u, MAX_GARBAGE_LEN)?,
        })
    }
}

fuzz_target!(|input: FuzzInput| {
    let first_frame = input.first.to_frame();
    let mut frames = Vec::with_capacity(input.trailing.len() + 1);
    frames.push(first_frame.clone());
    frames.extend(input.trailing.iter().map(FrameSeed::to_frame));

    let base_bytes = encode_frames(&frames);
    if base_bytes.len() > MAX_WIRE_LEN {
        return;
    }

    let first_len = first_frame.to_bytes().len();
    let first_profile = profile_for_frame(&first_frame);
    let sequence_profile = frames.iter().fold(FrameProfile::default(), |acc, frame| {
        merge_profiles(acc, profile_for_frame(frame))
    });
    let tight = tight_config(first_profile, input.limit_mode);
    let loose = loose_config(sequence_profile);

    match input.mutation {
        Mutation::None => assert_valid_sequence(
            base_bytes.as_slice(),
            first_frame,
            frames.as_slice(),
            first_len,
            tight,
            loose,
        ),
        mutation => {
            let mutated = apply_mutation(base_bytes, mutation, input.garbage.as_slice());
            if mutated.len() > MAX_WIRE_LEN {
                return;
            }
            assert_mutated_sequence(mutated.as_slice(), tight, loose);
        }
    }
});

impl FrameSeed {
    fn to_frame(&self) -> RespFrame {
        match self {
            Self::SimpleString(text) => RespFrame::SimpleString(text.clone()),
            Self::Error(text) => RespFrame::Error(text.clone()),
            Self::Integer(value) => RespFrame::Integer(*value),
            Self::BulkString(bytes) => RespFrame::BulkString(bytes.clone()),
            Self::Array(items) => RespFrame::Array(
                items
                    .as_ref()
                    .map(|frames| frames.iter().map(FrameSeed::to_frame).collect()),
            ),
        }
    }
}

fn arbitrary_frame<'a>(u: &mut Unstructured<'a>, depth: usize) -> ArbitraryResult<FrameSeed> {
    let max_variant = if depth >= MAX_DEPTH { 3 } else { 4 };
    match u.int_in_range(0..=max_variant)? {
        0 => Ok(FrameSeed::SimpleString(bounded_text(u)?)),
        1 => Ok(FrameSeed::Error(bounded_text(u)?)),
        2 => Ok(FrameSeed::Integer(i64::arbitrary(u)?)),
        3 => {
            if bool::arbitrary(u)? {
                Ok(FrameSeed::BulkString(None))
            } else {
                Ok(FrameSeed::BulkString(Some(bounded_bytes(u, MAX_BULK_LEN)?)))
            }
        }
        _ => {
            if bool::arbitrary(u)? {
                Ok(FrameSeed::Array(None))
            } else {
                let len = u.int_in_range(0..=MAX_ARRAY_ITEMS)?;
                let mut items = Vec::with_capacity(len);
                for _ in 0..len {
                    items.push(arbitrary_frame(u, depth + 1)?);
                }
                Ok(FrameSeed::Array(Some(items)))
            }
        }
    }
}

fn bounded_bytes<'a>(u: &mut Unstructured<'a>, max_len: usize) -> ArbitraryResult<Vec<u8>> {
    let mut bytes = Vec::<u8>::arbitrary(u)?;
    bytes.truncate(max_len);
    Ok(bytes)
}

fn bounded_text<'a>(u: &mut Unstructured<'a>) -> ArbitraryResult<String> {
    let bytes = bounded_bytes(u, MAX_TEXT_LEN)?;
    let text = bytes
        .into_iter()
        .map(|byte| match byte {
            b'\r' | b'\n' => 'x',
            0x20..=0x7e => char::from(byte),
            _ => char::from(b'a' + (byte % 26)),
        })
        .collect();
    Ok(text)
}

fn encode_frames(frames: &[RespFrame]) -> Vec<u8> {
    let mut out = Vec::new();
    for frame in frames {
        frame.encode_into(&mut out);
    }
    out
}

fn profile_for_frame(frame: &RespFrame) -> FrameProfile {
    match frame {
        RespFrame::SimpleString(_) | RespFrame::Error(_) | RespFrame::Integer(_) => {
            FrameProfile::default()
        }
        RespFrame::BulkString(None) => FrameProfile::default(),
        RespFrame::BulkString(Some(bytes)) => FrameProfile {
            max_bulk_len: bytes.len(),
            ..FrameProfile::default()
        },
        RespFrame::Array(None) | RespFrame::Map(None) => FrameProfile {
            max_depth: 1,
            ..FrameProfile::default()
        },
        RespFrame::Array(Some(items)) | RespFrame::Push(items) => {
            let mut profile = FrameProfile {
                max_array_len: items.len(),
                max_depth: 1,
                ..FrameProfile::default()
            };
            for item in items {
                let nested = profile_for_frame(item);
                profile.max_bulk_len = profile.max_bulk_len.max(nested.max_bulk_len);
                profile.max_array_len = profile.max_array_len.max(nested.max_array_len);
                profile.max_depth = profile.max_depth.max(nested.max_depth.saturating_add(1));
            }
            profile
        }
        // RESP3 maps profile as 2N flat entries (key+value pairs)
        // because that's how parse_resp3_map downgrades them
        // when allow_resp3 is set; mirrors the parser's bound on
        // pair_count = count * 2 (br-frankenredis-ozcx).
        RespFrame::Map(Some(entries)) => {
            let mut profile = FrameProfile {
                max_array_len: entries.len().saturating_mul(2),
                max_depth: 1,
                ..FrameProfile::default()
            };
            for (key, value) in entries {
                for child in [key, value] {
                    let nested = profile_for_frame(child);
                    profile.max_bulk_len = profile.max_bulk_len.max(nested.max_bulk_len);
                    profile.max_array_len = profile.max_array_len.max(nested.max_array_len);
                    profile.max_depth =
                        profile.max_depth.max(nested.max_depth.saturating_add(1));
                }
            }
            profile
        }
        RespFrame::Sequence(items) => items.iter().fold(FrameProfile::default(), |acc, item| {
            merge_profiles(acc, profile_for_frame(item))
        }),
    }
}

fn merge_profiles(lhs: FrameProfile, rhs: FrameProfile) -> FrameProfile {
    FrameProfile {
        max_bulk_len: lhs.max_bulk_len.max(rhs.max_bulk_len),
        max_array_len: lhs.max_array_len.max(rhs.max_array_len),
        max_depth: lhs.max_depth.max(rhs.max_depth),
    }
}

fn loose_config(profile: FrameProfile) -> ParserConfig {
    ParserConfig {
        max_bulk_len: profile.max_bulk_len.saturating_add(8),
        max_array_len: profile.max_array_len.saturating_add(2),
        max_recursion_depth: profile.max_depth.saturating_add(1),
        // The harness encodes RESP2-shaped fixtures by default;
        // accept RESP3 too so the encoder/decoder roundtrips exercise
        // the documented RESP3-downgrade path when frames include
        // Map/Push variants.
        allow_resp3: true,
    }
}

fn tight_config(profile: FrameProfile, mode: LimitMode) -> ParserConfig {
    let mut config = loose_config(profile);
    match mode {
        LimitMode::Exact => {}
        LimitMode::TightBulk => {
            if profile.max_bulk_len > 0 {
                config.max_bulk_len = profile.max_bulk_len.saturating_sub(1);
            }
        }
        LimitMode::TightArray => {
            if profile.max_array_len > 0 {
                config.max_array_len = profile.max_array_len.saturating_sub(1);
            }
        }
        LimitMode::TightDepth => {
            if profile.max_depth > 0 {
                config.max_recursion_depth = profile.max_depth.saturating_sub(1);
            }
        }
    }
    config
}

fn assert_valid_sequence(
    bytes: &[u8],
    first_frame: RespFrame,
    frames: &[RespFrame],
    first_len: usize,
    tight: ParserConfig,
    loose: ParserConfig,
) {
    let loose_parsed =
        parse_frame_with_config(bytes, &loose).expect("valid frame must parse under loose config");
    assert_eq!(loose_parsed.frame, first_frame);
    assert_eq!(loose_parsed.consumed, first_len);

    assert_iterative_parse(bytes, &loose, frames);

    match parse_frame_with_config(bytes, &tight) {
        Ok(tight_parsed) => {
            assert_eq!(tight_parsed.frame, loose_parsed.frame);
            assert_eq!(tight_parsed.consumed, loose_parsed.consumed);
        }
        Err(error) => assert!(matches!(
            error,
            RespParseError::BulkLengthTooLarge
                | RespParseError::MultibulkLengthTooLarge
                | RespParseError::RecursionLimitExceeded
        )),
    }
}

fn assert_mutated_sequence(bytes: &[u8], tight: ParserConfig, loose: ParserConfig) {
    let tight_result = parse_frame_with_config(bytes, &tight);
    let loose_result = parse_frame_with_config(bytes, &loose);

    if let (Ok(tight_parsed), Ok(loose_parsed)) = (&tight_result, &loose_result) {
        assert_eq!(tight_parsed.frame, loose_parsed.frame);
        assert_eq!(tight_parsed.consumed, loose_parsed.consumed);
    }

    if let Ok(loose_parsed) = loose_result {
        assert!(loose_parsed.consumed > 0);
        assert!(loose_parsed.consumed <= bytes.len());
        let prefix = &bytes[..loose_parsed.consumed];
        let reparsed = parse_frame_with_config(prefix, &loose)
            .expect("accepted prefix must remain parseable under same config");
        assert_eq!(reparsed.frame, loose_parsed.frame);
        assert_eq!(reparsed.consumed, prefix.len());
    }
}

fn assert_iterative_parse(bytes: &[u8], config: &ParserConfig, frames: &[RespFrame]) {
    let mut offset = 0_usize;
    for expected in frames {
        let parsed = parse_frame_with_config(&bytes[offset..], config)
            .expect("encoded frame sequence must parse without drift");
        let expected_len = expected.to_bytes().len();
        assert_eq!(parsed.frame, *expected);
        assert_eq!(parsed.consumed, expected_len);
        offset += parsed.consumed;
    }
    assert_eq!(offset, bytes.len());
}

fn apply_mutation(mut bytes: Vec<u8>, mutation: Mutation, garbage: &[u8]) -> Vec<u8> {
    match mutation {
        Mutation::None => {}
        Mutation::Truncate => {
            if !bytes.is_empty() {
                bytes.truncate(bytes.len().saturating_sub(1));
            }
        }
        Mutation::AppendGarbage => {
            let tail = garbage.get(..MAX_GARBAGE_LEN).unwrap_or(garbage);
            bytes.extend_from_slice(tail);
        }
        Mutation::CorruptLineEnding => {
            if let Some(pos) = bytes.windows(2).position(|window| window == b"\r\n") {
                bytes[pos + 1] = b'x';
            }
        }
        Mutation::CorruptDeclaredLength => {
            if matches!(bytes.first(), Some(b'$' | b'*')) && bytes.len() > 1 {
                bytes[1] = b'x';
            }
        }
        Mutation::ReplacePrefixWithResp3 => {
            if !bytes.is_empty() {
                bytes[0] = b'~';
            }
        }
    }
    bytes
}
