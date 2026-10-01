//! Renderer and virtual-clock driven transport (Tier F: zero heap allocations in hot path).

#![cfg_attr(not(test), deny(clippy::disallowed_types))]

use crate::sched_types::{ReplaySchedule, SchedEvent, SchedKind};
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
/// R8 layout: exactly 16 bytes — `release_vt` moved to the parallel `vts`
/// array so the directory stream is 4 entries per cache line (the poll loop
/// and its lookahead walk metas at 4/line instead of 2.67). One u32 of the
/// old 24B padded struct per frame, back in L1.
#[derive(Debug, Clone, Copy)]
struct FrameMeta {
    offset: u32,
    len: u16,
    feed: FeedId,
    patch: bool,
    blk_base: u32,
    blk_count: u16,
    valid: u16,
}

pub struct ReplayTransport {
    schedule: ReplaySchedule,
    event_idx: usize,
    virtual_clock: u64,
    /// All event frames rendered once at construction (startup-only work, outside
    /// every measurement window). poll() only slices + patches session prefix.
    /// ~15MB for the 505k-msg mini schedule; dropped with the transport.
    frames: Box<[u8]>,
    meta: Box<[FrameMeta]>,
    /// R8: parallel release-time stream (event order, one u64 per event).
    /// Split from `meta` so the directory is 16B/entry; the clock check
    /// reads this stream and the per-frame metadata reads the other — both
    /// sequential, both hardware-prefetched.
    vts: Box<[u64]>,
    /// Flat precomputed block index: one `(seq, start, end)` triple per message
    /// block (~8MB mini schedule), grouped per event via `FrameMeta.blk_base /
    /// blk_count`. Read via `batch_blocks()`; session-patch never touches the
    /// body so triples stay valid across `reset()`.
    triples: Box<[(u64, u32, u32)]>,
    /// Event index per pushed batch slot — maps batch position back to `meta`
    /// even when tombstones are skipped (no push). Written in poll(), read by
    /// `batch_blocks()`.
    batch_event: [u32; 256],
    /// Batch length of the most recent poll — bounds `batch_blocks()` so stale
    /// slots from older polls are unreachable.
    batch_event_len: usize,
    session: [u8; 10],
    clock_clamp: Option<u64>,
    /// R6: when false, poll() skips the frame-BODY prefetch (workers read
    /// bodies on their own cores and issue their own head-start prefetch —
    /// main-side body prefetch is pure overhead in fabric mode). The block
    /// TRIPLE prefetch stays on: the main-thread ingest reads triples[0]
    /// and triples[n-1] of every frame. Not semantically observable.
    body_prefetch: bool,
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
                    });
                    vts.push(ev.release_vt);
                }
            }
        }
        Self {
            schedule,
            event_idx: 0,
            virtual_clock: first_vt,
            frames: blob.into_boxed_slice(),
            meta: meta.into_boxed_slice(),
            vts: vts.into_boxed_slice(),
            triples: triples.into_boxed_slice(),
            batch_event: [0u32; 256],
            batch_event_len: 0,
            session,
            clock_clamp: None,
            body_prefetch: true,
        }
    }

    /// R6: control main-side frame-body prefetching in poll() (see field
    /// doc). Default ON (single-core TITAN behavior preserved bit-for-bit).
    #[inline]
    pub fn set_body_prefetch(&mut self, on: bool) {
        self.body_prefetch = on;
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
    }

    #[inline]
    pub fn set_clock_clamp(&mut self, clamp: Option<u64>) {
        self.clock_clamp = clamp;
    }

    /// P3: always-inline + hoisted len/capacity, cold clamp path.
    /// P9a: per released frame: 2 indexed loads + 10B session patch + batch push.
    /// No cursor seeks, no length re-walk, no payload memcpy in-window.
    /// R8: (a) the release-time check reads the parallel `vts` stream while
    /// the per-frame metadata comes from the 16B directory — two sequential
    /// streams instead of one straddling 24B struct; (b) the frame slice is
    /// unchecked (construction-valid offsets — bounds re-proven per push in
    /// debug builds only); (c) the slot carries the Q1/R2 index inline, so
    /// the sequencer's batch apply loop needs no side-table lookups.
    #[inline(always)]
    pub fn poll_clamped(&mut self, batch: &mut FrameBatch, max_vt: Option<u64>) -> usize {
        batch.clear();

        let events_len = self.meta.len();
        if self.event_idx >= events_len {
            return 0;
        }

        let next_vt = self.vts[self.event_idx];
        // HOT: max_vt=None + clock_clamp=None (steady replay) — clamp is cold.
        let jump_to = match max_vt.or(self.clock_clamp) {
            Some(clamp) => {
                std::hint::cold_path();
                std::cmp::min(next_vt, clamp)
            }
            None => next_vt,
        };

        if jump_to > self.virtual_clock {
            self.virtual_clock = jump_to;
        }
        let vclock = self.virtual_clock;
        let cap = FrameBatch::capacity();
        let session = self.session;

        while self.event_idx < events_len && batch.len() < cap {
            let evt = self.event_idx;
            if self.vts[evt] > vclock {
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
            if m.patch {
                frame[0..10].copy_from_slice(&session);
            }
            let slot_idx = batch.len();
            batch.push_indexed(
                frame.as_ptr(),
                m.len,
                m.feed,
                m.blk_base,
                m.blk_count,
                m.valid,
            );
            // Map batch position back to its event for batch_blocks(),
            // tombstone-safe (only pushed frames occupy batch slots).
            self.batch_event[slot_idx] = evt as u32;
            // R4/R8: DLP warm-up — software-prefetch the FIRST cache line of
            // the frames arriving over the next few events, plus the first
            // line of their block triples. The metas are a sequential 16B
            // stream (hardware-prefetched; one 16B load per level); the
            // bodies and triples are the streams the prefetch must bridge.
            // `prefetcht0` accepts any readable address and never faults, so
            // a tombstoned level is a harmless nearby line.
            #[cfg(target_arch = "x86_64")]
            for k in 0..4usize {
                let ahead = evt + 1 + k;
                if ahead >= events_len {
                    break;
                }
                let mk = self.meta[ahead];
                if mk.len == 0 {
                    continue;
                }
                let b = mk.offset as usize;
                // SAFETY: prefetcht0 never faults; `b` is a construction-
                // valid offset (or a tombstone's 0, i.e. the blob start).
                unsafe {
                    if self.body_prefetch {
                        std::arch::x86_64::_mm_prefetch(
                            self.frames.as_ptr().add(b) as *const i8,
                            std::arch::x86_64::_MM_HINT_T0,
                        );
                    }
                    if mk.blk_count > 0 {
                        std::arch::x86_64::_mm_prefetch(
                            self.triples.as_ptr().add(mk.blk_base as usize) as *const i8,
                            std::arch::x86_64::_MM_HINT_T0,
                        );
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
        batch.frames().iter().enumerate().map(move |(pos, f)| {
            let (blk_base, blk_count, valid) = batch.slot_index(pos);
            let blocks: &'b [(u64, u32, u32)] = if blk_count == 0 {
                &[]
            } else {
                // SAFETY: batch slots are written only inside this crate —
                // `push_indexed` copies construction-valid blk_base/blk_count
                // (bounded by the triples store built at transport
                // construction), and `push_raw`/`FrameBatch::new` write
                // 0/0, which the guard above routes to the empty slice.
                // `pos < batch.len()` by the enumerate bounds.
                unsafe {
                    let tp = self.triples.as_ptr().add(blk_base as usize);
                    std::slice::from_raw_parts(tp, blk_count as usize)
                }
            };
            nf_protocol::packet::FrameEntry {
                bytes: f.bytes(),
                feed: f.feed,
                blocks,
                memo: (blk_count != 0).then_some(nf_protocol::packet::FrameMemo {
                    valid_count: valid,
                }),
            }
        })
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
