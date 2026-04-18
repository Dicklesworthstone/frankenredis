#![no_main]
use libfuzzer_sys::fuzz_target;
use fr_sentinel::discovery::{HelloMessage, parse_replica_info_from_master};

fuzz_target!(|data: &[u8]| {
    if data.len() > 1_000_000 {
        return;
    }

    if let Ok(s) = std::str::from_utf8(data) {
        let _ = HelloMessage::parse(s);
        let _ = parse_replica_info_from_master(s);
    }
});
