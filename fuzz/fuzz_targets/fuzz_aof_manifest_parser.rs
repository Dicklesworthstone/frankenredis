#![no_main]
use fr_persist::parse_aof_manifest;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 1_000_000 {
        return;
    }
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = parse_aof_manifest(s);
    }
});
