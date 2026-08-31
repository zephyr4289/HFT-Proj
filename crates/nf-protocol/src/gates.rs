//! Canonical performance gate thresholds and criteria (doc 00, doc 11).
//! Single source of truth for both documentation generation and machine assertion (F-22).

pub const PR1_MIN_SUSTAINED_MSG_PER_SEC: u64 = 10_000_000;
pub const PR2_TARGET_P50_CYCLES: u64 = 60;
pub const PR2_TARGET_P99_CYCLES: u64 = 150;
pub const PR3_MAX_ALLOC_DELTA: u64 = 0;
pub const SAMPLING_INTERVAL: usize = 256;

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
pub fn evaluate_pr3(alloc_delta: u64) -> GateVerdict {
    if alloc_delta == PR3_MAX_ALLOC_DELTA {
        GateVerdict::Pass
    } else {
        GateVerdict::Fail
    }
}
