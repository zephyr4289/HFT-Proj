//! Renderer and virtual-clock driven transport (Tier F: zero heap allocations in hot path).

#![cfg_attr(not(test), deny(clippy::disallowed_types))]

use crate::sched_types::{ReplaySchedule, SchedEvent, SchedKind};
use crate::{FeedId, FrameBatch, FrameView, Transport};
use nf_protocol::moldudp64::{EOS_COUNT, HEADER_LEN, HEARTBEAT_COUNT};

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
#[derive(Debug, Clone, Copy)]
struct FrameMeta {
    release_vt: u64,
    offset: u32,
    len: u16,
    feed: FeedId,
    patch: bool,
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
    session: [u8; 10],
    clock_clamp: Option<u64>,
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
                    meta.push(FrameMeta {
                        release_vt: ev.release_vt,
                        offset: off,
                        len: len as u16,
                        feed: ev.feed,
                        patch,
                    });
                } else {
                    // Unrenderable event (frame >1500B scratch — unreachable for
                    // MTU-bound schedules): tombstone keeps event_idx aligned,
                    // exactly as the old skip-without-push did.
                    std::hint::cold_path();
                    meta.push(FrameMeta {
                        release_vt: ev.release_vt,
                        offset: 0,
                        len: 0,
                        feed: ev.feed,
                        patch: false,
                    });
                }
            }
        }
        Self {
            schedule,
            event_idx: 0,
            virtual_clock: first_vt,
            frames: blob.into_boxed_slice(),
            meta: meta.into_boxed_slice(),
            session,
            clock_clamp: None,
        }
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
    #[inline(always)]
    pub fn poll_clamped(&mut self, batch: &mut FrameBatch, max_vt: Option<u64>) -> usize {
        batch.clear();

        let events_len = self.meta.len();
        if self.event_idx >= events_len {
            return 0;
        }

        let next_vt = self.meta[self.event_idx].release_vt;
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
            let m = self.meta[self.event_idx];
            if m.release_vt > vclock {
                break;
            }
            if m.len > 0 {
                let base = m.offset as usize;
                let end = base + m.len as usize;
                let frame = &mut self.frames[base..end];
                if m.patch {
                    frame[0..10].copy_from_slice(&session);
                }
                batch.push(FrameView {
                    ptr: frame.as_ptr(),
                    len: m.len,
                    feed: m.feed,
                });
            }
            self.event_idx += 1;
        }

        batch.len()
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
