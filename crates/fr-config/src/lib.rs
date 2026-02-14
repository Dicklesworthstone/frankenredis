#![forbid(unsafe_code)]

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

#[cfg(test)]
mod tests {
    use super::{
        DecisionAction, DriftSeverity, HARDENED_ALLOWLIST_DEFAULT, HardenedDeviationCategory, Mode,
        RuntimePolicy, ThreatClass,
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
}
