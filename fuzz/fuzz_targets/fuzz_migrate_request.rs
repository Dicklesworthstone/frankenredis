#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_command::{CommandError, MigrateRequest, parse_migrate_request};
use libfuzzer_sys::fuzz_target;
use std::time::Duration;

const MAX_INPUT_LEN: usize = 4_096;
const MAX_RAW_LEN: usize = 2_048;
const MAX_ARGS: usize = 24;
const MAX_ARG_LEN: usize = 96;
const MAX_KEYS: usize = 8;
const MIGRATE_KEYS_REQUIRES_EMPTY_KEY: &str =
    "ERR When using MIGRATE KEYS option, the key argument must be set to the empty string";

#[derive(Debug, Arbitrary)]
enum StructuredMigrateCase {
    Valid(ValidMigrateCase),
    Invalid(InvalidMigrateCase),
}

#[derive(Debug, Arbitrary)]
struct ValidMigrateCase {
    host: Vec<u8>,
    port: u16,
    key_arg: Vec<u8>,
    destination_db: i16,
    timeout_ms: i16,
    option_steps: Vec<ValidOptionStep>,
    keys_mode: bool,
    keys: Vec<Vec<u8>>,
}

#[derive(Debug, Arbitrary)]
enum ValidOptionStep {
    Copy,
    Replace,
    Auth(Vec<u8>),
    Auth2 { username: Vec<u8>, password: Vec<u8> },
}

#[derive(Debug, Arbitrary)]
enum InvalidMigrateCase {
    TooShort { args: Vec<Vec<u8>> },
    InvalidHostUtf8 { tail: Vec<u8> },
    InvalidPort { token: Vec<u8> },
    InvalidDb { token: Vec<u8> },
    InvalidTimeout { token: Vec<u8> },
    MissingAuthArg,
    MissingAuth2Args { username_only: bool, username: Vec<u8> },
    UnknownOption { token: Vec<u8> },
    InvalidOptionUtf8 { tail: Vec<u8> },
    KeysRequiresEmptyKey { key_arg: Vec<u8>, keys: Vec<Vec<u8>> },
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    fuzz_raw_migrate_argv(data);

    let mut unstructured = Unstructured::new(data);
    let Ok(case) = StructuredMigrateCase::arbitrary(&mut unstructured) else {
        return;
    };
    fuzz_structured_migrate_case(case);
});

fn fuzz_raw_migrate_argv(data: &[u8]) {
    let argv = raw_argv(data);
    let Ok(request) = parse_migrate_request(&argv) else {
        return;
    };
    assert_success_invariants(&request);
}

fn fuzz_structured_migrate_case(case: StructuredMigrateCase) {
    match case {
        StructuredMigrateCase::Valid(case) => {
            let argv = render_valid_argv(&case);
            let expected = expected_request(&case);
            assert_eq!(
                parse_migrate_request(&argv),
                Ok(expected.clone()),
                "valid MIGRATE cases must parse to the expected semantic request",
            );
            assert_success_invariants(&expected);
        }
        StructuredMigrateCase::Invalid(case) => {
            let (argv, expected) = render_invalid_case(case);
            assert_eq!(
                parse_migrate_request(&argv),
                Err(expected),
                "invalid MIGRATE cases must reject with the expected error",
            );
        }
    }
}

fn assert_success_invariants(request: &MigrateRequest) {
    assert_eq!(
        parse_migrate_request(&canonical_migrate_argv(request)),
        Ok(request.clone()),
        "accepted MIGRATE requests must round-trip through a canonical argv rendering",
    );
    assert!(
        request.auth_username.is_none() || request.auth_password.is_some(),
        "AUTH2-derived usernames must always carry a password as well",
    );
    assert!(
        request.timeout >= Duration::from_millis(1),
        "accepted MIGRATE requests must clamp to a positive timeout",
    );
}

fn render_valid_argv(case: &ValidMigrateCase) -> Vec<Vec<u8>> {
    let mut argv = base_valid_argv(
        host_bytes(&case.host),
        case.port.to_string().into_bytes(),
        if case.keys_mode {
            Vec::new()
        } else {
            limit_arg_len(case.key_arg.clone())
        },
        case.destination_db.to_string().into_bytes(),
        case.timeout_ms.to_string().into_bytes(),
    );

    for step in case.option_steps.iter().take(MAX_ARGS) {
        match step {
            ValidOptionStep::Copy => argv.push(b"COPY".to_vec()),
            ValidOptionStep::Replace => argv.push(b"REPLACE".to_vec()),
            ValidOptionStep::Auth(password) => {
                argv.push(b"AUTH".to_vec());
                argv.push(limit_arg_len(password.clone()));
            }
            ValidOptionStep::Auth2 { username, password } => {
                argv.push(b"AUTH2".to_vec());
                argv.push(limit_arg_len(username.clone()));
                argv.push(limit_arg_len(password.clone()));
            }
        }
    }

    if case.keys_mode {
        argv.push(b"KEYS".to_vec());
        argv.extend(case.keys.iter().take(MAX_KEYS).cloned().map(limit_arg_len));
    }

    argv
}

fn expected_request(case: &ValidMigrateCase) -> MigrateRequest {
    let mut auth_username = None;
    let mut auth_password = None;

    for step in case.option_steps.iter().take(MAX_ARGS) {
        match step {
            ValidOptionStep::Copy | ValidOptionStep::Replace => {}
            ValidOptionStep::Auth(password) => {
                auth_password = Some(limit_arg_len(password.clone()));
            }
            ValidOptionStep::Auth2 { username, password } => {
                auth_username = Some(limit_arg_len(username.clone()));
                auth_password = Some(limit_arg_len(password.clone()));
            }
        }
    }

    let keys = if case.keys_mode {
        case.keys.iter().take(MAX_KEYS).cloned().map(limit_arg_len).collect()
    } else {
        let key_arg = limit_arg_len(case.key_arg.clone());
        if key_arg.is_empty() {
            Vec::new()
        } else {
            vec![key_arg]
        }
    };

    MigrateRequest {
        host: String::from_utf8(host_bytes(&case.host)).expect("host must be valid utf8"),
        port: case.port,
        destination_db: i64::from(case.destination_db),
        timeout: Duration::from_millis(normalize_timeout_ms(i64::from(case.timeout_ms))),
        copy: case
            .option_steps
            .iter()
            .any(|step| matches!(step, ValidOptionStep::Copy)),
        replace: case
            .option_steps
            .iter()
            .any(|step| matches!(step, ValidOptionStep::Replace)),
        auth_username,
        auth_password,
        keys,
    }
}

fn render_invalid_case(case: InvalidMigrateCase) -> (Vec<Vec<u8>>, CommandError) {
    match case {
        InvalidMigrateCase::TooShort { args } => {
            let mut argv = vec![b"MIGRATE".to_vec()];
            argv.extend(args.into_iter().take(4).map(limit_arg_len));
            (argv, CommandError::WrongArity("MIGRATE"))
        }
        InvalidMigrateCase::InvalidHostUtf8 { tail } => (
            base_valid_argv(
                invalid_utf8_token(tail),
                b"6379".to_vec(),
                b"key".to_vec(),
                b"0".to_vec(),
                b"5000".to_vec(),
            ),
            CommandError::InvalidUtf8Argument,
        ),
        InvalidMigrateCase::InvalidPort { token } => (
            base_valid_argv(
                b"localhost".to_vec(),
                invalid_integer_token(token),
                b"key".to_vec(),
                b"0".to_vec(),
                b"5000".to_vec(),
            ),
            CommandError::InvalidInteger,
        ),
        InvalidMigrateCase::InvalidDb { token } => (
            base_valid_argv(
                b"localhost".to_vec(),
                b"6379".to_vec(),
                b"key".to_vec(),
                invalid_integer_token(token),
                b"5000".to_vec(),
            ),
            CommandError::InvalidInteger,
        ),
        InvalidMigrateCase::InvalidTimeout { token } => (
            base_valid_argv(
                b"localhost".to_vec(),
                b"6379".to_vec(),
                b"key".to_vec(),
                b"0".to_vec(),
                invalid_integer_token(token),
            ),
            CommandError::InvalidInteger,
        ),
        InvalidMigrateCase::MissingAuthArg => {
            let mut argv = valid_scalar_base();
            argv.push(b"AUTH".to_vec());
            (argv, CommandError::SyntaxError)
        }
        InvalidMigrateCase::MissingAuth2Args {
            username_only,
            username,
        } => {
            let mut argv = valid_scalar_base();
            argv.push(b"AUTH2".to_vec());
            if username_only {
                argv.push(limit_arg_len(username));
            }
            (argv, CommandError::SyntaxError)
        }
        InvalidMigrateCase::UnknownOption { token } => {
            let mut argv = valid_scalar_base();
            argv.push(unknown_option_token(token));
            (argv, CommandError::SyntaxError)
        }
        InvalidMigrateCase::InvalidOptionUtf8 { tail } => {
            let mut argv = valid_scalar_base();
            argv.push(invalid_utf8_token(tail));
            (argv, CommandError::InvalidUtf8Argument)
        }
        InvalidMigrateCase::KeysRequiresEmptyKey { key_arg, keys } => {
            let mut argv = base_valid_argv(
                b"localhost".to_vec(),
                b"6379".to_vec(),
                non_empty_key_token(key_arg),
                b"0".to_vec(),
                b"5000".to_vec(),
            );
            argv.push(b"KEYS".to_vec());
            argv.extend(keys.into_iter().take(MAX_KEYS).map(limit_arg_len));
            (
                argv,
                CommandError::Custom(MIGRATE_KEYS_REQUIRES_EMPTY_KEY.to_string()),
            )
        }
    }
}

fn canonical_migrate_argv(request: &MigrateRequest) -> Vec<Vec<u8>> {
    let mut argv = vec![
        b"MIGRATE".to_vec(),
        request.host.as_bytes().to_vec(),
        request.port.to_string().into_bytes(),
    ];

    if request.keys.len() <= 1 {
        argv.push(request.keys.first().cloned().unwrap_or_default());
    } else {
        argv.push(Vec::new());
    }

    argv.push(request.destination_db.to_string().into_bytes());
    argv.push(request.timeout.as_millis().to_string().into_bytes());

    if request.copy {
        argv.push(b"COPY".to_vec());
    }
    if request.replace {
        argv.push(b"REPLACE".to_vec());
    }

    if let Some(password) = &request.auth_password {
        if let Some(username) = &request.auth_username {
            argv.push(b"AUTH2".to_vec());
            argv.push(username.clone());
            argv.push(password.clone());
        } else {
            argv.push(b"AUTH".to_vec());
            argv.push(password.clone());
        }
    }

    if request.keys.len() > 1 {
        argv.push(b"KEYS".to_vec());
        argv.extend(request.keys.iter().cloned());
    }

    argv
}

fn raw_argv(data: &[u8]) -> Vec<Vec<u8>> {
    let mut argv = vec![b"MIGRATE".to_vec()];
    for segment in data[..data.len().min(MAX_RAW_LEN)]
        .split(|byte| matches!(*byte, b'\n' | b'\r' | 0))
        .take(MAX_ARGS.saturating_sub(1))
    {
        argv.push(limit_arg_len(segment.to_vec()));
    }
    argv
}

fn valid_scalar_base() -> Vec<Vec<u8>> {
    base_valid_argv(
        b"localhost".to_vec(),
        b"6379".to_vec(),
        b"key".to_vec(),
        b"0".to_vec(),
        b"5000".to_vec(),
    )
}

fn base_valid_argv(
    host: Vec<u8>,
    port: Vec<u8>,
    key: Vec<u8>,
    destination_db: Vec<u8>,
    timeout: Vec<u8>,
) -> Vec<Vec<u8>> {
    vec![
        b"MIGRATE".to_vec(),
        host,
        port,
        key,
        destination_db,
        timeout,
    ]
}

fn host_bytes(host: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(&limit_arg_len(host.to_vec()))
        .into_owned()
        .into_bytes()
}

fn invalid_integer_token(token: Vec<u8>) -> Vec<u8> {
    let mut token = limit_arg_len(token);
    if token.len() > 19 {
        token.truncate(19);
    }
    if token.is_empty() {
        return b"x".to_vec();
    }
    if is_strict_integer_like(&token) {
        token.push(b'x');
        return token;
    }
    token
}

fn is_strict_integer_like(token: &[u8]) -> bool {
    if token.is_empty() {
        return false;
    }
    if token == b"0" {
        return true;
    }

    let (negative, digits) = if token[0] == b'-' {
        (true, &token[1..])
    } else {
        (false, token)
    };
    if digits.is_empty() {
        return false;
    }
    if digits[0] == b'0' {
        return false;
    }
    if !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }

    let _ = negative;
    true
}

fn invalid_utf8_token(tail: Vec<u8>) -> Vec<u8> {
    let mut tail = limit_arg_len(tail);
    if tail.len() == MAX_ARG_LEN {
        tail.pop();
    }
    tail.push(0xff);
    tail
}

fn unknown_option_token(token: Vec<u8>) -> Vec<u8> {
    let mut token = limit_arg_len(token);
    if token.is_empty() {
        return b"NOPE".to_vec();
    }
    token.make_ascii_uppercase();
    if matches!(
        token.as_slice(),
        b"COPY" | b"REPLACE" | b"AUTH" | b"AUTH2" | b"KEYS"
    ) {
        let mut prefixed = b"NOPE-".to_vec();
        prefixed.extend(token);
        prefixed.truncate(MAX_ARG_LEN);
        return prefixed;
    }
    token
}

fn non_empty_key_token(key_arg: Vec<u8>) -> Vec<u8> {
    let key_arg = limit_arg_len(key_arg);
    if key_arg.is_empty() {
        b"key".to_vec()
    } else {
        key_arg
    }
}

fn normalize_timeout_ms(timeout_ms: i64) -> u64 {
    if timeout_ms <= 0 {
        1000
    } else {
        timeout_ms as u64
    }
}

fn limit_arg_len(mut value: Vec<u8>) -> Vec<u8> {
    value.truncate(MAX_ARG_LEN);
    value
}
