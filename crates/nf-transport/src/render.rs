//! Renderer and virtual-clock driven transport (Tier F: zero heap allocations in hot path).

#![cfg_attr(not(test), deny(clippy::disallowed_types))]

use crate::sched_types::{ReplaySchedule, SchedEvent, SchedKind};
use crate::{FrameBatch, FrameView, Transport};
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

pub struct ReplayTransport<'a> {
    gt: &'a [u8],
    schedule: ReplaySchedule,
    event_idx: usize,
    virtual_clock: u64,
    arena: Box<[[u8; ARENA_SLOT_SIZE]; ARENA_SLOTS]>,
    cursors: [Cursor; 2],
    session: [u8; 10],
    clock_clamp: Option<u64>,
}

impl<'a> ReplayTransport<'a> {
    pub fn new(gt: &'a [u8], schedule: ReplaySchedule, session: [u8; 10]) -> Self {
        let first_vt = schedule
            .events
            .first()
            .map(|e| e.release_vt)
            .unwrap_or(0);
        Self {
            gt,
            schedule,
            event_idx: 0,
            virtual_clock: first_vt,
            arena: Box::new([[0u8; ARENA_SLOT_SIZE]; ARENA_SLOTS]),
            cursors: [Cursor::default(), Cursor::default()],
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
        self.cursors = [Cursor::default(), Cursor::default()];
        self.session = session;
        self.clock_clamp = None;
    }

    #[inline]
    pub fn set_clock_clamp(&mut self, clamp: Option<u64>) {
        self.clock_clamp = clamp;
    }

    /// P3: always-inline + hoisted len/capacity, cold clamp path, single bounds check.
    #[inline(always)]
    pub fn poll_clamped(&mut self, batch: &mut FrameBatch, max_vt: Option<u64>) -> usize {
        batch.clear();

        let events_len = self.schedule.events.len();
        if self.event_idx >= events_len {
            return 0;
        }

        let next_vt = self.schedule.events[self.event_idx].release_vt;
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

        while self.event_idx < events_len && batch.len() < cap {
            let ev = self.schedule.events[self.event_idx];
            if ev.release_vt > vclock {
                break;
            }

            let slot_idx = batch.len();
            let slot_ptr = self.arena[slot_idx].as_ptr();
            if let Some(frame_len) = self.render_event(&ev, slot_idx) {
                batch.push(FrameView {
                    ptr: slot_ptr,
                    len: frame_len,
                    feed: ev.feed,
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

impl<'a> ReplayTransport<'a> {
    #[inline(always)]
    fn render_event(&mut self, ev: &SchedEvent, slot_idx: usize) -> Option<u16> {
        let feed_idx = (ev.feed as usize) & 1;
        let slot = &mut self.arena[slot_idx];
        render_event_standalone(
            self.gt,
            ev,
            &self.schedule,
            self.session,
            slot,
            &mut self.cursors[feed_idx],
        )
        .map(|l| l as u16)
    }
}

impl<'a> Transport for ReplayTransport<'a> {
    #[inline(always)]
    fn poll(&mut self, batch: &mut FrameBatch) -> usize {
        self.poll_clamped(batch, None)
    }

    #[inline(always)]
    fn now_ns(&self) -> u64 {
        self.virtual_clock
    }
}
