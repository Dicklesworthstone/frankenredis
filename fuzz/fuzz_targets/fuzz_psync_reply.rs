#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_repl::{PsyncReply, ReplOffset, parse_psync_reply};
use libfuzzer_sys::fuzz_target;

const MAX_RAW_LEN: usize = 256;
const MAX_TOKEN_LEN: usize = 64;

#[derive(Debug, Arbitrary)]
enum StructuredPsyncReplyCase {
    Continue(ContinueCase),
    FullResync(FullResyncCase),
    InvalidContinue(InvalidContinueCase),
    InvalidFullResync(InvalidFullResyncCase),
}

#[derive(Debug, Arbitrary)]
struct ContinueCase {
    replid: Option<Vec<u8>>,
    leading_ws: u8,
    separator_ws: u8,
    trailing_ws: u8,
}

#[derive(Debug, Arbitrary)]
struct FullResyncCase {
    replid: Vec<u8>,
    offset: u64,
    leading_ws: u8,
    first_separator_ws: u8,
    second_separator_ws: u8,
    trailing_ws: u8,
}

#[derive(Debug, Arbitrary)]
struct InvalidContinueCase {
    replid: Vec<u8>,
    trailing: Vec<u8>,
    leading_ws: u8,
    first_separator_ws: u8,
    second_separator_ws: u8,
    trailing_ws: u8,
}

#[derive(Debug, Arbitrary)]
enum InvalidFullResyncCase {
    MissingReplid,
    MissingOffset {
        replid: Vec<u8>,
    },
    InvalidOffset {
        replid: Vec<u8>,
        offset_text: Vec<u8>,
    },
    ExtraToken {
        replid: Vec<u8>,
        offset: u64,
        extra: Vec<u8>,
    },
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 2_048 {
        return;
    }

    let Some((&mode, body)) = data.split_first() else {
        return;
    };

    match mode % 2 {
        0 => fuzz_raw_psync_reply(body),
        _ => {
            let mut unstructured = Unstructured::new(body);
            let Ok(case) = StructuredPsyncReplyCase::arbitrary(&mut unstructured) else {
                return;
            };
            fuzz_structured_psync_reply(case);
        }
    }
});

fn fuzz_raw_psync_reply(body: &[u8]) {
    let mut raw = body.to_vec();
    raw.truncate(MAX_RAW_LEN);
    let raw = String::from_utf8_lossy(&raw);
    if let Ok(reply) = parse_psync_reply(&raw) {
        assert_eq!(
            parse_psync_reply(&canonical_reply(&reply)),
            Ok(reply),
            "accepted raw PSYNC replies must canonicalize back to the same semantic reply",
        );
    }
}

fn fuzz_structured_psync_reply(case: StructuredPsyncReplyCase) {
    match case {
        StructuredPsyncReplyCase::Continue(case) => {
            let rendered = render_continue(&case);
            assert_eq!(
                parse_psync_reply(&rendered),
                Ok(PsyncReply::Continue),
                "CONTINUE replies with an optional PSYNC2 replid must parse",
            );
            assert_eq!(
                parse_psync_reply("CONTINUE"),
                Ok(PsyncReply::Continue),
                "canonical CONTINUE must remain accepted",
            );
        }
        StructuredPsyncReplyCase::FullResync(case) => {
            let replid = sanitize_replid_40(case.replid.clone());
            let rendered = render_fullresync(&case, &replid);
            let expected = PsyncReply::FullResync {
                replid: replid.clone(),
                offset: ReplOffset(case.offset),
            };
            assert_eq!(
                parse_psync_reply(&rendered),
                Ok(expected.clone()),
                "FULLRESYNC replies with exactly replid and offset must parse",
            );
            assert_eq!(
                parse_psync_reply(&canonical_reply(&expected)),
                Ok(expected),
                "canonical FULLRESYNC must remain accepted",
            );
        }
        StructuredPsyncReplyCase::InvalidContinue(case) => {
            let rendered = render_invalid_continue(&case);
            assert!(
                parse_psync_reply(&rendered).is_err(),
                "CONTINUE replies with more than one trailing token must reject",
            );
        }
        StructuredPsyncReplyCase::InvalidFullResync(case) => {
            let rendered = render_invalid_fullresync(case);
            assert!(
                parse_psync_reply(&rendered).is_err(),
                "malformed FULLRESYNC replies must reject",
            );
        }
    }
}

fn render_continue(case: &ContinueCase) -> String {
    let mut rendered = String::new();
    rendered.push_str(render_optional_ws(case.leading_ws));
    rendered.push_str("CONTINUE");
    if let Some(replid) = case.replid.as_ref() {
        rendered.push_str(render_required_ws(case.separator_ws));
        rendered.push_str(&sanitize_token(replid.clone(), "replid"));
    }
    rendered.push_str(render_optional_ws(case.trailing_ws));
    rendered
}

fn render_fullresync(case: &FullResyncCase, replid: &str) -> String {
    format!(
        "{}FULLRESYNC{}{}{}{}{}",
        render_optional_ws(case.leading_ws),
        render_required_ws(case.first_separator_ws),
        replid,
        render_required_ws(case.second_separator_ws),
        case.offset,
        render_optional_ws(case.trailing_ws),
    )
}

fn render_invalid_continue(case: &InvalidContinueCase) -> String {
    format!(
        "{}CONTINUE{}{}{}{}{}",
        render_optional_ws(case.leading_ws),
        render_required_ws(case.first_separator_ws),
        sanitize_token(case.replid.clone(), "replid"),
        render_required_ws(case.second_separator_ws),
        sanitize_token(case.trailing.clone(), "extra"),
        render_optional_ws(case.trailing_ws),
    )
}

fn render_invalid_fullresync(case: InvalidFullResyncCase) -> String {
    match case {
        InvalidFullResyncCase::MissingReplid => "FULLRESYNC".to_string(),
        InvalidFullResyncCase::MissingOffset { replid } => {
            format!("FULLRESYNC {}", sanitize_replid_40(replid))
        }
        InvalidFullResyncCase::InvalidOffset {
            replid,
            offset_text,
        } => format!(
            "FULLRESYNC {} {}",
            sanitize_replid_40(replid),
            sanitize_token(offset_text, "offset")
        ),
        InvalidFullResyncCase::ExtraToken {
            replid,
            offset,
            extra,
        } => format!(
            "FULLRESYNC {} {} {}",
            sanitize_replid_40(replid),
            offset,
            sanitize_token(extra, "extra")
        ),
    }
}

fn canonical_reply(reply: &PsyncReply) -> String {
    match reply {
        PsyncReply::Continue => "CONTINUE".to_string(),
        PsyncReply::FullResync { replid, offset } => format!("FULLRESYNC {replid} {}", offset.0),
    }
}

fn render_optional_ws(seed: u8) -> &'static str {
    match seed % 5 {
        0 => "",
        1 => " ",
        2 => "\t",
        3 => " \t ",
        _ => "\r\n",
    }
}

fn render_required_ws(seed: u8) -> &'static str {
    match seed % 4 {
        0 => " ",
        1 => "\t",
        2 => " \t ",
        _ => "\r\n",
    }
}

fn sanitize_token(bytes: Vec<u8>, fallback: &str) -> String {
    let token: String = bytes
        .into_iter()
        .filter_map(|byte| {
            let ch = byte as char;
            (ch.is_ascii_graphic() && !ch.is_ascii_whitespace()).then_some(ch)
        })
        .take(MAX_TOKEN_LEN)
        .collect();
    if token.is_empty() {
        fallback.to_string()
    } else {
        token
    }
}

fn sanitize_replid_40(bytes: Vec<u8>) -> String {
    let mut replid = String::with_capacity(40);
    for byte in bytes {
        let hi = byte >> 4;
        let lo = byte & 0x0f;
        replid.push(hex_char(hi));
        if replid.len() == 40 {
            return replid;
        }
        replid.push(hex_char(lo));
        if replid.len() == 40 {
            return replid;
        }
    }
    while replid.len() < 40 {
        replid.push('0');
    }
    replid
}

fn hex_char(nibble: u8) -> char {
    match nibble {
        0..=9 => char::from(b'0' + nibble),
        10..=15 => char::from(b'a' + (nibble - 10)),
        _ => unreachable!("nibble is masked to four bits"),
    }
}
