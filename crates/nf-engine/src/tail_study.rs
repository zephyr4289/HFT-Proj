//! Tail Attribution Study (doc 15 / G12-T1).
//! Zero-allocation in-window instrumentation, 32k conditional capture ring,
//! first-touch page tracking, and post-window 8-way taxonomy classification.

#![allow(warnings)]
#![allow(clippy::all)]

pub const CAPTURE_RING_SIZE: usize = 32_768;
pub const CAPTURE_THRESHOLD_CYCLES: u64 = 256;

#[repr(C, align(64))]
#[derive(Debug, Clone, Copy, Default)]
pub struct TailRecord {
    pub latency: u32,
    pub seq: u64,
    pub m0_stamp: u64,
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
    pub page_bitmap: Vec<u64>, // 1 bit per 4096-byte page
    pub page_count: usize,
    pub last_was_capture: bool,
}

impl TailStudyContext {
    pub fn new(total_bytes: usize) -> Self {
        let page_count = (total_bytes + 4095) / 4096;
        let bitmap_words = (page_count + 63) / 64;
        Self {
            records: Vec::with_capacity(CAPTURE_RING_SIZE),
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
    pub fn record_tail_sample(
        &mut self,
        latency: u64,
        seq: u64,
        m0: u64,
        offset: usize,
        batch_pos: usize,
        batch_size: usize,
        first_touch: bool,
        is_hb_eos: bool,
    ) {
        if latency > CAPTURE_THRESHOLD_CYCLES {
            if self.records.len() < CAPTURE_RING_SIZE {
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
                    latency: latency.min(u32::MAX as u64) as u32,
                    seq,
                    m0_stamp: m0,
                    input_offset: offset as u32,
                    batch_pos: batch_pos as u16,
                    batch_size: batch_size as u16,
                    flags,
                    reserved: 0,
                });
            } else {
                self.overflow_count += 1;
            }
            self.last_was_capture = true;
        } else {
            self.last_was_capture = false;
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
    pub fn classify(records: &[TailRecord], p99_threshold: u64) -> (Self, Self) {
        let mut above_p90 = TaxonomyBreakdown::default();
        let mut above_p99 = TaxonomyBreakdown::default();

        let mut prev_m0 = 0u64;

        for rec in records {
            let lat = rec.latency as u64;
            let gap = if prev_m0 > 0 && rec.m0_stamp > prev_m0 {
                rec.m0_stamp.saturating_sub(prev_m0)
            } else {
                0
            };
            prev_m0 = rec.m0_stamp;

            let is_p99 = lat >= p99_threshold;

            let mut classified = false;

            let apply = |breakdown: &mut TaxonomyBreakdown| {
                breakdown.total_samples += 1;
                if (rec.flags & FLAG_FIRST_TOUCH) != 0 {
                    breakdown.first_touch += 1;
                } else if (rec.flags & FLAG_PREV_CAPTURE) != 0 {
                    breakdown.prev_capture += 1;
                } else if gap > 2000 {
                    breakdown.inter_msg_gap += 1;
                } else if rec.batch_pos == 0 || rec.batch_size == 1 {
                    breakdown.batch_boundary += 1;
                } else if (rec.flags & FLAG_HB_EOS) != 0 {
                    breakdown.hb_eos += 1;
                } else if (rec.flags & FLAG_EPOCH_EVENT) != 0 {
                    breakdown.epoch_event += 1;
                } else {
                    breakdown.unknown += 1;
                }
            };

            apply(&mut above_p90);
            if is_p99 {
                apply(&mut above_p99);
            }
        }

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
