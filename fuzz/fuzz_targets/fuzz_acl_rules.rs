#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_runtime::{acl_list_entries_from_rules, canonicalize_acl_rules};
use libfuzzer_sys::fuzz_target;

const MAX_USERS: usize = 8;
const MAX_RULES_PER_USER: usize = 12;
const MAX_TOKEN_LEN: usize = 24;
const MAX_RAW_LEN: usize = 2_048;

#[derive(Arbitrary, Debug)]
enum AclFuzzInput {
    Valid(ValidAclFileCase),
    Raw(Vec<u8>),
}

#[derive(Arbitrary, Debug)]
struct ValidAclFileCase {
    users: Vec<ValidAclUser>,
    include_comments: bool,
    include_blank_lines: bool,
}

#[derive(Arbitrary, Debug)]
struct ValidAclUser {
    username: Vec<u8>,
    rules: Vec<AclRuleSpec>,
}

#[derive(Arbitrary, Debug)]
enum AclRuleSpec {
    On,
    Off,
    Nopass,
    ResetPass,
    Reset,
    AllCommands,
    NoCommands,
    AllKeys,
    AllChannels,
    AllowCategory(Vec<u8>),
    DenyCategory(Vec<u8>),
    AllowCommand(Vec<u8>),
    DenyCommand(Vec<u8>),
    AddPassword(Vec<u8>),
    RemovePassword(Vec<u8>),
    KeyPattern(Vec<u8>),
    ChannelPattern(Vec<u8>),
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 4_096 {
        return;
    }

    let mut unstructured = Unstructured::new(data);
    let Ok(input) = AclFuzzInput::arbitrary(&mut unstructured) else {
        return;
    };

    match input {
        AclFuzzInput::Valid(case) => fuzz_valid_acl_rules(case),
        AclFuzzInput::Raw(raw) => fuzz_raw_acl_rules(raw),
    }
});

fn fuzz_valid_acl_rules(case: ValidAclFileCase) {
    let content = render_acl_file(case);
    let canonical =
        canonicalize_acl_rules(&content).expect("structure-aware ACL content must always parse");
    let list_entries = acl_list_entries_from_rules(&content)
        .expect("structure-aware ACL content must always list");

    let reparsed =
        canonicalize_acl_rules(&canonical).expect("canonical ACL serialization must reparse");
    assert_eq!(
        reparsed, canonical,
        "canonical ACL serialization must be stable across reparses"
    );

    let canonical_list =
        acl_list_entries_from_rules(&canonical).expect("canonical ACL serialization must list");
    assert_eq!(
        canonical_list, list_entries,
        "ACL LIST output must remain stable after ACL SAVE canonicalization"
    );
}

fn fuzz_raw_acl_rules(raw: Vec<u8>) {
    let mut raw = raw;
    raw.truncate(MAX_RAW_LEN);
    let content = String::from_utf8_lossy(&raw);
    if let Ok(canonical) = canonicalize_acl_rules(&content) {
        let reparsed =
            canonicalize_acl_rules(&canonical).expect("successful canonical ACL must reparse");
        assert_eq!(
            reparsed, canonical,
            "raw ACL inputs that parse must stabilize under canonicalization"
        );
    }
}

fn render_acl_file(case: ValidAclFileCase) -> String {
    let mut lines = Vec::new();
    if case.include_comments {
        lines.push("# fuzz seed".to_string());
    }
    if case.include_blank_lines {
        lines.push(String::new());
    }

    let mut users = case.users;
    users.truncate(MAX_USERS);
    if users.is_empty() {
        users.push(ValidAclUser {
            username: b"alice".to_vec(),
            rules: vec![
                AclRuleSpec::Reset,
                AclRuleSpec::On,
                AclRuleSpec::AddPassword(b"pass".to_vec()),
                AclRuleSpec::NoCommands,
                AclRuleSpec::AllowCommand(b"get".to_vec()),
                AclRuleSpec::AllKeys,
                AclRuleSpec::AllChannels,
            ],
        });
    }

    for user in users {
        let username = sanitize_ident(user.username, "user");
        let mut parts = vec!["user".to_string(), username];
        let mut rules = user.rules;
        rules.truncate(MAX_RULES_PER_USER);
        if rules.is_empty() {
            rules.push(AclRuleSpec::Reset);
            rules.push(AclRuleSpec::On);
            rules.push(AclRuleSpec::Nopass);
            rules.push(AclRuleSpec::AllCommands);
        }
        parts.extend(rules.into_iter().map(render_rule));
        lines.push(parts.join(" "));
    }

    lines.join("\n")
}

fn render_rule(rule: AclRuleSpec) -> String {
    match rule {
        AclRuleSpec::On => "on".to_string(),
        AclRuleSpec::Off => "off".to_string(),
        AclRuleSpec::Nopass => "nopass".to_string(),
        AclRuleSpec::ResetPass => "resetpass".to_string(),
        AclRuleSpec::Reset => "reset".to_string(),
        AclRuleSpec::AllCommands => "+@all".to_string(),
        AclRuleSpec::NoCommands => "-@all".to_string(),
        AclRuleSpec::AllKeys => "~*".to_string(),
        AclRuleSpec::AllChannels => "&*".to_string(),
        AclRuleSpec::AllowCategory(cat) => format!("+@{}", sanitize_ident(cat, "read")),
        AclRuleSpec::DenyCategory(cat) => format!("-@{}", sanitize_ident(cat, "write")),
        AclRuleSpec::AllowCommand(cmd) => format!("+{}", sanitize_ident(cmd, "get")),
        AclRuleSpec::DenyCommand(cmd) => format!("-{}", sanitize_ident(cmd, "set")),
        AclRuleSpec::AddPassword(pass) => format!(">{}", sanitize_token(pass, "pass")),
        AclRuleSpec::RemovePassword(pass) => format!("<{}", sanitize_token(pass, "pass")),
        AclRuleSpec::KeyPattern(pattern) => render_pattern('~', pattern, "k"),
        AclRuleSpec::ChannelPattern(pattern) => render_pattern('&', pattern, "chan"),
    }
}

fn render_pattern(prefix: char, pattern: Vec<u8>, fallback: &str) -> String {
    let token = sanitize_ident(pattern, fallback);
    if token == "*" {
        format!("{prefix}*")
    } else {
        format!("{prefix}{token}:*")
    }
}

fn sanitize_ident(bytes: Vec<u8>, fallback: &str) -> String {
    let token = sanitize_token(bytes, fallback);
    let filtered: String = token
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':'))
        .collect();
    if filtered.is_empty() {
        fallback.to_string()
    } else {
        filtered
    }
}

fn sanitize_token(bytes: Vec<u8>, fallback: &str) -> String {
    let mut token: String = bytes
        .into_iter()
        .filter_map(|byte| {
            let ch = byte as char;
            (ch.is_ascii_graphic() && !ch.is_ascii_whitespace()).then_some(ch)
        })
        .take(MAX_TOKEN_LEN)
        .collect();
    if token.is_empty() {
        token.push_str(fallback);
    }
    token
}
