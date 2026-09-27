//! HYDRA — bit-exact parallel span-verification fabric (docs/20-hydra.md).
//!
//! # The physics that forced this design (H1)
//!
//! The TITAN program's end-to-end arm is *verification-bound*, not
//! engine-bound: [`SpanConformanceSink`](crate::sink::SpanConformanceSink)
//! must read and CRC32C-verify every emitted byte of every span in-window.
//! Span bodies average ~31.65 B/msg (29.65 B payload + 2 B length prefix),
//! and the `crc32` hardware instruction sustains at most 8 B/cycle of
//! *throughput* on the CI silicon (AMD EPYC 7763 / Zen 3, and every other
//! SSE4.2 x86_64 part in the fleet). That is a hard floor of
//! ~3.96 cyc/msg → ~619M msg/s **even with a zero-cycle sequencer, zero
//! transport, and zero fold**. The measured single-core ceiling of the
//! fully-fused pipeline is therefore ~450-550M msg/s — no amount of
//! micro-optimization on one core can reach 800M-1B while the byte
//! verification stays in-window. Physics, not engineering.
//!
//! # The decomposition (H2, H3)
//!
//! * **H2 (purity)**: `span_crc32c_8lane(body)` is a *pure function of the
//!   span bytes*. It may be evaluated on ANY hardware context without
//!   changing the result.
//! * **H3 (ordered fold)**: the running span hash is the serial fold
//!   `h ← (rotl(h,13) ^ v_i) · K` over span values `v_i` in **emission
//!   order**. The per-span fold is O(1) and cheap; only its *order* matters.
//!
//! HYDRA therefore splits the pipeline across the runner's vCPUs:
//!
//! * **Main core** — transport poll → sequencer arbitration → `on_span`
//!   buffers a 16-byte descriptor `(body_ptr, len, span_id)`; every
//!   `CHUNK`-span run is published to its lane with ONE atomic store.
//! * **Worker cores (N)** — dequeue chunk batches and evaluate the *exact
//!   same* `span_crc32c_8lane` kernel (shared `pub(crate)` symbol — bit
//!   parity by construction) over the immutable span bytes, publishing each
//!   result batch with one atomic store.
//! * **Main core** — folds completed values in emission order and applies
//!   the identical invariant asserts (G-INV era monotonicity, strict
//!   sequence continuity, count).
//!
//! The final (count, hash, msg_hash) triple is **bit-identical** to the
//! sequential `SpanConformanceSink` for any schedule, because (a) every
//! per-span value is the same pure function of the same bytes, and (b) the
//! fold applies those values in the same order. Worker timing cannot
//! influence the result — it only influences *when* values become available,
//! never *what* they are or where they land in the fold sequence.
//!
//! # Chunked handoff protocol (H4 — the anti-ping-pong law)
//!
//! A naive per-span SPSC handoff bounces 5-6 cache lines between the main
//! core and each worker core on EVERY span (head cursor, tail cursor, 4
//! slot-lines, result cursor): at ~100-200 cycles per cross-core line
//! transfer that is +500-1000 cycles/span — measured 3.2x REGRESSION on the
//! v1 fabric (138M vs 264M sequential). The fix is chunk granularity:
//!
//! * descriptors are assigned to lanes in contiguous `CHUNK`-span runs
//!   (`lane = (span_id / CHUNK) mod W`) — one lane's chunk is written as a
//!   bulk block and published with a single Release store;
//! * workers consume whole chunks and publish result batches with a single
//!   Release store;
//! * the fold drains whole batches with a single cursor advance.
//!
//! Because `CHUNK` divides both ring capacities, every chunk is
//! ring-boundary-aligned and never wraps — handoff traffic collapses to
//! ~2 atomics and ~8 line transfers per CHUNK (16 spans, ~3600 cycles of
//! worker CRC work). The pipeline stays fed because the main core is always
//! at least one chunk ahead of the slowest worker.
//!
//! # No-skipping guarantee
//!
//! Every emitted byte is still read and CRC32C-verified *inside the measured
//! window* — on worker cores instead of the main core. No result is
//! memoized across passes (unlike R2 `FrameMemo`, which memoizes only
//! validation verdicts of immutable bytes); the CRC work is re-executed for
//! every measured pass. The measured wall-clock window spans the entire
//! pipeline including the final blocking fold drain (`finish()`).
//!
//! # Flow control & deadlock freedom
//!
//! Chunk-round-robin submission *and* chunk-round-robin folding over the
//! same span-id prefix keep every lane's in-flight balance within ±1 chunk
//! of every other lane. The descriptor ring (2048 slots) is strictly smaller
//! than the result ring (4096 slots), so `processed − folded` per lane can
//! never exhaust the result ring. The main thread only ever blocks on a
//! lane that still holds unprocessed work, so the pipeline always makes
//! progress.
//!
//! # Zero-allocation window
//!
//! All rings, lanes and threads are constructed at [`HydraFabric::spawn`]
//! time (startup, outside every measurement window). The hot path performs
//! no heap allocation; `ALLOC_DELTA == 0` is asserted by every benchmark arm
//! that uses this fabric.

use crate::sink::span_crc32c_8lane;
use nf_arbitrator::types::{Event, LiveFeedProof, Sink};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Spans per handoff chunk. Must divide both ring capacities (ring-aligned
/// chunks never wrap) and be large enough to amortize the cross-core
/// handoff (~2 atomics + ~8 line transfers) against the ~225c/span CRC work.
const CHUNK: u64 = 16;

/// Result-ring capacity (power of two, multiple of CHUNK).
const RES_CAP: usize = 4096;
/// Descriptor-ring capacity (power of two, multiple of CHUNK, strictly <
/// RES_CAP with margin for the ±1-chunk lane skew).
const DESC_CAP: u64 = 2048;
const RES_MASK: u64 = (RES_CAP as u64) - 1;
const DESC_MASK: u64 = DESC_CAP - 1;

/// Main → worker work item: the span body to verify. 16 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    ptr: *const u8,
    len: u32,
    span_id: u32,
}

/// Worker → main result: the exact `span_crc32c_8lane` value of that body.
/// 16 bytes. `span_id` is asserted on fold — a mis-ordered fold is a
/// fail-stop condition, never a silent wrong answer.
#[repr(C)]
#[derive(Clone, Copy)]
struct Res {
    span_id: u32,
    _pad: u32,
    value: u64,
}

/// Cache-line-padded cursor to keep producer and consumer writes on
/// different lines (no false sharing between main and worker cores).
#[repr(align(64))]
struct Pad(AtomicU64);

impl Pad {
    const fn zeroed() -> Self {
        Self(AtomicU64::new(0))
    }
}

impl std::ops::Deref for Pad {
    type Target = AtomicU64;
    #[inline(always)]
    fn deref(&self) -> &AtomicU64 {
        &self.0
    }
}

/// One SPSC lane: a descriptor ring (main → worker) and a result ring
/// (worker → main), chunk-granular handoff.
///
/// SAFETY CONTRACT (justifies `unsafe impl Send/Sync`): `Desc.ptr` is an
/// immutable borrow of span-body bytes owned by a live `ReplayTransport`
/// blob. Session patching never touches bodies (only frame bytes [0..10]),
/// and the submitting sink guarantees via `finish()` that all descriptors
/// are consumed before the owning pass ends (transport reset/drop). Workers
/// only dereference the pointer between submit and the matching fold drain.
pub struct HydraLane {
    /// Descriptor slots. `UnsafeCell`: slot ownership is transferred
    /// producer→consumer by the ring atomics (the current slot OWNER is the
    /// only accessor — never true aliasing).
    desc: UnsafeCell<Box<[Desc; DESC_CAP as usize]>>,
    /// Written by main (Release) after storing a whole chunk; read by worker
    /// (Acquire) to observe new work. Always advances in multiples of CHUNK.
    desc_head: Pad,
    /// Written by worker (Release) after consuming whole chunks; read by main
    /// (Acquire) to reuse slots. Always advances in multiples of CHUNK.
    desc_tail: Pad,
    /// Result slots (same UnsafeCell ownership-transfer protocol).
    res: UnsafeCell<Box<[Res; RES_CAP]>>,
    /// Written by worker (Release) after storing a result batch; read by
    /// main (Acquire) to observe completed values.
    res_head: Pad,
    /// Written by main (Release) after folding a batch; read by worker
    /// (Acquire) before overwriting (defensive — never blocking by proof).
    res_tail: Pad,
}

// SAFETY: see the struct-level contract. Pointer dereference is confined to
// the submit/consume window proven by `finish()`; atomics order everything
// else. One producer (main) and one consumer (worker) per ring.
unsafe impl Send for HydraLane {}
unsafe impl Sync for HydraLane {}

impl HydraLane {
    fn new() -> Box<Self> {
        Box::new(Self {
            desc: UnsafeCell::new(Box::new(
                [Desc {
                    ptr: std::ptr::null(),
                    len: 0,
                    span_id: 0,
                }; DESC_CAP as usize],
            )),
            desc_head: Pad::zeroed(),
            desc_tail: Pad::zeroed(),
            res: UnsafeCell::new(Box::new(
                [Res {
                    span_id: 0,
                    _pad: 0,
                    value: 0,
                }; RES_CAP],
            )),
            res_head: Pad::zeroed(),
            res_tail: Pad::zeroed(),
        })
    }

    /// Producer-side descriptor slots (main thread only).
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    fn desc_slots(&self) -> &mut [Desc; DESC_CAP as usize] {
        // SAFETY: SPSC protocol — slots in [tail, head) are exclusively owned
        // by the producer between the space check and the head Release store.
        unsafe { &mut *self.desc.get() }
    }

    /// Consumer-side descriptor slots (worker thread only).
    #[inline(always)]
    fn desc_slots_read(&self) -> &[Desc; DESC_CAP as usize] {
        // SAFETY: slots below the Acquire-loaded head cursor are published and
        // immutable to the consumer until it releases the tail.
        unsafe { &*self.desc.get() }
    }

    /// Producer-side result slots (worker thread only).
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    fn res_slots(&self) -> &mut [Res; RES_CAP] {
        // SAFETY: SPSC protocol — slots owned by worker until res_head Release.
        unsafe { &mut *self.res.get() }
    }

    /// Consumer-side result slots (main thread only).
    #[inline(always)]
    fn res_slots_read(&self) -> &[Res; RES_CAP] {
        // SAFETY: slots below the Acquire-loaded res_head are published and
        // immutable to main until it releases res_tail.
        unsafe { &*self.res.get() }
    }
}

/// Prefetch the first lines of a span body (T0) — gives the hardware
/// streamer a head start on the worker's upcoming CRC pass.
#[inline(always)]
#[cfg(target_arch = "x86_64")]
fn prefetch_body(ptr: *const u8) {
    unsafe {
        std::arch::x86_64::_mm_prefetch(ptr as *const i8, std::arch::x86_64::_MM_HINT_T0);
        std::arch::x86_64::_mm_prefetch(
            (ptr as usize + 64) as *const i8,
            std::arch::x86_64::_MM_HINT_T0,
        );
    }
}

#[inline(always)]
#[cfg(not(target_arch = "x86_64"))]
fn prefetch_body(_ptr: *const u8) {}

/// Worker batch budget: descriptors processed per outer-loop iteration
/// (multiple of CHUNK so cursors stay chunk-aligned).
const WORKER_BATCH: u64 = 64;

/// Diagnostic (H5): when `HFT_HYDRA_NULL=1`, workers skip the CRC kernel
/// and return a constant-derived value. This BREAKS bit parity by design —
/// it exists solely to isolate the pipeline's non-CRC overhead ceiling
/// (ring mechanics + main-thread sequencer path) on machines where the
/// worker-side stalls need attribution. The bench prints NULL-MODE loudly
/// and skips its parity asserts in this mode. Never set in CI.
fn null_mode() -> bool {
    std::env::var("HFT_HYDRA_NULL").as_deref() == Ok("1")
}

/// Worker main loop: pure function evaluation + chunk-granular SPSC ring
/// mechanics. Never allocates, never blocks on locks, exits only on shutdown
/// with an empty queue.
///
/// PREFETCH PIPELINE (H5): span bodies are ~21 cache lines separated by
/// inter-frame gaps in the transport blob, so the hardware streamer restarts
/// at every body and the first 2-3 lines of each body stall on L3 latency.
/// The worker therefore software-prefetches the body starts of the NEXT
/// FOUR queued spans while CRC-ing the current one — the ~200c per-span CRC
/// pass gives the prefetches ample lead time, converting body-start latency
/// stalls into overlapped L3 bandwidth.
fn lane_worker(lane: Arc<HydraLane>, shutdown: Arc<AtomicBool>) {
    let null = null_mode();
    let mut tail: u64 = 0; // desc cursor (worker-owned)
    let mut rhead: u64 = 0; // result cursor (worker-owned)
    /// How many spans ahead to prefetch body starts (1 line each).
    const LOOKAHEAD: u64 = 4;
    // H4: idle-spin backoff (see the worker loop doc). A busy-waiting worker
    // reloads `desc_head` in a tight PAUSE loop, ping-ponging the head line
    // against the main thread and burning shared execution resources on
    // SMT/co-tenant silicon. Capped exponential backoff keeps the
    // worst-case wake latency at ~1K cycles (half a chunk of main-side
    // work) while cutting idle traffic by 32x.
    let mut backoff: u32 = 0;
    loop {
        let head = lane.desc_head.load(Ordering::Acquire);
        if tail == head {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            let spins = 1u32 << backoff.min(5); // 1..32 pauses
            for _ in 0..spins {
                std::hint::spin_loop();
            }
            backoff += 1;
            continue;
        }
        backoff = 0;
        let n = (head - tail).min(WORKER_BATCH); // ≤ 64 descs (partial tails allowed)
        // SAFETY: slots in [tail, head) are published (Acquire above).
        let slots = lane.desc_slots_read();
        // Warm the lookahead window at batch entry.
        for k in 0..LOOKAHEAD.min(n) {
            prefetch_body(slots[((tail + k) & DESC_MASK) as usize].ptr);
        }
        // Result-space check: once per batch (invariant guarantees space;
        // defensive spin keeps drop-safety if the invariant were violated).
        loop {
            let rt = lane.res_tail.load(Ordering::Acquire);
            if rhead.saturating_sub(rt) + n <= (RES_CAP as u64) - CHUNK {
                break;
            }
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            std::hint::spin_loop();
        }
        // Evaluate + buffer the batch's results locally, then publish with
        // ONE Release store. Slot writes stay unpublished until the store,
        // so intermediate states are invisible to the consumer.
        // SAFETY: res slots in [rhead, rhead+n) are owned by this worker.
        let res_slots = lane.res_slots();
        let mut i = 0u64;
        while i < n {
            // Keep the lookahead window warm while the current span's CRC
            // pass (≈200c) covers the prefetch lead time.
            if i + LOOKAHEAD < n {
                prefetch_body(slots[((tail + i + LOOKAHEAD) & DESC_MASK) as usize].ptr);
            }
            // SAFETY: published descriptor slot (see above).
            let d = slots[((tail + i) & DESC_MASK) as usize];
            // SAFETY: body slice per the HydraLane contract — immutable
            // bytes, valid until the owning pass's finish() drain.
            let value = if null {
                // Diagnostic: constant work, no body read, wrong value (by
                // design — see null_mode doc).
                (d.len as u64) | ((d.span_id as u64) << 32)
            } else {
                let body = unsafe { std::slice::from_raw_parts(d.ptr, d.len as usize) };
                span_crc32c_8lane(body)
            };
            res_slots[((rhead + i) & RES_MASK) as usize] = Res {
                span_id: d.span_id,
                _pad: 0,
                value,
            };
            i += 1;
        }
        std::hint::black_box(&res_slots[(rhead & RES_MASK) as usize]);
        lane.res_head.store(rhead + n, Ordering::Release);
        rhead += n;
        // Free the consumed descriptor slots — one Release store per batch.
        lane.desc_tail.store(tail + n, Ordering::Release);
        tail += n;
    }
}

/// The parallel verification fabric: N lanes + N worker threads + a
/// shutdown flag. Construct ONCE (startup), reused across every benchmark
/// pass; dropped (joining workers) after the last pass.
pub struct HydraFabric {
    lanes: Vec<Arc<HydraLane>>,
    shutdown: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    pub workers: usize,
}

impl HydraFabric {
    /// Spawn a fabric with `workers` lanes (0 → degenerate: callers then use
    /// [`HydraSpanSink::new_inline`], which is the sequential-equivalent
    /// code path). All allocation and thread spawn happens here — outside
    /// every measurement window.
    pub fn spawn(workers: usize) -> Box<Self> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(workers);
        let mut lanes: Vec<Arc<HydraLane>> = Vec::with_capacity(workers);
        for _ in 0..workers {
            // Box→Arc: single heap object, ownership moved (leak-free).
            lanes.push(Arc::from(HydraLane::new()));
        }
        for lane in lanes.iter() {
            let lane = lane.clone();
            let sd = shutdown.clone();
            let h = std::thread::Builder::new()
                .stack_size(512 * 1024)
                .name("hydra-worker".to_string())
                .spawn(move || lane_worker(lane, sd))
                .expect("hydra worker spawn");
            handles.push(h);
        }
        Box::new(Self {
            lanes,
            shutdown,
            handles,
            workers,
        })
    }

    /// Default worker count: every core except the one the main thread runs
    /// on. `HFT_HYDRA_WORKERS` overrides (0 → inline mode).
    pub fn default_workers() -> usize {
        if let Ok(v) = std::env::var("HFT_HYDRA_WORKERS") {
            if let Ok(n) = v.trim().parse::<usize>() {
                return n;
            }
        }
        let avail = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        avail.saturating_sub(1)
    }

    /// Lane owning the chunk that contains `span_id` (diagnostic path —
    /// the hot paths use the sink's incremental trackers).
    #[inline(always)]
    #[allow(dead_code)]
    fn lane_of(&self, span_id: u64) -> &HydraLane {
        &self.lanes[((span_id / CHUNK) % self.lanes.len() as u64) as usize]
    }
}

impl Drop for HydraFabric {
    fn drop(&mut self) {
        // Release the gate; workers finish their queues then exit (they only
        // check shutdown on an empty queue, so no in-flight result is lost
        // mid-pass — and by contract finish() drained everything anyway).
        self.shutdown.store(true, Ordering::Release);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

/// HYDRA span-mode conformance sink: identical observable state and asserts
/// as [`SpanConformanceSink`](crate::sink::SpanConformanceSink), with the
/// 8-lane CRC32C evaluation offloaded to the fabric's worker cores and the
/// ordered fold executed on the main thread.
///
/// Two modes:
/// * **fabric** (`new(&fabric)`) — parallel evaluation, deferred fold;
/// * **inline** (`new_inline()`) — synchronous evaluation + immediate fold,
///   bit-identical semantics with zero threads (the differential-testing
///   baseline and single-core fallback).
pub struct HydraSpanSink<'a> {
    fabric: Option<&'a HydraFabric>,
    // ── observable state: field-for-field identical to SpanConformanceSink ──
    pub hash: u64,
    pub count: u64,
    pub last_gen: u64,
    pub gap_open_gen: Option<u64>,
    pub gap_open_from: Option<u64>,
    pub gap_opens: u64,
    pub reanchors: u64,
    pub session_boundaries: u64,
    pub end_of_sessions: u64,
    pub session_deads: u64,
    pub last_seq: u64,
    pub msg_hash: u64,
    // ── fabric bookkeeping ──
    /// Number of spans submitted (next span id).
    next_span: u64,
    /// Number of spans folded (fold cursor).
    fold_pos: u64,
    /// Chunk-buffered descriptors not yet published to the lane.
    pending: [Desc; CHUNK as usize],
    /// Filled slots in `pending`.
    pending_len: u64,
    /// Lane of the current partial chunk (valid when pending_len > 0).
    pending_lane: usize,
    /// desc_head cursor value at the start of the current chunk.
    pending_head: u64,
    /// H6: division-free lane tracking. `lane = (span/CHUNK) mod W` needs a
    /// runtime `idiv` (~20-40c) per call — called twice per span that is
    /// ~40-80c/span of pure main-thread fat. Incremental advance + wrap
    /// replaces the division with an add and a compare.
    n_lanes: usize,
    /// Lane of the chunk containing `next_span` (submit side).
    submit_lane: usize,
    /// Spans remaining in the submit-side chunk.
    submit_rem: u64,
    /// Lane of the chunk containing `fold_pos` (fold side).
    fold_lane: usize,
    /// Spans remaining in the fold-side chunk.
    fold_rem: u64,
}

impl<'a> HydraSpanSink<'a> {
    pub const SPAN_SEED: u64 = 0xcbf29ce484222325;

    fn blank(fabric: Option<&'a HydraFabric>) -> Self {
        Self {
            fabric,
            hash: Self::SPAN_SEED,
            count: 0,
            last_gen: 0,
            gap_open_gen: None,
            gap_open_from: None,
            gap_opens: 0,
            reanchors: 0,
            session_boundaries: 0,
            end_of_sessions: 0,
            session_deads: 0,
            last_seq: 0,
            msg_hash: Self::SPAN_SEED,
            next_span: 0,
            fold_pos: 0,
            pending: [Desc {
                ptr: std::ptr::null(),
                len: 0,
                span_id: 0,
            }; CHUNK as usize],
            pending_len: 0,
            pending_lane: 0,
            pending_head: 0,
            n_lanes: fabric.map(|f| f.lanes.len()).unwrap_or(0),
            // Chunk 0 → lane 0; advance on chunk completion (after-use).
            submit_lane: 0,
            submit_rem: CHUNK,
            fold_lane: 0,
            fold_rem: CHUNK,
        }
    }

    pub fn new(fabric: &'a HydraFabric) -> Self {
        Self::blank(Some(fabric))
    }

    /// Sequential-equivalent mode: no fabric, no threads — evaluates and
    /// folds synchronously. Used for differential bit-parity testing and as
    /// the single-core fallback (`HFT_HYDRA_WORKERS=0`).
    pub fn new_inline() -> Self {
        Self::blank(None)
    }

    /// Reset all observable + bookkeeping state for a fresh pass. Only
    /// valid when the previous pass fully drained (`fold_pos == next_span`
    /// and nothing pending) — asserted.
    pub fn reset(&mut self) {
        assert_eq!(
            self.fold_pos, self.next_span,
            "hydra reset with undrained spans"
        );
        assert_eq!(self.pending_len, 0, "hydra reset with pending chunk");
        self.hash = Self::SPAN_SEED;
        self.count = 0;
        self.last_gen = 0;
        self.gap_open_gen = None;
        self.gap_open_from = None;
        self.gap_opens = 0;
        self.reanchors = 0;
        self.session_boundaries = 0;
        self.end_of_sessions = 0;
        self.session_deads = 0;
        self.last_seq = 0;
        self.msg_hash = Self::SPAN_SEED;
        self.next_span = 0;
        self.fold_pos = 0;
        self.pending_len = 0;
        // Chunk 0 → lane 0; advance on chunk completion (after-use).
        self.submit_rem = CHUNK;
        self.fold_rem = CHUNK;
        self.submit_lane = 0;
        self.fold_lane = 0;
    }

    /// The per-span fold — EXACTLY `SpanConformanceSink::on_span`'s mix.
    #[inline(always)]
    fn fold_value(&mut self, v: u64) {
        self.hash = self.hash.rotate_left(13) ^ v;
        self.hash = self.hash.wrapping_mul(0x9e3779b97f4a7c15);
    }

    /// Publish the buffered descriptors to their lane: bulk slot writes + ONE
    /// Release store. Full chunks (CHUNK spans) keep the cursors chunk-
    /// aligned; the pass tail publishes a partial run — all slot indexing is
    /// ring-masked, so unaligned publishes are safe. Backpressure (ring full)
    /// folds ready results and spins until the worker frees slots —
    /// deadlock-free by the lane-balance proof.
    fn flush_pending(&mut self) {
        let n = self.pending_len;
        if n == 0 {
            return;
        }
        let fabric = match self.fabric {
            Some(f) => f,
            None => unreachable!("flush_pending in inline mode"),
        };
        let lane = &fabric.lanes[self.pending_lane];
        let h0 = self.pending_head;
        loop {
            let t = lane.desc_tail.load(Ordering::Acquire);
            if h0.saturating_sub(t) + n <= DESC_CAP {
                break;
            }
            // Backpressure: fold what's ready (keeps result rings flowing),
            // then re-check. The worker is guaranteed to make progress.
            self.fold_available();
            std::hint::spin_loop();
        }
        // Write exactly `n` slots (never more — stale buffer entries beyond
        // `pending_len` must NOT be published: the worker would evaluate
        // dead descriptors and poison the fold order).
        // SAFETY: slots [h0 & MASK, +n) are producer-owned until the head
        // Release store below; per-slot masking handles the ring wrap.
        {
            let slots = lane.desc_slots();
            for i in 0..n as usize {
                slots[((h0 + i as u64) & DESC_MASK) as usize] = self.pending[i];
            }
        }
        lane.desc_head.store(h0 + n, Ordering::Release);
        self.pending_len = 0;
    }

    /// Buffer a span descriptor into the current chunk (flushing the
    /// previous chunk when it fills). H6: lane tracking is incremental —
    /// no division in the hot path.
    #[inline]
    fn submit_span(&mut self, body: &[u8]) {
        let fabric = match self.fabric {
            Some(f) => f,
            None => unreachable!("submit_span called in inline mode"),
        };
        if self.pending_len == 0 {
            // Start a new chunk on the lane that owns this span id.
            self.pending_lane = self.submit_lane;
            let lane = &fabric.lanes[self.pending_lane];
            self.pending_head = lane.desc_head.load(Ordering::Relaxed);
        }
        debug_assert_eq!(
            ((self.next_span / CHUNK) % self.n_lanes as u64) as usize,
            self.pending_lane,
            "hydra pending buffer crossed a lane boundary"
        );
        self.pending[self.pending_len as usize] = Desc {
            ptr: body.as_ptr(),
            len: body.len() as u32,
            span_id: self.next_span as u32,
        };
        self.pending_len += 1;
        // Advance the division-free submit-chunk tracker (after-use: the
        // lane advances when the chunk it belongs to is complete).
        self.submit_rem -= 1;
        if self.submit_rem == 0 {
            self.submit_rem = CHUNK;
            self.submit_lane = if self.submit_lane + 1 == self.n_lanes {
                0
            } else {
                self.submit_lane + 1
            };
        }
        if self.pending_len == CHUNK as u64 {
            self.flush_pending();
        }
    }

    /// Fold every completed-but-unfolded result batch (non-blocking, in
    /// emission order). One cursor advance per batch.
    ///
    /// ORDER LAW: a lane's result ring may hold several of its own chunks
    /// (c, c+W, c+2W, ...) whose span ids are NOT globally adjacent — the
    /// chunks of the OTHER lanes interleave between them. The fold therefore
    /// caps each lane-drain at the current chunk boundary (`fold_pos`'s
    /// chunk), guaranteeing values are applied in strict emission order.
    fn fold_available(&mut self) {
        let fabric = match self.fabric {
            Some(f) => f,
            None => return,
        };
        while self.fold_pos < self.next_span {
            // H6: `fold_lane` tracks the chunk containing `fold_pos` — no
            // division in the hot path (advance after chunk completion).
            let lane = &fabric.lanes[self.fold_lane];
            let head = lane.res_head.load(Ordering::Acquire);
            let tail = lane.res_tail.load(Ordering::Relaxed); // main-owned cursor
            if tail == head {
                break; // this lane's next batch isn't ready — order is strict
            }
            // Never fold past the end of fold_pos's chunk (lane changes there).
            let n = (head - tail).min(self.fold_rem);
            // SAFETY: slots [tail & RES_MASK, +n) published by the worker's
            // Release store to res_head (loaded Acquire above) and owned by
            // main until the res_tail Release store below. Per-slot masking
            // handles the ring wrap.
            let slots = lane.res_slots_read();
            for i in 0..n as usize {
                let r = &slots[((tail + i as u64) & RES_MASK) as usize];
                // Fail-stop ordering guard: a mis-routed result is a fabric
                // bug, never a silent hash corruption.
                assert_eq!(
                    r.span_id as u64,
                    self.fold_pos + i as u64,
                    "hydra fold-order violation: got span {}, expected {}",
                    r.span_id,
                    self.fold_pos + i as u64
                );
                let v = r.value;
                self.fold_value(v);
            }
            lane.res_tail.store(tail + n, Ordering::Release);
            self.fold_pos += n;
            self.fold_rem -= n;
            if self.fold_rem == 0 {
                self.fold_rem = CHUNK;
                self.fold_lane = if self.fold_lane + 1 == self.n_lanes {
                    0
                } else {
                    self.fold_lane + 1
                };
            }
        }
    }

    /// Opportunistic fold of everything currently ready (called between
    /// polls by the harness). Never blocks, never publishes.
    #[inline]
    pub fn drain_ready(&mut self) {
        if self.fabric.is_some() {
            self.fold_available();
        }
    }

    /// Blocking drain: publish any pending chunk, then fold ALL submitted
    /// spans (end-of-pass). The measurement window must include this — the
    /// verification is not finished until the last value is folded.
    pub fn finish(&mut self) {
        if self.fabric.is_some() {
            self.flush_pending();
            while self.fold_pos < self.next_span {
                if self.fold_available_once_or_spin() {
                    continue;
                }
                std::hint::spin_loop();
            }
        } else {
            // Inline mode folds eagerly; nothing to drain.
            debug_assert_eq!(self.fold_pos, self.next_span);
        }
    }

    /// One non-blocking fold attempt for `finish`'s spin loop (returns true
    /// if any progress was made).
    fn fold_available_once_or_spin(&mut self) -> bool {
        let before = self.fold_pos;
        self.fold_available();
        self.fold_pos > before
    }

    /// Number of spans submitted but not yet folded (fabric mode),
    /// including the unpublished partial chunk.
    #[inline]
    pub fn pending(&self) -> u64 {
        self.next_span - self.fold_pos
    }
}

impl<'a> Sink for HydraSpanSink<'a> {
    /// Fallback per-message path — field-for-field identical to
    /// `SpanConformanceSink::on_msg` (gaps, unmemoized frames, drain
    /// emissions all take this and hash inline on the main thread).
    #[inline(always)]
    fn on_msg(&mut self, proof: &LiveFeedProof, seq: u64, msg: &[u8]) {
        assert!(
            proof.gen() >= self.last_gen,
            "G-INV violation: proof gen {} is older than sink last_gen {}",
            proof.gen(),
            self.last_gen
        );
        if self.last_seq != 0 {
            assert_eq!(
                seq,
                self.last_seq + 1,
                "Non-monotonic sequence: expected {}, got {}",
                self.last_seq + 1,
                seq
            );
        }
        self.last_seq = seq;
        self.msg_hash = crate::sink::fast_hash_bytes(self.msg_hash, &(msg.len() as u16).to_le_bytes());
        self.msg_hash = crate::sink::fast_hash_bytes(self.msg_hash, msg);
        self.count += 1;
    }

    fn on_event(&mut self, ev: &Event) {
        match ev {
            Event::GapOpened { from, ahead: _, gen } => {
                assert!(*gen > self.last_gen);
                self.last_gen = *gen;
                assert!(self.gap_open_gen.is_none());
                self.gap_open_gen = Some(*gen);
                self.gap_open_from = Some(*from);
                self.gap_opens += 1;
            }
            Event::ReAnchored { gen, at } => {
                assert_eq!(self.gap_open_gen, Some(*gen));
                if let Some(f) = self.gap_open_from {
                    assert!(*at >= f);
                }
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.reanchors += 1;
            }
            Event::SessionBoundary { prev: _, next: _, gen } => {
                assert!(*gen > self.last_gen);
                self.last_gen = *gen;
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.session_boundaries += 1;
                self.last_seq = 0;
            }
            Event::EndOfSession { .. } => {
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.end_of_sessions += 1;
            }
            Event::SessionDead { .. } => {
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.session_deads += 1;
            }
        }
    }

    #[inline(always)]
    fn wants_spans(&self) -> bool {
        true
    }

    /// Span path: identical invariant asserts + count bookkeeping on the
    /// main thread; CRC evaluation on worker cores (fabric mode) or inline
    /// (inline mode). The fold happens in emission order via
    /// `drain_ready()`/`finish()`.
    #[inline(always)]
    fn on_span(
        &mut self,
        proof: &LiveFeedProof,
        first_seq: u64,
        count: u16,
        body: &[u8],
        _blocks: &[(u64, u32, u32)],
    ) {
        assert!(
            proof.gen() >= self.last_gen,
            "G-INV violation: proof gen {} is older than sink last_gen {}",
            proof.gen(),
            self.last_gen
        );
        if self.last_seq != 0 {
            assert_eq!(
                first_seq,
                self.last_seq + 1,
                "Non-monotonic span: expected {}, got {} (count={})",
                self.last_seq + 1,
                first_seq,
                count
            );
        }
        self.last_seq = first_seq + count as u64 - 1;
        self.count += count as u64;
        match self.fabric {
            None => {
                // Inline mode: evaluate + fold NOW — sequential-equivalent.
                let v = span_crc32c_8lane(body);
                self.fold_value(v);
                self.next_span += 1;
                self.fold_pos += 1;
            }
            Some(_) => {
                self.submit_span(body);
                self.next_span += 1;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::{build_schedule, DelayModel, LossModel, Packetize, ReplayConfig};
    use crate::sink::SpanConformanceSink;
    use nf_arbitrator::Sequencer;
    use nf_transport::replay::ReplayTransport;
    use nf_transport::{FrameBatch, Transport};

    const MINI_PATH: &str = "../../data/tests/sample-mini.itch";

    fn load_mini() -> Vec<u8> {
        std::fs::read(MINI_PATH)
            .unwrap_or_else(|_| std::fs::read("data/tests/sample-mini.itch").expect("sample"))
    }

    /// Sequential reference pass (SpanConformanceSink) → (count, hash, msg_hash).
    fn seq_pass(transport: &mut ReplayTransport, sess: [u8; 10]) -> (u64, u64, u64) {
        transport.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = SpanConformanceSink::new();
        let mut batch = FrameBatch::new();
        while transport.poll(&mut batch) > 0 {
            let now = transport.now_ns();
            for (pos, frame) in batch.frames().iter().enumerate() {
                seq.ingest_auto(
                    frame.bytes(),
                    frame.feed,
                    now,
                    &mut sink,
                    transport.batch_blocks(pos),
                    transport.batch_memo(pos),
                );
            }
        }
        (sink.count, sink.hash, sink.msg_hash)
    }

    /// HYDRA fabric pass: drain between polls, blocking finish at end.
    fn hydra_pass(transport: &mut ReplayTransport, sess: [u8; 10], fabric: &HydraFabric) -> (u64, u64, u64) {
        transport.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new(fabric);
        let mut batch = FrameBatch::new();
        while transport.poll(&mut batch) > 0 {
            let now = transport.now_ns();
            for (pos, frame) in batch.frames().iter().enumerate() {
                seq.ingest_auto(
                    frame.bytes(),
                    frame.feed,
                    now,
                    &mut sink,
                    transport.batch_blocks(pos),
                    transport.batch_memo(pos),
                );
            }
            sink.drain_ready();
        }
        sink.finish();
        (sink.count, sink.hash, sink.msg_hash)
    }

    /// HYDRA inline pass (no threads) — sequential-equivalent code path.
    fn inline_pass(transport: &mut ReplayTransport, sess: [u8; 10]) -> (u64, u64, u64) {
        transport.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new_inline();
        let mut batch = FrameBatch::new();
        while transport.poll(&mut batch) > 0 {
            let now = transport.now_ns();
            for (pos, frame) in batch.frames().iter().enumerate() {
                seq.ingest_auto(
                    frame.bytes(),
                    frame.feed,
                    now,
                    &mut sink,
                    transport.batch_blocks(pos),
                    transport.batch_memo(pos),
                );
            }
        }
        sink.finish();
        (sink.count, sink.hash, sink.msg_hash)
    }

    /// H3 bit-parity, default MtuBound dual-feed schedule, fabric with 2
    /// workers + inline mode, all vs the sequential SpanConformanceSink.
    #[test]
    fn t_hydra_bitparity_default_schedule() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"HYDRATEST1";
        let fabric = HydraFabric::spawn(2);
        let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
        let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);

        let want = seq_pass(&mut t1, sess);
        assert_eq!(want.0, 505_849);
        let got_inline = inline_pass(&mut t1, sess);
        assert_eq!(got_inline, want, "inline mode diverged");
        let got_fabric = hydra_pass(&mut t2, sess, &fabric);
        assert_eq!(got_fabric, want, "fabric mode diverged");
        // Determinism across passes (different worker interleavings).
        let got_fabric2 = hydra_pass(&mut t2, sess, &fabric);
        assert_eq!(got_fabric2, want, "fabric determinism diverged");
    }

    /// Bit-parity under chaos: loss, jitter, session change — exercises the
    /// on_msg fallback (gaps → staged drain) and control-plane events.
    #[test]
    fn t_hydra_bitparity_chaos_schedule() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            seed_a: 0xCAFE_BABE_0001_0002,
            seed_b: 0xDEAD_BEEF_0003_0004,
            msgs_per_packet: Packetize::MtuBound(1200),
            loss: [
                LossModel::Bernoulli { p_pm: 100 },
                LossModel::Bernoulli { p_pm: 100 },
            ],
            delay: [
                DelayModel::GaussianApprox { mean_ns: 30_000, sigma_ns: 8_000 },
                DelayModel::GaussianApprox { mean_ns: 30_000, sigma_ns: 8_000 },
            ],
            guarantee_coverage: true,
            session_change_at_msg: Some(300_000),
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"CHAOSHYDRA";
        let fabric = HydraFabric::spawn(2);
        let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
        let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);

        let want = seq_pass(&mut t1, sess);
        assert_eq!(want.0, 505_849, "chaos must still cover the full population");
        let got = hydra_pass(&mut t2, sess, &fabric);
        assert_eq!(got, want, "chaos fabric mode diverged");
    }

    /// Bit-parity at extreme span granularity: one message per packet
    /// (maximum span count, maximum ring churn).
    #[test]
    fn t_hydra_bitparity_fixed1() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::Fixed(1),
            feeds_enabled: 1,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"FIXED1HYDR";
        let fabric = HydraFabric::spawn(2);
        let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
        let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);

        let want = seq_pass(&mut t1, sess);
        let got = hydra_pass(&mut t2, sess, &fabric);
        assert_eq!(got, want, "fixed(1) fabric mode diverged");
    }

    /// Bit-parity with random packet sizes (mixed span lengths).
    #[test]
    fn t_hydra_bitparity_seeded_range() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::SeededRange { min: 3, max: 61 },
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"RANGEHYDRA";
        let fabric = HydraFabric::spawn(2);
        let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
        let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);

        let want = seq_pass(&mut t1, sess);
        let got = hydra_pass(&mut t2, sess, &fabric);
        assert_eq!(got, want, "seeded-range fabric mode diverged");
    }

    /// Backpressure path: 1 worker with a deliberately tiny consumer is
    /// still bit-exact (the main thread must block/fold correctly when the
    /// descriptor ring wraps many times over).
    #[test]
    fn t_hydra_bitparity_ring_wraparound() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"WRAPHYDRA1";
        let fabric = HydraFabric::spawn(1);
        let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
        let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);

        let want = seq_pass(&mut t1, sess);
        let got = hydra_pass(&mut t2, sess, &fabric);
        assert_eq!(got, want, "wraparound fabric mode diverged");
    }

    /// Fabric mode == inline mode == sequential on the full mini sample
    /// regardless of worker count (1, 2, 3 lanes).
    #[test]
    fn t_hydra_bitparity_worker_counts() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"WCOUNTHYDR";
        let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
        let want = seq_pass(&mut t1, sess);
        for w in 1..=3usize {
            let fabric = HydraFabric::spawn(w);
            let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);
            let got = hydra_pass(&mut t2, sess, &fabric);
            assert_eq!(got, want, "worker count {} diverged", w);
        }
    }
}
