//! R16b "rxdesc" — the shared span-descriptor array state (docs/29 §5).
//!
//! # The physics this module exists for
//!
//! The sustained full-verification wall on the 4-vCPU draws is the MAIN
//! thread (~1.86 cyc/msg at the 1.2348B record): the ladder (~0.63
//! cyc/msg), the ordered fold (~0.15), and the per-span descriptor
//! submission into the per-lane SPSC rings (~0.4-0.6 — the desc store,
//! the chunk-open anchor write, the space checks, the backpressure spin).
//! The workers, meanwhile, are SMT-stacked on one physical core (the
//! `2cpu_smt` ceiling ≈ 32.8 GB/s) while the distinct-core pool (~61 GB/s)
//! sits unused — and the R11 `distinct` experiment proved the system
//! main-bound: workers folded +15% more spans, sustained stayed flat.
//!
//! rxdesc removes the main-side submission cost: the per-span descriptor
//! stores, the anchors and the space checks leave the submitting core.
//! The steady-state per-span cost is ONE 8-byte load + compare (the
//! check-and-fix guard), and the array content comes from the WARM
//! START: the schedule is deterministic — every pass replays the SAME
//! span sequence over the SAME blob — so each window opens by copying
//! the PREVIOUS window's entries into its slot (one memcpy of the
//! previous window's span count, ~1µs per pass). The untimed reference
//! pass pays the full check-and-fix once (it fills the first slot);
//! every measured pass finds every entry already correct. Divergent
//! schedules (duplicates, cold frames, gaps) self-correct: the guard
//! compares each entry against the ACTUAL span body and fixes it in
//! place — exactly the ring protocol's cost and semantics on the slow
//! path, zero stores on the fast path.
//!
//! # The protocol (who writes what, and when)
//!
//! * **The submitting sink (main)** — at each window open it derives the
//!   slot (`last_slot + 1 mod 8` — the single source, sink-driven),
//!   gates on its OWN earlier use of the slot (the fold must have
//!   drained it), warm-starts the copy, and publishes the pass record
//!   `(generation, base_span)`. Per span it CHECKS the entry against the
//!   actual body and fixes on mismatch. Spans become visible to workers
//!   only via the `spans_ready` Release store, which orders the warm
//!   start and every fix.
//! * **The workers** — Acquire `spans_ready`, walk their chunk-grid
//!   chunks, resolve each span's pass record, and read descriptors
//!   directly from the arrays. They never touch a descriptor ring.
//!
//! # Slot lifetime (the reuse gate)
//!
//! Eight array slots circulate globally (the sequence never resets across
//! sinks/generations; `last_slot` is the single source). At most two are
//! live at any moment: the window the fold is draining (slot s) and the
//! window being submitted (slot s+1 — the warm start reads s's immutable
//! published entries, never writes them). A slot is reused by a sink only
//! after its OWN fold cursor passed the earlier window that used it (the
//! sink-side `slot_end` gate); cross-sink reuse is free because a new
//! sink requires the previous one fully drained (the standing fabric
//! contract, enforced fail-stop by the fold-order assert).
//!
//! # Inline-claim chunk states
//!
//! When the submitting core takes a chunk inline (the work-assist), the
//! chunk's spans are evaluated on the submitting core at submit time and
//! the chunk's state byte is set to [`RX_CHUNK_INLINE`] BEFORE the
//! `spans_ready` store that exposes the chunk's spans; workers read the
//! byte after their `spans_ready` Acquire and skip the chunk. The ring is
//! indexed by global chunk id mod 2^15; the live-lead bound (the res-ring
//! capacity + the 8-deep array gate ≈ 8 passes ≈ ~1.4k chunks) is far
//! below the ring size, so a slot is never reused while in flight.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// Number of circulating per-pass array slots (see the module doc).
pub const RX_NARR: usize = 8;

/// Chunk-state ring geometry: one u64 slot per chunk-grid position
/// (global chunk id mod 2^15). The slot holds `chunk_id + 1` for an
/// inline-claimed chunk (0 = none) — the VALUE CHECK makes aliasing
/// structurally impossible: a stale mark from chunk c − 2^15 carries a
/// different value and never matches. The live-lead bound (the res-ring
/// capacity + the 8-deep array gate ≈ 8 passes ≈ ~1.4k chunks) keeps a
/// slot from being REUSED while its chunk is still in flight.
pub const RX_CHUNK_BITS: u64 = 15;
pub const RX_CHUNK_CAP: u64 = 1 << RX_CHUNK_BITS;
pub const RX_CHUNK_MASK: u64 = RX_CHUNK_CAP - 1;

/// Default per-pass descriptor capacity (spans). The canonical corpus has
/// ~11k spans/pass; 128k leaves >10x headroom. `HFT_RXDESC_CAP` overrides
/// (clamped); a window exceeding the capacity fails stop.
pub const RX_CAP_DEFAULT: usize = 1 << 17;

/// The `spans_ready` / pass-record packing: `(gen:u16 << 48) | value:u48`.
/// `gen` is the sink generation (bumped per sink construction / reset);
/// `value` is the sink-relative span count (ready) or the global-within-
/// generation base span id (record). A 48-bit span id covers 280T spans.
#[inline(always)]
pub fn rx_pack_u64(gen: u64, value: u64) -> u64 {
    debug_assert!(gen < (1 << 16) && value < (1 << 48));
    (gen << 48) | value
}

/// The inverse of [`rx_pack_u64`].
#[inline(always)]
pub fn rx_unpack_u64(w: u64) -> (u64, u64) {
    (w >> 48, w & 0xFFFF_FFFF_FFFF)
}

/// Compact 8-byte span descriptor: `offset:u32 | len:u16` (flags in the
/// high 16 bits, currently zero). Identical formula to the R12 Desc8 ring
/// packing — one format across rings and arrays.
#[inline(always)]
pub fn rxdesc_pack_span(offset: u32, len: u16) -> u64 {
    offset as u64 | ((len as u64) << 32)
}

/// The inverse of [`rxdesc_pack_span`] (flags ignored by array users).
#[inline(always)]
pub fn rxdesc_unpack_span(w: u64) -> (u32, u16) {
    ((w & 0xFFFF_FFFF) as u32, ((w >> 32) & 0xFFFF) as u16)
}

/// Cache-line-padded atomic word (no false sharing between the RX, the
/// submitting core and the workers).
#[repr(align(64))]
pub struct RxPad(pub AtomicU64);

impl RxPad {
    pub const fn zeroed() -> Self {
        Self(AtomicU64::new(0))
    }
}

/// The shared rxdesc state: per-pass descriptor arrays, the pass-record
/// ring, the publication cursor, the chunk-state ring and the write-once
/// blob base. Constructed ONCE per fabric (outside every measurement
/// window); every access follows the module-doc protocol.
#[allow(clippy::disallowed_types)] // construction-time Vec handles only
pub struct RxdescState {
    /// Per-pass descriptor arrays (`RX_NARR` arrays, each `cap` words).
    /// Slot ownership is temporal and protocol-proven disjoint (see the
    /// module doc): the submitting sink owns the open window's slot
    /// (warm start + fixes) while workers read published slots only.
    arrays: UnsafeCell<Vec<Box<[u64]>>>,
    cap: usize,
    /// Pass records: `records[i]` = packed `(gen, base_span)` of the
    /// newest window that used slot i (0 = never used — gen 0 base 0 is
    /// distinguishable because a real gen-0 window-0 record has base 0
    /// too; the walk matches on generation + monotone base, so a zero
    /// word reads as gen 0/base 0 and is simply a stale/never entry the
    /// walk's monotone check rejects).
    records: [RxPad; RX_NARR],
    /// The publication cursor: packed `(gen, spans_ready)`. Release by
    /// the sink after array fixes; Acquire by workers.
    ready: RxPad,
    /// The slot of the newest open window (u8::MAX = none yet). Written
    /// by sinks at window open; read by the next sink at ITS window open
    /// (sequential single-sink contract). Ordered by the spans_ready
    /// chain for workers.
    last_slot: AtomicU8,
    /// The previous window's span count (the warm-start copy length) —
    /// stored by the closing sink, read by the next window's open.
    last_window_count: AtomicU64,
    /// Write-once blob base (the first span-body pointer anyone sees —
    /// the RX at its first sliced frame, or the sink at its first
    /// submitted span). All offsets are `body_ptr - blob_base`.
    blob_base: AtomicU64,
    /// The sink generation counter (bumped per sink construction/reset).
    gen: AtomicU64,
    /// Inline-claim chunk states (see the geometry doc above). Written by
    /// the sink BEFORE the exposing `spans_ready` store; read by workers
    /// after their Acquire — plain accesses suffice (the ready chain
    /// orders them).
    chunk_state: UnsafeCell<Box<[u64; RX_CHUNK_CAP as usize]>>,
    /// Prefill fixes applied by sinks (telemetry; Relaxed).
    pub fixes: AtomicU64,
}

// SAFETY: the protocol in the module doc confines every array/record slot
// and chunk-state byte to exactly one accessor at a time, with the
// Release/Acquire chains (mailbox, spans_ready) ordering the handoffs.
// The state itself is immutable after construction apart from those
// protocol-governed cells.
unsafe impl Send for RxdescState {}
unsafe impl Sync for RxdescState {}

impl RxdescState {
    /// Allocate the state (construction time — outside every window).
    pub fn new(cap: usize) -> Self {
        #[allow(clippy::disallowed_types)]
        let arrays: Vec<Box<[u64]>> = (0..RX_NARR)
            .map(|_| vec![0u64; cap].into_boxed_slice())
            .collect();
        Self {
            arrays: UnsafeCell::new(arrays),
            cap,
            records: std::array::from_fn(|_| RxPad::zeroed()),
            ready: RxPad::zeroed(),
            last_slot: AtomicU8::new(u8::MAX),
            last_window_count: AtomicU64::new(0),
            blob_base: AtomicU64::new(0),
            gen: AtomicU64::new(0),
            chunk_state: UnsafeCell::new(
                vec![0u64; RX_CHUNK_CAP as usize]
                    .into_boxed_slice()
                    .try_into()
                    .unwrap(),
            ),
            fixes: AtomicU64::new(0),
        }
    }

    /// The per-array capacity (spans).
    #[inline(always)]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The next generation id (sink construction / reset; the returned
    /// generation's spans start at 0).
    #[inline]
    pub fn next_gen(&self) -> u64 {
        self.gen.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Derive the next array slot from `last_slot` (the module-doc rule —
    /// the RX at bake points and the sink at window open MUST use this
    /// same function on the same read for the lockstep to hold).
    #[inline]
    pub fn next_slot(&self) -> u8 {
        let last = self.last_slot.load(Ordering::Relaxed);
        if last == u8::MAX {
            0
        } else {
            (last + 1) % RX_NARR as u8
        }
    }

    /// Publish the window's record (sink, at window open — AFTER the
    /// reuse gate and the warm start): `records[slot] = (gen, base_span)`
    /// and `last_slot = slot`.
    #[inline]
    pub fn publish_record(&self, slot: u8, gen: u64, base_span: u64) {
        self.records[slot as usize]
            .0
            .store(rx_pack_u64(gen, base_span), Ordering::Release);
        self.last_slot.store(slot, Ordering::Relaxed);
    }

    /// Record the closing window's span count (the warm-start length for
    /// the NEXT window — the schedule is deterministic, so the next
    /// window's span count is the same; a wrong guess only costs
    /// check-and-fix stores, never correctness).
    #[inline]
    pub fn set_last_window_count(&self, count: u64) {
        self.last_window_count.store(count.min(self.cap as u64), Ordering::Relaxed);
    }

    /// The previous window's span count (the warm-start length).
    #[inline]
    pub fn last_window_count(&self) -> usize {
        self.last_window_count.load(Ordering::Relaxed).min(self.cap as u64) as usize
    }

    /// The WARM START: copy `n` entries from slot `from` to slot `to`
    /// (the sink, at window open — after the reuse gate on `to`; `from`
    /// is the previous window's slot, whose published entries are
    /// immutable). Ordered to workers by the record + spans_ready
    /// Release stores that follow.
    pub fn copy_arr(&self, from: u8, to: u8, n: usize) {
        let n = n.min(self.cap);
        if n == 0 || from == to {
            return;
        }
        // SAFETY: `to` is exclusively this sink's (the reuse gate); `from`
        // holds the previous window's published, immutable entries.
        unsafe {
            let arrays = &mut *self.arrays.get();
            std::ptr::copy_nonoverlapping(
                arrays[from as usize].as_ptr(),
                arrays[to as usize].as_mut_ptr(),
                n,
            );
        }
    }

    /// Read a pass record (worker, during the record walk).
    #[inline]
    pub fn read_record(&self, slot: u8) -> (u64, u64) {
        rx_unpack_u64(self.records[slot as usize].0.load(Ordering::Acquire))
    }

    /// Publish the span-count cursor (sink, after array fixes — the
    /// Release that makes prefills, fixes and inline claims visible).
    #[inline]
    pub fn publish_ready(&self, gen: u64, spans: u64) {
        self.ready
            .0
            .store(rx_pack_u64(gen, spans), Ordering::Release);
    }

    /// Load the publication cursor (worker).
    #[inline]
    pub fn load_ready(&self) -> (u64, u64) {
        rx_unpack_u64(self.ready.0.load(Ordering::Acquire))
    }

    /// Capture the write-once blob base (RX at first sliced frame, sink
    /// at first submitted span). Returns the effective base.
    #[inline]
    pub fn capture_blob_base(&self, body_ptr: usize) -> u64 {
        match self
            .blob_base
            .compare_exchange(0, body_ptr as u64, Ordering::Release, Ordering::Relaxed)
        {
            Ok(_) => body_ptr as u64,
            Err(v) => v,
        }
    }

    /// The blob base (workers, after their first `spans_ready` Acquire —
    /// the sink's capture store precedes its first ready Release).
    #[inline]
    pub fn blob_base(&self) -> u64 {
        self.blob_base.load(Ordering::Relaxed)
    }

    /// Prefill one array word (RX thread, or the sink's fix path). The
    /// caller proves slot ownership (the module-doc protocol) and
    /// `idx < cap` (the sink asserts; the RX fail-stops below).
    #[inline(always)]
    pub fn set_arr(&self, slot: u8, idx: usize, w: u64) {
        debug_assert!((slot as usize) < RX_NARR);
        if idx >= self.cap {
            // Fail-stop: a pass with more frames/spans than the array
            // capacity is a configuration error (HFT_RXDESC_CAP), never a
            // silent truncation.
            panic!(
                "rxdesc: array slot {slot} index {idx} exceeds cap {}",
                self.cap
            );
        }
        // SAFETY: slot ownership per the module doc; disjoint access at
        // any instant (RX s+1 / sink s / workers ≤ s).
        unsafe {
            let arrays = &mut *self.arrays.get();
            arrays[slot as usize][idx] = w;
        }
    }

    /// Read one array word (sink's check-and-fix; workers' eval).
    #[inline(always)]
    pub fn get_arr(&self, slot: u8, idx: usize) -> u64 {
        debug_assert!((slot as usize) < RX_NARR && idx < self.cap);
        // SAFETY: as set_arr.
        unsafe {
            let arrays = &*self.arrays.get();
            arrays[slot as usize][idx]
        }
    }

    /// Mark a chunk inline-claimed (sink, BEFORE the exposing
    /// `spans_ready` store). The slot stores `chunk_id + 1` — the value
    /// IS the claim's identity, so a stale mark can never alias.
    #[inline]
    pub fn mark_inline(&self, chunk_id: u64) {
        // SAFETY: the live-lead bound (the geometry doc) keeps this slot
        // exclusively sink-owned at this moment.
        unsafe {
            (*self.chunk_state.get())[(chunk_id & RX_CHUNK_MASK) as usize] = chunk_id + 1;
        }
    }

    /// Is this chunk inline-claimed? (workers, after their ready Acquire).
    #[inline]
    pub fn is_inline(&self, chunk_id: u64) -> bool {
        // SAFETY: the same bound keeps the slot stable while the chunk is
        // in flight; the exact-value match rejects every stale entry
        // within a generation (chunk ids restart per generation — the
        // cross-generation reset is `clear_chunk_states`, called at each
        // sink's activation).
        unsafe { (*self.chunk_state.get())[(chunk_id & RX_CHUNK_MASK) as usize] == chunk_id + 1 }
    }

    /// Clear every inline-claim mark (a sink at its ACTIVATION — its
    /// first window open, which is necessarily after the previous
    /// generation drained; sink CONSTRUCTION order does not match
    /// consumption order). Chunk ids restart at 0 per generation, so a
    /// previous generation's mark would otherwise alias this
    /// generation's chunks: the worker would skip a healthy chunk and
    /// the fold would wait forever for an inline entry nobody claimed.
    /// The clear precedes this generation's first record publish and
    /// spans_ready store, which order it into the workers' gen-change
    /// Acquire.
    pub fn clear_chunk_states(&self) {
        // SAFETY: no live claims (the drained-generation contract); the
        // cell holds a Box — deref FIRST, then memset the boxed target.
        unsafe {
            let ring: &mut Box<[u64; RX_CHUNK_CAP as usize]> = &mut *self.chunk_state.get();
            std::ptr::write_bytes(ring.as_mut_ptr(), 0, RX_CHUNK_CAP as usize);
        }
    }
}
