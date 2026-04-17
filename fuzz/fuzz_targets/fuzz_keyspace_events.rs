#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_store::{
    NOTIFY_ALL, NOTIFY_EVICTED, NOTIFY_EXPIRED, NOTIFY_GENERIC, NOTIFY_HASH, NOTIFY_KEYEVENT,
    NOTIFY_KEYSPACE, NOTIFY_KEY_MISS, NOTIFY_LIST, NOTIFY_NEW, NOTIFY_SET, NOTIFY_STREAM,
    NOTIFY_STRING, NOTIFY_ZSET, keyspace_events_parse, keyspace_events_to_string,
};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 1024;
const MAX_RAW_LEN: usize = 256;
const MAX_TOKENS: usize = 32;

#[derive(Debug, Arbitrary)]
enum StructuredKeyspaceCase {
    Valid(ValidKeyspaceCase),
    Invalid(InvalidKeyspaceCase),
}

#[derive(Debug, Arbitrary)]
struct ValidKeyspaceCase {
    classes: Vec<ValidClass>,
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum ValidClass {
    All,
    Generic,
    String,
    List,
    Set,
    Hash,
    Zset,
    Expired,
    Evicted,
    Keyspace,
    Keyevent,
    Stream,
    Miss,
    New,
}

#[derive(Debug, Arbitrary)]
enum InvalidKeyspaceCase {
    Mixed {
        valid_prefix: Vec<ValidClass>,
        invalid: InvalidToken,
        valid_suffix: Vec<ValidClass>,
    },
    OnlyInvalid {
        invalids: Vec<InvalidToken>,
    },
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum InvalidToken {
    Question,
    Space,
    LowerA,
    UpperG,
    Digit,
    Utf8Replacement,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    fuzz_raw_keyspace_events(data);

    let mut unstructured = Unstructured::new(data);
    let Ok(case) = StructuredKeyspaceCase::arbitrary(&mut unstructured) else {
        return;
    };
    fuzz_structured_keyspace_events(case);
});

fn fuzz_raw_keyspace_events(data: &[u8]) {
    let raw = String::from_utf8_lossy(&data[..data.len().min(MAX_RAW_LEN)]).into_owned();
    let Some(flags) = keyspace_events_parse(&raw) else {
        return;
    };
    assert_success_invariants(flags);
}

fn fuzz_structured_keyspace_events(case: StructuredKeyspaceCase) {
    match case {
        StructuredKeyspaceCase::Valid(case) => {
            let input = render_valid_classes(&case.classes);
            let expected = expected_flags(&case.classes);
            assert_eq!(
                keyspace_events_parse(&input),
                Some(expected),
                "valid notify-keyspace-events classes must parse to the expected flags",
            );
            assert_success_invariants(expected);
        }
        StructuredKeyspaceCase::Invalid(case) => {
            let input = render_invalid_case(case);
            assert_eq!(
                keyspace_events_parse(&input),
                None,
                "invalid notify-keyspace-events strings must reject",
            );
        }
    }
}

fn assert_success_invariants(flags: u32) {
    let canonical = keyspace_events_to_string(flags);
    assert_eq!(
        keyspace_events_parse(&canonical),
        Some(flags),
        "accepted notify-keyspace-events flags must round-trip through canonical rendering",
    );

    if flags != 0 {
        assert!(
            flags & (NOTIFY_KEYSPACE | NOTIFY_KEYEVENT) != 0,
            "nonzero notify-keyspace-events flags must keep K or E enabled",
        );
    }

    let canonical_bytes = canonical.as_bytes();
    assert_eq!(
        canonical_bytes.iter().filter(|&&byte| byte == b'K').count(),
        usize::from(flags & NOTIFY_KEYSPACE != 0),
        "canonical rendering must include K exactly once when enabled",
    );
    assert_eq!(
        canonical_bytes.iter().filter(|&&byte| byte == b'E').count(),
        usize::from(flags & NOTIFY_KEYEVENT != 0),
        "canonical rendering must include E exactly once when enabled",
    );
    assert_eq!(
        canonical_bytes.iter().filter(|&&byte| byte == b'm').count(),
        usize::from(flags & NOTIFY_KEY_MISS != 0),
        "canonical rendering must include m exactly once when enabled",
    );
}

fn render_valid_classes(classes: &[ValidClass]) -> String {
    classes
        .iter()
        .take(MAX_TOKENS)
        .map(|class| valid_class_char(*class))
        .collect()
}

fn render_invalid_case(case: InvalidKeyspaceCase) -> String {
    match case {
        InvalidKeyspaceCase::Mixed {
            valid_prefix,
            invalid,
            valid_suffix,
        } => {
            let mut out = render_valid_classes(&valid_prefix);
            out.push(invalid_token_char(invalid));
            out.push_str(&render_valid_classes(&valid_suffix));
            out
        }
        InvalidKeyspaceCase::OnlyInvalid { invalids } => invalids
            .iter()
            .take(MAX_TOKENS)
            .map(|token| invalid_token_char(*token))
            .collect(),
    }
}

fn expected_flags(classes: &[ValidClass]) -> u32 {
    let mut flags = 0u32;
    for class in classes.iter().take(MAX_TOKENS) {
        flags |= valid_class_flag(*class);
    }
    if flags != 0 && (flags & (NOTIFY_KEYSPACE | NOTIFY_KEYEVENT)) == 0 {
        0
    } else {
        flags
    }
}

fn valid_class_char(class: ValidClass) -> char {
    match class {
        ValidClass::All => 'A',
        ValidClass::Generic => 'g',
        ValidClass::String => '$',
        ValidClass::List => 'l',
        ValidClass::Set => 's',
        ValidClass::Hash => 'h',
        ValidClass::Zset => 'z',
        ValidClass::Expired => 'x',
        ValidClass::Evicted => 'e',
        ValidClass::Keyspace => 'K',
        ValidClass::Keyevent => 'E',
        ValidClass::Stream => 't',
        ValidClass::Miss => 'm',
        ValidClass::New => 'n',
    }
}

fn valid_class_flag(class: ValidClass) -> u32 {
    match class {
        ValidClass::All => NOTIFY_ALL,
        ValidClass::Generic => NOTIFY_GENERIC,
        ValidClass::String => NOTIFY_STRING,
        ValidClass::List => NOTIFY_LIST,
        ValidClass::Set => NOTIFY_SET,
        ValidClass::Hash => NOTIFY_HASH,
        ValidClass::Zset => NOTIFY_ZSET,
        ValidClass::Expired => NOTIFY_EXPIRED,
        ValidClass::Evicted => NOTIFY_EVICTED,
        ValidClass::Keyspace => NOTIFY_KEYSPACE,
        ValidClass::Keyevent => NOTIFY_KEYEVENT,
        ValidClass::Stream => NOTIFY_STREAM,
        ValidClass::Miss => NOTIFY_KEY_MISS,
        ValidClass::New => NOTIFY_NEW,
    }
}

fn invalid_token_char(token: InvalidToken) -> char {
    match token {
        InvalidToken::Question => '?',
        InvalidToken::Space => ' ',
        InvalidToken::LowerA => 'a',
        InvalidToken::UpperG => 'G',
        InvalidToken::Digit => '7',
        InvalidToken::Utf8Replacement => '\u{2603}',
    }
}
