#![no_main]

use libfuzzer_sys::fuzz_target;

use fr_protocol::parse_frame;

fuzz_target!(|data: &[u8]| {
    // Guard against excessively large inputs
    if data.len() > 1_000_000 {
        return;
    }

    // Round-trip oracle: if we can parse it, encoding and re-parsing must yield same frame
    if let Ok(parsed) = parse_frame(data) {
        let encoded = parsed.frame.to_bytes();
        let reparsed = parse_frame(&encoded)
            .expect("re-encoding a parsed frame must be parseable");

        assert_eq!(
            parsed.frame, reparsed.frame,
            "Round-trip violation: parse(encode(frame)) != frame"
        );
    }
});
