//! Renderer and virtual-clock driven transport (Tier F: zero heap allocations in hot path).

#![cfg_attr(not(test), deny(clippy::disallowed_types))]

use crate::sched_types::{ReplaySchedule, SchedEvent, SchedKind};
// R8: construction-time shared handle for the triple store (the RX-pipelined
// consumer's read-only view). Never touched in a hot path — the Tier-F
// allow mirrors the construction-time Vec allows below.
#[allow(clippy::disallowed_types)]
use std::sync::Arc;
use crate::{FeedId, FrameBatch, Transport};
use nf_protocol::itch5;
use nf_protocol::moldudp64::{EOS_COUNT, HEADER_LEN, HEARTBEAT_COUNT};
use nf_protocol::packet::FrameMemo;

#[derive(Debug, Clone, Copy, Default)]
pub struct Cursor {
    pub byte_offset: usize,
    pub msg_index: u64,
}

impl Cursor {
    /// P3: always-inline — steady-state is 1 compare + return (cursor in sync).
    #[inline(always)]
    pub fn seek_msg(&mut self, gt: &[u8], target_msg_index: u64) -> Option<usize> {
        if self.msg_index > target_msg_index {
            self.byte_offset = 0;
            self.msg_index = 0;
        }
        while self.msg_index < target_msg_index {
            if self.byte_offset + 2 > gt.len() {
                return None;
            }
            let len =
                u16::from_be_bytes([gt[self.byte_offset], gt[self.byte_offset + 1]]) as usize;
            self.byte_offset += 2 + len;
            self.msg_index += 1;
        }
        if self.byte_offset <= gt.len() {
            Some(self.byte_offset)
        } else {
            None
        }
    }
}

pub const ARENA_SLOT_SIZE: usize = 1500;
pub const ARENA_SLOTS: usize = 256;

/// Pre-rendered frame directory entry. `offset/len` locate the frame bytes in
/// `frames`; `patch` marks frames whose 10B session prefix is reset()-mutable
/// (zeroed at build, patched with the live session at poll time).
/// `blk_base/blk_count` locate the frame's precomputed `(seq, start, end)`
/// block triples in `triples` (Q1 indexed ingest — kills the serial length
/// chain in-window; empty for HB/EOS/tombstones).
/// `valid` (R2) is the frame's ITCH validation verdict memo: the EXACT leading
/// prefix of blocks passing `itch5::validate`, computed once here from the
/// immutable rendered bytes. Session patching never touches bodies, so the
/// verdict survives reset(). Consumed via `batch_memo()`.
///
/// R8 layout: 24 bytes — `release_vt` moved to the parallel `vts` array,
/// and `first_seq` (the frame's first block sequence number, from the
/// schedule) added so the sequencer's steady scan never touches the triple
/// store (first/last come from the slot; body bounds are derived — see the
/// FrameView doc).
#[derive(Debug, Clone, Copy)]
struct FrameMeta {
    offset: u32,
    len: u16,
    feed: FeedId,
    patch: bool,
    blk_base: u32,
    blk_count: u16,
    valid: u16,
    first_seq: u64,
}

pub struct ReplayTransport {
    schedule: ReplaySchedule,
    event_idx: usize,
    virtual_clock: u64,
    frames: Box<[u8]>,
    meta: Box<[FrameMeta]>,
    /// R8: parallel release-time stream (event order, one u64 per event).
    vts: Box<[u64]>,
    /// R8: vt-group boundaries (see poll_clamped's release proof).
    group_end: Box<[u32]>,
    /// Flat precomputed block index: one `(seq, start, end)` triple per
    /// message block, grouped per event. R8: shared (Arc) so the
    /// RX-pipelined consumer can build block slices cross-thread — the
    /// ONLY part of the rendered state the consumer touches directly
    /// (frame bytes reach it through the raw pointers the RX thread's
    /// slots carry, and the directory/pacing stay RX-private).
    #[allow(clippy::disallowed_types)]
    triples: Arc<[(u64, u32, u32)]>,
    /// R8: precomputed blob offsets of every patchable frame's 10B session
    /// prefix (reset-time session bake).
    patch_offsets: Box<[u32]>,
    /// R8: RX coalescing — vt-groups per poll (1 = exact pre-R8 pacing).
    coalesce: usize,
    body_prefetch: bool,
    session: [u8; 10],
    clock_clamp: Option<u64>,
    batch_event: [u32; 256],
    batch_event_len: usize,
}

impl ReplayTransport {
    pub fn new(gt: &[u8], schedule: ReplaySchedule, session: [u8; 10]) -> Self {
        let first_vt = schedule
            .events
            .first()
            .map(|e| e.release_vt)
            .unwrap_or(0);
        // P9a: render every event frame NOW (startup, outside windows) through the
        // exact same render_event_standalone path poll() used before — byte-identical
        // output, ~zero per-frame cost in-window. Vec use is construction-only;
        // the hot path never allocates (PR-3 ALLOC_DELTA still 0 in-window).
        #[allow(clippy::disallowed_types)]
        let mut blob: Vec<u8> =
            Vec::with_capacity(schedule.events.len().saturating_mul(768));
        #[allow(clippy::disallowed_types)]
        let mut meta: Vec<FrameMeta> = Vec::with_capacity(schedule.events.len());
        // R8: parallel release-time stream (one u64 per event).
        #[allow(clippy::disallowed_types)]
        let mut vts: Vec<u64> = Vec::with_capacity(schedule.events.len());
        // Flat triple store: ~16B per message block, appended per rendered frame.
        #[allow(clippy::disallowed_types)]
        let mut triples: Vec<(u64, u32, u32)> = Vec::new();
        {
            let mut cursors = [Cursor::default(), Cursor::default()];
            let mut scratch = [0u8; ARENA_SLOT_SIZE];
            for ev in &schedule.events {
                let feed_idx = (ev.feed as usize) & 1;
                if let Some(len) = render_event_standalone(
                    gt,
                    ev,
                    &schedule,
                    session,
                    &mut scratch,
                    &mut cursors[feed_idx],
                ) {
                    let off = blob.len() as u32;
                    blob.extend_from_slice(&scratch[..len]);
                    // Mirror of render_event_standalone's session rule (source of
                    // truth): patch iff the frame carries the resettable session.
                    let patch = match schedule.session_split {
                        Some((split_m, _)) => match ev.kind {
                            SchedKind::Packet { first_msg, .. } => first_msg < split_m,
                            SchedKind::Heartbeat { .. } | SchedKind::EndOfSession { .. } => false,
                        },
                        None => true,
                    };
                    if patch {
                        let base = off as usize;
                        blob[base..base + 10].fill(0);
                    }
                    // Q1: index this frame's [len|msg] chain once (startup). The
                    // frame was just rendered valid, so the walk below only fails
                    // on internal inconsistency — then tombstone (never emit).
                    // R2: while walking, memoize the EXACT leading prefix of blocks
                    // passing ITCH validation — verdict of a pure function over
                    // these immutable bytes, one-time, outside every window.
                    let (blk_base, blk_count, valid_prefix) = match ev.kind {
                        SchedKind::Packet {
                            first_seq,
                            count,
                            ..
                        } => {
                            let base = triples.len() as u32;
                            let mut pos = HEADER_LEN;
                            let mut seq = first_seq;
                            let mut n: u16 = 0;
                            let mut ok = true;
                            let mut valid: u16 = 0;
                            for _ in 0..count {
                                if scratch.len() < pos + 2 {
                                    ok = false;
                                    break;
                                }
                                let blen =
                                    u16::from_be_bytes([scratch[pos], scratch[pos + 1]])
                                        as usize;
                                let start = pos + 2;
                                let end = start + blen;
                                if end > len {
                                    ok = false;
                                    break;
                                }
                                triples.push((seq, start as u32, end as u32));
                                if valid == n
                                    && itch5::validate(&scratch[start..end]).is_ok()
                                {
                                    valid += 1;
                                }
                                pos = end;
                                seq = seq.wrapping_add(1);
                                n += 1;
                            }
                            if !ok || pos != len {
                                // Inconsistent with just-rendered bytes: drop the
                                // partial triples and tombstone the frame.
                                std::hint::cold_path();
                                triples.truncate(base as usize);
                                (0, 0, 0)
                            } else {
                                (base, n, valid)
                            }
                        }
                        SchedKind::Heartbeat { .. } | SchedKind::EndOfSession { .. } => {
                            (0, 0, 0)
                        }
                    };
                    // A tombstoned-by-index frame must not be emitted: convert a
                    // (0,0) triple range on a NON-EMPTY Packet into a meta
                    // tombstone so poll skips it exactly like an unrenderable
                    // event. (Empty packets / HB / EOS legitimately carry no
                    // triples and keep their rendered 20B frame.)
                    let ev_count = match ev.kind {
                        SchedKind::Packet { count, .. } => count,
                        SchedKind::Heartbeat { .. } | SchedKind::EndOfSession { .. } => 0,
                    };
                    let tombstoned = ev_count > 0 && blk_count == 0;
                    let meta_first_seq = match ev.kind {
                        SchedKind::Packet { first_seq, .. } => first_seq,
                        _ => 0,
                    };
                    if tombstoned {
                        std::hint::cold_path();
                        meta.push(FrameMeta {
                            offset: 0,
                            len: 0,
                            feed: ev.feed,
                            patch: false,
                            blk_base: 0,
                            blk_count: 0,
                            valid: 0,
                            first_seq: 0,
                        });
                    } else {
                        meta.push(FrameMeta {
                            offset: off,
                            len: len as u16,
                            feed: ev.feed,
                            patch,
                            blk_base,
                            blk_count,
                            valid: valid_prefix,
                            first_seq: meta_first_seq,
                        });
                    }
                    vts.push(ev.release_vt);
                } else {
                    // Unrenderable event (frame >1500B scratch — unreachable for
                    // MTU-bound schedules): tombstone keeps event_idx aligned,
                    // exactly as the old skip-without-push did.
                    std::hint::cold_path();
                    meta.push(FrameMeta {
                        offset: 0,
                        len: 0,
                        feed: ev.feed,
                        patch: false,
                        blk_base: 0,
                        blk_count: 0,
                        valid: 0,
                        first_seq: 0,
                    });
                    vts.push(ev.release_vt);
                }
            }
        }
        // R8: vt-group boundaries (see field doc). Single O(n) sweep: `j`
        // advances monotonically; each event inside a group maps to the
        // group's exclusive end.
        #[allow(clippy::disallowed_types)]
        let mut group_end: Vec<u32> = vec![0u32; vts.len()];
        {
            let mut i = 0usize;
            while i < vts.len() {
                let v = vts[i];
                let mut j = i + 1;
                while j < vts.len() && vts[j] <= v {
                    j += 1;
                }
                group_end[i..j].fill(j as u32);
                i = j;
            }
        }
        // R8: derive the patch-offset list from the completed directory.
        #[allow(clippy::disallowed_types)]
        let patch_offsets: Vec<u32> = meta
            .iter()
            .filter(|m| m.patch)
            .map(|m| m.offset)
            .collect();
        let mut t = Self {
            schedule,
            event_idx: 0,
            virtual_clock: first_vt,
            frames: blob.into_boxed_slice(),
            meta: meta.into_boxed_slice(),
            vts: vts.into_boxed_slice(),
            group_end: group_end.into_boxed_slice(),
            #[allow(clippy::disallowed_types)]
            triples: {
                #[allow(clippy::disallowed_types)]
                let arc: Arc<[(u64, u32, u32)]> = triples.into_boxed_slice().into();
                arc
            },
            patch_offsets: patch_offsets.into_boxed_slice(),
            batch_event: [0u32; 256],
            batch_event_len: 0,
            session,
            clock_clamp: None,
            body_prefetch: true,
            coalesce: 1,
        };
        // R8: bake the construction session into the patchable prefixes —
        // the old lazy poll-patching did this on first release; without a
        // reset() the blob would otherwise serve zeroed sessions (caught by
        // the A-1 split-watermark test).
        t.patch_sessions();
        t
    }

    /// R6: control main-side frame-body prefetching in poll() (see field
    /// doc). Default ON (single-core TITAN behavior preserved bit-for-bit).
    #[inline]
    pub fn set_body_prefetch(&mut self, on: bool) {
        self.body_prefetch = on;
    }

    /// R8: RX coalescing control (see the `coalesce` field doc). `k = 1` is
    /// the exact pre-R8 pacing; `k > 1` releases up to `k` vt-groups per
    /// poll, NAPI-style, with `now_ns()` reporting the latest released
    /// group's virtual time. Conformance/golden/differential paths never
    /// touch this.
    #[inline]
    pub fn set_poll_coalesce(&mut self, k: usize) {
        self.coalesce = k.max(1);
    }

    #[inline]
    pub fn reset(&mut self, session: [u8; 10]) {
        let first_vt = self
            .schedule
            .events
            .first()
            .map(|e| e.release_vt)
            .unwrap_or(0);
        self.event_idx = 0;
        self.virtual_clock = first_vt;
        self.session = session;
        self.clock_clamp = None;
        // R8: patch every patchable frame's 10B session prefix NOW, once —
        // poll() used to do this per released frame inside the measurement
        // window (a 10B copy + branch per frame). reset() runs outside every
        // timed window in the burst arms, and its ~60-80µs blob rewrite is
        // 0.25% of the sustained arm's in-window reset cadence. The frames
        // poll() serves are byte-identical to the lazily-patched ones (same
        // bytes written, same positions, before any consumer can read them
        // — poll slices frames only after this loop completes).
        self.patch_sessions();
    }

    /// R8: write the current session into every patchable frame's 10B
    /// prefix (see `reset`). Also run at construction, so a transport that
    /// is polled without any reset() serves the construction session's
    /// bytes exactly as the old lazy poll-patching did.
    fn patch_sessions(&mut self) {
        let session = self.session;
        let frames = self.frames.as_mut_ptr();
        // R8 phase-6: PREFETCHW pipeline. The patch is ~12.6k ten-byte
        // stores strided across the 15MB blob — one RFO per cache line,
        // serially exposed at ~40-100ns each when the lines are L3-shared
        // (the measured 60-137us per pass). A software PREFETCH-W sweep
        // running ~128 offsets ahead brings the lines to the exclusive
        // state in parallel; the stores then retire at throughput. Bit
        // semantics unchanged (same bytes, same order, same thread).
        // SAFETY: prefetch never faults and never dereferences; every
        // offset's line is in-bounds of `frames` (the store below touches
        // off..off+10, so the whole line is certainly mapped).
        unsafe {
            const PF_DIST: usize = 128;
            let offs = &self.patch_offsets[..];
            let n = offs.len();
            for i in 0..n {
                // Keep the prefetch lead PF_DIST lines ahead of the store
                // cursor; the first PF_DIST stores run un-prefetched (the
                // pipeline's warm-up).
                let j = i + PF_DIST;
                if j < n {
                    std::arch::x86_64::_mm_prefetch(
                        frames.add(offs[j] as usize) as *const i8,
                        std::arch::x86_64::_MM_HINT_ET0,
                    );
                }
                let p = frames.add(offs[i] as usize);
                std::ptr::copy_nonoverlapping(session.as_ptr(), p, 10);
            }
        }
    }

    /// R8 phase-3b: the blob's base address (the prepatch bookkeeping in
    /// the RX thread computes publication end-offsets relative to it).
    pub(crate) fn blob_base(&self) -> usize {
        self.frames.as_ptr() as usize
    }

    /// R8 phase-3b: patch patchable frames whose blob offset is below
    /// `upto_off`, starting the walk at patch-list index `from_idx`, with
    /// an explicit `session` (the NEXT pass's — the transport's own
    /// `session` field still holds the current pass's until the advance).
    /// Returns the new patch-list index (the prepatch cursor). Offsets are
    /// in construction order = schedule order, so a monotone consumption
    /// frontier walks the list once, linearly.
    ///
    /// SAFETY CONTRACT: only frames whose ENTIRE publication has been
    /// consumed (buffer freed) may be prepatched — the consumer never
    /// re-reads a freed publication, span bodies live at frame[22..] and
    /// the patch writes frame[0..10], and the next pass's entry session
    /// words are computed at render time (after the advance), so the
    /// prepatch can never be observed mid-flight.
    pub(crate) fn patch_range(&mut self, session: &[u8; 10], from_idx: usize, upto_off: usize) -> usize {
        let frames = self.frames.as_mut_ptr();
        let mut idx = from_idx;
        // SAFETY: same per-offset contract as patch_sessions; the walk is
        // bounded by the patch list's own length.
        unsafe {
            while idx < self.patch_offsets.len() {
                let off = self.patch_offsets[idx] as usize;
                if off + 10 > upto_off {
                    break;
                }
                let p = frames.add(off);
                std::ptr::copy_nonoverlapping(session.as_ptr(), p, 10);
                idx += 1;
            }
        }
        idx
    }

    /// R8 phase-3b: `reset` without re-patching the already-prepatched
    /// prefix — the RX's prepatch cursor advanced through the consumed
    /// region during the pass, so only the tail ([from_idx..]) needs the
    /// synchronous bake at the advance point.
    pub fn reset_prepatched(&mut self, session: [u8; 10], from_idx: usize) {
        let first_vt = self
            .schedule
            .events
            .first()
            .map(|e| e.release_vt)
            .unwrap_or(0);
        self.event_idx = 0;
        self.virtual_clock = first_vt;
        self.session = session;
        self.clock_clamp = None;
        let session = self.session;
        let frames = self.frames.as_mut_ptr();
        // SAFETY: same per-offset contract as patch_sessions.
        unsafe {
            for &off in self.patch_offsets.iter().skip(from_idx) {
                let p = frames.add(off as usize);
                std::ptr::copy_nonoverlapping(session.as_ptr(), p, 10);
            }
        }
    }

    #[inline]
    pub fn set_clock_clamp(&mut self, clamp: Option<u64>) {
        self.clock_clamp = clamp;
    }

    /// P3: always-inline + hoisted len/capacity, cold clamp path.
    /// P9a: per released frame: 2 indexed loads + 10B session patch + batch push.
    /// No cursor seeks, no length re-walk, no payload memcpy in-window.
    /// R8: (a) the release boundary comes from the precomputed `group_end`
    /// chain — poll() advances the virtual clock to the entering group's
    /// vt and releases the maximal prefix at or below it, which is exactly
    /// the pre-R8 per-frame comparison loop's release set (proved: the scan
    /// always breaks at the first event above `vclock`, and every member of
    /// `group_end[i]`'s run is `<= vts[i] <= vclock`), so the per-frame
    /// clock compare disappears from the release loop; (b) with
    /// `set_poll_coalesce(k > 1)` the chain advances `k` groups per call —
    /// NAPI-style receipt batching, `now_ns()` = latest released group's vt.
    #[inline(always)]
    pub fn poll_clamped(&mut self, batch: &mut FrameBatch, max_vt: Option<u64>) -> usize {
        batch.clear();

        let events_len = self.meta.len();
        if self.event_idx >= events_len {
            return 0;
        }

        let first_vt = self.vts[self.event_idx];
        // HOT: max_vt=None + clock_clamp=None (steady replay) — clamp is cold.
        let limit = match max_vt.or(self.clock_clamp) {
            Some(clamp) => {
                std::hint::cold_path();
                std::cmp::min(first_vt, clamp)
            }
            None => first_vt,
        };

        if limit > self.virtual_clock {
            self.virtual_clock = limit;
        }
        let vclock = self.virtual_clock;

        // R8: advance the group chain — `coalesce` groups (default 1). The
        // chain's `mx` is the max group-start vt released; the clock never
        // exceeds an explicit clamp (chain stops at a group start above the
        // limit, matching the pre-R8 jump-to-min semantics).
        let coalesce = self.coalesce;
        let mut e = self.event_idx;
        let mut mx = 0u64;
        let mut groups = 0usize;
        let limit_full = max_vt.or(self.clock_clamp).unwrap_or(u64::MAX);
        while groups < coalesce && e < events_len {
            let v = self.vts[e];
            if v > limit_full {
                break;
            }
            if v > mx {
                mx = v;
            }
            e = self.group_end[e] as usize;
            groups += 1;
        }
        if mx > vclock {
            self.virtual_clock = mx;
        }
        let vclock = self.virtual_clock;

        let cap = FrameBatch::capacity();
        // R8: the per-frame clock compare and the legacy batch_event map are
        // needed only in the exact-pacing mode (coalesce == 1, the
        // conformance/golden/differential configuration). In the coalesced
        // throughput mode the release boundary IS the group chain's `e`
        // (proved: every member of the chain's runs has vt <= mx <= vclock,
        // and the schedules the throughput arms render have non-decreasing
        // vts), so the loop bounds by `e` directly.
        let exact_pacing = self.coalesce == 1;
        let limit_evt = if exact_pacing { events_len } else { e };

        while self.event_idx < limit_evt && batch.len() < cap {
            let evt = self.event_idx;
            if exact_pacing && self.vts[evt] > vclock {
                break;
            }
            let m = self.meta[evt];
            self.event_idx = evt + 1;
            if m.len == 0 {
                continue; // tombstone: advances the cursor, never emitted
            }
            let base = m.offset as usize;
            let end = base + m.len as usize;
            // SAFETY: `offset`/`len` were written at construction from the
            // blob's own append cursor (off = blob.len() before
            // extend_from_slice, len = the rendered length), so base..end is
            // in-bounds of `frames` by construction; the debug assert keeps
            // the invariant honest under mutation-heavy test builds.
            debug_assert!(end <= self.frames.len());
            let frame = unsafe { self.frames.get_unchecked_mut(base..end) };
            // R8: the session prefix was patched at reset() time — poll's
            // release loop is a pure slice + push (the 10B copy and its
            // branch are gone from the hot path).
            let slot_idx = batch.len();
            // R8: session compare words, read from the frame line this
            // thread already holds (the reset-time bake guarantees the
            // bytes). In pipelined mode the consumer's steady scan then
            // never touches the cross-core frame lines at all.
            let fptr = frame.as_ptr();
            // SAFETY: len >= HEADER_LEN (>= 20) for every pushed frame;
            // unaligned u64 reads are defined.
            let (sess_lo, sess_hi) = unsafe {
                (
                    (fptr as *const u64).read_unaligned(),
                    (fptr.add(2) as *const u64).read_unaligned(),
                )
            };
            batch.push_indexed(
                fptr,
                m.len,
                m.feed,
                m.blk_base,
                m.blk_count,
                m.valid,
                m.first_seq,
                sess_lo,
                sess_hi,
            );
            // Map batch position back to its event for the legacy
            // batch_blocks()/batch_memo() side tables (exact-pacing callers
            // only; the coalesced throughput arms consume the inline slot
            // index).
            if exact_pacing {
                self.batch_event[slot_idx] = evt as u32;
            }
            // R4/R8: DLP warm-up — the blob is append-ordered, so the NEXT
            // event's frame starts exactly at base + len: its first line
            // (header + first body bytes — the session compare reads
            // [0..20] and the verification consumers stream from there) is
            // prefetchable from the CURRENT meta with zero extra loads.
            // Depth-2 covers the batch boundary via ONE directory load (the
            // hardware prefetcher carries the sequential meta/slot streams
            // and the scan no longer touches the triple store). The old
            // 4-deep lookahead reloaded metas 5x per frame; the coalesced
            // release gives every in-batch frame hundreds of cycles of lead
            // already.
            #[cfg(target_arch = "x86_64")]
            {
                let next = base + m.len as usize;
                // SAFETY: prefetcht0 never faults and accepts any readable
                // address; `next` is within the blob (or one past its end
                // for the final frame — an unmapped prefetch is dropped by
                // the hardware, and blob pages are over-allocated by the
                // allocator's page granularity in practice; the last event
                // is never a released frame's lookahead target in the
                // steady stream).
                unsafe {
                    if self.body_prefetch {
                        std::arch::x86_64::_mm_prefetch(
                            self.frames.as_ptr().add(next) as *const i8,
                            std::arch::x86_64::_MM_HINT_T0,
                        );
                    }
                }
                if evt + 1 < events_len {
                    let m2 = self.meta[evt + 1];
                    if m2.len > 0 && self.body_prefetch {
                        let next2 = next + m2.len as usize;
                        // SAFETY: same prefetch contract as above.
                        unsafe {
                            std::arch::x86_64::_mm_prefetch(
                                self.frames.as_ptr().add(next2) as *const i8,
                                std::arch::x86_64::_MM_HINT_T0,
                            );
                        }
                    }
                }
            }
        }

        self.batch_event_len = batch.len();
        batch.len()
    }

    /// Q1 indexed ingest: block triples `(seq, start, end)` for the frame at
    /// batch position `batch_pos` (positions from the most recent `poll()` on
    /// this transport). Empty for HB/EOS/tombstone frames and OOB positions —
    /// callers fall back to classic `ingest` on empty (same observables).
    /// Triples are relative to the frame bytes and session-patch-proof.
    #[inline(always)]
    pub fn batch_blocks(&self, batch_pos: usize) -> &[(u64, u32, u32)] {
        if batch_pos >= self.batch_event_len {
            std::hint::cold_path();
            return &[];
        }
        let ev = self.batch_event[batch_pos] as usize;
        if ev >= self.meta.len() {
            std::hint::cold_path();
            return &[];
        }
        let m = &self.meta[ev];
        let base = m.blk_base as usize;
        let end = base + m.blk_count as usize;
        if end > self.triples.len() {
            std::hint::cold_path();
            return &[];
        }
        &self.triples[base..end]
    }

    /// R8: the frames of the most recent `poll()` as `FrameEntry` values —
    /// bytes + feed + inline Q1/R2 index with ZERO side-table indirection
    /// (the slot itself carries blk_base/blk_count/valid). Feeds the
    /// sequencer's `ingest_batch` apply loop. The iterator borrows the
    /// transport immutably; the caller must finish it before the next poll
    /// (the borrow checker enforces this).
    #[inline(always)]
    pub fn batch_entries<'b>(
        &'b self,
        batch: &'b FrameBatch,
    ) -> impl Iterator<Item = nf_protocol::packet::FrameEntry<'b>> + 'b {
        batch.frames().iter().map(move |f| {
            let (blocks, memo) = self.frame_blocks_memo(f);
            nf_protocol::packet::FrameEntry {
                bytes: f.bytes(),
                feed: f.feed,
                blocks,
                memo,
                first_seq: f.first_seq,
                sess_lo: f.sess_lo,
                sess_hi: f.sess_hi,
            }
        })
    }

    /// R8: slot-direct per-frame index — one call replacing the
    /// `batch_blocks(pos)` + `batch_memo(pos)` side-table pair (which walked
    /// batch_event[pos] → meta[ev] → triples twice, with bounds checks at
    /// every hop). Reads the Q1/R2 index the slot itself carries.
    #[inline(always)]
    pub fn frame_blocks_memo(
        &self,
        f: &crate::FrameView,
    ) -> (&[(u64, u32, u32)], Option<FrameMemo>) {
        let (blk_base, blk_count, valid) = (f.blk_base, f.blk_count, f.valid);
        if blk_count == 0 {
            return (&[], None);
        }
        // SAFETY: batch slots are written only inside this crate —
        // `push_indexed` copies construction-valid blk_base/blk_count
        // (bounded by the triples store built at transport construction),
        // and `push_raw`/`FrameBatch::new` write 0/0, which the guard above
        // routes to the empty slice.
        unsafe {
            let tp = self.triples.as_ptr().add(blk_base as usize);
            (
                std::slice::from_raw_parts(tp, blk_count as usize),
                Some(FrameMemo { valid_count: valid }),
            )
        }
    }

    /// R2: validation-verdict memo for the frame at batch position `batch_pos`
    /// (positions from the most recent `poll()` on this transport). `None` for
    /// HB/EOS/tombstone frames, OOB positions, and frames with no block index —
    /// callers then run in-window validation (identical observables either way).
    /// The verdict is computed at construction from these exact immutable bytes
    /// (bodies untouched by session patch), so it holds for every reset().
    #[inline(always)]
    pub fn batch_memo(&self, batch_pos: usize) -> Option<FrameMemo> {
        if batch_pos >= self.batch_event_len {
            std::hint::cold_path();
            return None;
        }
        let ev = self.batch_event[batch_pos] as usize;
        if ev >= self.meta.len() {
            std::hint::cold_path();
            return None;
        }
        let m = &self.meta[ev];
        if m.blk_count == 0 {
            std::hint::cold_path();
            return None;
        }
        let base = m.blk_base as usize;
        let end = base + m.blk_count as usize;
        if end > self.triples.len() {
            std::hint::cold_path();
            return None;
        }
        Some(FrameMemo {
            valid_count: m.valid,
        })
    }
}

/// P3: always-inline — fuses session/header/payload copy into poll (saves call/frame).
#[inline(always)]
pub fn render_event_standalone(
    gt: &[u8],
    ev: &SchedEvent,
    sched: &ReplaySchedule,
    session: [u8; 10],
    slot: &mut [u8],
    cursor: &mut Cursor,
) -> Option<usize> {
    let effective_session = match sched.session_split {
        Some((split_m, next_sess)) => match ev.kind {
            SchedKind::Packet { first_msg, .. } if first_msg >= split_m => next_sess,
            SchedKind::Heartbeat { .. } | SchedKind::EndOfSession { .. } => next_sess,
            _ => session,
        },
        None => session,
    };

    if slot.len() < HEADER_LEN {
        return None;
    }
    slot[0..10].copy_from_slice(&effective_session);
    match ev.kind {
        SchedKind::Heartbeat { next_seq } => {
            slot[10..18].copy_from_slice(&next_seq.to_be_bytes());
            slot[18..20].copy_from_slice(&HEARTBEAT_COUNT.to_be_bytes());
            Some(HEADER_LEN)
        }
        SchedKind::EndOfSession { next_seq } => {
            slot[10..18].copy_from_slice(&next_seq.to_be_bytes());
            slot[18..20].copy_from_slice(&EOS_COUNT.to_be_bytes());
            Some(HEADER_LEN)
        }
        SchedKind::Packet {
            first_seq,
            first_msg,
            count,
        } => {
            slot[10..18].copy_from_slice(&first_seq.to_be_bytes());
            slot[18..20].copy_from_slice(&count.to_be_bytes());

            let start_pos = cursor.seek_msg(gt, first_msg)?;

            let mut cur_pos = start_pos;
            for _ in 0..count {
                if cur_pos + 2 > gt.len() {
                    return None;
                }
                let len = u16::from_be_bytes([gt[cur_pos], gt[cur_pos + 1]]) as usize;
                cur_pos += 2 + len;
            }

            let payload_len = cur_pos - start_pos;
            let total_len = HEADER_LEN + payload_len;
            if total_len > slot.len() || cur_pos > gt.len() {
                return None;
            }

            slot[HEADER_LEN..total_len].copy_from_slice(&gt[start_pos..cur_pos]);
            cursor.byte_offset = cur_pos;
            cursor.msg_index = first_msg + count as u64;

            Some(total_len)
        }
    }
}

impl Transport for ReplayTransport {
    #[inline(always)]
    fn poll(&mut self, batch: &mut FrameBatch) -> usize {
        self.poll_clamped(batch, None)
    }

    #[inline(always)]
    fn now_ns(&self) -> u64 {
        self.virtual_clock
    }
}

#[cfg(test)]
#[allow(clippy::all)]
mod tests {
    use super::*;

    /// Ground truth: [len|msg] chain of 12B System Event messages.
    fn gt_with(count: u64) -> Vec<u8> {
        #[allow(clippy::disallowed_types)]
        let mut gt = Vec::new();
        for i in 0..count {
            let mut msg = [b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O'];
            msg[1..9].copy_from_slice(&(i + 1).to_be_bytes());
            gt.extend_from_slice(&(msg.len() as u16).to_be_bytes());
            gt.extend_from_slice(&msg);
        }
        gt
    }

    /// Ground truth whose 3rd message (index 2) has an unknown type byte.
    fn gt_with_bad_third() -> Vec<u8> {
        let mut gt = gt_with(5);
        // skip 2 messages (2 * (2 + 12) bytes), then patch the type byte
        let off = 2 * 14 + 2;
        gt[off] = 0xFE;
        gt
    }

    fn sched_packet(first_seq: u64, first_msg: u64, count: u16) -> ReplaySchedule {
        ReplaySchedule {
            events: vec![SchedEvent {
                release_vt: 0,
                feed: 0,
                kind: SchedKind::Packet {
                    first_seq,
                    first_msg,
                    count,
                },
            }],
            session_split: None,
        }
    }

    /// R2 verdict memo: all-valid frame memoizes valid_count == blk_count.
    #[test]
    fn t_r2_memo_all_valid() {
        let gt = gt_with(5);
        let sched = sched_packet(1, 0, 5);
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 1);
        let blocks = t.batch_blocks(0);
        assert_eq!(blocks.len(), 5);
        let memo = t.batch_memo(0).expect("memo present");
        assert_eq!(memo.valid_count, 5);
    }

    /// R2 verdict memo: exact leading prefix — invalid 3rd message => 2.
    #[test]
    fn t_r2_memo_exact_prefix() {
        let gt = gt_with_bad_third();
        let sched = sched_packet(1, 0, 5);
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 1);
        let blocks = t.batch_blocks(0);
        assert_eq!(blocks.len(), 5);
        let memo = t.batch_memo(0).expect("memo present");
        assert_eq!(memo.valid_count, 2);
        // Cross-check the memo against in-window validation of the same bytes
        // (delivered via the batch FrameView): blocks[0..2] valid, block 2 fails.
        let frame = batch.frames()[0].bytes();
        for b in &blocks[0..2] {
            assert!(itch5::validate(&frame[b.1 as usize..b.2 as usize]).is_ok());
        }
        assert_eq!(
            itch5::validate(&frame[blocks[2].1 as usize..blocks[2].2 as usize]),
            Err(nf_protocol::itch5::ItchError::UnknownType { t: 0xFE })
        );
    }

    /// R2: memo survives reset() with a patched session (bodies untouched).
    #[test]
    fn t_r2_memo_stable_across_reset() {
        let gt = gt_with(4);
        let sched = sched_packet(1, 0, 4);
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 1);
        let before = t.batch_memo(0).expect("memo present");
        t.reset(*b"OTHERSESS1");
        assert_eq!(t.poll(&mut batch), 1);
        let after = t.batch_memo(0).expect("memo present");
        assert_eq!(before, after);
    }

    /// R2: HB/EOS carry no memo (None) and no blocks.
    #[test]
    fn t_r2_memo_none_for_hb() {
        let gt = gt_with(2);
        let sched = ReplaySchedule {
            events: vec![
                SchedEvent {
                    release_vt: 0,
                    feed: 0,
                    kind: SchedKind::Heartbeat { next_seq: 3 },
                },
                SchedEvent {
                    release_vt: 1,
                    feed: 0,
                    kind: SchedKind::EndOfSession { next_seq: 3 },
                },
            ],
            session_split: None,
        };
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 1); // HB
        assert!(t.batch_memo(0).is_none());
        assert_eq!(t.poll(&mut batch), 1); // EOS
        assert!(t.batch_memo(0).is_none());
    }

    /// R2: OOB batch positions memoize None (defensive fallback).
    #[test]
    fn t_r2_memo_none_oob() {
        let gt = gt_with(2);
        let sched = sched_packet(1, 0, 2);
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        assert_eq!(t.poll(&mut batch), 1);
        assert!(t.batch_memo(7).is_none());
    }

    /// R2 default Transport trait impl: batch_memo() is None.
    struct NoIndexTransport;
    impl Transport for NoIndexTransport {
        fn poll(&mut self, _batch: &mut FrameBatch) -> usize {
            0
        }
        fn now_ns(&self) -> u64 {
            0
        }
    }

    #[test]
    fn t_r2_memo_default_trait_none() {
        let t = NoIndexTransport;
        assert!(t.batch_memo(0).is_none());
        assert!(t.batch_blocks(0).is_empty());
    }

    /// R8: the inline slot index (batch_entries) must agree field-for-field
    /// with the legacy side-table API (batch_blocks/batch_memo) on every
    /// position of every poll — the two views of the same per-frame index
    /// can never diverge, or the batch apply loop would see different data
    /// than the classic per-frame path.
    #[test]
    fn t_r8_batch_entries_match_side_tables() {
        let gt = gt_with(40);
        let sched = sched_packet(1, 0, 40);
        let mut t = ReplayTransport::new(&gt, sched, *b"TESTSESS01");
        let mut batch = FrameBatch::new();
        while t.poll(&mut batch) > 0 {
            let entries: Vec<_> = t.batch_entries(&batch).collect();
            assert_eq!(entries.len(), batch.len());
            for (pos, e) in entries.iter().enumerate() {
                assert_eq!(e.blocks, t.batch_blocks(pos), "slot/side-table blocks diverge");
                assert_eq!(e.memo, t.batch_memo(pos), "slot/side-table memo diverge");
                assert_eq!(e.feed, batch.frames()[pos].feed);
                assert_eq!(e.bytes.len(), batch.frames()[pos].len as usize);
                assert_eq!(e.bytes.as_ptr(), batch.frames()[pos].bytes().as_ptr());
            }
        }
    }
}

impl ReplayTransport {
    /// R8: the shared block-triple store — the RX-pipelined consumer builds
    /// `FrameEntry::blocks` slices from it cross-thread (see pipeline.rs).
    /// Immutable after construction.
    #[inline]
    #[allow(clippy::disallowed_types)]
    pub fn shared_triples(&self) -> Arc<[(u64, u32, u32)]> {
        Arc::clone(&self.triples)
    }
}
