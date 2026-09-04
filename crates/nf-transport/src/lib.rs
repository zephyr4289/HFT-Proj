pub mod render;
pub mod replay;
pub mod sched_types;
pub mod xdp;

pub type FeedId = u8;

pub struct FrameView {
    pub(crate) ptr: *const u8,
    pub len: u16,
    pub feed: FeedId,
}

impl FrameView {
    /// SAFETY CONTRACT (doc 01 §5, O-2): valid until next poll() on the
    /// owning transport.
    /// P3: always-inline — per-frame hot (22k calls/run).
    #[inline(always)]
    pub fn bytes(&self) -> &[u8] {
        if self.ptr.is_null() || self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len as usize) }
        }
    }
}

pub struct FrameBatch {
    slots: [FrameView; 256],
    len: usize,
}

impl FrameBatch {
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| FrameView {
                ptr: std::ptr::null(),
                len: 0,
                feed: 0,
            }),
            len: 0,
        }
    }

    #[inline(always)]
    pub fn clear(&mut self) {
        self.len = 0;
    }

    #[inline(always)]
    pub const fn capacity() -> usize {
        256
    }

    #[inline(always)]
    pub fn frames(&self) -> &[FrameView] {
        &self.slots[..self.len]
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline(always)]
    pub fn push(&mut self, frame: FrameView) -> bool {
        if self.len < 256 {
            self.slots[self.len] = frame;
            self.len += 1;
            true
        } else {
            false
        }
    }

    #[inline(always)]
    pub fn push_raw(&mut self, ptr: *const u8, len: usize, feed: FeedId) -> bool {
        if self.len < 256 {
            self.slots[self.len] = FrameView {
                ptr,
                len: len as u16,
                feed,
            };
            self.len += 1;
            true
        } else {
            false
        }
    }
}

impl Default for FrameBatch {
    fn default() -> Self {
        Self::new()
    }
}

pub trait Transport {
    /// Fill `batch`; return frame count. Zero allocations. Never blocks.
    fn poll(&mut self, batch: &mut FrameBatch) -> usize;
    /// Return current timestamp in nanoseconds (AM-1). Virtual clock under replay,
    /// kernel clock under live transports.
    fn now_ns(&self) -> u64;
    /// Q1 indexed ingest: precomputed `(seq, start, end)` block triples for the
    /// frame at batch position `batch_pos` (see `ReplayTransport::batch_blocks`).
    /// Default is empty (no index — e.g. live XDP path); callers fall back to
    /// classic parsing on empty with identical observables.
    #[inline(always)]
    fn batch_blocks(&self, _batch_pos: usize) -> &[(u64, u32, u32)] {
        &[]
    }
}
