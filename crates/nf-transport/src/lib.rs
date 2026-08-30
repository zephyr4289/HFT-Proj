pub type FeedId = u8;

pub struct FrameView {
    pub(crate) _ptr: *const u8,
    pub len: u16,
    pub feed: FeedId,
}

impl FrameView {
    /// SAFETY CONTRACT (doc 01 §5, O-2): valid until next poll() on the
    /// owning transport.
    pub fn bytes(&self) -> &[u8] {
        todo!("doc 09 / 04")
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
                _ptr: std::ptr::null(),
                len: 0,
                feed: 0,
            }),
            len: 0,
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub const fn capacity() -> usize {
        256
    }

    pub fn frames(&self) -> &[FrameView] {
        &self.slots[..self.len]
    }
}

impl Default for FrameBatch {
    fn default() -> Self {
        Self::new()
    }
}

pub trait Transport {
    fn poll(&mut self, batch: &mut FrameBatch) -> usize;
}
