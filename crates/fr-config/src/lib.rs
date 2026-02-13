#![forbid(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Strict,
    Hardened,
}

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
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            mode: Mode::Strict,
            gate: CompatibilityGate::default(),
            emit_evidence_ledger: true,
        }
    }
}

impl RuntimePolicy {
    #[must_use]
    pub fn hardened() -> Self {
        Self {
            mode: Mode::Hardened,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mode, RuntimePolicy};

    #[test]
    fn default_policy_is_strict() {
        let policy = RuntimePolicy::default();
        assert_eq!(policy.mode, Mode::Strict);
        assert!(policy.emit_evidence_ledger);
    }
}
