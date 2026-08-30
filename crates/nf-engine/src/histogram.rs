//! Static, zero-allocation log-linear latency histogram (doc 11 §4).

pub const LINEAR_BUCKETS: usize = 1024;
pub const LOG_BANDS: usize = 16;
pub const SUB_BUCKETS: usize = 64;
pub const TOTAL_BUCKETS: usize = LINEAR_BUCKETS + (LOG_BANDS * SUB_BUCKETS) + 1; // +1 anomaly overflow

pub struct StaticHistogram {
    buckets: [u32; TOTAL_BUCKETS],
    count: u64,
    min: u64,
    max: u64,
    sum: u64,
}

impl Default for StaticHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl StaticHistogram {
    pub const fn new() -> Self {
        Self {
            buckets: [0u32; TOTAL_BUCKETS],
            count: 0,
            min: u64::MAX,
            max: 0,
            sum: 0,
        }
    }

    pub fn clear(&mut self) {
        self.buckets.fill(0);
        self.count = 0;
        self.min = u64::MAX;
        self.max = 0;
        self.sum = 0;
    }

    #[inline(always)]
    pub fn record(&mut self, val: u64) {
        self.count += 1;
        self.sum += val;
        if val < self.min {
            self.min = val;
        }
        if val > self.max {
            self.max = val;
        }

        let idx = if val < 1024 {
            val as usize
        } else {
            let log = (63 - val.leading_zeros()) as usize; // 10..26
            if (10..26).contains(&log) {
                let band = log - 10;
                let base = 1u64 << log;
                let offset = val - base;
                let sub = ((offset << 6) >> log) as usize; // 64 sub-buckets
                LINEAR_BUCKETS + (band * SUB_BUCKETS) + sub.min(SUB_BUCKETS - 1)
            } else {
                TOTAL_BUCKETS - 1 // Anomaly bucket
            }
        };

        if idx < TOTAL_BUCKETS {
            self.buckets[idx] = self.buckets[idx].saturating_add(1);
        }
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn min(&self) -> u64 {
        if self.count == 0 { 0 } else { self.min }
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            (self.sum as f64) / (self.count as f64)
        }
    }

    pub fn percentile(&self, p: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = ((self.count as f64) * (p / 100.0)).ceil() as u64;
        let mut accumulated = 0u64;

        for (idx, &cnt) in self.buckets.iter().enumerate() {
            accumulated += cnt as u64;
            if accumulated >= target {
                return self.bucket_to_val(idx);
            }
        }
        self.max
    }

    fn bucket_to_val(&self, idx: usize) -> u64 {
        if idx < LINEAR_BUCKETS {
            idx as u64
        } else if idx < TOTAL_BUCKETS - 1 {
            let band_idx = idx - LINEAR_BUCKETS;
            let band = band_idx / SUB_BUCKETS;
            let sub = band_idx % SUB_BUCKETS;
            let log = band + 10;
            let base = 1u64 << log;
            let step = base >> 6;
            base + (sub as u64) * step
        } else {
            self.max
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_static_histogram_percentiles() {
        let mut h = StaticHistogram::new();
        for i in 1..=1000 {
            h.record(i);
        }

        assert_eq!(h.count(), 1000);
        assert_eq!(h.min(), 1);
        assert_eq!(h.max(), 1000);
        assert!((h.percentile(50.0) as i64 - 500).abs() <= 5);
        assert!((h.percentile(99.0) as i64 - 990).abs() <= 10);
    }
}
