#![no_main]

use fr_runtime::{acl_list_entries_from_rules, canonicalize_acl_rules};
use libfuzzer_sys::fuzz_target;

const MAX_RAW_LEN: usize = 4_096;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_RAW_LEN {
        return;
    }

    let content = String::from_utf8_lossy(data);

    // Test canonicalize_acl_rules
    if let Ok(canonical) = canonicalize_acl_rules(&content) {
        // Canonical output must reparse to the same canonical form
        let reparsed =
            canonicalize_acl_rules(&canonical).expect("canonical ACL output must reparse");
        assert_eq!(
            reparsed, canonical,
            "ACL canonicalization must be idempotent"
        );
    }

    // Test acl_list_entries_from_rules independently
    let _ = acl_list_entries_from_rules(&content);
});
