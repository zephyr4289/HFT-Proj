//! Recovery intent generator mechanism (doc 05 §10).

use crate::counters::Counters;
use crate::types::RecoveryIntent;

pub const TIME_TRIGGER_NS: u64 = 250_000; // 250 µs
pub const HB_TRIGGER_NS: u64 = 250_000; // 250 µs
pub const RESUGGEST_WINDOW_NS: u64 = 250_000; // 250 µs
pub const HWM_STAGED_THRESHOLD: u64 = 512;
pub const MAX_WIRE_REQUEST: u64 = 65535;

/// Generates a recovery intent if gap triggers (T-HWM, T-TIME, T-HB) or resuggest window trip.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn check_recovery_intent(
    w: u64,
    max_staged: u64,
    staged_count: u32,
    progress_vt: u64,
    hb_seq: u64,
    hb_vt: u64,
    pending_to: &mut Option<u64>,
    last_intent_vt: &mut u64,
    now_ns: u64,
    counters: &mut Counters,
) -> Option<RecoveryIntent> {
    let mut target = None;

    // T-HWM: max_staged - W >= 512, or T-TIME: staged_count > 0 and now - progress_vt >= 250µs
    if (max_staged > w && (max_staged - w) >= HWM_STAGED_THRESHOLD)
        || (staged_count > 0 && now_ns.saturating_sub(progress_vt) >= TIME_TRIGGER_NS)
    {
        target = Some(max_staged);
    }

    // T-HB: hb_seq > W and now - hb_vt >= 250µs
    else if hb_seq > w && now_ns.saturating_sub(hb_vt) >= HB_TRIGGER_NS {
        target = Some(hb_seq);
    }
    // Periodic resuggest of existing pending intent
    else if let Some(p) = *pending_to {
        if now_ns.saturating_sub(*last_intent_vt) >= RESUGGEST_WINDOW_NS {
            *last_intent_vt = now_ns;
            counters.intents_issued += 1;
            return Some(RecoveryIntent {
                from: w,
                to_excl: p,
            });
        }
    }

    if let Some(mut t) = target {
        t = t.min(w + MAX_WIRE_REQUEST);
        let should_emit = match *pending_to {
            None => true,
            Some(prev_p) => t > prev_p,
        };

        if should_emit {
            *pending_to = Some(t);
            *last_intent_vt = now_ns;
            counters.intents_issued += 1;
            return Some(RecoveryIntent {
                from: w,
                to_excl: t,
            });
        }
    }

    None
}
