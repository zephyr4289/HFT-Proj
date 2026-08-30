//! Replay transport: renders synthetic MoldUDP64 packets on demand from
//! ground-truth message block streams. Pure, zero-alloc in poll loop,
//! virtual-clock driven (doc 04 §5).

use crate::{FeedId, FrameBatch, FrameView, Transport};
use nf_protocol::moldudp64::{EOS_COUNT, HEADER_LEN, HEARTBEAT_COUNT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedKind {
    /// Render MoldUDP64 header(seq=first_seq, count) + count message blocks
    /// starting at ground-truth message index `first_msg`.
    Packet {
        first_seq: u64,
        first_msg: u64,
        count: u16,
    },
    /// Heartbeat (count = 0); next_seq is next expected sequence.
    Heartbeat { next_seq: u64 },
    /// End-of-Session (count = 0xFFFF); next_seq is final next-expected.
    EndOfSession { next_seq: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedEvent {
    pub release_vt: u64, // ns on the virtual clock
    pub feed: FeedId,    // 0 = A, 1 = B
    pub kind: SchedKind,
}

impl SchedEvent {
    #[inline]
    pub fn kind_rank(&self) -> u8 {
        match self.kind {
            SchedKind::Packet { .. } => 0,
            SchedKind::Heartbeat { .. } => 1,
            SchedKind::EndOfSession { .. } => 2,
        }
    }

    #[inline]
    pub fn tiebreak(&self) -> u64 {
        match self.kind {
            SchedKind::Packet { first_seq, .. } => first_seq,
            SchedKind::Heartbeat { next_seq } => next_seq,
            SchedKind::EndOfSession { next_seq } => next_seq,
        }
    }
}

impl Ord for SchedEvent {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.release_vt
            .cmp(&other.release_vt)
            .then_with(|| self.feed.cmp(&other.feed))
            .then_with(|| self.kind_rank().cmp(&other.kind_rank()))
            .then_with(|| self.tiebreak().cmp(&other.tiebreak()))
    }
}

impl PartialOrd for SchedEvent {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReplaySchedule {
    pub events: Vec<SchedEvent>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Cursor {
    byte_offset: usize,
    msg_index: u64,
}

impl Cursor {
    fn seek_msg(&mut self, gt: &[u8], target_msg_index: u64) -> Option<usize> {
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

const ARENA_SLOT_SIZE: usize = 1500;
const ARENA_SLOTS: usize = 256;

pub struct ReplayTransport<'a> {
    gt: &'a [u8],
    schedule: ReplaySchedule,
    event_idx: usize,
    virtual_clock: u64,
    arena: Box<[[u8; ARENA_SLOT_SIZE]; ARENA_SLOTS]>,
    cursors: [Cursor; 2],
    session: [u8; 10],
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
        }
    }

    #[inline]
    fn render_event(
        &mut self,
        ev: &SchedEvent,
        slot: &mut [u8; ARENA_SLOT_SIZE],
    ) -> Option<u16> {
        slot[0..10].copy_from_slice(&self.session);
        match ev.kind {
            SchedKind::Heartbeat { next_seq } => {
                slot[10..18].copy_from_slice(&next_seq.to_be_bytes());
                slot[18..20].copy_from_slice(&HEARTBEAT_COUNT.to_be_bytes());
                Some(HEADER_LEN as u16)
            }
            SchedKind::EndOfSession { next_seq } => {
                slot[10..18].copy_from_slice(&next_seq.to_be_bytes());
                slot[18..20].copy_from_slice(&EOS_COUNT.to_be_bytes());
                Some(HEADER_LEN as u16)
            }
            SchedKind::Packet {
                first_seq,
                first_msg,
                count,
            } => {
                slot[10..18].copy_from_slice(&first_seq.to_be_bytes());
                slot[18..20].copy_from_slice(&count.to_be_bytes());

                let feed_idx = (ev.feed as usize) & 1;
                let start_pos = self.cursors[feed_idx].seek_msg(self.gt, first_msg)?;

                let mut cur_pos = start_pos;
                for _ in 0..count {
                    if cur_pos + 2 > self.gt.len() {
                        return None;
                    }
                    let len = u16::from_be_bytes([self.gt[cur_pos], self.gt[cur_pos + 1]]) as usize;
                    cur_pos += 2 + len;
                }

                let payload_len = cur_pos - start_pos;
                let total_len = HEADER_LEN + payload_len;
                if total_len > ARENA_SLOT_SIZE || cur_pos > self.gt.len() {
                    return None;
                }

                slot[HEADER_LEN..total_len].copy_from_slice(&self.gt[start_pos..cur_pos]);
                self.cursors[feed_idx].byte_offset = cur_pos;
                self.cursors[feed_idx].msg_index = first_msg + count as u64;

                Some(total_len as u16)
            }
        }
    }
}

impl<'a> Transport for ReplayTransport<'a> {
    fn poll(&mut self, batch: &mut FrameBatch) -> usize {
        batch.clear();

        if self.event_idx >= self.schedule.events.len() {
            return 0;
        }

        // Virtual clock jump if nothing is deliverable at current virtual clock
        if self.schedule.events[self.event_idx].release_vt > self.virtual_clock {
            self.virtual_clock = self.schedule.events[self.event_idx].release_vt;
        }

        while self.event_idx < self.schedule.events.len()
            && batch.len() < FrameBatch::capacity()
        {
            let ev = self.schedule.events[self.event_idx];
            if ev.release_vt > self.virtual_clock {
                break;
            }

            let slot_idx = batch.len();
            let slot_ptr = self.arena[slot_idx].as_ptr();
            if let Some(frame_len) = self.render_event(&ev, &mut self.arena[slot_idx]) {
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

    #[inline]
    fn now_ns(&self) -> u64 {
        self.virtual_clock
    }
}
