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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    pub tls_port: Option<u16>,
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
    pub ca_file: Option<String>,
    pub protocols: Vec<TlsProtocol>,
    pub ciphers: Option<String>,
    pub auth_clients: TlsAuthClients,
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
            cluster_announce_tls_port: None,
            max_new_tls_connections_per_cycle: 1_000,
        }
    }
}

impl TlsConfig {
    #[must_use]
    pub fn tls_enabled(&self) -> bool {
        self.tls_port.is_some()
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

    if target_listener_enabled && !requires_connection_type_configure {
        return Err(TlsCfgError::ConnectionTypeConfigureViolation(
            "TLS listener would update without connection-type configure hook".to_string(),
        ));
    }

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
    let mut directives = Vec::with_capacity(9);
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
        DecisionAction, DriftSeverity, HARDENED_ALLOWLIST_DEFAULT, HardenedDeviationCategory, Mode,
        RuntimePolicy, ThreatClass, TlsAuthClients, TlsCfgError, TlsConfig, TlsDirective,
        TlsListenerTransition, TlsProtocol, TlsRuntimeState, evaluate_tls_hardened_deviation,
        parse_tls_protocols, plan_tls_runtime_apply, rewrite_tls_directives, tls_directive_policy,
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
            cluster_announce_tls_port: None,
            max_new_tls_connections_per_cycle: 128,
        };
        let err = validate_tls_config(&config).expect_err("must fail");
        assert_eq!(err.reason_code(), "tlscfg.context_build_contract_violation");
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
            cluster_announce_tls_port: None,
            max_new_tls_connections_per_cycle: 64,
        };
        let plan = plan_tls_runtime_apply(&current, candidate).expect("plan");
        assert_eq!(plan.listener_transition, TlsListenerTransition::Enable);
        assert!(plan.requires_context_swap);
        assert!(plan.requires_connection_type_configure);
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
                "tls-cert-file",
                "tls-key-file",
                "tls-ca-file",
                "tls-ciphers",
                "cluster-announce-tls-port",
            ]
        );
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
            cluster_announce_tls_port: None,
            max_new_tls_connections_per_cycle: 16,
        };
        let err = validate_tls_config(&config).expect_err("must fail");
        assert_eq!(err.reason_code(), "tlscfg.safety_gate_contract_violation");
        assert!(matches!(err, TlsCfgError::SafetyGateContractViolation(_)));
    }
}
