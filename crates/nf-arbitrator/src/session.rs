//! Session lifecycle, anchor law, EOS, and session boundary transitions (doc 05 §8).

use crate::counters::Counters;
use crate::gap::gap_evidence;
use crate::state::State;
use crate::types::{DeadReason, Event, FeedId, Sink};
use crate::window::WINDOW_SLOTS;
use nf_protocol::moldudp64::Header;

/// Handles session discovery or session boundary transition.
#[inline]
pub fn session_dispatch<S: Sink>(
    session: &mut [u8; 10],
    new_session: [u8; 10],
    lens: &mut [u8; WINDOW_SLOTS],
    staged_count: &mut u32,
    max_staged: &mut u64,
    gap_active: &mut bool,
    evidence_hwm: &mut u64,
    hb_seq: &mut u64,
    hb_vt: &mut u64,
    pending_to: &mut Option<u64>,
    gen: &mut u64,
    state: &mut State,
    counters: &mut Counters,
    sink: &mut S,
) {
    if *session == [0u8; 10] {
        // Initial session discovery
        *session = new_session;
        counters.sessions += 1;
    } else if *session != new_session {
        // Session boundary
        let prev = *session;
        *session = new_session;
        counters.sessions += 1;

        if *staged_count != 0 {
            lens.fill(0);
            *staged_count = 0;
            counters.window_flushed += 1;
        }
        *max_staged = 0;
        *gap_active = false;
        *evidence_hwm = 0;
        *hb_seq = 0;
        *hb_vt = 0;
        *pending_to = None;

        *gen = gen.wrapping_add(1);
        sink.on_event(&Event::SessionBoundary {
            prev,
            next: new_session,
            gen: *gen,
        });
        *state = State::Init;
    }
}

/// Handles heartbeat packet classification (count == 0).
#[inline]
pub fn handle_heartbeat<S: Sink>(
    seq: u64,
    _feed: FeedId,
    now_ns: u64,
    w: u64,
    hb_seq: &mut u64,
    hb_vt: &mut u64,
    gap_active: &mut bool,
    gen: &mut u64,
    evidence_hwm: &mut u64,
    state: &mut State,
    counters: &mut Counters,
    sink: &mut S,
) {
    counters.heartbeats += 1;
    if *state == State::Dead {
        return;
    }
    if *state == State::Init {
        *hb_seq = seq;
        *hb_vt = now_ns;
        return;
    }
    if *state == State::Ended {
        return;
    }

    if seq > w {
        *hb_seq = seq;
        *hb_vt = now_ns;
        gap_evidence(gap_active, gen, evidence_hwm, w, seq, state, counters, sink);
    }
}

/// Handles End-of-Session packet classification (count == 0xFFFF).
#[inline]
pub fn handle_eos<S: Sink>(
    hdr: &Header,
    w: u64,
    session: [u8; 10],
    state: &mut State,
    counters: &mut Counters,
    sink: &mut S,
) {
    if *state == State::Dead {
        return;
    }
    if *state == State::Ended {
        counters.eos_dup += 1;
        return;
    }

    counters.eos_seen += 1;
    let final_wm = w;
    let announced_next = hdr.seq;
    *state = State::Ended;

    sink.on_event(&Event::EndOfSession {
        session,
        final_wm,
        announced_next,
    });
}

/// Permanently seals the sequencer to DEAD state (doc 05 §10).
#[inline]
pub fn seal<S: Sink>(
    reason: DeadReason,
    w: u64,
    state: &mut State,
    sink: &mut S,
) {
    if *state != State::Dead {
        *state = State::Dead;
        sink.on_event(&Event::SessionDead {
            reason,
            last_wm: w,
        });
    }
}
