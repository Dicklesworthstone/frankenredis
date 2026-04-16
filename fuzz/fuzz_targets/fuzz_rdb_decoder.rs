#![no_main]

use libfuzzer_sys::fuzz_target;

use fr_persist::decode_rdb;

fuzz_target!(|data: &[u8]| {
    // Guard against excessively large inputs
    if data.len() > 10_000_000 {
        return;
    }

    // RDB decoder should never panic on any input
    let _ = decode_rdb(data);
});
