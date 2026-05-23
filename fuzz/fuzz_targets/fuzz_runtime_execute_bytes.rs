#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_protocol::{ParserConfig, RespFrame, parse_frame, parse_frame_with_config};
use fr_runtime::Runtime;
use libfuzzer_sys::fuzz_target;

const MAX_RAW_LEN: usize = 8_192;
const MAX_ARGC: usize = 32;
const MAX_ARG_LEN: usize = 256;
const NOW_MS: u64 = 1_000;

#[derive(Debug, Arbitrary)]
struct StructuredCommand {
    argv: Vec<Vec<u8>>,
    now_offset_ms: u16,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 16_384 {
        return;
    }

    let Some((&mode, body)) = data.split_first() else {
        return;
    };

    match mode % 2 {
        0 => fuzz_raw_bytes(body.to_vec()),
        _ => {
            let mut unstructured = Unstructured::new(body);
            let Ok(case) = StructuredCommand::arbitrary(&mut unstructured) else {
                return;
            };
            fuzz_valid_command(case);
        }
    }
});

fn fuzz_valid_command(case: StructuredCommand) {
    let argv = normalize_argv(case.argv);
    let frame = argv_to_frame(argv.clone());
    if command_has_nondeterministic_output(&frame) {
        return;
    }
    let input = frame.to_bytes();
    let now_ms = NOW_MS + u64::from(case.now_offset_ms);

    let mut runtime_bytes = Runtime::default_strict();
    let mut runtime_frame = Runtime::default_strict();
    let from_bytes = decode_all_frames(&runtime_bytes.execute_bytes(&input, now_ms));
    let from_frame = decode_all_frames(&runtime_frame.execute_frame(frame, now_ms).to_bytes());

    assert_eq!(
        from_bytes, from_frame,
        "execute_bytes must match execute_frame for well-formed command arrays"
    );
}

fn fuzz_raw_bytes(raw: Vec<u8>) {
    let raw = truncate_bytes(raw, MAX_RAW_LEN);
    let mut runtime = Runtime::default_strict();
    let output = runtime.execute_bytes(&raw, NOW_MS);
    let output_frames = decode_all_frames(&output);

    if let Ok(parsed) = parse_frame_with_config(&raw, &default_runtime_parser_config()) {
        // Use a fresh runtime for the frame path so we compare semantics,
        // not ephemeral per-runtime state. Commands with non-deterministic
        // output (INFO, CLIENT ID, etc.) are filtered out below.
        if command_has_nondeterministic_output(&parsed.frame) {
            return;
        }
        let mut runtime_frame = Runtime::default_strict();
        let expected = runtime_frame.execute_frame(parsed.frame, NOW_MS).to_bytes();
        let expected_frames = decode_all_frames(&expected);
        assert_eq!(
            output_frames, expected_frames,
            "execute_bytes must match execute_frame for any raw input whose first frame parses under the live runtime parser limits"
        );
    }
}

fn command_has_nondeterministic_output(frame: &RespFrame) -> bool {
    let RespFrame::Array(Some(parts)) = frame else {
        return false;
    };
    let Some(RespFrame::BulkString(Some(cmd))) = parts.first() else {
        return false;
    };
    // INFO contains run_id, process_id, uptime, etc. that differ per-runtime
    if cmd.eq_ignore_ascii_case(b"INFO") {
        return true;
    }
    // HELLO response includes server info with run_id
    if cmd.eq_ignore_ascii_case(b"HELLO") {
        return true;
    }
    // CLIENT ID returns per-runtime client counter
    if cmd.eq_ignore_ascii_case(b"CLIENT") {
        if let Some(RespFrame::BulkString(Some(sub))) = parts.get(1) {
            if sub.eq_ignore_ascii_case(b"ID") || sub.eq_ignore_ascii_case(b"INFO") {
                return true;
            }
        }
    }
    // DEBUG SLEEP, DEBUG STRUCTSIZE vary by runtime
    if cmd.eq_ignore_ascii_case(b"DEBUG") {
        return true;
    }
    // TIME varies
    if cmd.eq_ignore_ascii_case(b"TIME") {
        return true;
    }
    // RANDOMKEY varies by hash seed
    if cmd.eq_ignore_ascii_case(b"RANDOMKEY") {
        return true;
    }
    false
}

fn argv_to_frame(argv: Vec<Vec<u8>>) -> RespFrame {
    RespFrame::Array(Some(
        argv.into_iter()
            .map(|arg| RespFrame::BulkString(Some(arg)))
            .collect(),
    ))
}

fn normalize_argv(mut argv: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    argv.truncate(MAX_ARGC);
    for arg in &mut argv {
        arg.truncate(MAX_ARG_LEN);
    }
    if argv.is_empty() {
        argv.push(b"PING".to_vec());
    } else if argv[0].is_empty() {
        argv[0] = b"PING".to_vec();
    }
    argv
}

fn truncate_bytes(mut bytes: Vec<u8>, max_len: usize) -> Vec<u8> {
    bytes.truncate(max_len);
    bytes
}

fn default_runtime_parser_config() -> ParserConfig {
    ParserConfig {
        max_bulk_len: 8 * 1024 * 1024,
        max_array_len: 1024,
        max_recursion_depth: 128,
        // The runtime's wire-side parser is fail-closed on RESP3
        // prefixes from untrusted input (matches the production
        // parser default).
        allow_resp3: false,
    }
}

fn decode_all_frames(bytes: &[u8]) -> Vec<RespFrame> {
    let mut frames = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let parsed = parse_frame(&bytes[offset..]).expect("runtime output must remain valid RESP");
        assert!(
            parsed.consumed > 0,
            "runtime output parser must make progress"
        );
        offset += parsed.consumed;
        frames.push(parsed.frame);
    }
    frames
}
