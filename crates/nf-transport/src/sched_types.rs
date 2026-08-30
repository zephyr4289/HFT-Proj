//! Schedule event types and schedule builder structures (Tier S: startup only).

#![allow(clippy::disallowed_types)]

use crate::FeedId;

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
