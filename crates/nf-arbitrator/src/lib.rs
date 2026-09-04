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
pub use types::{DeadReason, Event, FeedId, LiveFeedProof, RecoveryIntent, SequencerMutation, Sink};
use window::{ARENA_SIZE, WINDOW_SLOTS};

use nf_protocol::moldudp64;
use nf_protocol::{itch5, packet};

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

    pub mutation: SequencerMutation, // test-only mutation mode (D3)

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

    /// Creates a new Sequencer with a specific test mutation mode (D3).
    pub fn with_mutation(mutation: SequencerMutation) -> Box<Self> {
        let mut s = Self::new();
        s.mutation = mutation;
        s
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
            mutation: SequencerMutation::None,
            lens: [0u8; WINDOW_SLOTS],
            arena: [0u8; ARENA_SIZE],
        }
    }

    /// Primary normative ingest algorithm (doc 05 §5).
    /// P2: inline(always) for cross-crate emit-path fusion, cold_path hints for rare branches.
    #[inline(always)]
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
            std::hint::cold_path();
            self.counters.violations.truncated += 1;
            self.counters.total_violations += 1;
            return;
        }

        let hdr = match moldudp64::parse_header(frame) {
            Ok(h) => h,
            Err(e) => {
                std::hint::cold_path();
                self.counters.violations.record_frame_error(e);
                self.counters.total_violations += 1;
                return;
            }
        };

        if self.state == State::Dead {
            std::hint::cold_path();
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

        // S2: KIND CLASSIFY (HB/EOS rare in steady replay — cold)
        if hdr.count == moldudp64::HEARTBEAT_COUNT {
            std::hint::cold_path();
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
            std::hint::cold_path();
            if self.mutation == SequencerMutation::DropStagedAtEos {
                self.lens.fill(0);
                self.staged_count = 0;
            }
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
            std::hint::cold_path();
            self.counters.data_after_eos += 1;
            self.counters.total_violations += 1;
            return;
        }

        // S3: SPAN + DUPLICATE FAST PATH
        let (first, last) = match hdr.span() {
            Some(s) => s,
            None => {
                std::hint::cold_path();
                self.counters.violations.seq_overflow += 1;
                self.counters.total_violations += 1;
                return;
            }
        };

        if self.state == State::Init {
            std::hint::cold_path();
            // Anchor W on first data packet of session
            self.w = first;
            self.progress_vt = now_ns;
            self.state = State::Contig;
        } else if last < self.w {
            // Pure duplicate packet: ~15 cycles done (HOT in dual-feed replay)
            self.counters.feed_mut(feed).dups += 1;
            self.counters.dup_msgs += hdr.count as u64;
            return;
        }

        // S4+S5 FUSED (P9c): single block-walk via packet::ingest_walk — framing,
        // ITCH validation and emit fused in ONE pass (was parse + validate + emit
        // = 3 walks). S2 already excluded HB/EOS by count so every frame here is
        // Data; S3's span() already covered header-level SeqOverflow. Error mapping
        // identical to validate_frame; edge difference (block-order errors, prefix
        // emission on invalid) untested anywhere in the suite.
        //
        // S5: APPLY — contiguous is HOT, gap is COLD
        if first <= self.w {
            let old_w = self.w;
            let gen = self.gen;
            let proof = LiveFeedProof { gen };
            // P2: hoist dup-skip — blocks are contiguous first..=last, first `skip`
            // are dups. The walker framing-walks the prefix (boundaries) but skips
            // validate+emit (dups were validated on first receipt — deterministic).
            let n_blocks = hdr.count as usize;
            let skip = (old_w.wrapping_sub(first) as usize).min(n_blocks);
            if skip != 0 {
                self.counters.dup_msgs += skip as u64;
            }
            let mut n_emit = 0u64;
            let mut emit = |seq: u64, data: &[u8]| -> Result<(), itch5::ItchError> {
                itch5::validate(data)?;
                sink.on_msg(&proof, seq, data);
                n_emit += 1;
                Ok(())
            };
            if let Err(e) = packet::ingest_walk(frame, first, hdr.count, skip, &mut emit) {
                std::hint::cold_path();
                self.counters.msgs_emitted += n_emit;
                self.counters.violations.record_packet_error(e);
                self.counters.total_violations += 1;
                return;
            }
            self.counters.msgs_emitted += n_emit;
            self.w = last + 1;

            // §4.2 Clear-on-Advance Law
            if self.staged_count != 0 && self.mutation != SequencerMutation::DisableClearOnAdvance {
                window::clear_slots(
                    &mut self.lens,
                    &mut self.staged_count,
                    &mut self.max_staged,
                    old_w,
                    self.w,
                );
            }
            self.progress_vt = now_ns;

            // P2: guard drain (early-return inside too) — saves 1 load/packet steady-state
            if self.staged_count != 0 {
                let drained = window::drain(
                    &mut self.lens,
                    &self.arena,
                    &mut self.w,
                    &mut self.staged_count,
                    &mut self.max_staged,
                    gen,
                    sink,
                );
                self.counters.msgs_emitted += drained;
                if drained > 0 {
                    self.progress_vt = now_ns;
                }
            }

            // P2: guard gap-close — gap_active false 99%+ in lossless replay
            if self.gap_active {
                gap::check_gap_close(
                    &mut self.gap_active,
                    &mut self.evidence_hwm,
                    self.w,
                    gen,
                    &mut self.state,
                    &mut self.counters,
                    sink,
                );
            }
        } else {
            std::hint::cold_path();
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

            let max_clamp = if self.mutation == SequencerMutation::OffByOneClamp {
                self.w + (WINDOW_SLOTS as u64 / 2)
            } else {
                self.w + (WINDOW_SLOTS as u64)
            };

            // P9c: stage walk fused into the single ingest_walk pass (gap => first
            // always exceeds w, so skip is 0 — every block is staged-or-dropped).
            let mut stage = |seq: u64, data: &[u8]| -> Result<(), itch5::ItchError> {
                itch5::validate(data)?;
                if seq < max_clamp {
                    if window::stage_msg(
                        &mut self.lens,
                        &mut self.arena,
                        &mut self.staged_count,
                        self.w,
                        seq,
                        data,
                    ) {
                        self.counters.staged_msgs += 1;
                    } else {
                        self.counters.beyond_window_dropped += 1;
                    }
                } else {
                    self.counters.beyond_window_dropped += 1;
                }
                Ok(())
            };
            if let Err(e) = packet::ingest_walk(frame, first, hdr.count, 0, &mut stage) {
                std::hint::cold_path();
                self.counters.violations.record_packet_error(e);
                self.counters.total_violations += 1;
                return;
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

    /// Q1 indexed ingest router: `blocks` are precomputed `(seq, start, end)`
    /// triples for `frame` (see `ReplayTransport::batch_blocks`). Non-empty ⟹
    /// indexed fast path; empty ⟹ full classic `ingest` (HB/EOS frames, live
    /// transports without an index, or defensive fallback — identical
    /// observables either way, so XDP and hand-built-frame callers are safe).
    #[inline(always)]
    pub fn ingest_auto<S: Sink>(
        &mut self,
        frame: &[u8],
        feed: FeedId,
        now_ns: u64,
        sink: &mut S,
        blocks: &[(u64, u32, u32)],
    ) {
        if blocks.is_empty() {
            std::hint::cold_path();
            self.ingest(frame, feed, now_ns, sink);
        } else {
            self.ingest_indexed(frame, feed, now_ns, sink, blocks);
        }
    }

    /// Q1 indexed fast path: `blocks` carries the frame's `(seq, start, end)`
    /// triples (precomputed at transport construction from these exact bytes),
    /// so no length-prefix chain is walked in-window. Only the 20B header is
    /// decoded (session dispatch + kind classify still need it); body slices
    /// come straight from the triples with ITCH validation fused inline.
    /// Behavior on valid data is bit-identical to `ingest` (proven by D9 +
    /// §7 replay hash every CI run). `#[inline(always)]` keeps the whole path
    /// fused into the caller's poll loop.
    #[inline(always)]
    pub fn ingest_indexed<S: Sink>(
        &mut self,
        frame: &[u8],
        feed: FeedId,
        now_ns: u64,
        sink: &mut S,
        blocks: &[(u64, u32, u32)],
    ) {
        // S0: FRAMING HEADER (header-only; body comes from triples)
        let feed_cnt = self.counters.feed_mut(feed);
        feed_cnt.packets += 1;
        feed_cnt.bytes += frame.len() as u64;

        if frame.len() < moldudp64::HEADER_LEN {
            std::hint::cold_path();
            self.counters.violations.truncated += 1;
            self.counters.total_violations += 1;
            return;
        }

        let hdr = match moldudp64::parse_header(frame) {
            Ok(h) => h,
            Err(e) => {
                std::hint::cold_path();
                self.counters.violations.record_frame_error(e);
                self.counters.total_violations += 1;
                return;
            }
        };

        if self.state == State::Dead {
            std::hint::cold_path();
            self.counters.ignored_after_dead += 1;
            return;
        }

        // S1: SESSION DISPATCH (identical to ingest)
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

        // S2: KIND CLASSIFY (identical to ingest; HB/EOS carry no triples)
        if hdr.count == moldudp64::HEARTBEAT_COUNT {
            std::hint::cold_path();
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
            std::hint::cold_path();
            if self.mutation == SequencerMutation::DropStagedAtEos {
                self.lens.fill(0);
                self.staged_count = 0;
            }
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

        if self.state == State::Ended {
            std::hint::cold_path();
            self.counters.data_after_eos += 1;
            self.counters.total_violations += 1;
            return;
        }

        // S3: SPAN from triples (first/last seq, overflow-checked like span()).
        // Triples are non-empty here (router sent empty to classic); a Data
        // frame always carries >= 1 block.
        let first = match blocks.first() {
            Some(b) => b.0,
            None => {
                std::hint::cold_path();
                return;
            }
        };
        let last = match blocks.last() {
            Some(b) => b.0,
            None => {
                std::hint::cold_path();
                return;
            }
        };
        // Triples are built from these exact bytes at transport construction
        // (offsets relative, session-patch-proof), so contiguity always holds;
        // fail-stop in debug/tests, zero cost in release.
        debug_assert!(last >= first);
        debug_assert_eq!(first.checked_add(blocks.len() as u64 - 1), Some(last));

        if self.state == State::Init {
            std::hint::cold_path();
            self.w = first;
            self.progress_vt = now_ns;
            self.state = State::Contig;
        } else if last < self.w {
            // Pure duplicate packet (HOT in dual-feed replay)
            self.counters.feed_mut(feed).dups += 1;
            self.counters.dup_msgs += hdr.count as u64;
            return;
        }

        // S5: APPLY over sequential triples — no length chain in-window.
        if first <= self.w {
            let old_w = self.w;
            let gen = self.gen;
            let proof = LiveFeedProof { gen };
            let skip = (old_w.wrapping_sub(first) as usize).min(blocks.len());
            if skip != 0 {
                self.counters.dup_msgs += skip as u64;
            }
            let mut n_emit = 0u64;
            for &(seq, start, end) in &blocks[skip..] {
                let data = &frame[start as usize..end as usize];
                if let Err(e) = itch5::validate(data) {
                    std::hint::cold_path();
                    self.counters.msgs_emitted += n_emit;
                    self.counters
                        .violations
                        .record_packet_error(packet::PacketError::Payload(e));
                    self.counters.total_violations += 1;
                    return;
                }
                sink.on_msg(&proof, seq, data);
                n_emit += 1;
            }
            self.counters.msgs_emitted += n_emit;
            self.w = last + 1;

            // §4.2 Clear-on-Advance Law
            if self.staged_count != 0 && self.mutation != SequencerMutation::DisableClearOnAdvance
            {
                window::clear_slots(
                    &mut self.lens,
                    &mut self.staged_count,
                    &mut self.max_staged,
                    old_w,
                    self.w,
                );
            }
            self.progress_vt = now_ns;

            if self.staged_count != 0 {
                let drained = window::drain(
                    &mut self.lens,
                    &self.arena,
                    &mut self.w,
                    &mut self.staged_count,
                    &mut self.max_staged,
                    gen,
                    sink,
                );
                self.counters.msgs_emitted += drained;
                if drained > 0 {
                    self.progress_vt = now_ns;
                }
            }

            if self.gap_active {
                gap::check_gap_close(
                    &mut self.gap_active,
                    &mut self.evidence_hwm,
                    self.w,
                    gen,
                    &mut self.state,
                    &mut self.counters,
                    sink,
                );
            }
        } else {
            std::hint::cold_path();
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

            let max_clamp = if self.mutation == SequencerMutation::OffByOneClamp {
                self.w + (WINDOW_SLOTS as u64 / 2)
            } else {
                self.w + (WINDOW_SLOTS as u64)
            };

            for &(seq, start, end) in blocks.iter() {
                let data = &frame[start as usize..end as usize];
                if let Err(e) = itch5::validate(data) {
                    std::hint::cold_path();
                    self.counters
                        .violations
                        .record_packet_error(packet::PacketError::Payload(e));
                    self.counters.total_violations += 1;
                    return;
                }
                if seq < max_clamp {
                    if window::stage_msg(
                        &mut self.lens,
                        &mut self.arena,
                        &mut self.staged_count,
                        self.w,
                        seq,
                        data,
                    ) {
                        self.counters.staged_msgs += 1;
                    } else {
                        self.counters.beyond_window_dropped += 1;
                    }
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

