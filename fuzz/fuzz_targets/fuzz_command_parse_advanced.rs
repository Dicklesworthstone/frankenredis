#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_command::{parse_client_tracking_state, parse_migrate_request};
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Arbitrary)]
struct FuzzInput {
    args: Vec<Vec<u8>>,
}

fuzz_target!(|input: FuzzInput| {
    // Guard against excessively large inputs
    if input.args.len() > 1000 {
        return;
    }
    for arg in &input.args {
        if arg.len() > 10_000 {
            return;
        }
    }

    // Try parsing as MIGRATE request
    let _ = parse_migrate_request(&input.args);

    // Try parsing as CLIENT TRACKING state
    let _ = parse_client_tracking_state(&input.args);
});
