//! Canonical performance gate thresholds and criteria (doc 00, doc 11, doc 18).
//! Single source of truth for both documentation generation and machine assertion (F-22 / F-29 / F-35).

pub const PR1_MIN_SUSTAINED_MSG_PER_SEC: u64 = 10_000_000;

// Strict Tier 3 Bare-Metal / Reference Target (doc 00)
pub const PR2_TARGET_P50_CYCLES: u64 = 60;
pub const PR2_TARGET_P99_CYCLES: u64 = 150;

// Tier 2 Virtualized CI VM Margin Envelope (doc 11 §7 / F-29)
pub const PR2_TIER2_VM_P50_CYCLES: u64 = 130;
pub const PR2_TIER2_VM_P99_CYCLES: u64 = 185;

pub const PR3_MAX_ALLOC_DELTA: u64 = 0;
pub const SAMPLING_INTERVAL: usize = 256;
pub const MAX_RECONCILIATION_RESIDUAL_PCT: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    Pass,
    Fail,
}

impl GateVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
        }
    }
}

#[inline]
pub fn evaluate_pr1(sustained_rate: u64) -> GateVerdict {
    if sustained_rate >= PR1_MIN_SUSTAINED_MSG_PER_SEC {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

#[inline]
pub fn evaluate_pr2_p50(p50_cycles: u64) -> GateVerdict {
    if p50_cycles < PR2_TARGET_P50_CYCLES {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

#[inline]
pub fn evaluate_pr2_p99(p99_cycles: u64) -> GateVerdict {
    if p99_cycles < PR2_TARGET_P99_CYCLES {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

#[inline]
pub fn evaluate_pr2_tier2_p50(p50_cycles: u64) -> GateVerdict {
    if p50_cycles < PR2_TIER2_VM_P50_CYCLES {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

#[inline]
pub fn evaluate_pr2_tier2_p99(p99_cycles: u64) -> GateVerdict {
    if p99_cycles < PR2_TIER2_VM_P99_CYCLES {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

#[inline]
pub fn evaluate_pr3(alloc_delta: u64) -> GateVerdict {
    if alloc_delta == PR3_MAX_ALLOC_DELTA {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

#[inline]
pub fn evaluate_reconciliation_residual(residual_pct: f64) -> GateVerdict {
    if residual_pct <= MAX_RECONCILIATION_RESIDUAL_PCT {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Law B-4 (Gate Self-Test): Assert that every gate function produces FAIL on out-of-band inputs.
    #[test]
    fn test_gates_tripwires_fail_on_bad_inputs() {
        assert_eq!(evaluate_pr1(9_999_999), GateVerdict::Fail);
        assert_eq!(evaluate_pr1(0), GateVerdict::Fail);
        assert_eq!(evaluate_pr1(10_000_000), GateVerdict::Pass);

        assert_eq!(evaluate_pr2_p50(60), GateVerdict::Fail);
        assert_eq!(evaluate_pr2_p50(122), GateVerdict::Fail);
        assert_eq!(evaluate_pr2_p50(59), GateVerdict::Pass);

        assert_eq!(evaluate_pr2_p99(150), GateVerdict::Fail);
        assert_eq!(evaluate_pr2_p99(172), GateVerdict::Fail);
        assert_eq!(evaluate_pr2_p99(149), GateVerdict::Pass);

        assert_eq!(evaluate_pr2_tier2_p50(130), GateVerdict::Fail);
        assert_eq!(evaluate_pr2_tier2_p50(131), GateVerdict::Fail);
        assert_eq!(evaluate_pr2_tier2_p50(122), GateVerdict::Pass);

        assert_eq!(evaluate_pr2_tier2_p99(185), GateVerdict::Fail);
        assert_eq!(evaluate_pr2_tier2_p99(186), GateVerdict::Fail);
        assert_eq!(evaluate_pr2_tier2_p99(172), GateVerdict::Pass);

        assert_eq!(evaluate_pr3(1), GateVerdict::Fail);
        assert_eq!(evaluate_pr3(4096), GateVerdict::Fail);
        assert_eq!(evaluate_pr3(0), GateVerdict::Pass);

        // F-35: 7.26% residual must explicitly produce FAIL
        assert_eq!(evaluate_reconciliation_residual(7.26), GateVerdict::Fail);
        assert_eq!(evaluate_reconciliation_residual(2.01), GateVerdict::Fail);
        assert_eq!(evaluate_reconciliation_residual(2.00), GateVerdict::Pass);
        assert_eq!(evaluate_reconciliation_residual(0.50), GateVerdict::Pass);
    }
}
