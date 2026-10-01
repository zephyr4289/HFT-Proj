pub mod render;
pub mod replay;
pub mod sched_types;
pub mod xdp;

pub type FeedId = u8;

pub struct FrameView {
    pub(crate) ptr: *const u8,
    pub len: u16,
    pub feed: FeedId,
    /// R8: inline Q1 index — the frame's block triples live at
    /// `triples[blk_base .. blk_base + blk_count]` in the owning
    /// `ReplayTransport` (0/0 for index-less transports such as live XDP,
    /// and for HB/EOS frames which never carry triples).
    pub(crate) blk_base: u32,
    pub(crate) blk_count: u16,
    /// R8: inline R2 memo — the frame's ITCH validation verdict prefix
    /// (`FrameMemo::valid_count`; 0 when `blk_count == 0`).
    pub(crate) valid: u16,
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
                blk_base: 0,
                blk_count: 0,
                valid: 0,
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

    /// R8: index-carrying push (rendered replay frames). Callers must have
    /// checked `len < capacity` (the poll loop condition does); a debug
    /// assert guards it in test builds.
    #[inline(always)]
    pub(crate) fn push_indexed(
        &mut self,
        ptr: *const u8,
        len: u16,
        feed: FeedId,
        blk_base: u32,
        blk_count: u16,
        valid: u16,
    ) {
        debug_assert!(self.len < 256, "FrameBatch index push overflow");
        self.slots[self.len] = FrameView {
            ptr,
            len,
            feed,
            blk_base,
            blk_count,
            valid,
        };
        self.len += 1;
    }

    #[inline(always)]
    pub fn push_raw(&mut self, ptr: *const u8, len: usize, feed: FeedId) -> bool {
        if self.len < 256 {
            self.slots[self.len] = FrameView {
                ptr,
                len: len as u16,
                feed,
                blk_base: 0,
                blk_count: 0,
                valid: 0,
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

    /// R2: validation-verdict memo (`FrameMemo`) for the frame at batch position
    /// `batch_pos` — exact leading prefix of blocks passing `itch5::validate`,
    /// precomputed at construction from immutable rendered bytes (deterministic
    /// replay transports only). Default `None` (unmemoized — e.g. live XDP):
    /// callers then run in-window validation, identical observables either way.
    #[inline(always)]
    fn batch_memo(&self, _batch_pos: usize) -> Option<nf_protocol::packet::FrameMemo> {
        None
    }
}
