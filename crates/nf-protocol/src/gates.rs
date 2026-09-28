//! Canonical performance gate thresholds and criteria (doc 00, doc 11, doc 18).
//! Single source of truth for both documentation generation and machine assertion (F-22 / F-29 / F-35).

pub const PR1_MIN_SUSTAINED_MSG_PER_SEC: u64 = 10_000_000;

// R4: PR-1 TITAN target — 100M msg/s wall-rate with full byte-level
// conformance verification (SpanConformanceSink: every emitted byte read and
// CRC32C-checked in-window). Gates-as-Code law (F-22): the threshold lives
// here, consumed by bench verdict lines and CI JSON checks alike.
pub const PR1_TITAN_MIN_MSG_PER_SEC: u64 = 100_000_000;

// R6: PR-1 HYDRA target — 800M msg/s wall-rate with the SAME full byte-level
// span conformance verification, bit-exact, but evaluated across the runner's
// vCPU fabric (docs/20-hydra.md): the single-core CRC32C throughput ceiling
// (8 B/cycle × ~31.65 B/msg ≈ 3.96 cyc/msg ≈ 619M msg/s absolute) is broken by
// moving the pure span-CRC evaluation onto worker cores while the ordered fold
// stays on the main core. The 800M threshold is the program target on the
// 4-vCPU GitHub runner; the sustained arm must match the burst arm's verdict.
pub const PR1_HYDRA_MIN_MSG_PER_SEC: u64 = 800_000_000;

// R7: PR-1 GIGAHFT target (Project 1.0B) — >= 1,000,000,000 msg/s sustained
// on the 4-vCPU GitHub Actions runner with the SAME invariants as HYDRA
// (bit-exact, every emitted byte read + CRC32C-verified in-window,
// ALLOC_DELTA = 0). Achieved by the four GIGAHFT levers (docs/21-gigahft.md):
// vector CRC folding, in-place ring stores, fused header decode, and the
// cross-pass double-buffered fabric. The HYDRA 800M gate above remains the
// R6 program level; GIGAHFT is the 1B milestone the sustained arm gates on.
pub const PR1_GIGAHFT_MIN_MSG_PER_SEC: u64 = 1_000_000_000;

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

/// R4: PR-1 TITAN verdict — wall-rate with full byte-level span conformance
/// verification must reach 100M msg/s (see PR1_TITAN_MIN_MSG_PER_SEC).
#[inline]
pub fn evaluate_pr1_titan(wall_rate_msg_per_sec: u64) -> GateVerdict {
    if wall_rate_msg_per_sec >= PR1_TITAN_MIN_MSG_PER_SEC {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

/// R6: PR-1 HYDRA verdict — bit-exact multi-core span conformance wall-rate
/// must reach 800M msg/s (see PR1_HYDRA_MIN_MSG_PER_SEC).
#[inline]
pub fn evaluate_pr1_hydra(wall_rate_msg_per_sec: u64) -> GateVerdict {
    if wall_rate_msg_per_sec >= PR1_HYDRA_MIN_MSG_PER_SEC {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

/// R7: PR-1 GIGAHFT verdict — the 1B msg/s sustained milestone (see
/// PR1_GIGAHFT_MIN_MSG_PER_SEC).
#[inline]
pub fn evaluate_pr1_gigahft(sustained_rate_msg_per_sec: u64) -> GateVerdict {
    if sustained_rate_msg_per_sec >= PR1_GIGAHFT_MIN_MSG_PER_SEC {
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
