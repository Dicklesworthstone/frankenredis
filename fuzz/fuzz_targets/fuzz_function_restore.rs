#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_store::Store;
use libfuzzer_sys::fuzz_target;

const MAX_SOURCE_LEN: usize = 2_048;
const MAX_PAYLOAD_LEN: usize = 2_048;
const MAX_IDENT_LEN: usize = 32;
const MAX_FUNCTIONS: usize = 8;

#[derive(Debug, Arbitrary)]
struct ValidFunctionLibrary {
    library_name: Vec<u8>,
    registrations: Vec<FunctionRegistration>,
    replace_existing: bool,
}

#[derive(Debug, Arbitrary)]
struct FunctionRegistration {
    name: Vec<u8>,
    style: RegistrationStyle,
    include_comment: bool,
}

#[derive(Debug, Arbitrary)]
enum RegistrationStyle {
    Call,
    Table,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 4_096 {
        return;
    }

    let Some((&mode, body)) = data.split_first() else {
        return;
    };

    match mode % 3 {
        0 => fuzz_raw_function_source(body, mode),
        1 => {
            let mut unstructured = Unstructured::new(body);
            let Ok(case) = ValidFunctionLibrary::arbitrary(&mut unstructured) else {
                return;
            };
            fuzz_valid_function_library(case);
        }
        _ => fuzz_raw_function_restore(body),
    }
});

fn fuzz_raw_function_source(body: &[u8], mode: u8) {
    let source = truncate_bytes(body.to_vec(), MAX_SOURCE_LEN);
    let Ok(source) = std::str::from_utf8(&source) else {
        return;
    };

    let mut store = Store::new();
    if store
        .function_load(source.as_bytes(), mode & 0b1000 != 0)
        .is_ok()
    {
        assert_dump_restore_roundtrip(&store);
    }
}

fn fuzz_valid_function_library(case: ValidFunctionLibrary) {
    let library_name = sanitize_ident(case.library_name, "seedlib");
    let source = render_function_library(&library_name, case.registrations);
    let mut store = Store::new();

    if case.replace_existing {
        let shadow_source = render_seed_library(&library_name, "shadow");
        store
            .function_load(shadow_source.as_bytes(), false)
            .expect("seed shadow library must load");
    }

    store
        .function_load(source.as_bytes(), case.replace_existing)
        .expect("structure-aware function library must load");
    assert_dump_restore_roundtrip(&store);
}

fn fuzz_raw_function_restore(body: &[u8]) {
    let (policy_selector, payload) = body
        .split_first()
        .map_or((0, &[][..]), |(&head, tail)| (head, tail));
    let payload = truncate_bytes(payload.to_vec(), MAX_PAYLOAD_LEN);
    let policy = restore_policy(policy_selector);

    let mut store = Store::new();
    let sentinel_source = render_seed_library("sentinel", "sentinel_fn");
    store
        .function_load(sentinel_source.as_bytes(), false)
        .expect("seed sentinel library must load");
    let before = function_library_snapshot(&store);
    let result = store.function_restore(&payload, &policy);

    match result {
        Ok(()) => assert_dump_restore_roundtrip(&store),
        Err(_) => assert_eq!(
            function_library_snapshot(&store),
            before,
            "failed FUNCTION RESTORE must leave the preexisting libraries untouched",
        ),
    }
}

fn assert_dump_restore_roundtrip(store: &Store) {
    let expected = function_library_snapshot(store);
    let dumped = store.function_dump();

    let mut restored = Store::new();
    restored
        .function_restore(&dumped, "REPLACE")
        .expect("self-generated FUNCTION DUMP payload must restore");

    assert_eq!(
        function_library_snapshot(&restored),
        expected,
        "FUNCTION DUMP/RESTORE round-trip must preserve the library snapshot",
    );
}

fn function_library_snapshot(store: &Store) -> Vec<(String, String, Vec<u8>, Vec<String>)> {
    store
        .function_list(None)
        .into_iter()
        .map(|library| {
            (
                library.name.clone(),
                library.engine.clone(),
                library.code.clone(),
                library
                    .functions
                    .iter()
                    .map(|function| function.name.clone())
                    .collect(),
            )
        })
        .collect()
}

fn render_function_library(
    library_name: &str,
    mut registrations: Vec<FunctionRegistration>,
) -> String {
    registrations.truncate(MAX_FUNCTIONS);
    if registrations.is_empty() {
        registrations.push(FunctionRegistration {
            name: b"seed_fn".to_vec(),
            style: RegistrationStyle::Call,
            include_comment: false,
        });
    }

    let mut lines = vec![format!("#!lua name={library_name}")];
    let mut seen_names = std::collections::HashSet::new();
    for (index, registration) in registrations.into_iter().enumerate() {
        // Ensure unique function names by appending index if needed
        let mut function_name = sanitize_ident(registration.name, &format!("fn_{index}"));
        while seen_names.contains(&function_name) {
            function_name = format!("{}_{}", function_name, index);
        }
        seen_names.insert(function_name.clone());
        match registration.style {
            RegistrationStyle::Call => lines.push(format!(
                "redis.register_function('{function_name}', function(keys, args) return {} end)",
                index
            )),
            RegistrationStyle::Table => lines.push(format!(
                "redis.register_function{{function_name='{function_name}', callback=function(keys, args) return {} end}}",
                index
            )),
        }
        if registration.include_comment {
            lines.push(format!("-- fuzz registration {index}"));
        }
    }

    lines.join("\n")
}

fn render_seed_library(library_name: &str, function_name: &str) -> String {
    format!(
        "#!lua name={library_name}\nredis.register_function('{function_name}', function(keys, args) return #keys + #args end)\n"
    )
}

fn sanitize_ident(bytes: Vec<u8>, fallback: &str) -> String {
    // Library/function names only allow letters, numbers, and underscore (no dash)
    let filtered: String = bytes
        .into_iter()
        .filter_map(|byte| {
            let ch = byte as char;
            (ch.is_ascii_alphanumeric() || ch == '_').then_some(ch)
        })
        .take(MAX_IDENT_LEN)
        .collect();
    if filtered.is_empty() {
        fallback.to_string()
    } else {
        filtered
    }
}

fn truncate_bytes(mut bytes: Vec<u8>, max_len: usize) -> Vec<u8> {
    bytes.truncate(max_len);
    bytes
}

fn restore_policy(selector: u8) -> String {
    match selector % 4 {
        0 => "APPEND".to_string(),
        1 => "REPLACE".to_string(),
        2 => "FLUSH".to_string(),
        _ => "BOGUS".to_string(),
    }
}
