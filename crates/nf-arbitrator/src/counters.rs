//! Sequencer telemetry counters (doc 05 §11). Copy struct; zero allocations.

use crate::types::FeedId;
use nf_protocol::moldudp64::FrameError;
use nf_protocol::packet::PacketError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeedCounters {
    pub packets: u64,
    pub dups: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViolationCounters {
    pub truncated: u64,
    pub trailing_bytes: u64,
    pub block_overrun: u64,
    pub zero_length_message: u64,
    pub seq_overflow: u64,
    pub payload_invalid: u64,
    pub other: u64,
}

impl ViolationCounters {
    #[inline(always)]
    pub fn record_frame_error(&mut self, err: FrameError) {
        match err {
            FrameError::Truncated { .. } => self.truncated += 1,
            FrameError::TrailingBytes { .. } => self.trailing_bytes += 1,
            FrameError::BlockOverrun => self.block_overrun += 1,
            FrameError::ZeroLengthMessage => self.zero_length_message += 1,
            FrameError::SeqOverflow => self.seq_overflow += 1,
        }
    }

    #[inline(always)]
    pub fn record_packet_error(&mut self, err: PacketError) {
        match err {
            PacketError::Framing(fe) => self.record_frame_error(fe),
            PacketError::Payload(_) => self.payload_invalid += 1,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub feed_a: FeedCounters,
    pub feed_b: FeedCounters,
    pub violations: ViolationCounters,
    pub total_violations: u64,
    pub staged_msgs: u64,
    pub dup_msgs: u64,
    pub beyond_window_dropped: u64,
    pub window_flushed: u64,
    pub gap_opens: u64,
    pub reanchors: u64,
    pub heartbeats: u64,
    pub eos_seen: u64,
    pub eos_dup: u64,
    pub sessions: u64,
    pub data_after_eos: u64,
    pub ignored_after_dead: u64,
    pub intents_issued: u64,
    pub msgs_emitted: u64,
}

impl Counters {
    #[inline(always)]
    pub fn feed_mut(&mut self, feed: FeedId) -> &mut FeedCounters {
        if (feed & 1) == 0 {
            &mut self.feed_a
        } else {
            &mut self.feed_b
        }
    }

    #[inline(always)]
    pub fn feed(&self, feed: FeedId) -> &FeedCounters {
        if (feed & 1) == 0 {
            &self.feed_a
        } else {
            &self.feed_b
        }
    }
}
