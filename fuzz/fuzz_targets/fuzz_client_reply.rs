#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_command::{CommandError, apply_client_reply_state};
use fr_store::ClientReplyState;
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 4_096;
const MAX_RAW_LEN: usize = 2_048;
const MAX_COMMANDS: usize = 32;
const MAX_ARGS: usize = 8;
const MAX_ARG_LEN: usize = 96;

#[derive(Debug, Arbitrary)]
struct StructuredSequence {
    steps: Vec<StructuredStep>,
}

#[derive(Debug, Arbitrary)]
enum StructuredStep {
    ReplyOn,
    ReplyOff,
    ReplySkip,
    ReplyMissingMode,
    ReplyExtraArg {
        mode: StructuredMode,
        tail: Vec<u8>,
    },
    ReplyInvalidMode {
        token: Vec<u8>,
    },
    Echo {
        payload: Vec<u8>,
    },
    Ping,
    OtherClientSubcommand {
        subcommand: Vec<u8>,
        value: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum StructuredMode {
    On,
    Off,
    Skip,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    fuzz_raw_command_stream(data);

    let mut unstructured = Unstructured::new(data);
    let Ok(sequence) = StructuredSequence::arbitrary(&mut unstructured) else {
        return;
    };
    fuzz_structured_sequence(sequence);
});

fn fuzz_raw_command_stream(data: &[u8]) {
    let commands = commands_from_raw(data);
    if commands.is_empty() {
        return;
    }
    assert_sequence_matches_shadow_model(&commands);
}

fn fuzz_structured_sequence(sequence: StructuredSequence) {
    let commands: Vec<Vec<Vec<u8>>> = sequence
        .steps
        .into_iter()
        .map(render_step)
        .collect();
    if commands.is_empty() {
        return;
    }
    assert_sequence_matches_shadow_model(&commands);
}

fn assert_sequence_matches_shadow_model(commands: &[Vec<Vec<u8>>]) {
    let mut actual = ClientReplyState::default();
    let mut expected = ClientReplyState::default();

    for argv in commands {
        let actual_result = apply_client_reply_state(argv, &mut actual);
        let expected_result = model_apply_client_reply_state(argv, &mut expected);

        assert_eq!(
            actual_result, expected_result,
            "CLIENT REPLY result drifted for argv {:?}",
            argv
        );
        assert_eq!(
            actual, expected,
            "CLIENT REPLY state drifted for argv {:?}",
            argv
        );
        assert_postconditions(argv, &actual, &actual_result);
    }
}

fn model_apply_client_reply_state(
    argv: &[Vec<u8>],
    state: &mut ClientReplyState,
) -> Result<(), CommandError> {
    let prior_off = state.off;
    let prior_skip_next = state.skip_next;
    state.suppress_current_response = false;

    let is_client_reply = argv.len() >= 2
        && argv[0].eq_ignore_ascii_case(b"CLIENT")
        && argv[1].eq_ignore_ascii_case(b"REPLY");
    if !is_client_reply {
        state.suppress_current_response = prior_off || prior_skip_next;
        state.skip_next = false;
        return Ok(());
    }

    if argv.len() != 3 {
        state.suppress_current_response = prior_off || prior_skip_next;
        state.skip_next = false;
        return Err(reply_wrong_arity_error());
    }

    let mode = std::str::from_utf8(&argv[2]).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if mode.eq_ignore_ascii_case("ON") {
        state.off = false;
        state.skip_next = false;
        return Ok(());
    }
    if mode.eq_ignore_ascii_case("OFF") {
        state.off = true;
        state.skip_next = false;
        state.suppress_current_response = true;
        return Ok(());
    }
    if mode.eq_ignore_ascii_case("SKIP") {
        state.off = prior_off;
        state.skip_next = !prior_off;
        state.suppress_current_response = true;
        return Ok(());
    }

    state.suppress_current_response = prior_off || prior_skip_next;
    state.skip_next = false;
    Err(CommandError::SyntaxError)
}

fn assert_postconditions(
    argv: &[Vec<u8>],
    state: &ClientReplyState,
    result: &Result<(), CommandError>,
) {
    let is_client_reply = argv.len() >= 2
        && argv[0].eq_ignore_ascii_case(b"CLIENT")
        && argv[1].eq_ignore_ascii_case(b"REPLY");

    if !is_client_reply {
        assert!(
            !state.skip_next,
            "non-CLIENT REPLY commands must consume any pending SKIP"
        );
    }

    if let Ok(()) = result
        && is_client_reply
        && argv.len() == 3
    {
        let mode = &argv[2];
        if mode.eq_ignore_ascii_case(b"ON") {
            assert!(
                !state.off && !state.skip_next && !state.suppress_current_response,
                "CLIENT REPLY ON must clear suppression state"
            );
        } else if mode.eq_ignore_ascii_case(b"OFF") {
            assert!(
                state.off && !state.skip_next && state.suppress_current_response,
                "CLIENT REPLY OFF must enter persistent suppression mode"
            );
        } else if mode.eq_ignore_ascii_case(b"SKIP") {
            assert!(
                state.suppress_current_response,
                "CLIENT REPLY SKIP must suppress the current response"
            );
        }
    }
}

fn render_step(step: StructuredStep) -> Vec<Vec<u8>> {
    match step {
        StructuredStep::ReplyOn => client_reply_mode(b"ON"),
        StructuredStep::ReplyOff => client_reply_mode(b"OFF"),
        StructuredStep::ReplySkip => client_reply_mode(b"SKIP"),
        StructuredStep::ReplyMissingMode => vec![b"CLIENT".to_vec(), b"REPLY".to_vec()],
        StructuredStep::ReplyExtraArg { mode, tail } => vec![
            b"CLIENT".to_vec(),
            b"REPLY".to_vec(),
            structured_mode_bytes(mode).to_vec(),
            sanitize_tail(tail),
        ],
        StructuredStep::ReplyInvalidMode { token } => vec![
            b"CLIENT".to_vec(),
            b"REPLY".to_vec(),
            sanitize_invalid_mode(token),
        ],
        StructuredStep::Echo { payload } => vec![b"ECHO".to_vec(), limit_arg_len(payload)],
        StructuredStep::Ping => vec![b"PING".to_vec()],
        StructuredStep::OtherClientSubcommand { subcommand, value } => vec![
            b"CLIENT".to_vec(),
            sanitize_non_reply_subcommand(subcommand),
            limit_arg_len(value),
        ],
    }
}

fn commands_from_raw(data: &[u8]) -> Vec<Vec<Vec<u8>>> {
    let mut commands = Vec::new();
    let mut current_command = Vec::new();
    let mut current_arg = Vec::new();

    for &byte in data.iter().take(MAX_RAW_LEN) {
        if byte == b'\n' || byte == 0 {
            flush_arg(&mut current_command, &mut current_arg);
            flush_command(&mut commands, &mut current_command);
            if commands.len() == MAX_COMMANDS {
                break;
            }
        } else if byte.is_ascii_whitespace() {
            flush_arg(&mut current_command, &mut current_arg);
        } else if current_command.len() < MAX_ARGS {
            current_arg.push(byte);
            if current_arg.len() == MAX_ARG_LEN {
                flush_arg(&mut current_command, &mut current_arg);
            }
        }
    }

    flush_arg(&mut current_command, &mut current_arg);
    flush_command(&mut commands, &mut current_command);
    commands
}

fn flush_arg(command: &mut Vec<Vec<u8>>, current_arg: &mut Vec<u8>) {
    if current_arg.is_empty() || command.len() == MAX_ARGS {
        current_arg.clear();
        return;
    }
    command.push(limit_arg_len(std::mem::take(current_arg)));
}

fn flush_command(commands: &mut Vec<Vec<Vec<u8>>>, current_command: &mut Vec<Vec<u8>>) {
    if current_command.is_empty() || commands.len() == MAX_COMMANDS {
        current_command.clear();
        return;
    }
    commands.push(std::mem::take(current_command));
}

fn client_reply_mode(mode: &[u8]) -> Vec<Vec<u8>> {
    vec![b"CLIENT".to_vec(), b"REPLY".to_vec(), mode.to_vec()]
}

fn structured_mode_bytes(mode: StructuredMode) -> &'static [u8] {
    match mode {
        StructuredMode::On => b"ON",
        StructuredMode::Off => b"OFF",
        StructuredMode::Skip => b"SKIP",
    }
}

fn sanitize_invalid_mode(mut token: Vec<u8>) -> Vec<u8> {
    token.truncate(MAX_ARG_LEN);
    if token.is_empty() {
        token = b"MAYBE".to_vec();
    }
    if token.eq_ignore_ascii_case(b"ON")
        || token.eq_ignore_ascii_case(b"OFF")
        || token.eq_ignore_ascii_case(b"SKIP")
    {
        token.push(b'X');
    }
    token
}

fn sanitize_non_reply_subcommand(mut subcommand: Vec<u8>) -> Vec<u8> {
    subcommand.truncate(MAX_ARG_LEN);
    if subcommand.is_empty() {
        return b"INFO".to_vec();
    }
    if subcommand.eq_ignore_ascii_case(b"REPLY") {
        subcommand.push(b'X');
    }
    subcommand
}

fn sanitize_tail(mut tail: Vec<u8>) -> Vec<u8> {
    tail.truncate(MAX_ARG_LEN);
    if tail.is_empty() {
        tail = b"NOW".to_vec();
    }
    tail
}

fn limit_arg_len(mut arg: Vec<u8>) -> Vec<u8> {
    arg.truncate(MAX_ARG_LEN);
    arg
}

fn reply_wrong_arity_error() -> CommandError {
    CommandError::Custom("ERR wrong number of arguments for 'client|reply' command".to_string())
}
