#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_command::{
    CommandError, client_tracking_getredir_value, client_trackinginfo_frame,
    parse_client_tracking_state, validate_client_caching_mode,
};
use fr_protocol::RespFrame;
use fr_store::ClientTrackingState;
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeSet;

const CLIENT_TRACKING_PREFIX_REQUIRES_BCAST: &str =
    "ERR PREFIX option requires BCAST mode to be enabled";
const CLIENT_TRACKING_OPTIN_OPTOUT_CONFLICT: &str =
    "ERR OPTIN and OPTOUT are not compatible";
const CLIENT_TRACKING_BCAST_OPT_CONFLICT: &str =
    "ERR OPTIN or OPTOUT are not compatible with BCAST";
const CLIENT_TRACKING_REDIRECT_MISSING: &str =
    "ERR The client ID you want redirect to does not exist";
const CLIENT_CACHING_REQUIRES_TRACKING: &str =
    "ERR CLIENT CACHING can be called only when the client is in tracking mode with OPTIN or OPTOUT mode enabled";
const CLIENT_CACHING_YES_REQUIRES_OPTIN: &str =
    "ERR CLIENT CACHING YES is only valid when tracking is enabled in OPTIN mode.";
const CLIENT_CACHING_NO_REQUIRES_OPTOUT: &str =
    "ERR CLIENT CACHING NO is only valid when tracking is enabled in OPTOUT mode.";

const MAX_INPUT_LEN: usize = 4_096;
const MAX_RAW_LEN: usize = 2_048;
const MAX_ARGS: usize = 32;
const MAX_ARG_LEN: usize = 96;
const MAX_PREFIXES: usize = 6;

#[derive(Debug, Arbitrary)]
enum StructuredTrackingCase {
    Valid(ValidTrackingCase),
    Invalid(InvalidTrackingCase),
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum TrackingFlavor {
    Off,
    Plain,
    Bcast,
    Optin,
    Optout,
}

#[derive(Debug, Arbitrary)]
struct ValidTrackingCase {
    flavor: TrackingFlavor,
    redirect: Option<u16>,
    prefixes: Vec<Vec<u8>>,
    noloop: bool,
    duplicate_prefixes: bool,
    order_seed: u8,
}

#[derive(Debug, Arbitrary)]
enum InvalidTrackingCase {
    PrefixWithoutBcast {
        prefixes: Vec<Vec<u8>>,
        noloop: bool,
    },
    OptinAndOptout {
        noloop: bool,
    },
    BcastWithOptin {
        noloop: bool,
    },
    BcastWithOptout {
        noloop: bool,
    },
    RedirectZero,
    MissingRedirectArg,
    MissingPrefixArg,
    InvalidMode {
        token: Vec<u8>,
    },
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    fuzz_raw_client_tracking(data);

    let mut unstructured = Unstructured::new(data);
    let Ok(case) = StructuredTrackingCase::arbitrary(&mut unstructured) else {
        return;
    };
    fuzz_structured_client_tracking(case);
});

fn fuzz_raw_client_tracking(data: &[u8]) {
    let argv = argv_from_raw(data);
    let Ok(state) = parse_client_tracking_state(&argv) else {
        return;
    };

    assert_success_invariants(&state);
    assert_eq!(
        parse_client_tracking_state(&canonical_tracking_argv(&state)),
        Ok(state.clone()),
        "accepted CLIENT TRACKING states must canonicalize back to the same semantic state",
    );
}

fn fuzz_structured_client_tracking(case: StructuredTrackingCase) {
    match case {
        StructuredTrackingCase::Valid(case) => {
            let state = expected_state(case.flavor, case.redirect, case.prefixes, case.noloop);
            let argv = render_valid_argv(&state, case.duplicate_prefixes, case.order_seed);
            assert_eq!(
                parse_client_tracking_state(&argv),
                Ok(state.clone()),
                "valid CLIENT TRACKING option sets must parse to the expected state",
            );
            assert_success_invariants(&state);
        }
        StructuredTrackingCase::Invalid(case) => {
            let (argv, expected) = render_invalid_case(case);
            assert_eq!(
                parse_client_tracking_state(&argv),
                Err(expected),
                "invalid CLIENT TRACKING option sets must reject with the expected error",
            );
        }
    }
}

fn assert_success_invariants(state: &ClientTrackingState) {
    if !state.enabled {
        assert_eq!(
            state,
            &ClientTrackingState::default(),
            "disabled tracking must always collapse to the default state",
        );
    } else {
        assert!(
            state.prefixes.is_empty() || state.bcast,
            "PREFIX requires BCAST mode",
        );
        assert!(
            !(state.optin && state.optout),
            "OPTIN and OPTOUT must never both be enabled",
        );
        assert!(
            !(state.bcast && (state.optin || state.optout)),
            "BCAST must remain incompatible with OPTIN/OPTOUT",
        );
    }

    assert_eq!(
        client_trackinginfo_frame(state),
        expected_trackinginfo_frame(state),
        "TRACKINGINFO output must stay synchronized with the parsed tracking state",
    );
    assert_eq!(
        validate_client_caching_mode("MAYBE", state),
        Err(CommandError::SyntaxError),
        "CLIENT CACHING should reject unknown modes before examining tracking state",
    );

    if !state.enabled || (!state.optin && !state.optout) {
        assert_eq!(
            validate_client_caching_mode("YES", state),
            Err(CommandError::Custom(
                CLIENT_CACHING_REQUIRES_TRACKING.to_string(),
            )),
            "CLIENT CACHING YES must require OPTIN/OPTOUT tracking mode",
        );
        assert_eq!(
            validate_client_caching_mode("NO", state),
            Err(CommandError::Custom(
                CLIENT_CACHING_REQUIRES_TRACKING.to_string(),
            )),
            "CLIENT CACHING NO must require OPTIN/OPTOUT tracking mode",
        );
    } else if state.optin {
        assert_eq!(
            validate_client_caching_mode("YES", state),
            Ok(()),
            "OPTIN tracking must accept CLIENT CACHING YES",
        );
        assert_eq!(
            validate_client_caching_mode("NO", state),
            Err(CommandError::Custom(
                CLIENT_CACHING_NO_REQUIRES_OPTOUT.to_string(),
            )),
            "OPTIN tracking must reject CLIENT CACHING NO",
        );
    } else {
        assert_eq!(
            validate_client_caching_mode("NO", state),
            Ok(()),
            "OPTOUT tracking must accept CLIENT CACHING NO",
        );
        assert_eq!(
            validate_client_caching_mode("YES", state),
            Err(CommandError::Custom(
                CLIENT_CACHING_YES_REQUIRES_OPTIN.to_string(),
            )),
            "OPTOUT tracking must reject CLIENT CACHING YES",
        );
    }
}

fn expected_state(
    flavor: TrackingFlavor,
    redirect: Option<u16>,
    prefixes: Vec<Vec<u8>>,
    noloop: bool,
) -> ClientTrackingState {
    let mut prefixes: BTreeSet<Vec<u8>> = prefixes
        .into_iter()
        .map(limit_arg_len)
        .take(MAX_PREFIXES)
        .collect();

    match flavor {
        TrackingFlavor::Off => ClientTrackingState::default(),
        TrackingFlavor::Plain => {
            prefixes.clear();
            ClientTrackingState {
                enabled: true,
                redirect: redirect.map(|value| u64::from(value) + 1),
                bcast: false,
                optin: false,
                optout: false,
                noloop,
                prefixes,
            }
        }
        TrackingFlavor::Bcast => ClientTrackingState {
            enabled: true,
            redirect: redirect.map(|value| u64::from(value) + 1),
            bcast: true,
            optin: false,
            optout: false,
            noloop,
            prefixes,
        },
        TrackingFlavor::Optin => {
            prefixes.clear();
            ClientTrackingState {
                enabled: true,
                redirect: redirect.map(|value| u64::from(value) + 1),
                bcast: false,
                optin: true,
                optout: false,
                noloop,
                prefixes,
            }
        }
        TrackingFlavor::Optout => {
            prefixes.clear();
            ClientTrackingState {
                enabled: true,
                redirect: redirect.map(|value| u64::from(value) + 1),
                bcast: false,
                optin: false,
                optout: true,
                noloop,
                prefixes,
            }
        }
    }
}

fn render_valid_argv(
    state: &ClientTrackingState,
    duplicate_prefixes: bool,
    order_seed: u8,
) -> Vec<Vec<u8>> {
    if !state.enabled {
        return canonical_tracking_argv(state);
    }

    let mut argv = vec![
        b"CLIENT".to_vec(),
        b"TRACKING".to_vec(),
        b"ON".to_vec(),
    ];
    let mut chunks = Vec::new();

    if let Some(redirect) = state.redirect {
        chunks.push(vec![b"REDIRECT".to_vec(), redirect.to_string().into_bytes()]);
    }
    if state.bcast {
        chunks.push(vec![b"BCAST".to_vec()]);
    }
    if state.optin {
        chunks.push(vec![b"OPTIN".to_vec()]);
    }
    if state.optout {
        chunks.push(vec![b"OPTOUT".to_vec()]);
    }
    if state.noloop {
        chunks.push(vec![b"NOLOOP".to_vec()]);
    }
    for prefix in &state.prefixes {
        let prefix = prefix.clone();
        chunks.push(vec![b"PREFIX".to_vec(), prefix.clone()]);
        if duplicate_prefixes {
            chunks.push(vec![b"PREFIX".to_vec(), prefix]);
        }
    }

    reorder_chunks(&mut chunks, order_seed);
    argv.extend(chunks.into_iter().flatten());
    argv
}

fn render_invalid_case(case: InvalidTrackingCase) -> (Vec<Vec<u8>>, CommandError) {
    match case {
        InvalidTrackingCase::PrefixWithoutBcast { prefixes, noloop } => {
            let mut argv = vec![b"CLIENT".to_vec(), b"TRACKING".to_vec(), b"ON".to_vec()];
            if noloop {
                argv.push(b"NOLOOP".to_vec());
            }
            let prefixes = fallback_prefixes(prefixes);
            for prefix in prefixes {
                argv.push(b"PREFIX".to_vec());
                argv.push(prefix);
            }
            (
                argv,
                CommandError::Custom(CLIENT_TRACKING_PREFIX_REQUIRES_BCAST.to_string()),
            )
        }
        InvalidTrackingCase::OptinAndOptout { noloop } => {
            let mut argv = vec![
                b"CLIENT".to_vec(),
                b"TRACKING".to_vec(),
                b"ON".to_vec(),
                b"OPTIN".to_vec(),
                b"OPTOUT".to_vec(),
            ];
            if noloop {
                argv.push(b"NOLOOP".to_vec());
            }
            (
                argv,
                CommandError::Custom(CLIENT_TRACKING_OPTIN_OPTOUT_CONFLICT.to_string()),
            )
        }
        InvalidTrackingCase::BcastWithOptin { noloop } => {
            let mut argv = vec![
                b"CLIENT".to_vec(),
                b"TRACKING".to_vec(),
                b"ON".to_vec(),
                b"BCAST".to_vec(),
                b"OPTIN".to_vec(),
            ];
            if noloop {
                argv.push(b"NOLOOP".to_vec());
            }
            (
                argv,
                CommandError::Custom(CLIENT_TRACKING_BCAST_OPT_CONFLICT.to_string()),
            )
        }
        InvalidTrackingCase::BcastWithOptout { noloop } => {
            let mut argv = vec![
                b"CLIENT".to_vec(),
                b"TRACKING".to_vec(),
                b"ON".to_vec(),
                b"BCAST".to_vec(),
                b"OPTOUT".to_vec(),
            ];
            if noloop {
                argv.push(b"NOLOOP".to_vec());
            }
            (
                argv,
                CommandError::Custom(CLIENT_TRACKING_BCAST_OPT_CONFLICT.to_string()),
            )
        }
        InvalidTrackingCase::RedirectZero => (
            vec![
                b"CLIENT".to_vec(),
                b"TRACKING".to_vec(),
                b"ON".to_vec(),
                b"REDIRECT".to_vec(),
                b"0".to_vec(),
            ],
            CommandError::Custom(CLIENT_TRACKING_REDIRECT_MISSING.to_string()),
        ),
        InvalidTrackingCase::MissingRedirectArg => (
            vec![
                b"CLIENT".to_vec(),
                b"TRACKING".to_vec(),
                b"ON".to_vec(),
                b"REDIRECT".to_vec(),
            ],
            CommandError::SyntaxError,
        ),
        InvalidTrackingCase::MissingPrefixArg => (
            vec![
                b"CLIENT".to_vec(),
                b"TRACKING".to_vec(),
                b"ON".to_vec(),
                b"BCAST".to_vec(),
                b"PREFIX".to_vec(),
            ],
            CommandError::SyntaxError,
        ),
        InvalidTrackingCase::InvalidMode { token } => (
            vec![
                b"CLIENT".to_vec(),
                b"TRACKING".to_vec(),
                sanitize_invalid_mode(token),
            ],
            CommandError::SyntaxError,
        ),
    }
}

fn expected_trackinginfo_frame(state: &ClientTrackingState) -> RespFrame {
    let flags = if !state.enabled {
        vec![bulk(b"off")]
    } else {
        let mut flags = vec![bulk(b"on")];
        if state.bcast {
            flags.push(bulk(b"bcast"));
        }
        if state.optin {
            flags.push(bulk(b"optin"));
        }
        if state.optout {
            flags.push(bulk(b"optout"));
        }
        if state.noloop {
            flags.push(bulk(b"noloop"));
        }
        flags
    };
    let prefixes = state.prefixes.iter().cloned().map(bulk_owned).collect();

    RespFrame::Array(Some(vec![
        bulk(b"flags"),
        RespFrame::Array(Some(flags)),
        bulk(b"redirect"),
        RespFrame::Integer(client_tracking_getredir_value(state)),
        bulk(b"prefixes"),
        RespFrame::Array(Some(prefixes)),
    ]))
}

fn canonical_tracking_argv(state: &ClientTrackingState) -> Vec<Vec<u8>> {
    let mut argv = vec![b"CLIENT".to_vec(), b"TRACKING".to_vec()];
    if !state.enabled {
        argv.push(b"OFF".to_vec());
        return argv;
    }

    argv.push(b"ON".to_vec());
    if let Some(redirect) = state.redirect {
        argv.push(b"REDIRECT".to_vec());
        argv.push(redirect.to_string().into_bytes());
    }
    if state.bcast {
        argv.push(b"BCAST".to_vec());
    }
    if state.optin {
        argv.push(b"OPTIN".to_vec());
    }
    if state.optout {
        argv.push(b"OPTOUT".to_vec());
    }
    if state.noloop {
        argv.push(b"NOLOOP".to_vec());
    }
    for prefix in &state.prefixes {
        argv.push(b"PREFIX".to_vec());
        argv.push(prefix.clone());
    }
    argv
}

fn argv_from_raw(data: &[u8]) -> Vec<Vec<u8>> {
    let mut argv = Vec::new();
    let mut current = Vec::new();
    for &byte in data.iter().take(MAX_RAW_LEN) {
        if byte == 0 || byte.is_ascii_whitespace() {
            if !current.is_empty() {
                argv.push(limit_arg_len(std::mem::take(&mut current)));
                if argv.len() == MAX_ARGS {
                    break;
                }
            }
        } else {
            current.push(byte);
            if current.len() == MAX_ARG_LEN {
                argv.push(std::mem::take(&mut current));
                if argv.len() == MAX_ARGS {
                    break;
                }
            }
        }
    }
    if !current.is_empty() && argv.len() < MAX_ARGS {
        argv.push(limit_arg_len(current));
    }
    argv
}

fn reorder_chunks(chunks: &mut [Vec<Vec<u8>>], order_seed: u8) {
    if chunks.len() < 2 {
        return;
    }
    match order_seed % 3 {
        0 => {}
        1 => chunks.rotate_left(1),
        _ => chunks.reverse(),
    }
}

fn fallback_prefixes(prefixes: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut prefixes: Vec<Vec<u8>> = prefixes
        .into_iter()
        .map(limit_arg_len)
        .take(MAX_PREFIXES)
        .collect();
    if prefixes.is_empty() {
        prefixes.push(b"foo".to_vec());
    }
    prefixes
}

fn sanitize_invalid_mode(token: Vec<u8>) -> Vec<u8> {
    let mut token = limit_arg_len(token);
    if token.is_empty() {
        token = b"MAYBE".to_vec();
    }
    if token.eq_ignore_ascii_case(b"ON") || token.eq_ignore_ascii_case(b"OFF") {
        token.push(b'X');
    }
    token
}

fn limit_arg_len(mut arg: Vec<u8>) -> Vec<u8> {
    arg.truncate(MAX_ARG_LEN);
    arg
}

fn bulk(data: &[u8]) -> RespFrame {
    RespFrame::BulkString(Some(data.to_vec()))
}

fn bulk_owned(data: Vec<u8>) -> RespFrame {
    RespFrame::BulkString(Some(data))
}
