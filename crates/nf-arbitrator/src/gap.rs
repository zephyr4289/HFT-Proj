//! Gap lifecycle, evidence tracking, gen law, and event grammar (doc 05 §7).

use crate::counters::Counters;
use crate::state::State;
use crate::types::{Event, Sink};

/// Records gap evidence. A gap opens if not already open and x > W.
/// P2: always-inline — cold path out-of-lined via cold_path() at call site.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn gap_evidence<S: Sink>(
    gap_active: &mut bool,
    gen: &mut u64,
    evidence_hwm: &mut u64,
    w: u64,
    x: u64,
    state: &mut State,
    counters: &mut Counters,
    sink: &mut S,
) {
    if !*gap_active && x > w {
        *gap_active = true;
        *gen = gen.wrapping_add(1);
        counters.gap_opens += 1;
        *state = State::Gap;
        *evidence_hwm = x;
        sink.on_event(&Event::GapOpened {
            from: w,
            ahead: Some(x),
            gen: *gen,
        });
    } else if *gap_active {
        *evidence_hwm = (*evidence_hwm).max(x);
    }
}

/// Checks if an active gap is fully closed by W advancing >= evidence_hwm.
/// P2: always-inline — guarded by `if gap_active` at hot call site.
#[inline(always)]
pub fn check_gap_close<S: Sink>(
    gap_active: &mut bool,
    evidence_hwm: &mut u64,
    w: u64,
    gen: u64,
    state: &mut State,
    counters: &mut Counters,
    sink: &mut S,
) {
    if *gap_active && w >= *evidence_hwm {
        *gap_active = false;
        *state = State::Contig;
        counters.reanchors += 1;
        sink.on_event(&Event::ReAnchored {
            gen,
            at: w,
        });
        *evidence_hwm = 0;
    }
}
