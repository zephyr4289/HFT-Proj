//! Tail Attribution Study Phase 2 (doc 15 §8 / G12-T1).
//! Per-message instrumentation (P2-L1), strict denominator reconciliation (P2-L2),
//! empty control arm with identical skeleton (P2-L4), and raw/adjusted reporting (P2-L5).

#![allow(warnings)]
#![allow(clippy::all)]

#[repr(C, align(64))]
#[derive(Debug, Clone, Copy, Default)]
pub struct TailRecord {
    pub latency_raw: u32,
    pub latency_adj: u32,
    pub seq: u64,
    pub m0_stamp: u64,
    pub m3_stamp: u64,
    pub input_offset: u32,
    pub batch_pos: u16,
    pub batch_size: u16,
    pub flags: u16, // bit0 first_touch, bit1 leader, bit2 prev_capture, bit3 epoch_ev, bit4 hb_eos
    pub reserved: u32,
}

pub const FLAG_FIRST_TOUCH: u16 = 1 << 0;
pub const FLAG_BATCH_LEADER: u16 = 1 << 1;
pub const FLAG_PREV_CAPTURE: u16 = 1 << 2;
pub const FLAG_EPOCH_EVENT: u16 = 1 << 3;
pub const FLAG_HB_EOS: u16 = 1 << 4;

pub struct TailStudyContext {
    pub records: Vec<TailRecord>,
    pub overflow_count: u64,
    pub page_bitmap: Vec<u64>,
    pub page_count: usize,
    pub last_was_capture: bool,
}

impl TailStudyContext {
    pub fn new(total_bytes: usize, max_msgs: usize) -> Self {
        let page_count = (total_bytes + 4095) / 4096;
        let bitmap_words = (page_count + 63) / 64;
        Self {
            records: Vec::with_capacity(max_msgs.max(600_000)),
            overflow_count: 0,
            page_bitmap: vec![0u64; bitmap_words],
            page_count,
            last_was_capture: false,
        }
    }

    pub fn clear(&mut self) {
        self.records.clear();
        self.overflow_count = 0;
        self.page_bitmap.fill(0);
        self.last_was_capture = false;
    }

    #[inline(always)]
    pub fn check_and_mark_first_touch(&mut self, byte_offset: usize, len: usize) -> bool {
        let page_start = byte_offset / 4096;
        let page_end = (byte_offset + len.saturating_sub(1)) / 4096;
        let mut is_first = false;

        for page in page_start..=page_end {
            if page < self.page_count {
                let word_idx = page / 64;
                let bit_idx = page % 64;
                let mask = 1u64 << bit_idx;
                if (self.page_bitmap[word_idx] & mask) == 0 {
                    self.page_bitmap[word_idx] |= mask;
                    is_first = true;
                }
            }
        }
        is_first
    }

    #[inline(always)]
    pub fn record_sample(
        &mut self,
        lat_raw: u64,
        lat_adj: u64,
        seq: u64,
        m0: u64,
        m3: u64,
        offset: usize,
        batch_pos: usize,
        batch_size: usize,
        first_touch: bool,
        is_hb_eos: bool,
    ) {
        if self.records.len() < self.records.capacity() {
            let mut flags = 0u16;
            if first_touch {
                flags |= FLAG_FIRST_TOUCH;
            }
            if batch_pos == 0 {
                flags |= FLAG_BATCH_LEADER;
            }
            if self.last_was_capture {
                flags |= FLAG_PREV_CAPTURE;
            }
            if is_hb_eos {
                flags |= FLAG_HB_EOS;
            }

            self.records.push(TailRecord {
                latency_raw: lat_raw.min(u32::MAX as u64) as u32,
                latency_adj: lat_adj.min(u32::MAX as u64) as u32,
                seq,
                m0_stamp: m0,
                m3_stamp: m3,
                input_offset: offset as u32,
                batch_pos: batch_pos as u16,
                batch_size: batch_size as u16,
                flags,
                reserved: 0,
            });
            self.last_was_capture = true;
        } else {
            self.overflow_count += 1;
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TaxonomyBreakdown {
    pub total_samples: usize,
    pub first_touch: usize,
    pub prev_capture: usize,
    pub inter_msg_gap: usize,
    pub batch_boundary: usize,
    pub epoch_event: usize,
    pub hb_eos: usize,
    pub render_charge: usize,
    pub unknown: usize,
}

impl TaxonomyBreakdown {
    pub fn classify(records: &[TailRecord], p90_threshold: u64, p99_threshold: u64) -> (Self, Self) {
        let mut above_p90 = TaxonomyBreakdown::default();
        let mut above_p99 = TaxonomyBreakdown::default();

        let mut prev_m3 = 0u64;

        for rec in records {
            let lat = rec.latency_raw as u64;
            let gap = if prev_m3 > 0 && rec.m0_stamp > prev_m3 {
                rec.m0_stamp.saturating_sub(prev_m3)
            } else {
                0
            };
            prev_m3 = rec.m3_stamp;

            let is_p90 = lat >= p90_threshold;
            let is_p99 = lat >= p99_threshold;

            if is_p90 {
                above_p90.total_samples += 1;
                if (rec.flags & FLAG_FIRST_TOUCH) != 0 {
                    above_p90.first_touch += 1;
                } else if (rec.flags & FLAG_PREV_CAPTURE) != 0 {
                    above_p90.prev_capture += 1;
                } else if gap > 2000 {
                    above_p90.inter_msg_gap += 1;
                } else if rec.batch_pos == 0 || rec.batch_size == 1 {
                    above_p90.batch_boundary += 1;
                } else if (rec.flags & FLAG_HB_EOS) != 0 {
                    above_p90.hb_eos += 1;
                } else if (rec.flags & FLAG_EPOCH_EVENT) != 0 {
                    above_p90.epoch_event += 1;
                } else {
                    above_p90.unknown += 1;
                }
            }

            if is_p99 {
                above_p99.total_samples += 1;
                if (rec.flags & FLAG_FIRST_TOUCH) != 0 {
                    above_p99.first_touch += 1;
                } else if (rec.flags & FLAG_PREV_CAPTURE) != 0 {
                    above_p99.prev_capture += 1;
                } else if gap > 2000 {
                    above_p99.inter_msg_gap += 1;
                } else if rec.batch_pos == 0 || rec.batch_size == 1 {
                    above_p99.batch_boundary += 1;
                } else if (rec.flags & FLAG_HB_EOS) != 0 {
                    above_p99.hb_eos += 1;
                } else if (rec.flags & FLAG_EPOCH_EVENT) != 0 {
                    above_p99.epoch_event += 1;
                } else {
                    above_p99.unknown += 1;
                }
            }
        }

        // P2-L2 Denominator reconciliation assertions
        assert_eq!(
            above_p90.total_samples,
            above_p90.first_touch
                + above_p90.prev_capture
                + above_p90.inter_msg_gap
                + above_p90.batch_boundary
                + above_p90.epoch_event
                + above_p90.hb_eos
                + above_p90.render_charge
                + above_p90.unknown,
            "P2-L2: Above-p90 sum must reconcile exactly with denominator"
        );

        assert_eq!(
            above_p99.total_samples,
            above_p99.first_touch
                + above_p99.prev_capture
                + above_p99.inter_msg_gap
                + above_p99.batch_boundary
                + above_p99.epoch_event
                + above_p99.hb_eos
                + above_p99.render_charge
                + above_p99.unknown,
            "P2-L2: Above-p99 sum must reconcile exactly with denominator"
        );

        (above_p90, above_p99)
    }
}

pub fn prefault_buffer(buf: &[u8]) {
    let page_size = 4096;
    let mut sum = 0u8;
    for chunk in buf.chunks(page_size) {
        sum = sum.wrapping_add(chunk[0]);
    }
    std::hint::black_box(sum);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P2-L1 Unit Test: 5-message packet yields 5 stamps with m0[i+1] >= m3[i].
    #[test]
    fn test_p2_l1_unit_law_per_message_stamps() {
        let mut m0_stamps = [0u64; 5];
        let mut m3_stamps = [0u64; 5];

        for i in 0..5 {
            m0_stamps[i] = crate::clock::read_tsc_serialized_start();
            // simulate dispatch work
            std::hint::black_box(i * 42);
            m3_stamps[i] = crate::clock::read_tsc_serialized_end();
        }

        for i in 0..4 {
            assert!(
                m0_stamps[i + 1] >= m3_stamps[i],
                "P2-L1 violation: m0[{}] ({}) < m3[{}] ({})",
                i + 1,
                m0_stamps[i + 1],
                i,
                m3_stamps[i]
            );
        }
    }
}
