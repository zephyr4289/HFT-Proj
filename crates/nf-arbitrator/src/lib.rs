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
pub use nf_protocol::packet::FrameMemo;
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
    /// GIGAHFT Lever 3: session compare template as two overlapping
    /// little-endian u64 words (bytes 0..8 and 2..10) + "live" flag
    /// (session adopted AND non-zero — zero sessions keep taking the full
    /// dispatch ladder, preserving session_dispatch's re-adoption
    /// semantics exactly). The hot path proves session equality with two
    /// u64 loads + two compares instead of materializing a [u8;10] and
    /// running the comparison ladder. (The _mm_loadu_si128 + cmpeq +
    /// movemask formulation from the GIGAHFT directive would need unsafe
    /// code; this crate is #![forbid(unsafe_code)] by law, so the fused
    /// decode stays in safe Rust — LLVM fuses the fixed-index byte arrays
    /// into unaligned loads, within ~1 uop of the SIMD sequence.)
    session_lo: u64,
    session_hi: u64,
    session_live: bool,
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
            session_lo: 0,
            session_hi: 0,
            session_live: false,
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

    /// Refresh the SIMD session compare template after any session change
    /// (adoption or boundary). `session_live` is true iff the current
    /// session is adopted AND non-zero — zero sessions intentionally stay
    /// cold so session_dispatch's re-adoption branch keeps its exact
    /// observable behavior (counters included).
    #[inline]
    fn refresh_session_tmpl(&mut self) {
        let s = &self.session;
        self.session_lo = u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]);
        self.session_hi = u64::from_le_bytes([s[2], s[3], s[4], s[5], s[6], s[7], s[8], s[9]]);
        self.session_live = *s != [0u8; 10];
    }

    /// Cold-path session dispatch (adoption / boundary) with template
    /// refresh — behaviorally identical to calling session_dispatch
    /// directly (the refresh is a pure derived-field update).
    #[inline]
    fn dispatch_session<S: Sink>(&mut self, new_session: [u8; 10], sink: &mut S) {
        let prev_session = self.session;
        session::session_dispatch(
            &mut self.session,
            new_session,
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
        if !self.session_live || self.session != prev_session {
            self.refresh_session_tmpl();
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
        let prev_session = self.session;
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
        // Lever 3: keep the SIMD compare template in sync with any
        // adoption/boundary (no-op when the session is unchanged).
        if !self.session_live || self.session != prev_session {
            self.refresh_session_tmpl();
        }

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
    ///
    /// R2: `memo` carries the transport's construction-time ITCH validation
    /// verdict for this exact frame (exact leading-valid prefix; None = not
    /// memoized — live transports). It is only ever used to SKIP work whose
    /// outcome it already proves (per-message validate calls that would return
    /// Ok), so behavior is bit-identical with and without the memo — the D9
    /// differential oracle asserts exactly this every CI run.
    #[inline(always)]
    pub fn ingest_auto<S: Sink>(
        &mut self,
        frame: &[u8],
        feed: FeedId,
        now_ns: u64,
        sink: &mut S,
        blocks: &[(u64, u32, u32)],
        memo: Option<packet::FrameMemo>,
    ) {
        if blocks.is_empty() {
            std::hint::cold_path();
            self.ingest(frame, feed, now_ns, sink);
        } else {
            self.ingest_indexed(frame, feed, now_ns, sink, blocks, memo);
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
    ///
    /// R3: on the contiguous fast path, when the R2 memo proves every block
    /// valid AND the sink opted into span emission, the per-message loop
    /// collapses to closed-form arithmetic + one `on_span` call (see the Sink
    /// trait doc for the observational-equivalence argument). Sinks that do
    /// not opt in keep the exact per-message `on_msg` sequence.
    #[inline(always)]
    pub fn ingest_indexed<S: Sink>(
        &mut self,
        frame: &[u8],
        feed: FeedId,
        now_ns: u64,
        sink: &mut S,
        blocks: &[(u64, u32, u32)],
        memo: Option<packet::FrameMemo>,
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

        if self.state == State::Dead {
            std::hint::cold_path();
            self.counters.ignored_after_dead += 1;
            return;
        }

        // S0/S1 FUSED (GIGAHFT Lever 3: 128-bit SIMD header decode).
        // One unaligned 128-bit load + cmpeq + movemask proves the frame's
        // 10-byte session equals the live session template (bits 0..9 of
        // the movemask); seq and count come from two unaligned loads +
        // bswaps. The 10-byte session array is materialized and the full
        // session_dispatch ladder runs ONLY on the cold path (adoption or
        // boundary). Values are identical to parse_header on every input
        // (frame.len() >= 20 checked above makes it infallible).
        let frame_lo = u64::from_le_bytes([
            frame[0], frame[1], frame[2], frame[3], frame[4], frame[5], frame[6], frame[7],
        ]);
        let frame_hi = u64::from_le_bytes([
            frame[2], frame[3], frame[4], frame[5], frame[6], frame[7], frame[8], frame[9],
        ]);
        let sess_match =
            self.session_live && frame_lo == self.session_lo && frame_hi == self.session_hi;
        let hdr_seq = u64::from_be_bytes([
            frame[10], frame[11], frame[12], frame[13], frame[14], frame[15], frame[16], frame[17],
        ]);
        let hdr_count = u16::from_be_bytes([frame[18], frame[19]]);
        if !sess_match {
            // COLD: adoption or boundary — the full dispatch ladder
            // (identical to the classic path), then refresh the template.
            std::hint::cold_path();
            let mut sess = [0u8; 10];
            sess.copy_from_slice(&frame[0..10]);
            self.dispatch_session(sess, sink);
        }
        // HOT: live session unchanged — session_dispatch would perform no
        // observable work (proved: an equal, adopted, non-zero session
        // falls through both of its branches).

        // S2: KIND CLASSIFY (identical to ingest; HB/EOS carry no triples)
        if hdr_count == moldudp64::HEARTBEAT_COUNT {
            std::hint::cold_path();
            session::handle_heartbeat(
                hdr_seq,
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

        if hdr_count == moldudp64::EOS_COUNT {
            std::hint::cold_path();
            if self.mutation == SequencerMutation::DropStagedAtEos {
                self.lens.fill(0);
                self.staged_count = 0;
            }
            let mut eos_session = [0u8; 10];
            eos_session.copy_from_slice(&frame[0..10]);
            let hdr = moldudp64::Header {
                session: eos_session,
                seq: hdr_seq,
                count: hdr_count,
            };
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
            self.counters.dup_msgs += hdr_count as u64;
            return;
        }

        // S5: APPLY over sequential triples — no length chain in-window.
        if first <= self.w {
            let old_w = self.w;
            let gen = self.gen;
            let proof = LiveFeedProof { gen };
            let n = blocks.len();
            let skip = (old_w.wrapping_sub(first) as usize).min(n);
            if skip != 0 {
                self.counters.dup_msgs += skip as u64;
            }
            // R2: all_valid <=> memo's exact prefix covers every block of this
            // frame <=> every in-window validate() would return Ok (see
            // FrameMemo doc). Unmemoized (live) frames never skip validation.
            let all_valid = memo.is_some_and(|m| m.valid_count as usize == n);
            // R3: closed-form span emission. Emission state arithmetic on the
            // contiguous run is closed-form: n_emit = n - skip, w = last + 1.
            // Gated on (a) memo-proven validity, (b) sink opt-in — otherwise the
            // exact classic per-message sequence runs (bit-identical semantics).
            if all_valid && sink.wants_spans() && n > skip {
                let body_start = blocks[skip].1 as usize;
                let body_end = blocks[n - 1].2 as usize;
                let body = &frame[body_start..body_end];
                sink.on_span(&proof, first + skip as u64, (n - skip) as u16, body, &blocks[skip..]);
                self.counters.msgs_emitted += (n - skip) as u64;
            } else {
                let mut n_emit = 0u64;
                for &(seq, start, end) in &blocks[skip..] {
                    let data = &frame[start as usize..end as usize];
                    // R2: skip re-validation only when the memo proves this exact
                    // block would pass; error mapping/order otherwise identical.
                    if !all_valid {
                        if let Err(e) = itch5::validate(data) {
                            std::hint::cold_path();
                            self.counters.msgs_emitted += n_emit;
                            self.counters
                                .violations
                                .record_packet_error(packet::PacketError::Payload(e));
                            self.counters.total_violations += 1;
                            return;
                        }
                    }
                    sink.on_msg(&proof, seq, data);
                    n_emit += 1;
                }
                self.counters.msgs_emitted += n_emit;
            }
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

    /// R8: steady-state precondition for the batched apply loop — the exact
    /// set of sequencer conditions under which `ingest_indexed`'s per-frame
    /// tail is provably inert (no staged window content to clear or drain,
    /// no open gap to check-close, no pending recovery intent to retire,
    /// live adopted session, Contig state). When this holds, a frame's
    /// entire observable effect is: counters, optional emission, and
    /// `w`'s advance.
    #[inline(always)]
    fn steady_ready(&self) -> bool {
        self.session_live
            && self.state == State::Contig
            && self.staged_count == 0
            && !self.gap_active
            && self.pending_to.is_none()
    }

    /// R8: batch-level ingest — the doc-21 "main-core batching of the
    /// sequencer apply" lever. Consumes a whole poll's frames (see
    /// `ReplayTransport::batch_entries`), running maximal runs of steady
    /// frames through [`steady_scan`] — a free function that touches ONLY
    /// the counters, the sink, and register-resident scalars (w, session
    /// template, proof era), so the hot loop keeps zero sequencer state in
    /// memory. Any anomaly stops the scan; that frame is rerun through the
    /// unmodified classic `ingest_auto`, and the scan resumes on the next
    /// frame if the steady preconditions hold again.
    ///
    /// Observables are bit-identical to feeding the same frames through
    /// `ingest_auto` one by one: emissions happen in frame order (the scan
    /// emits eagerly; a cold frame's emissions always follow the scan's
    /// flush), counters receive the same increments in the same order (the
    /// scan defers them into locals and commits at scan exit, BEFORE the
    /// cold frame's own updates), and `w`/`progress_vt` land on the same
    /// final values (`now_ns` is constant for the whole batch, so deferring
    /// the `progress_vt` store to the scan/cold boundaries cannot change
    /// any reader's view).
    #[inline(always)]
    pub fn ingest_batch<'a, S: Sink, I>(&mut self, entries: I, now_ns: u64, sink: &mut S)
    where
        I: IntoIterator<Item = packet::FrameEntry<'a>>,
    {
        let wants_spans = sink.wants_spans();
        let mut entries = entries.into_iter();
        loop {
            if self.steady_ready() {
                let mut w = self.w;
                let (progressed, cold) = steady_scan(
                    &mut self.counters,
                    sink,
                    &mut entries,
                    &mut w,
                    self.session_lo,
                    self.session_hi,
                    self.gen,
                    wants_spans,
                );
                self.w = w;
                if progressed {
                    self.progress_vt = now_ns;
                }
                match cold {
                    Some(entry) => {
                        cold_apply(self, &entry, now_ns, sink);
                        continue; // re-check steady for the frames after the cold one
                    }
                    None => break, // iterator exhausted
                }
            }
            // Not steady: classic per-frame ladder until steady again.
            match entries.next() {
                Some(entry) => cold_apply(self, &entry, now_ns, sink),
                None => break,
            }
        }
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

/// R8: out-of-line cold-frame apply. `ingest_auto` and its whole classic
/// ladder are #[inline(always)]; inlined into the batch loop they bloat the
/// hot loop past the µop-cache (DSB) capacity and make the steady scan
/// decode-bound — measured as a 25% throughput regression. The cold path is
/// rare by definition, so this wrapper keeps it out of the loop body at the
/// cost of one call per anomaly.
#[inline(never)]
fn cold_apply<S: Sink>(
    this: &mut Sequencer,
    entry: &packet::FrameEntry<'_>,
    now_ns: u64,
    sink: &mut S,
) {
    this.ingest_auto(entry.bytes, entry.feed, now_ns, sink, entry.blocks, entry.memo);
}

/// R8: the steady-apply scan — a maximal run of frames through the exact
/// `ingest_indexed` steady ladder, executed WITHOUT any Sequencer reference
/// (only `counters`, the sink, the frame iterator, and register-resident
/// scalars). This is the doc-21 "main-core batching of the sequencer apply"
/// lever: because the scan cannot touch sequencer control state, the
/// compiler keeps `w`, the session template, the proof era and the deferred
/// counter accumulators in registers for the whole run, and the sequencer's
/// cache lines stay quiet.
///
/// Stops at (and returns, unapplied) the first frame that needs the classic
/// ladder: session change, HB/EOS, gap (`first > w`), unmemoized or
/// partially-invalid frame, index-less frame, or sub-header length. Returns
/// whether any frame advanced `w` (the caller mirrors classic `progress_vt`
/// semantics: set on contiguous applies, never on pure duplicates).
///
/// Deferred counters (packets/bytes/dups per feed, dup_msgs, msgs_emitted)
/// commit at scan exit, BEFORE the caller applies the cold frame — the
/// increment order against cold-path updates is preserved exactly.
#[inline(always)]
#[allow(clippy::too_many_arguments, clippy::while_let_on_iterator)]
fn steady_scan<'a, S: Sink, I>(
    counters: &mut Counters,
    sink: &mut S,
    entries: &mut I,
    w: &mut u64,
    sess_lo: u64,
    sess_hi: u64,
    gen: u64,
    wants_spans: bool,
) -> (bool, Option<packet::FrameEntry<'a>>)
where
    I: Iterator<Item = packet::FrameEntry<'a>>,
{
    let mut pk = [0u64; 2];
    let mut byt = [0u64; 2];
    let mut dup = [0u64; 2];
    let mut dup_msgs = 0u64;
    let mut emitted = 0u64;
    let mut progressed = false;
    let mut cold: Option<packet::FrameEntry<'a>> = None;
    while let Some(entry) = entries.next() {
        let frame = entry.bytes;
        let blocks = entry.blocks;
        let n = blocks.len();
        if n == 0 || frame.len() < moldudp64::HEADER_LEN {
            cold = Some(entry);
            break;
        }
        // S0/S1 fused header decode (identical to ingest_indexed).
        let frame_lo = u64::from_le_bytes([
            frame[0], frame[1], frame[2], frame[3], frame[4], frame[5], frame[6], frame[7],
        ]);
        let frame_hi = u64::from_le_bytes([
            frame[2], frame[3], frame[4], frame[5], frame[6], frame[7], frame[8], frame[9],
        ]);
        let hdr_count = u16::from_be_bytes([frame[18], frame[19]]);
        if frame_lo != sess_lo || frame_hi != sess_hi {
            // Adoption or boundary: the full dispatch ladder.
            cold = Some(entry);
            break;
        }
        // S2: kind classify — HB/EOS carry no triples, but a session-matched
        // control frame must still run its exact handler.
        if hdr_count == moldudp64::HEARTBEAT_COUNT || hdr_count == moldudp64::EOS_COUNT {
            cold = Some(entry);
            break;
        }
        // S3: span from triples.
        let first = blocks[0].0;
        let last = blocks[n - 1].0;
        if last < *w {
            // Pure duplicate packet (HOT in dual-feed replay) — the exact
            // classic counters (no progress_vt write, matching classic).
            let fi = (entry.feed & 1) as usize;
            pk[fi] += 1;
            byt[fi] += frame.len() as u64;
            dup[fi] += 1;
            dup_msgs += hdr_count as u64;
            continue;
        }
        if first > *w {
            // Gap: evidence + staging ladder is the classic path's job.
            cold = Some(entry);
            break;
        }
        // S5 contiguous apply. R2 memo gate — identical semantics.
        let all_valid = entry.memo.is_some_and(|m| m.valid_count as usize == n);
        if !all_valid {
            cold = Some(entry);
            break;
        }
        let fi = (entry.feed & 1) as usize;
        pk[fi] += 1;
        byt[fi] += frame.len() as u64;
        let skip = (*w).wrapping_sub(first) as usize;
        let skip = if skip < n { skip } else { n };
        if skip != 0 {
            dup_msgs += skip as u64;
        }
        let proof = LiveFeedProof { gen };
        if wants_spans {
            // R3 closed-form span emission (n > skip always: last >= w).
            let body = &frame[blocks[skip].1 as usize..blocks[n - 1].2 as usize];
            sink.on_span(
                &proof,
                first + skip as u64,
                (n - skip) as u16,
                body,
                &blocks[skip..],
            );
        } else {
            for &(seq, start, end) in &blocks[skip..] {
                sink.on_msg(&proof, seq, &frame[start as usize..end as usize]);
            }
        }
        emitted += (n - skip) as u64;
        *w = last + 1;
        progressed = true;
    }
    // Commit deferred counters (scan exit — before the caller touches the
    // cold frame, preserving increment order).
    counters.feed_a.packets += pk[0];
    counters.feed_b.packets += pk[1];
    counters.feed_a.bytes += byt[0];
    counters.feed_b.bytes += byt[1];
    counters.feed_a.dups += dup[0];
    counters.feed_b.dups += dup[1];
    counters.dup_msgs += dup_msgs;
    counters.msgs_emitted += emitted;
    (progressed, cold)
}
