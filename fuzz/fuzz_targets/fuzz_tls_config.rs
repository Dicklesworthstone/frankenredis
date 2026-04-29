#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_config::{
    TlsAuthClients, TlsConfig, TlsDirective, TlsListenerTransition, TlsProtocol, TlsRuntimeState,
    parse_tls_protocols, plan_tls_runtime_apply, rewrite_tls_directives, validate_tls_config,
    validate_tls_directive_value,
};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 4_096;
const MAX_RAW_LEN: usize = 256;
const MAX_PROTOCOL_ENTRIES: usize = 8;
const MAX_TEXT_LEN: usize = 64;

#[derive(Debug, Arbitrary)]
struct StructuredTlsCase {
    protocol_list: RawProtocolListCase,
    directive: RawDirectiveCase,
    config: RawConfigPlanCase,
}

#[derive(Debug, Arbitrary)]
struct RawProtocolListCase {
    entries: Vec<RawProtocolToken>,
    separators: Vec<u8>,
    leading_separator: u8,
    trailing_separator: u8,
}

#[derive(Debug, Clone, Arbitrary)]
enum RawProtocolToken {
    TlsV1_2(u8),
    TlsV1_3(u8),
    Invalid(Vec<u8>),
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum RawDirectiveKind {
    TlsPort,
    TlsCertFile,
    TlsKeyFile,
    TlsCaFile,
    TlsProtocols,
    TlsCiphers,
    TlsAuthClients,
    ClusterAnnounceTlsPort,
    MaxNewTlsConnectionsPerCycle,
    // TLS session-resumption directives — added when TlsConfig
    // grew session_caching / session_cache_size / cache_timeout.
    TlsSessionCaching,
    TlsSessionCacheSize,
    TlsSessionCacheTimeout,
}

#[derive(Debug, Arbitrary)]
struct RawDirectiveCase {
    kind: RawDirectiveKind,
    valid_hint: bool,
    number: u16,
    text: Vec<u8>,
    protocol_list: RawProtocolListCase,
    auth_seed: u8,
}

#[derive(Debug, Arbitrary)]
struct RawConfigPlanCase {
    candidate: RawTlsConfig,
    current: RawRuntimeStateCase,
}

#[derive(Debug, Clone, Arbitrary)]
struct RawTlsConfig {
    tls_port: Option<u16>,
    cert_file: Option<Vec<u8>>,
    key_file: Option<Vec<u8>>,
    ca_file: Option<Vec<u8>>,
    protocols: Vec<RawProtocolChoice>,
    ciphers: Option<Vec<u8>>,
    auth_seed: u8,
    cluster_announce_tls_port: Option<u16>,
    max_new_tls_connections_per_cycle: u16,
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum RawProtocolChoice {
    TlsV1_2,
    TlsV1_3,
}

#[derive(Debug, Arbitrary)]
struct RawRuntimeStateCase {
    active_config: Option<RawTlsConfig>,
    tls_listener_enabled: bool,
    tcp_listener_enabled: bool,
    connection_type_configured: bool,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    fuzz_raw_protocols(data);

    let mut unstructured = Unstructured::new(data);
    let Ok(case) = StructuredTlsCase::arbitrary(&mut unstructured) else {
        return;
    };

    fuzz_protocol_list(case.protocol_list);
    fuzz_directive_case(case.directive);
    fuzz_config_case(case.config);
});

fn fuzz_raw_protocols(data: &[u8]) {
    let mut raw = data.to_vec();
    raw.truncate(MAX_RAW_LEN);
    let raw = String::from_utf8_lossy(&raw);

    let parse_result = parse_tls_protocols(&raw);
    let validate_result = validate_tls_directive_value(TlsDirective::TlsProtocols, &raw);
    assert_eq!(
        parse_result.is_ok(),
        validate_result.is_ok(),
        "tls-protocols directive validation must mirror protocol parsing",
    );

    if let Ok(protocols) = parse_result {
        assert_canonical_protocol_roundtrip(&protocols);
    }
}

fn fuzz_protocol_list(case: RawProtocolListCase) {
    let (rendered, expected, is_valid) = render_protocol_list_case(&case);
    let parse_result = parse_tls_protocols(&rendered);
    assert_eq!(
        parse_result.is_ok(),
        is_valid,
        "structured tls-protocols rendering must classify valid and invalid inputs consistently",
    );

    let directive_result = validate_tls_directive_value(TlsDirective::TlsProtocols, &rendered);
    assert_eq!(
        directive_result.is_ok(),
        is_valid,
        "tls-protocols directive validation must mirror parser acceptance",
    );

    if let Ok(protocols) = parse_result {
        assert_eq!(
            protocols, expected,
            "parser must preserve first-seen protocol order while deduplicating aliases",
        );
        assert_canonical_protocol_roundtrip(&protocols);
    }
}

fn fuzz_directive_case(case: RawDirectiveCase) {
    let directive = case.kind.directive();
    let (value, expected_valid) = render_directive_value(&case);
    let result = validate_tls_directive_value(directive, &value);
    assert_eq!(
        result.is_ok(),
        expected_valid,
        "directive value classification must stay stable for generated inputs",
    );

    match directive {
        TlsDirective::TlsPort | TlsDirective::ClusterAnnounceTlsPort => {
            if expected_valid {
                assert!(value.parse::<u16>().is_ok());
            } else {
                assert!(value.parse::<u16>().is_err());
            }
        }
        TlsDirective::TlsCertFile
        | TlsDirective::TlsKeyFile
        | TlsDirective::TlsCaFile
        | TlsDirective::TlsCiphers => {
            if expected_valid {
                assert!(!value.trim().is_empty());
            } else {
                assert!(value.trim().is_empty());
            }
        }
        TlsDirective::TlsProtocols => {
            let parse_result = parse_tls_protocols(&value);
            assert_eq!(
                parse_result.is_ok(),
                expected_valid,
                "tls-protocols directive generation must agree with parser acceptance",
            );
            if let Ok(protocols) = parse_result {
                assert_canonical_protocol_roundtrip(&protocols);
            }
        }
        TlsDirective::TlsAuthClients => {
            let parse_result = TlsAuthClients::parse(&value);
            assert_eq!(
                parse_result.is_some(),
                expected_valid,
                "tls-auth-clients parser must agree with directive validation",
            );
        }
        TlsDirective::MaxNewTlsConnectionsPerCycle => {
            let parse_result = value.parse::<usize>();
            assert_eq!(
                parse_result.is_ok_and(|parsed| parsed > 0),
                expected_valid,
                "max-new-tls-connections-per-cycle validation must reject zero and non-numeric values",
            );
        }
        TlsDirective::TlsSessionCaching => {
            // Bool: "yes" / "on" / "1" / etc. accepted; anything
            // else rejected. We can't reproduce the exact accept
            // set without the parser, so just check that the
            // value reproducibly parses to the expected verdict.
            let parsed_ok = matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "yes" | "y" | "true" | "1" | "on" | "no" | "n" | "false" | "0" | "off"
            );
            assert_eq!(
                parsed_ok, expected_valid,
                "tls-session-caching validation must agree with the parser's bool acceptance",
            );
        }
        TlsDirective::TlsSessionCacheSize | TlsDirective::TlsSessionCacheTimeout => {
            let parse_result = value.parse::<usize>();
            assert_eq!(
                parse_result.is_ok(),
                expected_valid,
                "tls-session-cache-size/timeout validation must agree with usize parse",
            );
        }
    }
}

fn fuzz_config_case(case: RawConfigPlanCase) {
    let candidate = build_candidate_config(&case.candidate);
    let current = build_runtime_state(&case.current);

    match validate_tls_config(&candidate) {
        Ok(()) => {
            let rewrite = rewrite_tls_directives(&candidate)
                .expect("validated TLS config must rewrite deterministically");
            assert_eq!(
                rewrite_tls_directives(&candidate),
                Ok(rewrite.clone()),
                "rewriting a validated TLS config must be deterministic",
            );
            assert_rewrite_matches_config(&candidate, &rewrite);

            let plan = plan_tls_runtime_apply(&current, candidate.clone())
                .expect("validated TLS config must yield a runtime apply plan");
            let expected_transition =
                expected_listener_transition(current.tls_listener_enabled, candidate.tls_enabled());
            let expected_swap = current.active_config.as_ref() != Some(&candidate);
            let expected_configure =
                candidate.tls_enabled() && (expected_swap || !current.connection_type_configured);

            assert_eq!(plan.candidate_config, candidate);
            assert_eq!(plan.listener_transition, expected_transition);
            assert_eq!(plan.requires_context_swap, expected_swap);
            assert_eq!(
                plan.requires_connection_type_configure, expected_configure,
                "runtime apply planning must only request connection-type reconfiguration when TLS is active and state changed",
            );
        }
        Err(_) => {
            assert!(
                rewrite_tls_directives(&candidate).is_err(),
                "invalid TLS configs must not persist as rewritten directives",
            );
            assert!(
                plan_tls_runtime_apply(&current, candidate).is_err(),
                "invalid TLS configs must not yield a runtime apply plan",
            );
        }
    }
}

fn render_protocol_list_case(case: &RawProtocolListCase) -> (String, Vec<TlsProtocol>, bool) {
    let mut rendered = String::new();
    rendered.push_str(optional_separator(case.leading_separator));

    let mut expected = Vec::new();
    let mut is_valid = true;
    let mut saw_entry = false;

    for (index, token) in case.entries.iter().take(MAX_PROTOCOL_ENTRIES).enumerate() {
        if index > 0 {
            let separator_seed = case.separators.get(index - 1).copied().unwrap_or_default();
            rendered.push_str(required_separator(separator_seed));
        }

        let (text, protocol) = render_protocol_token(token);
        rendered.push_str(&text);
        saw_entry = true;

        if let Some(protocol) = protocol {
            if !expected.contains(&protocol) {
                expected.push(protocol);
            }
        } else {
            is_valid = false;
        }
    }

    rendered.push_str(optional_separator(case.trailing_separator));

    if !saw_entry || expected.is_empty() {
        is_valid = false;
    }

    (rendered, expected, is_valid)
}

fn render_valid_protocol_list(case: &RawProtocolListCase) -> (String, Vec<TlsProtocol>) {
    let mut rendered = String::new();
    rendered.push_str(optional_separator(case.leading_separator));

    let mut expected = Vec::new();
    let mut count = 0usize;

    for (index, token) in case.entries.iter().take(MAX_PROTOCOL_ENTRIES).enumerate() {
        if index > 0 {
            let separator_seed = case.separators.get(index - 1).copied().unwrap_or_default();
            rendered.push_str(required_separator(separator_seed));
        }

        let protocol = match token {
            RawProtocolToken::TlsV1_2(_) => TlsProtocol::TlsV1_2,
            RawProtocolToken::TlsV1_3(_) => TlsProtocol::TlsV1_3,
            RawProtocolToken::Invalid(_) => {
                if index % 2 == 0 {
                    TlsProtocol::TlsV1_2
                } else {
                    TlsProtocol::TlsV1_3
                }
            }
        };
        rendered.push_str(render_protocol_alias(protocol, index as u8));
        if !expected.contains(&protocol) {
            expected.push(protocol);
        }
        count += 1;
    }

    if count == 0 {
        rendered.push_str(render_protocol_alias(TlsProtocol::TlsV1_2, 0));
        expected.push(TlsProtocol::TlsV1_2);
    }

    rendered.push_str(optional_separator(case.trailing_separator));
    (rendered, expected)
}

fn render_protocol_token(token: &RawProtocolToken) -> (String, Option<TlsProtocol>) {
    match token {
        RawProtocolToken::TlsV1_2(seed) => (
            render_protocol_alias(TlsProtocol::TlsV1_2, *seed).to_string(),
            Some(TlsProtocol::TlsV1_2),
        ),
        RawProtocolToken::TlsV1_3(seed) => (
            render_protocol_alias(TlsProtocol::TlsV1_3, *seed).to_string(),
            Some(TlsProtocol::TlsV1_3),
        ),
        RawProtocolToken::Invalid(bytes) => (sanitize_invalid_token(bytes, "TLSv1.4"), None),
    }
}

fn render_directive_value(case: &RawDirectiveCase) -> (String, bool) {
    match case.kind.directive() {
        TlsDirective::TlsPort | TlsDirective::ClusterAnnounceTlsPort => {
            if case.valid_hint {
                (case.number.to_string(), true)
            } else {
                (sanitize_invalid_numeric(&case.text, "invalid-port"), false)
            }
        }
        TlsDirective::TlsCertFile => render_text_directive(case.valid_hint, &case.text, "cert.pem"),
        TlsDirective::TlsKeyFile => render_text_directive(case.valid_hint, &case.text, "key.pem"),
        TlsDirective::TlsCaFile => render_text_directive(case.valid_hint, &case.text, "ca.pem"),
        TlsDirective::TlsCiphers => {
            render_text_directive(case.valid_hint, &case.text, "HIGH:!aNULL")
        }
        TlsDirective::TlsProtocols => {
            if case.valid_hint {
                let (value, _) = render_valid_protocol_list(&case.protocol_list);
                (value, true)
            } else if case.text.first().is_some_and(|byte| byte % 2 == 0) {
                (blank_string(case.auth_seed), false)
            } else {
                (sanitize_invalid_token(&case.text, "TLSv1.4"), false)
            }
        }
        TlsDirective::TlsAuthClients => {
            if case.valid_hint {
                (render_auth_alias(case.auth_seed).to_string(), true)
            } else {
                (sanitize_invalid_token(&case.text, "maybe"), false)
            }
        }
        TlsDirective::MaxNewTlsConnectionsPerCycle => {
            if case.valid_hint {
                (usize::from(case.number).max(1).to_string(), true)
            } else if case.number.is_multiple_of(2) {
                ("0".to_string(), false)
            } else {
                (
                    sanitize_invalid_numeric(&case.text, "invalid-budget"),
                    false,
                )
            }
        }
        // ── TLS session-resumption directives ────────────────────
        TlsDirective::TlsSessionCaching => {
            if case.valid_hint {
                let token = if case.auth_seed.is_multiple_of(2) {
                    "yes"
                } else {
                    "no"
                };
                (token.to_string(), true)
            } else {
                (sanitize_invalid_token(&case.text, "maybe"), false)
            }
        }
        TlsDirective::TlsSessionCacheSize | TlsDirective::TlsSessionCacheTimeout => {
            // Both are usize-parseable; valid → any non-negative
            // number; invalid → bytes that don't parse as usize.
            if case.valid_hint {
                (usize::from(case.number).to_string(), true)
            } else {
                (
                    sanitize_invalid_numeric(&case.text, "invalid-tls-session-int"),
                    false,
                )
            }
        }
    }
}

fn render_text_directive(valid_hint: bool, text: &[u8], fallback: &str) -> (String, bool) {
    if valid_hint {
        (sanitize_nonempty_text(text, fallback), true)
    } else {
        (
            blank_string(text.first().copied().unwrap_or_default()),
            false,
        )
    }
}

fn build_candidate_config(raw: &RawTlsConfig) -> TlsConfig {
    TlsConfig {
        tls_port: raw.tls_port,
        cert_file: sanitize_material(raw.cert_file.as_deref(), "cert.pem"),
        key_file: sanitize_material(raw.key_file.as_deref(), "key.pem"),
        ca_file: sanitize_material(raw.ca_file.as_deref(), "ca.pem"),
        protocols: sanitize_protocol_choices(&raw.protocols),
        ciphers: sanitize_material(raw.ciphers.as_deref(), "HIGH:!aNULL"),
        auth_clients: auth_mode(raw.auth_seed),
        cluster_announce_tls_port: raw.cluster_announce_tls_port,
        max_new_tls_connections_per_cycle: usize::from(raw.max_new_tls_connections_per_cycle),
        // TLS session-resumption knobs were added to TlsConfig
        // alongside the directive registry expansion. Fuzz with
        // upstream defaults (caching on, 20 KiB session cache,
        // 300 s timeout) so candidate configs the harness builds
        // pass the same validation gate the production loader
        // uses.
        session_caching: true,
        session_cache_size: 20 * 1024,
        session_cache_timeout_sec: 300,
    }
}

fn build_runtime_state(raw: &RawRuntimeStateCase) -> TlsRuntimeState {
    TlsRuntimeState {
        active_config: raw.active_config.as_ref().map(build_valid_runtime_config),
        tls_listener_enabled: raw.tls_listener_enabled,
        tcp_listener_enabled: raw.tcp_listener_enabled,
        connection_type_configured: raw.connection_type_configured,
    }
}

fn build_valid_runtime_config(raw: &RawTlsConfig) -> TlsConfig {
    let mut config = build_candidate_config(raw);

    if config.protocols.is_empty() {
        config.protocols.push(TlsProtocol::TlsV1_2);
    }

    if config.max_new_tls_connections_per_cycle == 0 {
        config.max_new_tls_connections_per_cycle = 1;
    }

    if config.tls_enabled() {
        fill_required_material(&mut config.cert_file, "cert.pem");
        fill_required_material(&mut config.key_file, "key.pem");
        fill_required_material(&mut config.ca_file, "ca.pem");
        fill_required_material(&mut config.ciphers, "HIGH:!aNULL");
    } else {
        config.cert_file = None;
        config.key_file = None;
        config.ca_file = None;
        config.ciphers = None;
        config.cluster_announce_tls_port = None;
    }

    config
}

fn fill_required_material(slot: &mut Option<String>, fallback: &str) {
    if slot.as_ref().is_none_or(|value| value.trim().is_empty()) {
        *slot = Some(fallback.to_string());
    }
}

fn sanitize_material(bytes: Option<&[u8]>, fallback: &str) -> Option<String> {
    let bytes = bytes?;
    if bytes.first().is_some_and(|byte| byte % 7 == 0) {
        return Some(blank_string(bytes.first().copied().unwrap_or_default()));
    }
    Some(sanitize_nonempty_text(bytes, fallback))
}

fn sanitize_protocol_choices(choices: &[RawProtocolChoice]) -> Vec<TlsProtocol> {
    let mut protocols = Vec::new();
    for choice in choices.iter().take(MAX_PROTOCOL_ENTRIES) {
        let protocol = match choice {
            RawProtocolChoice::TlsV1_2 => TlsProtocol::TlsV1_2,
            RawProtocolChoice::TlsV1_3 => TlsProtocol::TlsV1_3,
        };
        if !protocols.contains(&protocol) {
            protocols.push(protocol);
        }
    }
    protocols
}

fn auth_mode(seed: u8) -> TlsAuthClients {
    match seed % 3 {
        0 => TlsAuthClients::Off,
        1 => TlsAuthClients::Optional,
        _ => TlsAuthClients::Required,
    }
}

fn render_protocol_alias(protocol: TlsProtocol, seed: u8) -> &'static str {
    match (protocol, seed % 3) {
        (TlsProtocol::TlsV1_2, 0) => "TLSv1.2",
        (TlsProtocol::TlsV1_2, 1) => "tls1.2",
        (TlsProtocol::TlsV1_2, _) => "TLSv1_2",
        (TlsProtocol::TlsV1_3, 0) => "TLSv1.3",
        (TlsProtocol::TlsV1_3, 1) => "tls1.3",
        (TlsProtocol::TlsV1_3, _) => "TLSv1_3",
    }
}

fn render_auth_alias(seed: u8) -> &'static str {
    match seed % 6 {
        0 => "off",
        1 => "no",
        2 => "optional",
        3 => "yes",
        4 => "on",
        _ => "required",
    }
}

fn expected_listener_transition(
    current_tls_listener_enabled: bool,
    target_tls_listener_enabled: bool,
) -> TlsListenerTransition {
    match (current_tls_listener_enabled, target_tls_listener_enabled) {
        (false, true) => TlsListenerTransition::Enable,
        (true, false) => TlsListenerTransition::Disable,
        (false, false) | (true, true) => TlsListenerTransition::Keep,
    }
}

fn assert_rewrite_matches_config(config: &TlsConfig, directives: &[(String, String)]) {
    let names: Vec<&str> = directives.iter().map(|(name, _)| name.as_str()).collect();
    let mut expected_names = vec![
        "tls-port",
        "tls-protocols",
        "tls-auth-clients",
        "max-new-tls-connections-per-cycle",
    ];
    if config.tls_enabled() {
        expected_names.extend([
            "tls-cert-file",
            "tls-key-file",
            "tls-ca-file",
            "tls-ciphers",
        ]);
    }
    if config.cluster_announce_tls_port.is_some() {
        expected_names.push("cluster-announce-tls-port");
    }
    assert_eq!(
        names, expected_names,
        "rewritten TLS directives must stay in the documented stable order",
    );

    assert_eq!(
        directive_value(directives, "tls-port"),
        Some(config.tls_port.unwrap_or(0).to_string()),
    );
    assert_eq!(
        directive_value(directives, "tls-auth-clients"),
        Some(config.auth_clients.as_token().to_string()),
    );
    assert_eq!(
        directive_value(directives, "max-new-tls-connections-per-cycle"),
        Some(config.max_new_tls_connections_per_cycle.to_string()),
    );

    let rewritten_protocols = directive_value(directives, "tls-protocols")
        .expect("validated TLS config rewrite must emit tls-protocols");
    assert_eq!(
        parse_tls_protocols(&rewritten_protocols),
        Ok(config.protocols.clone()),
        "rewritten tls-protocols must parse back to the candidate config",
    );

    if config.tls_enabled() {
        assert_eq!(
            directive_value(directives, "tls-cert-file"),
            config.cert_file.clone(),
        );
        assert_eq!(
            directive_value(directives, "tls-key-file"),
            config.key_file.clone(),
        );
        assert_eq!(
            directive_value(directives, "tls-ca-file"),
            config.ca_file.clone(),
        );
        assert_eq!(
            directive_value(directives, "tls-ciphers"),
            config.ciphers.clone(),
        );
    }

    if let Some(port) = config.cluster_announce_tls_port {
        assert_eq!(
            directive_value(directives, "cluster-announce-tls-port"),
            Some(port.to_string()),
        );
    }
}

fn directive_value(directives: &[(String, String)], name: &str) -> Option<String> {
    directives
        .iter()
        .find_map(|(directive_name, value)| (directive_name == name).then(|| value.clone()))
}

fn assert_canonical_protocol_roundtrip(protocols: &[TlsProtocol]) {
    let canonical = protocols
        .iter()
        .map(|protocol| protocol.as_token())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        parse_tls_protocols(&canonical),
        Ok(protocols.to_vec()),
        "accepted protocol sets must survive canonical rewrite and reparse",
    );
}

fn sanitize_nonempty_text(bytes: &[u8], fallback: &str) -> String {
    let text: String = bytes
        .iter()
        .filter_map(|byte| {
            let ch = *byte as char;
            (ch.is_ascii_graphic() && !ch.is_ascii_whitespace()).then_some(ch)
        })
        .take(MAX_TEXT_LEN)
        .collect();
    if text.is_empty() {
        fallback.to_string()
    } else {
        text
    }
}

fn sanitize_invalid_token(bytes: &[u8], fallback: &str) -> String {
    let token = sanitize_nonempty_text(bytes, fallback);
    if TlsProtocol::parse(&token).is_some() || TlsAuthClients::parse(&token).is_some() {
        format!("x{token}")
    } else {
        token
    }
}

fn sanitize_invalid_numeric(bytes: &[u8], fallback: &str) -> String {
    let token = sanitize_nonempty_text(bytes, fallback);
    if token.chars().all(|ch| ch.is_ascii_digit()) {
        format!("x{token}")
    } else {
        token
    }
}

fn blank_string(seed: u8) -> String {
    match seed % 3 {
        0 => String::new(),
        1 => " ".to_string(),
        _ => "\t".to_string(),
    }
}

fn optional_separator(seed: u8) -> &'static str {
    match seed % 5 {
        0 => "",
        1 => " ",
        2 => ",",
        3 => "\t",
        _ => "\n",
    }
}

fn required_separator(seed: u8) -> &'static str {
    match seed % 5 {
        0 => " ",
        1 => ",",
        2 => ", ",
        3 => "\t",
        _ => "\n",
    }
}

impl RawDirectiveKind {
    fn directive(self) -> TlsDirective {
        match self {
            Self::TlsPort => TlsDirective::TlsPort,
            Self::TlsCertFile => TlsDirective::TlsCertFile,
            Self::TlsKeyFile => TlsDirective::TlsKeyFile,
            Self::TlsCaFile => TlsDirective::TlsCaFile,
            Self::TlsProtocols => TlsDirective::TlsProtocols,
            Self::TlsCiphers => TlsDirective::TlsCiphers,
            Self::TlsAuthClients => TlsDirective::TlsAuthClients,
            Self::ClusterAnnounceTlsPort => TlsDirective::ClusterAnnounceTlsPort,
            Self::MaxNewTlsConnectionsPerCycle => TlsDirective::MaxNewTlsConnectionsPerCycle,
            Self::TlsSessionCaching => TlsDirective::TlsSessionCaching,
            Self::TlsSessionCacheSize => TlsDirective::TlsSessionCacheSize,
            Self::TlsSessionCacheTimeout => TlsDirective::TlsSessionCacheTimeout,
        }
    }
}
