#![forbid(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpiryDecision {
    pub should_evict: bool,
    pub remaining_ms: i64,
}

// (cc_fr) `#[inline]` so downstream crates (fr-store calls this on EVERY guarded expiry check —
// GET/SET/INCR/…) can inline this tiny fn across the crate boundary WITHOUT relying on LTO, and so
// the compiler can DCE the `remaining_ms` computation for the many `should_evict`-only callers.
// Byte-identical (codegen hint only); no-op under the release-perf thin-LTO the gauntlet measures.
#[inline]
#[must_use]
pub fn evaluate_expiry(now_ms: u64, expires_at_ms: Option<u64>) -> ExpiryDecision {
    match expires_at_ms {
        None => ExpiryDecision {
            should_evict: false,
            remaining_ms: -1,
        },
        // Upstream treats a key as expired only when the clock is STRICTLY past
        // the deadline: keyIsExpired (db.c:1743) and activeExpireCycleTryExpire
        // (expire.c:56) both `return now > when`. A key whose deadline equals
        // `now` is still alive (its final millisecond), so PTTL reports 0 rather
        // than -2. The distinct set-time "already expired" decision
        // (expireGenericCommand / checkAlreadyExpired uses `when <= now`) is
        // handled separately by Store::expire_at_milliseconds, not here.
        Some(deadline) if deadline < now_ms => ExpiryDecision {
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

/// Frozen pre-`frankenredis-u6uwo` comparator for the same-binary performance proof.
#[cfg(feature = "bench-reference")]
#[doc(hidden)]
#[inline(never)]
#[must_use]
pub fn evaluate_expiry_reference(now_ms: u64, expires_at_ms: Option<u64>) -> ExpiryDecision {
    // Equal-cost opaque tags keep LLVM from folding the two attributable bench entry points into
    // one symbol before their arithmetic differs. They are bench-only and intentionally symmetric.
    std::hint::black_box(0_u8);
    match expires_at_ms {
        None => ExpiryDecision {
            should_evict: false,
            remaining_ms: -1,
        },
        Some(deadline) if deadline < now_ms => ExpiryDecision {
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

/// No-inline entry point that keeps the production implementation attributable in profiles.
#[cfg(feature = "bench-reference")]
#[doc(hidden)]
#[inline(never)]
#[must_use]
pub fn evaluate_expiry_candidate(now_ms: u64, expires_at_ms: Option<u64>) -> ExpiryDecision {
    std::hint::black_box(1_u8);
    match expires_at_ms {
        None => ExpiryDecision {
            should_evict: false,
            remaining_ms: -1,
        },
        Some(deadline) if deadline < now_ms => ExpiryDecision {
            should_evict: true,
            remaining_ms: -2,
        },
        Some(deadline) => ExpiryDecision {
            should_evict: false,
            remaining_ms: (deadline - now_ms).min(i64::MAX as u64) as i64,
        },
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

    #[test]
    fn deadline_equal_to_now_is_alive() {
        // Upstream keyIsExpired/activeExpireCycleTryExpire use `now > when`, so a
        // key is alive in its final millisecond (deadline == now): not evicted,
        // PTTL reports 0. Eviction only happens once `now` passes the deadline.
        let alive = evaluate_expiry(100, Some(100));
        assert_eq!(alive.remaining_ms, 0);
        assert!(!alive.should_evict);

        let evicted = evaluate_expiry(101, Some(100));
        assert_eq!(evicted.remaining_ms, -2);
        assert!(evicted.should_evict);
    }

    #[test]
    fn future_deadline_reports_positive_remaining_ms() {
        let decision = evaluate_expiry(100, Some(250));
        assert_eq!(decision.remaining_ms, 150);
        assert!(!decision.should_evict);
    }

    #[test]
    fn far_future_deadline_clamps_to_i64_max() {
        let decision = evaluate_expiry(0, Some(u64::MAX));
        assert_eq!(decision.remaining_ms, i64::MAX);
        assert!(!decision.should_evict);
    }

    #[test]
    fn subtraction_can_land_exactly_on_i64_max_boundary() {
        let now_ms = 5_u64;
        let deadline = (i64::MAX as u64) + now_ms;
        let decision = evaluate_expiry(now_ms, Some(deadline));
        assert_eq!(decision.remaining_ms, i64::MAX);
        assert!(!decision.should_evict);
    }

    #[test]
    fn subtraction_above_i64_max_boundary_clamps() {
        let now_ms = 5_u64;
        let deadline = (i64::MAX as u64) + now_ms + 1;
        let decision = evaluate_expiry(now_ms, Some(deadline));
        assert_eq!(decision.remaining_ms, i64::MAX);
        assert!(!decision.should_evict);
    }
}
