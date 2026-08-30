pub mod replay;

pub type FeedId = u8;

pub struct FrameView {
    pub(crate) ptr: *const u8,
    pub len: u16,
    pub feed: FeedId,
}

impl FrameView {
    /// SAFETY CONTRACT (doc 01 §5, O-2): valid until next poll() on the
    /// owning transport.
    #[inline]
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

    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
    }

    #[inline]
    pub const fn capacity() -> usize {
        256
    }

    #[inline]
    pub fn frames(&self) -> &[FrameView] {
        &self.slots[..self.len]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn push(&mut self, frame: FrameView) -> bool {
        if self.len < 256 {
            self.slots[self.len] = frame;
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
}
