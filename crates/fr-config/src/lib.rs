#![forbid(unsafe_code)]

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Strict,
    Hardened,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HardenedDeviationCategory {
    /// Allow additional parser diagnostics while preserving wire-level error behavior.
    BoundedParserDiagnostics,
    /// Allow bounded replay repair for recoverable persistence edge cases.
    BoundedReplayRepair,
    /// Allow deterministic resource clamps for hostile or malformed load spikes.
    ResourceClamp,
    /// Allow metadata sanitization when compatibility metadata is malformed.
    MetadataSanitization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThreatClass {
    ParserAbuse,
    MetadataAmbiguity,
    VersionSkew,
    ResourceExhaustion,
    PersistenceTampering,
    ReplicationOrderAttack,
    AuthPolicyConfusion,
    ConfigDowngradeAbuse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriftSeverity {
    S0,
    S1,
    S2,
    S3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecisionAction {
    FailClosed,
    BoundedDefense,
    RejectNonAllowlisted,
}

pub const HARDENED_ALLOWLIST_DEFAULT: [HardenedDeviationCategory; 4] = [
    HardenedDeviationCategory::BoundedParserDiagnostics,
    HardenedDeviationCategory::BoundedReplayRepair,
    HardenedDeviationCategory::ResourceClamp,
    HardenedDeviationCategory::MetadataSanitization,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibilityGate {
    pub max_array_len: usize,
    pub max_bulk_len: usize,
}

impl Default for CompatibilityGate {
    fn default() -> Self {
        Self {
            max_array_len: 1024,
            max_bulk_len: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePolicy {
    pub mode: Mode,
    pub gate: CompatibilityGate,
    pub emit_evidence_ledger: bool,
    pub hardened_allowlist: Vec<HardenedDeviationCategory>,
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            mode: Mode::Strict,
            gate: CompatibilityGate::default(),
            emit_evidence_ledger: true,
            hardened_allowlist: Vec::new(),
        }
    }
}

impl RuntimePolicy {
    #[must_use]
    pub fn hardened() -> Self {
        Self {
            mode: Mode::Hardened,
            hardened_allowlist: HARDENED_ALLOWLIST_DEFAULT.to_vec(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn is_deviation_allowed(&self, category: HardenedDeviationCategory) -> bool {
        self.mode == Mode::Hardened && self.hardened_allowlist.contains(&category)
    }

    #[must_use]
    pub fn decide(
        &self,
        threat: ThreatClass,
        preferred_deviation: Option<HardenedDeviationCategory>,
    ) -> (DecisionAction, DriftSeverity) {
        match self.mode {
            Mode::Strict => (DecisionAction::FailClosed, DriftSeverity::S0),
            Mode::Hardened => match preferred_deviation {
                Some(category) if self.is_deviation_allowed(category) => {
                    (DecisionAction::BoundedDefense, DriftSeverity::S1)
                }
                Some(_) => (DecisionAction::RejectNonAllowlisted, DriftSeverity::S2),
                None => match threat {
                    ThreatClass::ParserAbuse
                    | ThreatClass::MetadataAmbiguity
                    | ThreatClass::VersionSkew
                    | ThreatClass::ResourceExhaustion
                    | ThreatClass::PersistenceTampering
                    | ThreatClass::ReplicationOrderAttack
                    | ThreatClass::AuthPolicyConfusion
                    | ThreatClass::ConfigDowngradeAbuse => {
                        (DecisionAction::FailClosed, DriftSeverity::S0)
                    }
                },
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedConfigFile {
    pub directives: Vec<ParsedConfigDirective>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedConfigDirective {
    pub line_number: usize,
    pub name: Vec<u8>,
    pub args: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFileParseError {
    pub line_number: usize,
    pub reason: ConfigFileParseErrorReason,
    pub line: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFileParseErrorReason {
    InvalidQuotedToken,
}

const INVALID_QUOTED_TOKEN_REASON: ConfigFileParseErrorReason =
    ConfigFileParseErrorReason::InvalidQuotedToken;

impl ConfigFileParseError {
    #[must_use]
    pub fn reason_code(&self) -> &'static str {
        match self.reason {
            INVALID_QUOTED_TOKEN_REASON => "configfile.invalid_quoted_token",
        }
    }
}

impl fmt::Display for ConfigFileParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            INVALID_QUOTED_TOKEN_REASON => write!(
                f,
                "{} at line {}: unbalanced quotes or quote followed by non-whitespace",
                self.reason_code(),
                self.line_number
            ),
        }
    }
}

impl std::error::Error for ConfigFileParseError {}

pub fn parse_redis_config(input: &str) -> Result<ParsedConfigFile, ConfigFileParseError> {
    parse_redis_config_bytes(input.as_bytes())
}

pub fn parse_redis_config_bytes(input: &[u8]) -> Result<ParsedConfigFile, ConfigFileParseError> {
    let input = input
        .iter()
        .position(|byte| *byte == 0)
        .map_or(input, |nul_pos| &input[..nul_pos]);
    let mut directives = Vec::new();

    for (line_idx, raw_line) in input.split(|byte| *byte == b'\n').enumerate() {
        let line_number = line_idx + 1;
        let line = trim_redis_config_line(raw_line);
        if line.is_empty() || line.first() == Some(&b'#') {
            continue;
        }

        let mut tokens =
            split_config_line_args_bytes(line).map_err(|reason| ConfigFileParseError {
                line_number,
                reason,
                line: String::from_utf8_lossy(line).into_owned(),
            })?;

        if tokens.is_empty() {
            continue;
        }

        let mut name = tokens.remove(0);
        name.make_ascii_lowercase();
        directives.push(ParsedConfigDirective {
            line_number,
            name,
            args: tokens,
        });
    }

    Ok(ParsedConfigFile { directives })
}

pub fn split_config_line_args(line: &str) -> Result<Vec<Vec<u8>>, ConfigFileParseErrorReason> {
    split_config_line_args_bytes(line.as_bytes())
}

pub fn split_config_line_args_bytes(
    bytes: &[u8],
) -> Result<Vec<Vec<u8>>, ConfigFileParseErrorReason> {
    let mut pos = 0;
    let mut args = Vec::new();

    loop {
        while byte_at(bytes, pos) != 0 && is_c_isspace(byte_at(bytes, pos)) {
            pos += 1;
        }

        if byte_at(bytes, pos) == 0 {
            return Ok(args);
        }

        let mut current = Vec::new();
        let mut in_double_quote = false;
        let mut in_single_quote = false;
        let mut done = false;

        while !done {
            let byte = byte_at(bytes, pos);
            if in_double_quote {
                if byte == b'\\'
                    && byte_at(bytes, pos + 1) == b'x'
                    && is_ascii_hex(byte_at(bytes, pos + 2))
                    && is_ascii_hex(byte_at(bytes, pos + 3))
                {
                    let decoded = (hex_value(byte_at(bytes, pos + 2)) << 4)
                        + hex_value(byte_at(bytes, pos + 3));
                    current.push(decoded);
                    pos += 3;
                } else if byte == b'\\' && byte_at(bytes, pos + 1) != 0 {
                    pos += 1;
                    current.push(match byte_at(bytes, pos) {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'b' => 0x08,
                        b'a' => 0x07,
                        other => other,
                    });
                } else if byte == b'"' {
                    if byte_at(bytes, pos + 1) != 0 && !is_c_isspace(byte_at(bytes, pos + 1)) {
                        return Err(ConfigFileParseErrorReason::InvalidQuotedToken);
                    }
                    done = true;
                } else if byte == 0 {
                    return Err(ConfigFileParseErrorReason::InvalidQuotedToken);
                } else {
                    current.push(byte);
                }
            } else if in_single_quote {
                if byte == b'\\' && byte_at(bytes, pos + 1) == b'\'' {
                    pos += 1;
                    current.push(b'\'');
                } else if byte == b'\'' {
                    if byte_at(bytes, pos + 1) != 0 && !is_c_isspace(byte_at(bytes, pos + 1)) {
                        return Err(ConfigFileParseErrorReason::InvalidQuotedToken);
                    }
                    done = true;
                } else if byte == 0 {
                    return Err(ConfigFileParseErrorReason::InvalidQuotedToken);
                } else {
                    current.push(byte);
                }
            } else {
                match byte {
                    b' ' | b'\n' | b'\r' | b'\t' | 0 => done = true,
                    b'"' => in_double_quote = true,
                    b'\'' => in_single_quote = true,
                    other => current.push(other),
                }
            }

            if byte != 0 {
                pos += 1;
            }
        }

        args.push(current);
    }
}

fn trim_redis_config_line(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .map_or(start, |idx| idx + 1);
    &line[start..end]
}

fn byte_at(bytes: &[u8], pos: usize) -> u8 {
    bytes.get(pos).copied().unwrap_or(0)
}

fn is_c_isspace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\n' | b'\r' | b'\t' | 0x0b | 0x0c)
}

fn is_ascii_hex(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TlsProtocol {
    TlsV1_2,
    TlsV1_3,
}

impl TlsProtocol {
    #[must_use]
    pub fn as_token(self) -> &'static str {
        match self {
            Self::TlsV1_2 => "TLSv1.2",
            Self::TlsV1_3 => "TLSv1.3",
        }
    }

    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        if token.eq_ignore_ascii_case("tlsv1.2")
            || token.eq_ignore_ascii_case("tls1.2")
            || token.eq_ignore_ascii_case("tlsv1_2")
        {
            return Some(Self::TlsV1_2);
        }
        if token.eq_ignore_ascii_case("tlsv1.3")
            || token.eq_ignore_ascii_case("tls1.3")
            || token.eq_ignore_ascii_case("tlsv1_3")
        {
            return Some(Self::TlsV1_3);
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsAuthClients {
    Off,
    Optional,
    Required,
}

impl TlsAuthClients {
    #[must_use]
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Optional => "optional",
            Self::Required => "yes",
        }
    }

    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        if token.eq_ignore_ascii_case("off") || token.eq_ignore_ascii_case("no") {
            return Some(Self::Off);
        }
        if token.eq_ignore_ascii_case("optional") {
            return Some(Self::Optional);
        }
        if token.eq_ignore_ascii_case("yes")
            || token.eq_ignore_ascii_case("on")
            || token.eq_ignore_ascii_case("required")
        {
            return Some(Self::Required);
        }
        None
    }
}

#[must_use]
pub fn parse_tls_bool(token: &str) -> Option<bool> {
    if token.eq_ignore_ascii_case("yes") || token.eq_ignore_ascii_case("on") {
        return Some(true);
    }
    if token.eq_ignore_ascii_case("no") || token.eq_ignore_ascii_case("off") {
        return Some(false);
    }
    None
}

#[must_use]
pub fn tls_bool_token(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    pub tls_port: Option<u16>,
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
    pub ca_file: Option<String>,
    pub protocols: Vec<TlsProtocol>,
    pub ciphers: Option<String>,
    pub auth_clients: TlsAuthClients,
    pub session_caching: bool,
    pub session_cache_size: usize,
    pub session_cache_timeout_sec: usize,
    pub cluster_announce_tls_port: Option<u16>,
    pub max_new_tls_connections_per_cycle: usize,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            tls_port: None,
            cert_file: None,
            key_file: None,
            ca_file: None,
            protocols: vec![TlsProtocol::TlsV1_2, TlsProtocol::TlsV1_3],
            ciphers: None,
            auth_clients: TlsAuthClients::Required,
            session_caching: true,
            session_cache_size: 20 * 1024,
            session_cache_timeout_sec: 300,
            cluster_announce_tls_port: None,
            max_new_tls_connections_per_cycle: 1_000,
        }
    }
}

impl TlsConfig {
    #[must_use]
    pub fn tls_enabled(&self) -> bool {
        self.tls_port.is_some_and(|port| port != 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsCfgError {
    ProtocolsParseContractViolation(String),
    ContextBuildContractViolation(String),
    AtomicReconfigureViolation(String),
    ListenerBootstrapContractViolation(String),
    HandshakeVerifyPolicyViolation(String),
    TlsIoStateTransitionViolation(String),
    TlsIoBudgetErrnoContractViolation(String),
    PeerIdentityContractViolation(String),
    DirectiveRegistryContractViolation(String),
    BindAtomicityViolation(String),
    RuntimeApplyContractViolation(String),
    ConnectionTypeConfigureViolation(String),
    OperationalKnobContractViolation(String),
    RewritePersistenceViolation(String),
    SafetyGateContractViolation(String),
    HardenedDeviationRejected(String),
    HardenedPolicyViolation(String),
}

impl TlsCfgError {
    #[must_use]
    pub fn reason_code(&self) -> &'static str {
        match self {
            Self::ProtocolsParseContractViolation(_) => "tlscfg.protocols_parse_contract_violation",
            Self::ContextBuildContractViolation(_) => "tlscfg.context_build_contract_violation",
            Self::AtomicReconfigureViolation(_) => "tlscfg.atomic_reconfigure_violation",
            Self::ListenerBootstrapContractViolation(_) => {
                "tls.listener_bootstrap_contract_violation"
            }
            Self::HandshakeVerifyPolicyViolation(_) => "tls.handshake_verify_policy_violation",
            Self::TlsIoStateTransitionViolation(_) => "tls.io_state_transition_violation",
            Self::TlsIoBudgetErrnoContractViolation(_) => "tls.io_budget_errno_contract_violation",
            Self::PeerIdentityContractViolation(_) => "tls.peer_identity_contract_violation",
            Self::DirectiveRegistryContractViolation(_) => {
                "tlscfg.directive_registry_contract_violation"
            }
            Self::BindAtomicityViolation(_) => "tlscfg.bind_atomicity_violation",
            Self::RuntimeApplyContractViolation(_) => "tlscfg.runtime_apply_contract_violation",
            Self::ConnectionTypeConfigureViolation(_) => {
                "tlscfg.connection_type_configure_violation"
            }
            Self::OperationalKnobContractViolation(_) => {
                "tlscfg.operational_knob_contract_violation"
            }
            Self::RewritePersistenceViolation(_) => "tlscfg.rewrite_persistence_violation",
            Self::SafetyGateContractViolation(_) => "tlscfg.safety_gate_contract_violation",
            Self::HardenedDeviationRejected(_) => "tlscfg.hardened_nonallowlisted_rejected",
            Self::HardenedPolicyViolation(_) => "tlscfg.hardened_policy_violation",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::ProtocolsParseContractViolation(detail)
            | Self::ContextBuildContractViolation(detail)
            | Self::AtomicReconfigureViolation(detail)
            | Self::ListenerBootstrapContractViolation(detail)
            | Self::HandshakeVerifyPolicyViolation(detail)
            | Self::TlsIoStateTransitionViolation(detail)
            | Self::TlsIoBudgetErrnoContractViolation(detail)
            | Self::PeerIdentityContractViolation(detail)
            | Self::DirectiveRegistryContractViolation(detail)
            | Self::BindAtomicityViolation(detail)
            | Self::RuntimeApplyContractViolation(detail)
            | Self::ConnectionTypeConfigureViolation(detail)
            | Self::OperationalKnobContractViolation(detail)
            | Self::RewritePersistenceViolation(detail)
            | Self::SafetyGateContractViolation(detail)
            | Self::HardenedDeviationRejected(detail)
            | Self::HardenedPolicyViolation(detail) => detail,
        }
    }
}

impl fmt::Display for TlsCfgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason_code(), self.detail())
    }
}

impl std::error::Error for TlsCfgError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsDirective {
    TlsPort,
    TlsCertFile,
    TlsKeyFile,
    TlsCaFile,
    TlsProtocols,
    TlsCiphers,
    TlsAuthClients,
    TlsSessionCaching,
    TlsSessionCacheSize,
    TlsSessionCacheTimeout,
    ClusterAnnounceTlsPort,
    MaxNewTlsConnectionsPerCycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsDirectivePolicy {
    pub mutable_at_runtime: bool,
    pub sensitive: bool,
}

#[must_use]
pub fn tls_directive_policy(directive: TlsDirective) -> TlsDirectivePolicy {
    match directive {
        TlsDirective::TlsPort => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: false,
        },
        TlsDirective::TlsCertFile => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: false,
        },
        TlsDirective::TlsKeyFile => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: true,
        },
        TlsDirective::TlsCaFile => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: false,
        },
        TlsDirective::TlsProtocols => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: false,
        },
        TlsDirective::TlsCiphers => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: false,
        },
        TlsDirective::TlsAuthClients => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: false,
        },
        TlsDirective::TlsSessionCaching => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: false,
        },
        TlsDirective::TlsSessionCacheSize => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: false,
        },
        TlsDirective::TlsSessionCacheTimeout => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: false,
        },
        TlsDirective::ClusterAnnounceTlsPort => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: false,
        },
        TlsDirective::MaxNewTlsConnectionsPerCycle => TlsDirectivePolicy {
            mutable_at_runtime: true,
            sensitive: false,
        },
    }
}

pub fn validate_tls_directive_value(
    directive: TlsDirective,
    value: &str,
) -> Result<(), TlsCfgError> {
    match directive {
        TlsDirective::TlsPort | TlsDirective::ClusterAnnounceTlsPort => {
            value.parse::<u16>().map_err(|_| {
                TlsCfgError::DirectiveRegistryContractViolation(format!(
                    "invalid port value '{value}'"
                ))
            })?;
            Ok(())
        }
        TlsDirective::TlsCertFile
        | TlsDirective::TlsKeyFile
        | TlsDirective::TlsCaFile
        | TlsDirective::TlsCiphers => {
            if value.trim().is_empty() {
                return Err(TlsCfgError::DirectiveRegistryContractViolation(
                    "empty TLS string directive".to_string(),
                ));
            }
            Ok(())
        }
        TlsDirective::TlsProtocols => {
            parse_tls_protocols(value)?;
            Ok(())
        }
        TlsDirective::TlsAuthClients => {
            TlsAuthClients::parse(value).ok_or_else(|| {
                TlsCfgError::DirectiveRegistryContractViolation(format!(
                    "invalid tls-auth-clients value '{value}'"
                ))
            })?;
            Ok(())
        }
        TlsDirective::TlsSessionCaching => {
            parse_tls_bool(value).ok_or_else(|| {
                TlsCfgError::DirectiveRegistryContractViolation(format!(
                    "invalid tls-session-caching value '{value}'"
                ))
            })?;
            Ok(())
        }
        TlsDirective::TlsSessionCacheSize => {
            value.parse::<usize>().map_err(|_| {
                TlsCfgError::OperationalKnobContractViolation(format!(
                    "invalid tls-session-cache-size value '{value}'"
                ))
            })?;
            Ok(())
        }
        TlsDirective::TlsSessionCacheTimeout => {
            value.parse::<usize>().map_err(|_| {
                TlsCfgError::OperationalKnobContractViolation(format!(
                    "invalid tls-session-cache-timeout value '{value}'"
                ))
            })?;
            Ok(())
        }
        TlsDirective::MaxNewTlsConnectionsPerCycle => {
            let parsed = value.parse::<usize>().map_err(|_| {
                TlsCfgError::OperationalKnobContractViolation(format!(
                    "invalid max-new-tls-connections-per-cycle value '{value}'"
                ))
            })?;
            if parsed == 0 {
                return Err(TlsCfgError::OperationalKnobContractViolation(
                    "max-new-tls-connections-per-cycle must be > 0".to_string(),
                ));
            }
            Ok(())
        }
    }
}

pub fn parse_tls_protocols(raw: &str) -> Result<Vec<TlsProtocol>, TlsCfgError> {
    let mut out = Vec::new();
    for token in raw
        .split(|ch: char| ch == ',' || ch.is_ascii_whitespace())
        .filter(|token| !token.is_empty())
    {
        let protocol = TlsProtocol::parse(token).ok_or_else(|| {
            TlsCfgError::ProtocolsParseContractViolation(format!("unsupported protocol '{token}'"))
        })?;
        if !out.contains(&protocol) {
            out.push(protocol);
        }
    }

    if out.is_empty() {
        return Err(TlsCfgError::ProtocolsParseContractViolation(
            "empty tls-protocols list".to_string(),
        ));
    }

    Ok(out)
}

pub fn validate_tls_config(config: &TlsConfig) -> Result<(), TlsCfgError> {
    if config.protocols.is_empty() {
        return Err(TlsCfgError::ProtocolsParseContractViolation(
            "tls-protocols must not be empty".to_string(),
        ));
    }

    if config.max_new_tls_connections_per_cycle == 0 {
        return Err(TlsCfgError::OperationalKnobContractViolation(
            "max-new-tls-connections-per-cycle must be > 0".to_string(),
        ));
    }

    if config.cluster_announce_tls_port.is_some() && !config.tls_enabled() {
        return Err(TlsCfgError::OperationalKnobContractViolation(
            "cluster-announce-tls-port requires tls-port".to_string(),
        ));
    }

    if config.tls_enabled() {
        let context_material = [
            ("tls-cert-file", config.cert_file.as_deref()),
            ("tls-key-file", config.key_file.as_deref()),
            ("tls-ca-file", config.ca_file.as_deref()),
            ("tls-ciphers", config.ciphers.as_deref()),
        ];
        for (directive, value) in context_material {
            if value.is_none_or(|inner| inner.trim().is_empty()) {
                return Err(TlsCfgError::ContextBuildContractViolation(format!(
                    "missing required TLS context field '{directive}'"
                )));
            }
        }
        return Ok(());
    }

    if config.cert_file.is_some()
        || config.key_file.is_some()
        || config.ca_file.is_some()
        || config.ciphers.is_some()
    {
        return Err(TlsCfgError::SafetyGateContractViolation(
            "TLS material is configured while tls-port is disabled".to_string(),
        ));
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TlsRuntimeState {
    pub active_config: Option<TlsConfig>,
    pub tls_listener_enabled: bool,
    pub tcp_listener_enabled: bool,
    pub connection_type_configured: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsListenerTransition {
    Enable,
    Disable,
    Keep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsApplyPlan {
    pub candidate_config: TlsConfig,
    pub listener_transition: TlsListenerTransition,
    pub requires_context_swap: bool,
    pub requires_connection_type_configure: bool,
}

pub fn plan_tls_runtime_apply(
    current: &TlsRuntimeState,
    candidate: TlsConfig,
) -> Result<TlsApplyPlan, TlsCfgError> {
    validate_tls_config(&candidate)?;
    let target_listener_enabled = candidate.tls_enabled();
    let listener_transition = match (current.tls_listener_enabled, target_listener_enabled) {
        (false, true) => TlsListenerTransition::Enable,
        (true, false) => TlsListenerTransition::Disable,
        (false, false) | (true, true) => TlsListenerTransition::Keep,
    };
    let requires_context_swap = current.active_config.as_ref() != Some(&candidate);
    let requires_connection_type_configure =
        target_listener_enabled && (requires_context_swap || !current.connection_type_configured);

    Ok(TlsApplyPlan {
        candidate_config: candidate,
        listener_transition,
        requires_context_swap,
        requires_connection_type_configure,
    })
}

pub fn validate_bind_transition_atomicity(
    before: &TlsRuntimeState,
    after: &TlsRuntimeState,
    apply_succeeded: bool,
) -> Result<(), TlsCfgError> {
    if apply_succeeded {
        return Ok(());
    }

    if before.tcp_listener_enabled != after.tcp_listener_enabled
        || before.tls_listener_enabled != after.tls_listener_enabled
    {
        return Err(TlsCfgError::BindAtomicityViolation(
            "failed bind mutated TCP/TLS listener state".to_string(),
        ));
    }

    Ok(())
}

pub fn rewrite_tls_directives(config: &TlsConfig) -> Result<Vec<(String, String)>, TlsCfgError> {
    validate_tls_config(config)?;
    let mut directives = Vec::with_capacity(12);
    directives.push((
        "tls-port".to_string(),
        config.tls_port.unwrap_or(0).to_string(),
    ));
    directives.push((
        "tls-protocols".to_string(),
        config
            .protocols
            .iter()
            .map(|protocol| protocol.as_token())
            .collect::<Vec<_>>()
            .join(" "),
    ));
    directives.push((
        "tls-auth-clients".to_string(),
        config.auth_clients.as_token().to_string(),
    ));
    directives.push((
        "max-new-tls-connections-per-cycle".to_string(),
        config.max_new_tls_connections_per_cycle.to_string(),
    ));
    directives.push((
        "tls-session-caching".to_string(),
        tls_bool_token(config.session_caching).to_string(),
    ));
    directives.push((
        "tls-session-cache-size".to_string(),
        config.session_cache_size.to_string(),
    ));
    directives.push((
        "tls-session-cache-timeout".to_string(),
        config.session_cache_timeout_sec.to_string(),
    ));

    if config.tls_enabled() {
        directives.push((
            "tls-cert-file".to_string(),
            config.cert_file.clone().ok_or_else(|| {
                TlsCfgError::RewritePersistenceViolation(
                    "tls-cert-file missing for enabled TLS".to_string(),
                )
            })?,
        ));
        directives.push((
            "tls-key-file".to_string(),
            config.key_file.clone().ok_or_else(|| {
                TlsCfgError::RewritePersistenceViolation(
                    "tls-key-file missing for enabled TLS".to_string(),
                )
            })?,
        ));
        directives.push((
            "tls-ca-file".to_string(),
            config.ca_file.clone().ok_or_else(|| {
                TlsCfgError::RewritePersistenceViolation(
                    "tls-ca-file missing for enabled TLS".to_string(),
                )
            })?,
        ));
        directives.push((
            "tls-ciphers".to_string(),
            config.ciphers.clone().ok_or_else(|| {
                TlsCfgError::RewritePersistenceViolation(
                    "tls-ciphers missing for enabled TLS".to_string(),
                )
            })?,
        ));
    }

    if let Some(cluster_tls_port) = config.cluster_announce_tls_port {
        directives.push((
            "cluster-announce-tls-port".to_string(),
            cluster_tls_port.to_string(),
        ));
    }

    Ok(directives)
}

pub fn evaluate_tls_hardened_deviation(
    policy: &RuntimePolicy,
    deviation: HardenedDeviationCategory,
) -> Result<DecisionAction, TlsCfgError> {
    let (action, _) = policy.decide(ThreatClass::ConfigDowngradeAbuse, Some(deviation));

    if policy.mode == Mode::Hardened {
        return match action {
            DecisionAction::BoundedDefense => Ok(action),
            DecisionAction::RejectNonAllowlisted => Err(TlsCfgError::HardenedDeviationRejected(
                format!("deviation '{deviation:?}' is not allowlisted"),
            )),
            DecisionAction::FailClosed => Err(TlsCfgError::HardenedPolicyViolation(
                "hardened policy unexpectedly returned fail_closed".to_string(),
            )),
        };
    }

    Ok(action)
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigFileParseErrorReason, DecisionAction, DriftSeverity, HARDENED_ALLOWLIST_DEFAULT,
        HardenedDeviationCategory, Mode, RuntimePolicy, ThreatClass, TlsAuthClients, TlsCfgError,
        TlsConfig, TlsDirective, TlsListenerTransition, TlsProtocol, TlsRuntimeState,
        evaluate_tls_hardened_deviation, parse_redis_config, parse_redis_config_bytes,
        parse_tls_bool, parse_tls_protocols, plan_tls_runtime_apply, rewrite_tls_directives,
        split_config_line_args, split_config_line_args_bytes, tls_bool_token, tls_directive_policy,
        validate_bind_transition_atomicity, validate_tls_config, validate_tls_directive_value,
    };

    #[test]
    fn default_policy_is_strict() {
        let policy = RuntimePolicy::default();
        assert_eq!(policy.mode, Mode::Strict);
        assert!(policy.emit_evidence_ledger);
        assert!(policy.hardened_allowlist.is_empty());
        assert!(!policy.is_deviation_allowed(HardenedDeviationCategory::ResourceClamp));
    }

    #[test]
    fn hardened_policy_uses_explicit_allowlist() {
        let policy = RuntimePolicy::hardened();
        assert_eq!(policy.mode, Mode::Hardened);
        assert_eq!(
            policy.hardened_allowlist,
            HARDENED_ALLOWLIST_DEFAULT.to_vec()
        );
        for category in HARDENED_ALLOWLIST_DEFAULT {
            assert!(policy.is_deviation_allowed(category));
        }
    }

    #[test]
    fn strict_mode_decision_is_fail_closed_s0() {
        let policy = RuntimePolicy::default();
        let (action, severity) = policy.decide(ThreatClass::ParserAbuse, None);
        assert_eq!(action, DecisionAction::FailClosed);
        assert_eq!(severity, DriftSeverity::S0);
    }

    #[test]
    fn hardened_mode_respects_allowlist_for_bounded_defense() {
        let policy = RuntimePolicy::hardened();
        let (action, severity) = policy.decide(
            ThreatClass::ResourceExhaustion,
            Some(HardenedDeviationCategory::ResourceClamp),
        );
        assert_eq!(action, DecisionAction::BoundedDefense);
        assert_eq!(severity, DriftSeverity::S1);
    }

    #[test]
    fn hardened_mode_rejects_non_allowlisted_deviation() {
        let mut policy = RuntimePolicy::hardened();
        policy.hardened_allowlist.clear();
        let (action, severity) = policy.decide(
            ThreatClass::ResourceExhaustion,
            Some(HardenedDeviationCategory::ResourceClamp),
        );
        assert_eq!(action, DecisionAction::RejectNonAllowlisted);
        assert_eq!(severity, DriftSeverity::S2);
    }

    #[test]
    fn fr_p2c_002_u013_hardened_gate_rejects_non_allowlisted_parser_drift() {
        let mut policy = RuntimePolicy::hardened();
        policy
            .hardened_allowlist
            .retain(|category| *category != HardenedDeviationCategory::BoundedParserDiagnostics);
        let (action, severity) = policy.decide(
            ThreatClass::ParserAbuse,
            Some(HardenedDeviationCategory::BoundedParserDiagnostics),
        );
        assert_eq!(action, DecisionAction::RejectNonAllowlisted);
        assert_eq!(severity, DriftSeverity::S2);
    }

    #[test]
    fn fr_p2c_002_u013_strict_mode_parser_drift_is_fail_closed() {
        let policy = RuntimePolicy::default();
        let (action, severity) = policy.decide(
            ThreatClass::ParserAbuse,
            Some(HardenedDeviationCategory::BoundedParserDiagnostics),
        );
        assert_eq!(action, DecisionAction::FailClosed);
        assert_eq!(severity, DriftSeverity::S0);
    }

    #[test]
    fn fr_p2c_009_u001_protocol_parse_rejects_unknown_token() {
        let protocols = parse_tls_protocols("TLSv1.2,TLSv1.3").expect("supported protocols");
        assert_eq!(protocols, vec![TlsProtocol::TlsV1_2, TlsProtocol::TlsV1_3]);

        let err = parse_tls_protocols("TLSv1.2,TLSv1.4").expect_err("must reject unknown");
        assert_eq!(
            err.reason_code(),
            "tlscfg.protocols_parse_contract_violation"
        );
    }

    #[test]
    fn fr_p2c_009_u002_context_validation_requires_atomic_material() {
        let config = TlsConfig {
            tls_port: Some(6379),
            cert_file: Some("cert.pem".to_string()),
            key_file: None,
            ca_file: Some("ca.pem".to_string()),
            protocols: vec![TlsProtocol::TlsV1_2],
            ciphers: Some("HIGH:!aNULL".to_string()),
            auth_clients: TlsAuthClients::Required,
            session_caching: true,
            session_cache_size: 20 * 1024,
            session_cache_timeout_sec: 300,
            cluster_announce_tls_port: None,
            max_new_tls_connections_per_cycle: 128,
        };
        let err = validate_tls_config(&config).expect_err("must fail");
        assert_eq!(err.reason_code(), "tlscfg.context_build_contract_violation");
    }

    #[test]
    fn fr_p2c_009_u003_tls_port_zero_is_disabled_sentinel() {
        let config = TlsConfig {
            tls_port: Some(0),
            cert_file: None,
            key_file: None,
            ca_file: None,
            protocols: vec![TlsProtocol::TlsV1_2],
            ciphers: None,
            auth_clients: TlsAuthClients::Required,
            session_caching: true,
            session_cache_size: 20 * 1024,
            session_cache_timeout_sec: 300,
            cluster_announce_tls_port: None,
            max_new_tls_connections_per_cycle: 16,
        };

        assert!(!config.tls_enabled());
        validate_tls_config(&config).expect("tls-port 0 without material should stay disabled");
        let rewrite = rewrite_tls_directives(&config).expect("rewrite");
        assert_eq!(rewrite[0], ("tls-port".to_string(), "0".to_string()));
        assert!(!rewrite.iter().any(|(name, _)| matches!(
            name.as_str(),
            "tls-cert-file" | "tls-key-file" | "tls-ca-file" | "tls-ciphers"
        )));
    }

    #[test]
    fn fr_p2c_009_u004_cluster_tls_port_requires_nonzero_tls_port() {
        let config = TlsConfig {
            tls_port: Some(0),
            cert_file: None,
            key_file: None,
            ca_file: None,
            protocols: vec![TlsProtocol::TlsV1_2],
            ciphers: None,
            auth_clients: TlsAuthClients::Required,
            session_caching: true,
            session_cache_size: 20 * 1024,
            session_cache_timeout_sec: 300,
            cluster_announce_tls_port: Some(16380),
            max_new_tls_connections_per_cycle: 16,
        };

        let err =
            validate_tls_config(&config).expect_err("cluster announce should require enabled TLS");
        assert_eq!(
            err.reason_code(),
            "tlscfg.operational_knob_contract_violation"
        );
    }

    #[test]
    fn fr_p2c_009_u008_directive_registry_validation_contract() {
        let policy = tls_directive_policy(TlsDirective::TlsKeyFile);
        assert!(policy.mutable_at_runtime);
        assert!(policy.sensitive);

        validate_tls_directive_value(TlsDirective::TlsPort, "6380").expect("valid port");
        let err = validate_tls_directive_value(TlsDirective::TlsAuthClients, "invalid-mode")
            .expect_err("must fail");
        assert_eq!(
            err.reason_code(),
            "tlscfg.directive_registry_contract_violation"
        );
        assert_eq!(parse_tls_bool("yes"), Some(true));
        assert_eq!(parse_tls_bool("off"), Some(false));
        assert_eq!(tls_bool_token(true), "yes");
        assert_eq!(tls_bool_token(false), "no");
        validate_tls_directive_value(TlsDirective::TlsSessionCaching, "no")
            .expect("valid session caching toggle");
        validate_tls_directive_value(TlsDirective::TlsSessionCacheSize, "0")
            .expect("zero cache size means unlimited");
        validate_tls_directive_value(TlsDirective::TlsSessionCacheTimeout, "300")
            .expect("valid session timeout");
    }

    #[test]
    fn fr_p2c_009_u010_runtime_apply_plan_requires_configure_hook() {
        let current = TlsRuntimeState::default();
        let candidate = TlsConfig {
            tls_port: Some(6380),
            cert_file: Some("cert.pem".to_string()),
            key_file: Some("key.pem".to_string()),
            ca_file: Some("ca.pem".to_string()),
            protocols: vec![TlsProtocol::TlsV1_2, TlsProtocol::TlsV1_3],
            ciphers: Some("HIGH:!aNULL".to_string()),
            auth_clients: TlsAuthClients::Required,
            session_caching: true,
            session_cache_size: 20 * 1024,
            session_cache_timeout_sec: 300,
            cluster_announce_tls_port: None,
            max_new_tls_connections_per_cycle: 64,
        };
        let plan = plan_tls_runtime_apply(&current, candidate).expect("plan");
        assert_eq!(plan.listener_transition, TlsListenerTransition::Enable);
        assert!(plan.requires_context_swap);
        assert!(plan.requires_connection_type_configure);
    }

    #[test]
    fn fr_p2c_009_u011_runtime_apply_plan_disables_tls_port_zero_candidate() {
        let current = TlsRuntimeState {
            active_config: Some(TlsConfig {
                tls_port: Some(6380),
                cert_file: Some("cert.pem".to_string()),
                key_file: Some("key.pem".to_string()),
                ca_file: Some("ca.pem".to_string()),
                protocols: vec![TlsProtocol::TlsV1_2, TlsProtocol::TlsV1_3],
                ciphers: Some("HIGH:!aNULL".to_string()),
                auth_clients: TlsAuthClients::Required,
                session_caching: true,
                session_cache_size: 20 * 1024,
                session_cache_timeout_sec: 300,
                cluster_announce_tls_port: None,
                max_new_tls_connections_per_cycle: 64,
            }),
            tls_listener_enabled: true,
            tcp_listener_enabled: true,
            connection_type_configured: true,
        };
        let candidate = TlsConfig {
            tls_port: Some(0),
            cert_file: None,
            key_file: None,
            ca_file: None,
            protocols: vec![TlsProtocol::TlsV1_2],
            ciphers: None,
            auth_clients: TlsAuthClients::Required,
            session_caching: false,
            session_cache_size: 0,
            session_cache_timeout_sec: 0,
            cluster_announce_tls_port: None,
            max_new_tls_connections_per_cycle: 16,
        };

        let plan = plan_tls_runtime_apply(&current, candidate).expect("plan");
        assert_eq!(plan.listener_transition, TlsListenerTransition::Disable);
        assert!(plan.requires_context_swap);
        assert!(!plan.requires_connection_type_configure);
        assert!(!plan.candidate_config.tls_enabled());
    }

    #[test]
    fn fr_p2c_009_u014_runtime_apply_plan_allows_noop_reapply_for_enabled_tls() {
        let candidate = TlsConfig {
            tls_port: Some(6380),
            cert_file: Some("cert.pem".to_string()),
            key_file: Some("key.pem".to_string()),
            ca_file: Some("ca.pem".to_string()),
            protocols: vec![TlsProtocol::TlsV1_2, TlsProtocol::TlsV1_3],
            ciphers: Some("HIGH:!aNULL".to_string()),
            auth_clients: TlsAuthClients::Required,
            session_caching: true,
            session_cache_size: 20 * 1024,
            session_cache_timeout_sec: 300,
            cluster_announce_tls_port: None,
            max_new_tls_connections_per_cycle: 64,
        };
        let current = TlsRuntimeState {
            active_config: Some(candidate.clone()),
            tls_listener_enabled: true,
            tcp_listener_enabled: true,
            connection_type_configured: true,
        };

        let plan = plan_tls_runtime_apply(&current, candidate).expect("no-op reapply should plan");
        assert_eq!(plan.listener_transition, TlsListenerTransition::Keep);
        assert!(!plan.requires_context_swap);
        assert!(!plan.requires_connection_type_configure);
    }

    #[test]
    fn fr_p2c_009_u009_bind_atomicity_detects_partial_failure() {
        let before = TlsRuntimeState {
            active_config: None,
            tls_listener_enabled: false,
            tcp_listener_enabled: true,
            connection_type_configured: false,
        };
        let after = TlsRuntimeState {
            active_config: None,
            tls_listener_enabled: true,
            tcp_listener_enabled: true,
            connection_type_configured: false,
        };
        let err =
            validate_bind_transition_atomicity(&before, &after, false).expect_err("must fail");
        assert_eq!(err.reason_code(), "tlscfg.bind_atomicity_violation");
    }

    #[test]
    fn fr_p2c_009_u012_rewrite_is_deterministic() {
        let config = TlsConfig {
            tls_port: Some(6380),
            cert_file: Some("cert.pem".to_string()),
            key_file: Some("key.pem".to_string()),
            ca_file: Some("ca.pem".to_string()),
            protocols: vec![TlsProtocol::TlsV1_2, TlsProtocol::TlsV1_3],
            ciphers: Some("HIGH:!aNULL".to_string()),
            auth_clients: TlsAuthClients::Optional,
            session_caching: false,
            session_cache_size: 0,
            session_cache_timeout_sec: 60,
            cluster_announce_tls_port: Some(16380),
            max_new_tls_connections_per_cycle: 32,
        };
        let rewrite = rewrite_tls_directives(&config).expect("rewrite");
        let names: Vec<&str> = rewrite.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "tls-port",
                "tls-protocols",
                "tls-auth-clients",
                "max-new-tls-connections-per-cycle",
                "tls-session-caching",
                "tls-session-cache-size",
                "tls-session-cache-timeout",
                "tls-cert-file",
                "tls-key-file",
                "tls-ca-file",
                "tls-ciphers",
                "cluster-announce-tls-port",
            ]
        );
    }

    #[test]
    fn fr_p2c_009_u011_session_resumption_knobs_drive_context_reconfigure() {
        let current = TlsRuntimeState {
            active_config: Some(TlsConfig {
                tls_port: Some(6380),
                cert_file: Some("cert.pem".to_string()),
                key_file: Some("key.pem".to_string()),
                ca_file: Some("ca.pem".to_string()),
                protocols: vec![TlsProtocol::TlsV1_2, TlsProtocol::TlsV1_3],
                ciphers: Some("HIGH:!aNULL".to_string()),
                auth_clients: TlsAuthClients::Required,
                session_caching: true,
                session_cache_size: 20 * 1024,
                session_cache_timeout_sec: 300,
                cluster_announce_tls_port: None,
                max_new_tls_connections_per_cycle: 64,
            }),
            tls_listener_enabled: true,
            tcp_listener_enabled: true,
            connection_type_configured: true,
        };
        let mut candidate = current.active_config.clone().expect("active config");
        candidate.session_cache_timeout_sec = 60;

        let plan = plan_tls_runtime_apply(&current, candidate).expect("plan");
        assert_eq!(plan.listener_transition, TlsListenerTransition::Keep);
        assert!(plan.requires_context_swap);
        assert!(plan.requires_connection_type_configure);
    }

    #[test]
    fn fr_p2c_009_u013_hardened_gate_rejects_non_allowlisted_deviation() {
        let mut policy = RuntimePolicy::hardened();
        policy.hardened_allowlist.clear();
        let err = evaluate_tls_hardened_deviation(
            &policy,
            HardenedDeviationCategory::MetadataSanitization,
        )
        .expect_err("must reject");
        assert_eq!(err.reason_code(), "tlscfg.hardened_nonallowlisted_rejected");
    }

    #[test]
    fn fr_p2c_009_u013_hardened_gate_allows_allowlisted_deviation() {
        let policy = RuntimePolicy::hardened();
        let action = evaluate_tls_hardened_deviation(
            &policy,
            HardenedDeviationCategory::MetadataSanitization,
        )
        .expect("must allow");
        assert_eq!(action, DecisionAction::BoundedDefense);
    }

    #[test]
    fn fr_p2c_009_u013_strict_mode_returns_fail_closed_decision() {
        let policy = RuntimePolicy::default();
        let action = evaluate_tls_hardened_deviation(
            &policy,
            HardenedDeviationCategory::MetadataSanitization,
        )
        .expect("strict still returns decision");
        assert_eq!(action, DecisionAction::FailClosed);
    }

    #[test]
    fn fr_p2c_009_u013_safety_gate_detects_tls_material_when_disabled() {
        let config = TlsConfig {
            tls_port: None,
            cert_file: Some("cert.pem".to_string()),
            key_file: Some("key.pem".to_string()),
            ca_file: Some("ca.pem".to_string()),
            protocols: vec![TlsProtocol::TlsV1_2],
            ciphers: Some("HIGH:!aNULL".to_string()),
            auth_clients: TlsAuthClients::Required,
            session_caching: true,
            session_cache_size: 20 * 1024,
            session_cache_timeout_sec: 300,
            cluster_announce_tls_port: None,
            max_new_tls_connections_per_cycle: 16,
        };
        let err = validate_tls_config(&config).expect_err("must fail");
        assert_eq!(err.reason_code(), "tlscfg.safety_gate_contract_violation");
        assert!(matches!(err, TlsCfgError::SafetyGateContractViolation(_)));
    }

    #[test]
    fn redis_config_parser_skips_comments_and_lowercases_directives() {
        let parsed = parse_redis_config("  # comment\r\nPORT 6380\n\nappendonly yes\n")
            .expect("config should parse");
        assert_eq!(parsed.directives.len(), 2);
        assert_eq!(parsed.directives[0].line_number, 2);
        assert_eq!(parsed.directives[0].name, b"port");
        assert_eq!(parsed.directives[0].args, vec![b"6380".to_vec()]);
        assert_eq!(parsed.directives[1].name, b"appendonly");
        assert_eq!(parsed.directives[1].args, vec![b"yes".to_vec()]);
    }

    #[test]
    fn redis_config_parser_preserves_inline_hash_as_argument() {
        let parsed = parse_redis_config("port 6379 # inline comment is still an arg\n")
            .expect("config should parse like Redis sdssplitargs");
        assert_eq!(parsed.directives[0].name, b"port");
        assert_eq!(
            parsed.directives[0].args,
            vec![
                b"6379".to_vec(),
                b"#".to_vec(),
                b"inline".to_vec(),
                b"comment".to_vec(),
                b"is".to_vec(),
                b"still".to_vec(),
                b"an".to_vec(),
                b"arg".to_vec(),
            ]
        );
    }

    #[test]
    fn redis_config_parser_keeps_vertical_tab_prefixed_hash_line() {
        let parsed = parse_redis_config("\x0b# not a full-line comment\n")
            .expect("vertical tab is not trimmed before Redis comment check");
        assert_eq!(parsed.directives.len(), 1);
        assert_eq!(parsed.directives[0].line_number, 1);
        assert_eq!(parsed.directives[0].name, b"#");
        assert_eq!(
            parsed.directives[0].args,
            vec![
                b"not".to_vec(),
                b"a".to_vec(),
                b"full-line".to_vec(),
                b"comment".to_vec()
            ]
        );
    }

    #[test]
    fn redis_config_line_split_decodes_double_quote_escapes() {
        let args = split_config_line_args(r#"set "line\n\x41\t\\" tail"#)
            .expect("quoted line should parse");
        assert_eq!(
            args,
            vec![b"set".to_vec(), b"line\nA\t\\".to_vec(), b"tail".to_vec(),]
        );

        let args = split_config_line_args(r#"set "\x4g\xzz" tail"#)
            .expect("malformed hex escapes stay literal like Redis sdssplitargs");
        assert_eq!(
            args,
            vec![b"set".to_vec(), b"x4gxzz".to_vec(), b"tail".to_vec()]
        );

        let args = split_config_line_args(r#"set "a\x00b" tail"#)
            .expect("hex nul escape decodes inside SDS token");
        assert_eq!(
            args,
            vec![b"set".to_vec(), b"a\0b".to_vec(), b"tail".to_vec()]
        );
    }

    #[test]
    fn redis_config_line_split_decodes_single_quote_escape_only() {
        let args = split_config_line_args(r#"dir 'it\'s\nliteral'"#)
            .expect("single quoted line should parse");
        assert_eq!(args, vec![b"dir".to_vec(), br"it's\nliteral".to_vec()]);
    }

    #[test]
    fn redis_config_line_split_preserves_form_feed_in_bare_tokens() {
        assert_eq!(
            split_config_line_args("foo\x0cbar baz").expect("form feed inside bare token"),
            vec![b"foo\x0cbar".to_vec(), b"baz".to_vec()]
        );
        assert_eq!(
            split_config_line_args("\"foo\"\x0cbar").expect("form feed after closed quote"),
            vec![b"foo".to_vec(), b"bar".to_vec()]
        );
    }

    #[test]
    fn redis_config_line_split_rejects_invalid_quoted_tokens() {
        let err = split_config_line_args(r#"port "6379"#).expect_err("unterminated quote");
        assert_eq!(err, ConfigFileParseErrorReason::InvalidQuotedToken);

        let err = split_config_line_args(r#""foo"bar"#).expect_err("adjacent token after quote");
        assert_eq!(err, ConfigFileParseErrorReason::InvalidQuotedToken);
    }

    #[test]
    fn redis_config_line_split_treats_nul_as_c_string_end() {
        let args = split_config_line_args("port 6379\0ignored").expect("nul terminates line");
        assert_eq!(args, vec![b"port".to_vec(), b"6379".to_vec()]);
    }

    #[test]
    fn redis_config_parser_treats_nul_as_config_buffer_end() {
        let parsed = parse_redis_config("port 6380\0\nbind 0.0.0.0\n")
            .expect("nul should terminate full config input");
        assert_eq!(parsed.directives.len(), 1);
        assert_eq!(parsed.directives[0].name, b"port");
        assert_eq!(parsed.directives[0].args, vec![b"6380".to_vec()]);
    }

    #[test]
    fn redis_config_parser_preserves_raw_non_utf8_bytes() {
        let parsed = parse_redis_config_bytes(b"requirepass \xff\n")
            .expect("raw byte config should parse like Redis C strings");
        assert_eq!(parsed.directives.len(), 1);
        assert_eq!(parsed.directives[0].name, b"requirepass");
        assert_eq!(parsed.directives[0].args, vec![vec![0xff]]);

        let args =
            split_config_line_args_bytes(b"rename-command CONFIG \xfe").expect("raw byte token");
        assert_eq!(
            args,
            vec![b"rename-command".to_vec(), b"CONFIG".to_vec(), vec![0xfe]]
        );
    }

    /// Lock the contract for the structured corpus seeds in
    /// `fuzz/corpus/fuzz_config_file/`. The fuzz target runs every
    /// input through `parse_redis_config_bytes` AND
    /// `split_config_line_args_bytes`. The seed generator
    /// (`fuzz/scripts/gen_config_file_seeds.py`) writes byte-level
    /// .conf snippets covering each meaningful branch of the parser:
    ///
    ///   - empty / blank / whitespace / comment-only
    ///   - bare / single-quoted / double-quoted tokens
    ///   - escape sequences (\\xHH, \\n, \\r, \\t, \\\\, \\")
    ///   - leading/trailing whitespace, tabs, CRLF
    ///   - case-mixed directive names (parser must lowercase)
    ///   - mixed bare + quoted tokens (e.g. rename-command CONFIG "")
    ///   - long bare tokens, many-directive stress
    ///
    /// The test verifies:
    ///   1. Every accept seed parses cleanly into ≥0 directives, and
    ///      every directive name is ASCII-lowercased (the property
    ///      `fuzz_raw_config` asserts).
    ///   2. Pinned representative seeds produce the expected
    ///      directive count + names + args so the corpus locks the
    ///      parse outcome for the documented shapes.
    ///   3. Reject seeds (unterminated quotes, dangling backslash)
    ///      surface a `ConfigFileParseError` without panicking.
    #[test]
    fn fuzz_config_file_corpus_matches_documented_contract() -> Result<(), String> {
        use std::path::Path;

        let corpus_root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/fuzz_config_file");
        if !corpus_root.exists() {
            return Ok(());
        }

        fn read_seed(corpus_root: &Path, name: &str) -> Result<Vec<u8>, String> {
            std::fs::read(corpus_root.join(name)).map_err(|err| format!("read seed {name}: {err}"))
        }

        // Accept-class: every seed must parse and every directive
        // name must be lowercased (the harness invariant).
        let accepts: &[&str] = &[
            "empty.conf",
            "only_blank_lines.conf",
            "only_whitespace.conf",
            "only_comment.conf",
            "comment_with_leading_whitespace.conf",
            "single_directive.conf",
            "two_directives.conf",
            "directive_with_three_args.conf",
            "directive_with_two_save_args.conf",
            "tab_separated_tokens.conf",
            "trailing_whitespace.conf",
            "leading_whitespace.conf",
            "double_quoted_simple.conf",
            "double_quoted_with_spaces.conf",
            "double_quoted_hex_escape.conf",
            "double_quoted_special_escapes.conf",
            "double_quoted_backslash_and_quote.conf",
            "single_quoted_simple.conf",
            "single_quoted_backslash_and_quote.conf",
            "empty_double_quoted_arg.conf",
            "empty_single_quoted_arg.conf",
            "include_directive.conf",
            "module_load_directive.conf",
            "uppercase_directive_name.conf",
            "mixed_case_directive_name.conf",
            "mixed_quoted_and_bare_tokens.conf",
            "user_acl_directive.conf",
            "comment_then_directive.conf",
            "many_directives.conf",
            "long_bare_token.conf",
            "no_trailing_newline.conf",
            "crlf_line_endings.conf",
        ];
        assert!(
            accepts.len() >= 14,
            "fuzz_config_file accept seeds must have >= 14 entries"
        );
        for name in accepts {
            let body = read_seed(&corpus_root, name)?;
            let parsed = parse_redis_config_bytes(&body)
                .map_err(|err| format!("seed {name} must parse: {err:?}"))?;
            for directive in &parsed.directives {
                assert!(
                    !directive.name.is_empty(),
                    "seed {name}: directive name must not be empty"
                );
                assert!(
                    directive.name.iter().all(|byte| !byte.is_ascii_uppercase()),
                    "seed {name}: directive names must be ASCII-lowercased"
                );
            }
        }

        // Pinned outcomes for representative seeds.
        let parsed = parse_redis_config_bytes(&read_seed(&corpus_root, "empty.conf")?).unwrap();
        assert!(parsed.directives.is_empty());

        let parsed =
            parse_redis_config_bytes(&read_seed(&corpus_root, "only_comment.conf")?).unwrap();
        assert!(
            parsed.directives.is_empty(),
            "comments must yield 0 directives"
        );

        let parsed =
            parse_redis_config_bytes(&read_seed(&corpus_root, "single_directive.conf")?).unwrap();
        assert_eq!(parsed.directives.len(), 1);
        assert_eq!(parsed.directives[0].name, b"port");
        assert_eq!(parsed.directives[0].args, vec![b"6379".to_vec()]);

        let parsed =
            parse_redis_config_bytes(&read_seed(&corpus_root, "directive_with_three_args.conf")?)
                .unwrap();
        assert_eq!(parsed.directives.len(), 1);
        assert_eq!(parsed.directives[0].name, b"save");
        assert_eq!(
            parsed.directives[0].args,
            vec![b"900".to_vec(), b"1".to_vec()]
        );

        // Mixed-case name MUST lowercase per upstream sdstolower.
        let parsed =
            parse_redis_config_bytes(&read_seed(&corpus_root, "mixed_case_directive_name.conf")?)
                .unwrap();
        assert_eq!(parsed.directives[0].name, b"appendonly");

        // Hex escape inside double-quote decodes to the byte value.
        let parsed =
            parse_redis_config_bytes(&read_seed(&corpus_root, "double_quoted_hex_escape.conf")?)
                .unwrap();
        assert_eq!(parsed.directives[0].name, b"requirepass");
        assert_eq!(parsed.directives[0].args, vec![b"pass".to_vec()]);

        // tls-protocols quoted multi-token stays as ONE arg.
        let parsed =
            parse_redis_config_bytes(&read_seed(&corpus_root, "double_quoted_with_spaces.conf")?)
                .unwrap();
        assert_eq!(parsed.directives[0].args, vec![b"TLSv1.2 TLSv1.3".to_vec()]);

        // Empty quoted string is an empty arg (not absent).
        let parsed =
            parse_redis_config_bytes(&read_seed(&corpus_root, "empty_double_quoted_arg.conf")?)
                .unwrap();
        assert_eq!(parsed.directives[0].args, vec![Vec::<u8>::new()]);

        // Many-directive stress: count must match.
        let parsed =
            parse_redis_config_bytes(&read_seed(&corpus_root, "many_directives.conf")?).unwrap();
        assert_eq!(parsed.directives.len(), 12);

        // ── Reject-class: parser must surface ConfigFileParseError.
        let rejects: &[&str] = &[
            "unterminated_double_quote.conf",
            "unterminated_single_quote.conf",
            "backslash_at_eof_in_double_quote.conf",
        ];
        for name in rejects {
            let body = read_seed(&corpus_root, name)?;
            assert!(
                parse_redis_config_bytes(&body).is_err(),
                "seed {name} must reject (got Ok)"
            );
        }
        Ok(())
    }
}
