#![forbid(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpiryDecision {
    pub should_evict: bool,
    pub remaining_ms: i64,
}

#[must_use]
pub fn evaluate_expiry(now_ms: u64, expires_at_ms: Option<u64>) -> ExpiryDecision {
    match expires_at_ms {
        None => ExpiryDecision {
            should_evict: false,
            remaining_ms: -1,
        },
        Some(deadline) if deadline <= now_ms => ExpiryDecision {
            should_evict: true,
            remaining_ms: -2,
        },
        Some(deadline) => {
            let remaining = deadline.saturating_sub(now_ms);
            let remaining = i64::try_from(remaining).unwrap_or(i64::MAX);
            ExpiryDecision {
                should_evict: false,
                remaining_ms: remaining,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::evaluate_expiry;

    #[test]
    fn no_expiry_is_persistent() {
        let decision = evaluate_expiry(10, None);
        assert_eq!(decision.remaining_ms, -1);
        assert!(!decision.should_evict);
    }

    #[test]
    fn expired_key_is_evicted() {
        let decision = evaluate_expiry(100, Some(99));
        assert_eq!(decision.remaining_ms, -2);
        assert!(decision.should_evict);
    }
}
