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

// R8: PR-1 pure-ingest target (docs/22-r8-teraphase.md) — >= 2,000,000,000
// msg/s on the SAME single core the statistical gate pins (ci.sh step 16
// `taskset -c 1`), with the full ingest pipeline live every pass: transport
// poll, MoldUDP64 framing, session arbitration, duplicate rejection,
// watermark sequencing, and span emission over the exact canonical schedule.
// No byte-level verification is claimed in this arm (that is the full-verify
// gate below) — but nothing about the ingest itself may be skipped,
// memoized across passes, or redefined: the sequencer state machine runs
// for real on every frame, and the golden message population (505,849)
// must be emitted and counted every pass.
pub const PR1_R8_PURE_INGEST_MIN_MSG_PER_SEC: u64 = 2_000_000_000;

// R8: PR-1 full-verification target — >= 1,000,000,000 msg/s sustained
// (>= 5 s, fresh sessions, cross-pass double-buffered fabric) with the
// complete HYDRA/GIGAHFT invariant set: bit-exact three-layer parity
// (sequential == fabric == reference), every emitted byte read and
// CRC32C-verified in-window on worker cores, ALLOC_DELTA = 0. This is the
// GIGAHFT milestone carried to its 1B sustained level on the runner fabric.
pub const PR1_R8_FULL_VERIFY_MIN_MSG_PER_SEC: u64 = 1_000_000_000;

// R16: PR-1 full-verification target — >= 2,000,000,000 msg/s sustained on
// the SAME 4-vCPU standard-runner fabric (2 physical cores + SMT — the R12
// target ruling confines the claim to this shape; larger runners do not
// count) with the SAME invariant set (bit-exact, every byte verified
// in-window, ALLOC_DELTA = 0, no memoization across passes). The program
// that carries it is docs/29 (Double Helix): the array-driven submission
// (rxdesc — R16b) removes the main-side per-span wall, the distinct-core
// worker placement (R16d) unlocks the 2cpu_distinct fold pool, and the
// claim follows the R12 protocol (median of >= 3 healthy same-class draws,
// kbench fold512_r 1t >= 30.0 GB/s each). The 1B R8 gate above stays the
// CI-hard floor; this 2B verdict is REPORTED per draw (elevating it to an
// assert happens only when the median healthy draw crosses it — the same
// submission-time elevation rule the R8 2B pure-ingest gate used).
pub const PR1_R16_FULL_VERIFY_MIN_MSG_PER_SEC: u64 = 2_000_000_000;

// R16: PR-1 pure-ingest target — >= 5,000,000,000 msg/s on the pinned-core
// span arm (the Front A shape: full ingest pipeline live, no byte-level
// verification claimed). Reported per draw alongside the R8 2B hard gate;
// the lever is Lever B (rxbuild — docs/29 §5). Non-asserting until the
// median healthy draw crosses it (the R12 protocol).
pub const PR1_R16_PURE_INGEST_MIN_MSG_PER_SEC: u64 = 5_000_000_000;

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

/// R8: PR-1 pure-ingest verdict — the 2B msg/s single-core span-arm ceiling
/// (see PR1_R8_PURE_INGEST_MIN_MSG_PER_SEC). The span arm's median wall-rate
/// over the statistical-gate run set (30 runs + 5 warmup, pinned core) must
/// cross the threshold with the golden population emitted every pass.
#[inline]
pub fn evaluate_pr1_r8_pure_ingest(span_rate_msg_per_sec: u64) -> GateVerdict {
    if span_rate_msg_per_sec >= PR1_R8_PURE_INGEST_MIN_MSG_PER_SEC {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

/// R8: PR-1 full-verification verdict — the 1B msg/s sustained fabric target
/// (see PR1_R8_FULL_VERIFY_MIN_MSG_PER_SEC), same invariant set as GIGAHFT.
#[inline]
pub fn evaluate_pr1_r8_full_verify(sustained_rate_msg_per_sec: u64) -> GateVerdict {
    if sustained_rate_msg_per_sec >= PR1_R8_FULL_VERIFY_MIN_MSG_PER_SEC {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

/// R16: PR-1 full-verification verdict — the 2B msg/s sustained fabric
/// target (see PR1_R16_FULL_VERIFY_MIN_MSG_PER_SEC), same invariant set.
#[inline]
pub fn evaluate_pr1_r16_full_verify(sustained_rate_msg_per_sec: u64) -> GateVerdict {
    if sustained_rate_msg_per_sec >= PR1_R16_FULL_VERIFY_MIN_MSG_PER_SEC {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}

/// R16: PR-1 pure-ingest verdict — the 5B msg/s pinned-core span-arm
/// target (see PR1_R16_PURE_INGEST_MIN_MSG_PER_SEC).
#[inline]
pub fn evaluate_pr1_r16_pure_ingest(span_rate_msg_per_sec: u64) -> GateVerdict {
    if span_rate_msg_per_sec >= PR1_R16_PURE_INGEST_MIN_MSG_PER_SEC {
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

        // R8 tripwires: both new gates must FAIL on out-of-band inputs and
        // PASS exactly at the threshold (2B pure ingest / 1B full verify).
        assert_eq!(evaluate_pr1_r8_pure_ingest(1_999_999_999), GateVerdict::Fail);
        assert_eq!(evaluate_pr1_r8_pure_ingest(0), GateVerdict::Fail);
        assert_eq!(evaluate_pr1_r8_pure_ingest(2_000_000_000), GateVerdict::Pass);
        assert_eq!(evaluate_pr1_r8_pure_ingest(2_500_000_000), GateVerdict::Pass);

        assert_eq!(evaluate_pr1_r8_full_verify(999_999_999), GateVerdict::Fail);
        assert_eq!(evaluate_pr1_r8_full_verify(0), GateVerdict::Fail);
        assert_eq!(evaluate_pr1_r8_full_verify(1_000_000_000), GateVerdict::Pass);
        assert_eq!(evaluate_pr1_r8_full_verify(1_400_000_000), GateVerdict::Pass);

        // R16 tripwires: 2B full-verify / 5B pure-ingest — FAIL below,
        // PASS exactly at the threshold.
        assert_eq!(evaluate_pr1_r16_full_verify(1_999_999_999), GateVerdict::Fail);
        assert_eq!(evaluate_pr1_r16_full_verify(0), GateVerdict::Fail);
        assert_eq!(evaluate_pr1_r16_full_verify(2_000_000_000), GateVerdict::Pass);
        assert_eq!(evaluate_pr1_r16_pure_ingest(4_999_999_999), GateVerdict::Fail);
        assert_eq!(evaluate_pr1_r16_pure_ingest(5_000_000_000), GateVerdict::Pass);
        assert_eq!(evaluate_pr1_r16_pure_ingest(6_000_000_000), GateVerdict::Pass);
    }
}
