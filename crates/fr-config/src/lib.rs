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
}

#[cfg(test)]
mod tests {
    use super::{HARDENED_ALLOWLIST_DEFAULT, HardenedDeviationCategory, Mode, RuntimePolicy};

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
}
