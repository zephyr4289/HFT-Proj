#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::disallowed_types))]

pub mod counters;
pub mod gap;
pub mod intent;
pub mod session;
pub mod state;
pub mod types;
pub mod window;

pub use counters::{Counters, FeedCounters, ViolationCounters};
pub use state::State;
pub use types::{DeadReason, Event, FeedId, LiveFeedProof, RecoveryIntent, Sink};
use window::{ARENA_SIZE, WINDOW_SLOTS};

use nf_protocol::moldudp64;
use nf_protocol::packet;

#[repr(align(64))]
pub struct Sequencer {
    // ── line 0 ── written once per packet ──────────────────────
    w: u64,                      // watermark: next expected seq

    // ── line 1 ── written per event ────────────────────────────
    gen: u64,                    // proof era counter (§7)
    session: [u8; 10],
    state: State,                // §3 (u8-tagged)
    gap_active: bool,
    evidence_hwm: u64,           // highest seq KNOWN transmitted (gap-era)
    max_staged: u64,             // max staged seq (0 = none)
    staged_count: u32,
    progress_vt: u64,            // last W advance (or anchor) vt
    hb_seq: u64,
    hb_vt: u64,                  // last heartbeat evidence > W
    pending_to: Option<u64>,     // outstanding intent, exclusive end (§10)
    last_intent_vt: u64,

    // ── lines 2..3 ──
    counters: Counters,          // §11, Copy

    // ── lines 4..19 ── presence bitmap ────────────────────────
    lens: [u8; WINDOW_SLOTS],    // slot i: 0 = absent, else msg length

    // ── lines 20..1043 ── arena (64 KiB) ──────────────────────
    arena: [u8; ARENA_SIZE],     // slot i at byte offset i << 6
}

impl Sequencer {
    /// Creates a new Sequencer on heap (one startup allocation O-6).
    pub fn new() -> Box<Self> {
        Box::new(Self::new_unboxed())
    }

    /// Creates an unboxed Sequencer instance.
    pub fn new_unboxed() -> Self {
        Self {
            w: 0,
            gen: 0,
            session: [0u8; 10],
            state: State::Init,
            gap_active: false,
            evidence_hwm: 0,
            max_staged: 0,
            staged_count: 0,
            progress_vt: 0,
            hb_seq: 0,
            hb_vt: 0,
            pending_to: None,
            last_intent_vt: 0,
            counters: Counters::default(),
            lens: [0u8; WINDOW_SLOTS],
            arena: [0u8; ARENA_SIZE],
        }
    }

    /// Primary normative ingest algorithm (doc 05 §5).
    pub fn ingest<S: Sink>(
        &mut self,
        frame: &[u8],
        feed: FeedId,
        now_ns: u64,
        sink: &mut S,
    ) {
        // S0: FRAMING HEADER
        let feed_cnt = self.counters.feed_mut(feed);
        feed_cnt.packets += 1;
        feed_cnt.bytes += frame.len() as u64;

        if frame.len() < moldudp64::HEADER_LEN {
            self.counters.violations.truncated += 1;
            self.counters.total_violations += 1;
            return;
        }

        let hdr = match moldudp64::parse_header(frame) {
            Ok(h) => h,
            Err(e) => {
                self.counters.violations.record_frame_error(e);
                self.counters.total_violations += 1;
                return;
            }
        };

        if self.state == State::Dead {
            self.counters.ignored_after_dead += 1;
            return;
        }

        // S1: SESSION DISPATCH
        session::session_dispatch(
            &mut self.session,
            hdr.session,
            &mut self.lens,
            &mut self.staged_count,
            &mut self.max_staged,
            &mut self.gap_active,
            &mut self.evidence_hwm,
            &mut self.hb_seq,
            &mut self.hb_vt,
            &mut self.pending_to,
            &mut self.gen,
            &mut self.state,
            &mut self.counters,
            sink,
        );

        // S2: KIND CLASSIFY
        if hdr.count == moldudp64::HEARTBEAT_COUNT {
            session::handle_heartbeat(
                hdr.seq,
                feed,
                now_ns,
                self.w,
                &mut self.hb_seq,
                &mut self.hb_vt,
                &mut self.gap_active,
                &mut self.gen,
                &mut self.evidence_hwm,
                &mut self.state,
                &mut self.counters,
                sink,
            );
            return;
        }

        if hdr.count == moldudp64::EOS_COUNT {
            session::handle_eos(
                &hdr,
                self.w,
                self.session,
                &mut self.state,
                &mut self.counters,
                sink,
            );
            return;
        }

        // Check if data packet arrives after EOS in current session
        if self.state == State::Ended {
            self.counters.data_after_eos += 1;
            self.counters.total_violations += 1;
            return;
        }

        // S3: SPAN + DUPLICATE FAST PATH
        let (first, last) = match hdr.span() {
            Some(s) => s,
            None => {
                self.counters.violations.seq_overflow += 1;
                self.counters.total_violations += 1;
                return;
            }
        };

        if self.state == State::Init {
            // Anchor W on first data packet of session
            self.w = first;
            self.progress_vt = now_ns;
            self.state = State::Contig;
        } else if last < self.w {
            // Pure duplicate packet: ~15 cycles done
            self.counters.feed_mut(feed).dups += 1;
            self.counters.dup_msgs += hdr.count as u64;
            return;
        }

        // S4: FULL VALIDATION
        let parsed = match packet::validate_frame(frame) {
            Ok(p) => p,
            Err(e) => {
                self.counters.violations.record_packet_error(e);
                self.counters.total_violations += 1;
                return;
            }
        };

        let blocks = match parsed {
            moldudp64::Parsed::Data { blocks, .. } => blocks,
            _ => return,
        };

        // S5: APPLY
        if first <= self.w {
            let old_w = self.w;
            let proof = LiveFeedProof { gen: self.gen };
            for block in blocks {
                if block.seq >= self.w {
                    sink.on_msg(&proof, block.seq, block.data);
                    self.counters.msgs_emitted += 1;
                } else {
                    self.counters.dup_msgs += 1;
                }
            }
            self.w = last + 1;

            // §4.2 Clear-on-Advance Law
            if self.staged_count != 0 {
                window::clear_slots(
                    &mut self.lens,
                    &mut self.staged_count,
                    &mut self.max_staged,
                    old_w,
                    self.w,
                );
            }
            self.progress_vt = now_ns;

            let drained = window::drain(
                &mut self.lens,
                &self.arena,
                &mut self.w,
                &mut self.staged_count,
                &mut self.max_staged,
                self.gen,
                sink,
            );
            self.counters.msgs_emitted += drained;
            if drained > 0 {
                self.progress_vt = now_ns;
            }

            gap::check_gap_close(
                &mut self.gap_active,
                &mut self.evidence_hwm,
                self.w,
                self.gen,
                &mut self.state,
                &mut self.counters,
                sink,
            );
        } else {
            gap::gap_evidence(
                &mut self.gap_active,
                &mut self.gen,
                &mut self.evidence_hwm,
                self.w,
                first,
                &mut self.state,
                &mut self.counters,
                sink,
            );

            for block in blocks {
                let staged = window::stage_msg(
                    &mut self.lens,
                    &mut self.arena,
                    &mut self.staged_count,
                    self.w,
                    block.seq,
                    block.data,
                );
                if staged {
                    self.counters.staged_msgs += 1;
                } else {
                    self.counters.beyond_window_dropped += 1;
                }
            }
            if last >= self.w + (WINDOW_SLOTS as u64) {
                self.evidence_hwm = self.evidence_hwm.max(last + 1);
            }
            self.max_staged = self.max_staged.max(last);

            let drained = window::drain(
                &mut self.lens,
                &self.arena,
                &mut self.w,
                &mut self.staged_count,
                &mut self.max_staged,
                self.gen,
                sink,
            );
            self.counters.msgs_emitted += drained;
            if drained > 0 {
                self.progress_vt = now_ns;
            }

            gap::check_gap_close(
                &mut self.gap_active,
                &mut self.evidence_hwm,
                self.w,
                self.gen,
                &mut self.state,
                &mut self.counters,
                sink,
            );
        }

        if let Some(p) = self.pending_to {
            if self.w >= p {
                self.pending_to = None;
            }
        }
    }

    /// Evaluates gap recovery intent (doc 05 §10).
    pub fn recovery_intent(&mut self, now_ns: u64) -> Option<RecoveryIntent> {
        intent::check_recovery_intent(
            self.w,
            self.max_staged,
            self.staged_count,
            self.progress_vt,
            self.hb_seq,
            self.hb_vt,
            &mut self.pending_to,
            &mut self.last_intent_vt,
            now_ns,
            &mut self.counters,
        )
    }

    /// Seals the sequencer into permanent DEAD state.
    pub fn seal<S: Sink>(&mut self, reason: DeadReason, sink: &mut S) {
        session::seal(reason, self.w, &mut self.state, sink);
    }

    #[inline]
    pub fn watermark(&self) -> u64 {
        self.w
    }

    #[inline]
    pub fn session(&self) -> [u8; 10] {
        self.session
    }

    #[inline]
    pub fn state(&self) -> State {
        self.state
    }

    #[inline]
    pub fn gen(&self) -> u64 {
        self.gen
    }

    #[inline]
    pub fn staged_count(&self) -> u32 {
        self.staged_count
    }

    #[inline]
    pub fn is_gap_active(&self) -> bool {
        self.gap_active
    }

    #[inline]
    pub fn counters(&self) -> Counters {
        self.counters
    }

    #[inline]
    pub fn lens(&self) -> &[u8; WINDOW_SLOTS] {
        &self.lens
    }
}

impl Default for Sequencer {
    fn default() -> Self {
        Self::new_unboxed()
    }
}

impl Default for Box<Sequencer> {
    fn default() -> Self {
        Sequencer::new()
    }
}
