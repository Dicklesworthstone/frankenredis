#![no_main]

use libfuzzer_sys::fuzz_target;

use fr_persist::decode_aof_stream;

fuzz_target!(|data: &[u8]| {
    // Guard against excessively large inputs
    if data.len() > 10_000_000 {
        return;
    }

    // AOF decoder should never panic on any input
    let _ = decode_aof_stream(data);
});
