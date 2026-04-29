#![no_main]

use libfuzzer_sys::fuzz_target;

use fr_protocol::{ParserConfig, parse_frame_with_config};

fuzz_target!(|data: &[u8]| {
    // Guard against excessively large inputs
    if data.len() > 1_000_000 {
        return;
    }

    // Test with default config
    let _ = fr_protocol::parse_frame(data);

    // Test with restrictive config (low limits to explore edge cases)
    let restrictive_config = ParserConfig {
        max_bulk_len: 512,
        max_array_len: 16,
        max_recursion_depth: 4,
        // RESP3 dialects are off by default — keep parity with the
        // production parser so the fuzzer exercises the
        // fail-closed prefix matrix on untrusted input.
        allow_resp3: false,
    };
    let _ = parse_frame_with_config(data, &restrictive_config);

    // Test with permissive config (also exercise the RESP3 path)
    let permissive_config = ParserConfig {
        max_bulk_len: 64 * 1024 * 1024,
        max_array_len: 1_000_000,
        max_recursion_depth: 32,
        allow_resp3: true,
    };
    let _ = parse_frame_with_config(data, &permissive_config);
});
