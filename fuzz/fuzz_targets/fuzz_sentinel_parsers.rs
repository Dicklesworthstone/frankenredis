#![no_main]
use libfuzzer_sys::fuzz_target;
use fr_sentinel::discovery::{HelloMessage, parse_replica_info_from_master};
use fr_sentinel::health::parse_info_response;

fuzz_target!(|data: &[u8]| {
    if data.len() > 1_000_000 {
        return;
    }

    if let Ok(s) = std::str::from_utf8(data) {
        let _ = HelloMessage::parse(s);
        let _ = parse_replica_info_from_master(s);
        let _ = parse_info_response(s);
    }
});
