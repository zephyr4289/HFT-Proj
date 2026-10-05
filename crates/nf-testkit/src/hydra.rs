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

use crate::crcfold::CrcKernel;
use crate::sink::span_crc32c_8lane;
use nf_arbitrator::types::{Event, LiveFeedProof, Sink, SpanRec};
use nf_transport::rxdesc::{rxdesc_unpack_span, RxdescState, RX_NARR};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Spans per handoff chunk (GIGAHFT Lever 4: 16 -> 64). Must divide both
/// ring capacities (ring-aligned chunks never wrap) and amortizes the
/// cross-core handoff (~2 atomics + ~24 line transfers per chunk) 4x
/// further than R6's CHUNK=16, collapsing per-span atomic-fence traffic.
const CHUNK: u64 = 64;

/// Result-ring capacity (power of two, multiple of CHUNK).
const RES_CAP: usize = 4096;
/// Descriptor-ring capacity (power of two, multiple of CHUNK, strictly <
/// RES_CAP with margin for the ±1-chunk lane skew).
const DESC_CAP: u64 = 2048;
const RES_MASK: u64 = (RES_CAP as u64) - 1;
const DESC_MASK: u64 = DESC_CAP - 1;

/// Main → worker work item: the span body to verify.
///
/// R12 — DUAL FORMAT over one word array (`[u64; DESC_CAP * 2]`, 32 KB per
/// lane — the same total bytes as the R8 `[Desc; 2048]` ring):
///
/// * **legacy (16 B/desc)** — desc `i` occupies words `[(i & 2047)*2,
///   +1]`: `lo = body_ptr`, `hi = len | (span_id << 32)`. Full 2048-desc
///   capacity; the `HFT_DESC8=0` rollback arm (CI 11n) runs it.
/// * **compact Desc8 (8 B/desc)** — desc `i` occupies word `i & 2047`:
///   `offset:u32 | len:u16 | flags:u16` — the body's offset from the
///   lane's cached blob base (u32 covers the ~15 MB replay blob with
///   room to spare), the body length (u16 — every bench config's span
///   body is MTU-bounded ≤ 1378 B; the debug assert pins the contract),
///   and flag bit 0 = *block start* (the desc opens a 64-span chunk-grid
///   block). 8 descs per 64 B L1 line (vs 4) — the descriptor stream's
///   line traffic between the sequencer and the worker lanes halves, and
///   the ring's touched footprint halves (16 KB). The span id is DERIVED
///   worker-side from the block-start flag + the lane's block counter
///   (lane `l`'s `b`-th block is global chunk `b*W + l`, covering spans
///   `(b*W + l)*CHUNK .. +CHUNK`), and the fold's exact-match assert
///   pins the derivation with identical fail-stop semantics.
///
/// The blob base: the sink caches the first submitted body's pointer and
/// stores it into each lane's `base` word at chunk-open (before the
/// publish; ordered by the ring's Release/Acquire). All span bodies of a
/// run live in one contiguous THP-backed blob, so `base + offset` is
/// exact and offsets are non-negative and u32-ranged. The grid epoch:
/// `reset()` (fully drained, by assert) bumps every lane's `epoch`; the
/// worker resets its block counter on an epoch change, re-anchoring the
/// derivation at the fresh sink's chunk 0.
/// Desc8 flag bit: this word is an ANCHOR, not a span desc — the offset
/// field carries the chunk's FIRST SPAN ID (u32), len is 0, and the worker
/// re-anchors its derivation (`cur_span = anchor`) and consumes the slot
/// without evaluating or emitting. One anchor per grid-aligned chunk-open
/// (1 slot per 65): the derivation is robust to ANY chunk diversion — the
/// assist path taking chunks inline, pass-boundary splits — because it
/// never extrapolates across an anchor.
const DESC8_ANCHOR: u64 = 1;

#[inline(always)]
fn desc8_pack_span(offset: u32, len: u16) -> u64 {
    // R16b: one formula with the rxdesc arrays (nf-transport owns it —
    // the ring and the array must never drift apart).
    nf_transport::rxdesc::rxdesc_pack_span(offset, len)
}

#[inline(always)]
fn desc8_pack_anchor(first_span: u32) -> u64 {
    first_span as u64 | (DESC8_ANCHOR << 48)
}

#[inline(always)]
fn desc8_unpack(w: u64) -> (u32, u16, bool) {
    (
        (w & 0xFFFF_FFFF) as u32,
        ((w >> 32) & 0xFFFF) as u16,
        (w >> 48) & DESC8_ANCHOR != 0,
    )
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

/// One pass's deferred-result record (GIGAHFT Lever 4). `count`/`msg_hash`
/// are captured synchronously at `end_pass`; `hash` is captured when the
/// ordered fold crosses the pass's final span — possibly DURING a later
/// pass's polling (the double-buffered overlap). Fixed-size ring, zero
/// allocation.
#[derive(Clone, Copy)]
struct PassRec {
    /// Global span id one past the pass's last span (u64::MAX while open).
    end_span: u64,
    count: u64,
    msg_hash: u64,
    /// Captured span-fold hash (valid when `done`).
    hash: u64,
    done: bool,
}

/// Completed-pass records kept per sink (harvested by the harness each
/// pass). R16b: 8 -> 32 — the array-driven submission removed the
/// descriptor-ring backpressure, so the fold may lag up to the ARRAY
/// reuse gate (~8 windows ≈ 8 passes) behind submission when the workers
/// are throughput-bound (the ring protocol's desc-ring backpressure
/// capped the lag at ~1 pass). The harvest ring must hold that lag plus
/// margin; 32 gives 4x. The records are 40 B each — 1.3 KB total.
pub const PASS_RING: usize = 32;

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
    /// Descriptor slots as a raw word array (see the Desc doc for the two
    /// formats: legacy 16 B/desc at words [(i & 2047)*2, +1], compact
    /// Desc8 8 B/desc at word i & 2047). `UnsafeCell`: slot ownership is
    /// transferred producer→consumer by the ring atomics (the current slot
    /// OWNER is the only accessor — never true aliasing).
    desc: UnsafeCell<Box<[u64; DESC_CAP as usize * 2]>>,
    /// R12 Desc8: this lane's blob base (the sink's first submitted body
    /// pointer; 0 = unset). Written before the chunk's publish; read by the
    /// worker after its desc_head Acquire.
    base: Pad,
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
            desc: UnsafeCell::new(Box::new([0u64; DESC_CAP as usize * 2])),
            base: Pad(AtomicU64::new(0)),
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

    /// Consumer-side descriptor words (worker thread only).
    #[inline(always)]
    fn desc_words_read(&self) -> &[u64; DESC_CAP as usize * 2] {
        // SAFETY: slots below the Acquire-loaded head cursor are published and
        // immutable to the consumer until it releases the tail.
        unsafe { &*self.desc.get() }
    }

    /// Producer-side descriptor words (main thread only).
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    fn desc_words(&self) -> &mut [u64; DESC_CAP as usize * 2] {
        // SAFETY: SPSC protocol — producer-owned until the head Release.
        unsafe { &mut *self.desc.get() }
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

/// Legacy-format desc load: desc `pos` → (ptr, len, span_id).
#[inline(always)]
fn desc_legacy_at(words: &[u64; DESC_CAP as usize * 2], pos: u64) -> (u64, u32, u32) {
    let i = ((pos & DESC_MASK) as usize) * 2;
    let lo = words[i];
    let hi = words[i + 1];
    (lo, (hi & 0xFFFF_FFFF) as u32, (hi >> 32) as u32)
}

/// R12 Desc8: worker-side span-id derivation state — the running span id,
/// re-anchored by every anchor desc (see DESC8_ANCHOR).
#[derive(Clone, Copy)]
struct Deriv {
    cur_span: u64,
}

/// Read one SPAN desc in the run's format, advancing the derivation (EVAL
/// ORDER ONLY — never call ahead of the CRC cursor; the prefetch path uses
/// the pure `desc_ptr_len`). Anchors are handled by the eval loop (they
/// re-anchor and skip); this reads a real span desc.
#[inline(always)]
fn desc_read(
    words: &[u64; DESC_CAP as usize * 2],
    pos: u64,
    desc8: bool,
    base: u64,
    deriv: &mut Deriv,
) -> (u64, u32, u32) {
    if desc8 {
        let w = words[(pos & DESC_MASK) as usize];
        let (off, len, _) = desc8_unpack(w);
        let sid = deriv.cur_span;
        deriv.cur_span += 1;
        (base.wrapping_add(off as u64), len as u32, sid as u32)
    } else {
        desc_legacy_at(words, pos)
    }
}

/// Read one desc's kind: None for a span desc (ptr, len), Some(first_span)
/// for an anchor. Legacy descs are always span descs.
#[inline(always)]
fn desc_peek(words: &[u64; DESC_CAP as usize * 2], pos: u64, desc8: bool, base: u64) -> Desc8Word {
    if desc8 {
        let w = words[(pos & DESC_MASK) as usize];
        let (off, len, anchor) = desc8_unpack(w);
        if anchor {
            let _ = off;
            Desc8Word::Anchor
        } else {
            Desc8Word::Span {
                ptr: base.wrapping_add(off as u64),
                len: len as u32,
            }
        }
    } else {
        let (p, l, _) = desc_legacy_at(words, pos);
        Desc8Word::Span { ptr: p, len: l }
    }
}

enum Desc8Word {
    Span { ptr: u64, len: u32 },
    Anchor,
}

/// True when the desc at `pos` is an anchor (the pair-eval lookahead — a
/// pair must not consume an anchor as its second span).
#[inline(always)]
fn desc8_anchor_ahead(words: &[u64; DESC_CAP as usize * 2], pos: u64, desc8: bool) -> bool {
    desc8 && (words[(pos & DESC_MASK) as usize] >> 48) & DESC8_ANCHOR != 0
}

/// Pure (ptr, len) read for the prefetch spray — anchors prefetch nothing
/// (len 0); no derivation advance.
#[inline(always)]
fn desc_ptr_len(
    words: &[u64; DESC_CAP as usize * 2],
    pos: u64,
    desc8: bool,
    base: u64,
) -> (u64, u32) {
    match desc_peek(words, pos, desc8, base) {
        Desc8Word::Span { ptr, len } => (ptr, len),
        Desc8Word::Anchor => (0, 0),
    }
}

/// Prefetch the first lines of a span body (T0) — legacy helper for the
/// pre-phase-4 2-line lookahead (kept for the non-x86 build symmetry
/// contract; the worker loop now uses the line-indexed full-span spray
/// below).
#[inline(always)]
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
fn prefetch_body(ptr: *const u8) {
    unsafe {
        std::arch::x86_64::_mm_prefetch(ptr as *const i8, std::arch::x86_64::_MM_HINT_T0);
        std::arch::x86_64::_mm_prefetch(
            (ptr as usize + 64) as *const i8,
            std::arch::x86_64::_MM_HINT_T0,
        );
    }
}

/// R8 phase-4: line-indexed prefetch — the worker's full-span spray walks
/// bodies one cache line at a time (see PfCfg).
#[inline(always)]
#[cfg(target_arch = "x86_64")]
fn prefetch_line(ptr: *const u8, line: usize) {
    unsafe {
        std::arch::x86_64::_mm_prefetch(
            (ptr as usize + line * 64) as *const i8,
            std::arch::x86_64::_MM_HINT_T0,
        );
    }
}

#[inline(always)]
#[cfg(not(target_arch = "x86_64"))]
fn prefetch_line(_ptr: *const u8, _line: usize) {}

#[inline(always)]
#[cfg(not(target_arch = "x86_64"))]
fn prefetch_body(_ptr: *const u8) {}

/// Worker batch budget: descriptors processed per outer-loop iteration
/// (multiple of CHUNK so cursors stay chunk-aligned; two full chunks per
/// iteration amortizes the result-space check and publishes).
const WORKER_BATCH_DEFAULT: u64 = 128;

/// R15: the worker drain granularity is sweepable (`HFT_WORKER_BATCH`, a
/// multiple of CHUNK, clamped to [CHUNK, 4*CHUNK]) — the supply-side
/// rebalance point (docs/27 §7): with the R13/R14/R15 kernel gains the
/// workers drain faster and the batch shape that paced the result
/// publications against main's ordered fold may want re-tuning per class.
/// Read once per worker spawn, outside every window (the PfCfg precedent).
fn worker_batch() -> u64 {
    match std::env::var("HFT_WORKER_BATCH")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(b) => (b / CHUNK).clamp(1, 4) * CHUNK,
        None => WORKER_BATCH_DEFAULT,
    }
}

/// R16e (the RX Desc Diet): the strand-A wait's default depth. The wait
/// lives INSIDE the drain batch: on a publication-frontier hit it
/// publishes the partial run (liveness — the sink's pending pace and the
/// window-reuse gate spin on the fold, which cannot pass results still
/// buffered in the worker), then pause-bursts (2^min(lap,7) PAUSEs per
/// lap, one spans_ready reload per lap) until the frontier advances.
/// The default bridges every regular publication gap hot (the assist
/// chunk ~1.4µs, the window open + warm memcpy ~1µs, the RX's burst
/// cadence) without an outer-loop re-entry; beyond it the wait hands
/// TRUE idleness (a stalled RX, an EOS tail) to the outer loop's deep
/// pause, which yields only after ~100µs — the same design point the
/// deep pause was tuned for. Draw 11 measured the pre-diet shape at
/// 620-806K idle iters/worker (14-21% idle) paying scheduler wake
/// latency on every gap; `HFT_FRONTIER_LAPS` re-prices it per draw (see
/// frontier_laps below for the full economics).
const FRONTIER_LAPS_DEFAULT: u32 = 16;

/// R16e: the strand-A wait depth, fleet-sweepable (`HFT_FRONTIER_LAPS`,
/// clamped [0, 64]; the default is FRONTIER_LAPS_DEFAULT). `0` disarms
/// strand A entirely — the frontier hit publishes its partial and bails
/// to the outer loop's deep pause immediately, i.e. the pre-diet's wake
/// cadence with strands B+C (per-chunk resolution + the division-free
/// grid) still armed — the isolation arm for attributing A vs B+C. The
/// local latency-blind sandbox priced the FULL depth (16) at −9% vs the
/// pre-diet on the 1-worker shape (the wait's deferred result publishes
/// let the fold lag; the res-ring fullness that paced the pre-diet's
/// worker — 20.9K res-blocks — is the mechanism the wait replaces); the
/// CI draws (real silicon, workers 86%/79% busy — the wait's home
/// regime) decide the default per the class protocol, exactly as
/// vend/vtail were priced per class.
fn frontier_laps() -> u32 {
    match std::env::var("HFT_FRONTIER_LAPS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        Some(v) => v.min(64),
        None => FRONTIER_LAPS_DEFAULT,
    }
}

/// Diagnostic (H5): when `HFT_HYDRA_NULL=1`, workers skip the CRC kernel
/// and return a constant-derived value. This BREAKS bit parity by design —
/// it exists solely to isolate the pipeline's non-CRC overhead ceiling
/// (ring mechanics + main-thread sequencer path) on machines where the
/// worker-side stalls need attribution. The bench prints NULL-MODE loudly
/// and skips its parity asserts in this mode. Never set in CI.
fn null_mode() -> bool {
    std::env::var("HFT_HYDRA_NULL").as_deref() == Ok("1")
}

/// R8 phase-4: the worker prefetch pipeline's shape (parsed once per
/// worker spawn, outside every window; sweepable from CI without
/// recompiles).
///
/// * `HFT_PF_AHEAD` — spans of lead the prefetch cursor maintains over
///   the CRC cursor (default 2).
/// * `HFT_PF_LINES` — max cache lines prefetched per span (default 22;
///   span bodies average ~20 lines).
/// * `HFT_PF_BURST` — max prefetches issued per evaluated span (default
///   24). MUST exceed the per-span line demand (~20 lines for the ~1.3KB
///   bodies): the first Zen3 run with the rewrite shipped burst=12 and the
///   cursor's lead decayed to zero — the prefetch rate was capped below
///   the consumption rate and the workers fell back to stall-bound demand
///   loads (8.5 GB/s per core against the 24 GB/s measured ceiling).
///
/// WHY the rewrite: the pre-deep-mailbox shape (2 lines x 4 spans) dated
/// from an architecture where the consumer idled 66% of the wall and the
/// workers' L3 latency was hidden behind submission gaps. With the
/// consumer now ingesting continuously (CI run 36960041885), the workers'
/// demand loads stall on raw L3 latency and the fabric measured ~5 GB/s
/// per core-busy-second against the 24 GB/s measured kernel ceiling —
/// the bodies' ~20 lines per span arrive at MLP-limited rate (~10 lines
/// in flight per core) instead of prefetch-overlapped rate. The fix:
/// prefetch ENTIRE spans ahead with a persistent cursor, issuing a small
/// burst per evaluated span so the request stream stays smooth.
#[derive(Clone, Copy)]
struct PfCfg {
    ahead: u64,
    lines: usize,
    burst: usize,
}

impl PfCfg {
    /// Kernel-aware defaults. R9 flipped the fold512 default from no-spray
    /// to the SAME full-span spray as the scalar kernel: the no-spray
    /// decision rested on "the hardware streamer tracks fold512's sequential
    /// access pattern perfectly" — a conclusion drawn from kbench, whose
    /// buffer is PACKED and gap-free. The real blob (pre-R9) interleaved
    /// byte-identical duplicate-feed frames between the emitted ones, so
    /// the workers' real pattern was read-1.4KB/skip-1.4KB — untrackable
    /// by any streamer (fbench stage P vs K: 335 vs 517 cyc/span, a 35%
    /// layout penalty). R9's blob aliasing removes the dup gaps, but the
    /// ~20B frame headers between span bodies plus the L3-resident blob
    /// still leave latency exposure the spray hides: measured locally,
    /// aliasing-only 255M vs aliasing+spray(2,22,24) 348M sustained (+36%).
    /// Env overrides still win for CI sweeps.
    fn detect(kernel: CrcKernel) -> Self {
        let parse = |k: &str, d: u64| -> u64 {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(d)
                .clamp(0, 64)
        };
        let (d_ahead, d_lines, d_burst) = match kernel {
            CrcKernel::Scalar => (2, 22, 24),
            CrcKernel::Fold512 => (2, 22, 24),
            // R13: same L1 footprint class as the mirror fold (identical
            // access pattern; only the per-step ALU mix changed).
            CrcKernel::Reflect => (2, 22, 24),
        };
        Self {
            ahead: parse("HFT_PF_AHEAD", d_ahead),
            lines: parse("HFT_PF_LINES", d_lines) as usize,
            burst: parse("HFT_PF_BURST", d_burst) as usize,
        }
    }
}

/// R8 phase-2 diagnostics: per-worker telemetry (Relaxed atomics, one
/// padded line per worker — written by the worker, read once per run by
/// the harness; never in a span-granular inner loop).
#[repr(align(64))]
pub struct WorkerStats {
    /// Worker batches consumed (≤ WORKER_BATCH descriptor groups).
    pub batches: AtomicU64,
    /// Spans evaluated (CRC kernel invocations).
    pub spans: AtomicU64,
    /// Nanoseconds inside the evaluate+emit loop (CRC work).
    pub eval_ns: AtomicU64,
    /// Idle spin iterations waiting for descriptors (polite_spin laps).
    pub idle_iters: AtomicU64,
    /// Result-space defensive waits taken.
    pub res_waits: AtomicU64,
    /// R16e (the diet): frontier-wait EPISODES — batch-internal publication
    /// gaps bridged hot (each publishes a partial run first). The draw-11
    /// decomposition's wake-cadence signal: these replace the pre-diet's
    /// 620-806K outer idle iters; the DIAG prints both for the contrast.
    pub frontier_waits: AtomicU64,
    /// R16e: total nanoseconds spent in those waits (the DIAG's fw_ms) —
    /// the strand-A wall cost, kept OUT of eval_ns (busy% stays honest).
    pub frontier_ns: AtomicU64,
    /// This worker's pinned cpu (usize::MAX when unpinned).
    pub cpu: AtomicU64,
}

impl WorkerStats {
    fn new() -> Self {
        Self {
            batches: AtomicU64::new(0),
            spans: AtomicU64::new(0),
            eval_ns: AtomicU64::new(0),
            idle_iters: AtomicU64::new(0),
            res_waits: AtomicU64::new(0),
            frontier_waits: AtomicU64::new(0),
            frontier_ns: AtomicU64::new(0),
            cpu: AtomicU64::new(u64::MAX),
        }
    }
}

/// Worker main loop: pure function evaluation + chunk-granular SPSC ring
/// mechanics. Never allocates, never blocks on locks, exits only on shutdown
/// with an empty queue.
///
/// PREFETCH PIPELINE (H5, rewritten in R8 phase-4): span bodies are ~20
/// cache lines in the shared blob, and the consumer's continuous ingest
/// (post-deep-mailbox) leaves the workers exposed to raw L3 latency — the
/// measured ~5 GB/s per core-busy-second against the 24 GB/s kernel
/// ceiling was MLP-bound, ~10 lines in flight per core. The worker now
/// sprays ENTIRE spans ahead of the CRC cursor with a persistent
/// line-granular cursor (see PfCfg for the tunables and their defaults):
/// a small burst of prefetches per evaluated span keeps the request
/// stream smooth (the R8 3.5x flooding regression came from bursting 24
/// requests at batch entry), and the cursor caps at the published head —
/// producer-owned slots are never read.
fn lane_worker(
    lane: Arc<HydraLane>,
    shutdown: Arc<AtomicBool>,
    kernel: CrcKernel,
    stats: Arc<WorkerStats>,
    desc8: bool,
) {
    stats
        .cpu
        .store(crate::affinity::current_cpu() as u64, Ordering::Relaxed);
    let null = null_mode();
    // R9: the eval2 interleave experiment knob (read once at worker
    // start — outside every measurement window; see the loop's doc).
    let eval2 = std::env::var("HFT_WORKER_EVAL2").as_deref() == Ok("1");
    // R10: the software-pipelined tail experiment knob — consecutive span
    // pairs evaluate through `kernel.eval_pair` (A's vector fold, B's
    // vector fold, A's endings, B's endings — one sequential load stream,
    // the per-span ending overhead hidden under the next span's clmul
    // chains). Read once at worker start, outside every window.
    let pipe = std::env::var("HFT_WORKER_PIPE").as_deref() == Ok("1");
    // R11: the tri-stream fold knob — single spans evaluate through
    // `kernel.eval_tri` (three interleaved state pairs over one load
    // stream; six independent clmul chains instead of two). The kbench
    // `fold512_tri` row prices the kernel-level effect; this sweep prices
    // it on the real span mix through the full worker loop. Read once at
    // worker start, outside every window.
    let tri = std::env::var("HFT_WORKER_TRI").as_deref() == Ok("1");
    let pf = PfCfg::detect(kernel);
    // R15: the drain granularity (HFT_WORKER_BATCH, the supply-side sweep —
    // read once at worker start, outside every window).
    let batch = worker_batch();
    let mut tail: u64 = 0; // desc cursor (worker-owned)
    let mut rhead: u64 = 0; // result cursor (worker-owned)
                            // Prefetch cursor (GLOBAL desc positions; masked on access). Sprays
                            // whole spans ahead of the CRC cursor; caps at `head` — slots beyond
                            // the published head are producer-owned and must not be read.
    let mut pf_span: u64 = 0;
    let mut pf_line: usize = 0;
    // R12 Desc8: the running span-id derivation, re-anchored by anchor
    // descs (one per grid-aligned chunk-open; robust to assist diversions).
    let mut deriv = Deriv { cur_span: 0 };
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
            // R8: SMT-polite (a raw pause-loop starves the sibling worker
            // on Zen3 — mutual spin-starvation collapsed the fabric).
            stats.idle_iters.fetch_add(1, Ordering::Relaxed);
            crate::affinity::polite_spin(&mut backoff);
            continue;
        }
        backoff = 0;
        let n = (head - tail).min(batch); // ≤ batch descs (partial tails allowed)
        stats.spans.fetch_add(n, Ordering::Relaxed);
        stats.batches.fetch_add(1, Ordering::Relaxed);
        let t_eval = std::time::Instant::now();
        // SAFETY: slots in [tail, head) are published (Acquire above).
        let slots = lane.desc_words_read();
        // R12 Desc8: the lane's write-once blob base (read per batch;
        // cache-hot — the word sits in the lane the worker already owns).
        let lane_base = if desc8 {
            lane.base.load(Ordering::Relaxed)
        } else {
            0
        };
        // Re-anchor the prefetch cursor if it fell behind this batch
        // (stalled at a previous head, or a fresh worker start).
        if pf_span < tail {
            pf_span = tail;
            pf_line = 0;
        }
        // Result-space check: once per batch (invariant guarantees space;
        // defensive spin keeps drop-safety if the invariant were violated).
        let mut rb = 0u32;
        loop {
            let rt = lane.res_tail.load(Ordering::Acquire);
            if rhead.saturating_sub(rt) + n <= (RES_CAP as u64) - CHUNK {
                break;
            }
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            stats.res_waits.fetch_add(1, Ordering::Relaxed);
            crate::affinity::polite_spin(&mut rb);
        }
        // Evaluate + buffer the batch's results locally, then publish with
        // ONE Release store. Slot writes stay unpublished until the store,
        // so intermediate states are invisible to the consumer.
        // GIGAHFT Lever 1: the span CRC evaluation runs on the dispatched
        // kernel — the VPCLMULQDQ mirror-domain fold when the silicon has
        // AVX-512+GFNI (bit-exact equal to the scalar kernel by D11),
        // otherwise the scalar 8-lane crc32 chain. Two spans are evaluated
        // per call on the fold kernel (interleaved fold chains hide clmul
        // dependency latency).
        // SAFETY: res slots in [rhead, rhead+n) are owned by this worker.
        let res_slots = lane.res_slots();
        // R12: results emitted this batch (anchors emit none) — res_head
        // advances by THIS count.
        let mut nres: u64 = 0;
        let mut i = 0u64;
        while i < n {
            // Advance the prefetch pipeline: spray up to `burst` lines per
            // evaluated span until the cursor covers `pf.ahead` spans beyond
            // the CRC position (or exhausts the published batch).
            if pf.lines > 0 && pf.burst > 0 {
                let target = tail + i + pf.ahead + 1;
                let mut issued = 0usize;
                while pf_span < target && pf_span < head && issued < pf.burst {
                    let (dptr, dlen) = desc_ptr_len(slots, pf_span, desc8, lane_base);
                    let span_lines = (((dlen as usize) + 63) >> 6).min(pf.lines);
                    let end = span_lines.min(pf_line + (pf.burst - issued));
                    // SAFETY: prefetch never faults and never dereferences;
                    // the slot is published (below head, above tail).
                    for l in pf_line..end {
                        prefetch_line(dptr as *const u8, l);
                    }
                    issued += end - pf_line;
                    if end >= span_lines {
                        pf_span += 1;
                        pf_line = 0;
                    } else {
                        pf_line = end;
                    }
                }
            }
            // R12: results are indexed by RESULT count, not desc count —
            // anchor descs consume ring slots without emitting.
            let emit = |res_slots: &mut [Res], nres: &mut u64, span_id: u32, value: u64| {
                res_slots[((rhead + *nres) & RES_MASK) as usize] = Res {
                    span_id,
                    _pad: 0,
                    value,
                };
                *nres += 1;
            };
            // R8 phase-6: single-span eval for BOTH kernels. The eval2
            // interleave (two concurrent body streams per worker) was
            // designed to hide clmul latency on early AVX-512 silicon, but
            // the measured ceilings (kbench: eval 34.22 vs eval2 33.13 on
            // the 8573C) show the single-span path at parity or better —
            // and ONE sequential stream per worker is exactly the access
            // pattern those ceilings were measured with. Bit-exact by D11
            // either way.
            // R9: HFT_WORKER_EVAL2=1 re-enables the interleave as a
            // per-draw experiment — the kbench parity was measured on
            // PACKED buffers; the post-aliasing real layout is L3-latency
            // bound, where two concurrent span streams double the loads
            // in flight. The CI sweep decides per runner class.
            // SAFETY: published descriptor slot (Acquire above); body slice
            // per the HydraLane contract — immutable bytes, valid until the
            // owning pass's finish() drain.
            {
                // R12: anchors re-anchor the derivation and consume their
                // slot without evaluating or emitting (they carry no body).
                while i < n {
                    if desc8 {
                        let w = slots[((tail + i) & DESC_MASK) as usize];
                        if (w >> 48) & DESC8_ANCHOR != 0 {
                            deriv.cur_span = (w & 0xFFFF_FFFF) as u64;
                            i += 1;
                            continue;
                        }
                    }
                    if pipe && !null && i + 1 < n && !desc8_anchor_ahead(slots, tail + i + 1, desc8)
                    {
                        // R10: the pipelined pair — same two descriptors, same
                        // values, same emission order as two single-span evals;
                        // only the instruction schedule differs (A's endings
                        // issue behind B's vector fold).
                        let (p0, l0, s0) = desc_read(slots, tail + i, desc8, lane_base, &mut deriv);
                        let (p1, l1, s1) =
                            desc_read(slots, tail + i + 1, desc8, lane_base, &mut deriv);
                        // SAFETY: published descriptor slots (Acquire above);
                        // body slices per the HydraLane contract.
                        let b0 =
                            unsafe { std::slice::from_raw_parts(p0 as *const u8, l0 as usize) };
                        let b1 =
                            unsafe { std::slice::from_raw_parts(p1 as *const u8, l1 as usize) };
                        // SAFETY: feature contract verified at spawn.
                        let (v0, v1) = unsafe { kernel.eval_pair(b0, b1) };
                        emit(res_slots, &mut nres, s0, v0);
                        emit(res_slots, &mut nres, s1, v1);
                        i += 2;
                    } else if eval2
                        && !null
                        && i + 1 < n
                        && !desc8_anchor_ahead(slots, tail + i + 1, desc8)
                    {
                        let (p0, l0, s0) = desc_read(slots, tail + i, desc8, lane_base, &mut deriv);
                        let (p1, l1, s1) =
                            desc_read(slots, tail + i + 1, desc8, lane_base, &mut deriv);
                        // SAFETY: as the single-span path, twice.
                        let b0 =
                            unsafe { std::slice::from_raw_parts(p0 as *const u8, l0 as usize) };
                        let b1 =
                            unsafe { std::slice::from_raw_parts(p1 as *const u8, l1 as usize) };
                        // SAFETY: feature contract verified at spawn.
                        let (v0, v1) = unsafe { kernel.eval2(b0, b1) };
                        emit(res_slots, &mut nres, s0, v0);
                        emit(res_slots, &mut nres, s1, v1);
                        i += 2;
                    } else {
                        let (dptr, dlen, dsid) =
                            desc_read(slots, tail + i, desc8, lane_base, &mut deriv);
                        let value = if null {
                            // Diagnostic: constant work, no body read, wrong value (by
                            // design — see null_mode doc).
                            (dlen as u64) | ((dsid as u64) << 32)
                        } else if tri {
                            // R11: the tri-stream fold — same value as eval
                            // (D11-pinned), different ILP structure.
                            let body = unsafe {
                                std::slice::from_raw_parts(dptr as *const u8, dlen as usize)
                            };
                            // SAFETY: feature contract verified at spawn.
                            unsafe { kernel.eval_tri(body) }
                        } else {
                            let body = unsafe {
                                std::slice::from_raw_parts(dptr as *const u8, dlen as usize)
                            };
                            // SAFETY: feature contract verified at spawn.
                            unsafe { kernel.eval(body) }
                        };
                        emit(res_slots, &mut nres, dsid, value);
                        i += 1;
                    }
                }
            }
        }
        std::hint::black_box(&res_slots[(rhead & RES_MASK) as usize]);
        stats
            .eval_ns
            .fetch_add(t_eval.elapsed().as_nanos() as u64, Ordering::Relaxed);
        // R12: res_head advances by RESULTS emitted (anchors emit none) —
        // the fold reads exactly the published spans.
        lane.res_head.store(rhead + nres, Ordering::Release);
        rhead += nres;
        // Free the consumed descriptor slots — one Release store per batch.
        lane.desc_tail.store(tail + n, Ordering::Release);
        tail += n;
    }
}

/// R16b: the ARRAY-DRIVEN worker (rxdesc mode — see nf_transport::rxdesc
/// for the protocol). Replaces the per-lane descriptor ring with:
///
/// * a `spans_ready` poll (one Acquire per iteration — the sink's
///   publication cursor; the line is worker-shared read-only);
/// * the chunk-grid walk (this lane's chunks are `chunk ≡ lane_idx mod
///   n_lanes` — the same grid the ring protocol used, so the fold's
///   chunk-ordered drain and the fold-order assert are unchanged);
/// * per-span pass-record resolution + an 8-byte array descriptor read
///   (sequential within chunks — better L1 behavior than the ring).
///
/// R16e: this shape is preserved VERBATIM as the `HFT_RXDIET=0` rollback
/// (CI arm 11x) — the diet worker (lane_worker_rxdesc_diet, the default)
/// is its draw-11 follow-up; keeping this copy bit-identical keeps the
/// attribution arm honest.
///
/// GENERATION RE-ANCHORING: a fresh sink (or a reset sink) publishes
/// under a new generation; spans restart at 0 and the chunk grid
/// restarts at this lane's index. The previous generation drained before
/// the new one starts (the standing fabric contract — enforced fail-stop
/// by the fold-order assert), so the re-anchor is always from an idle
/// position.
///
/// INLINE-CLAIMED chunks (the submitting core's work-assist) are skipped
/// via the chunk-state ring — their values arrive through the inline
/// ring, exactly as in the ring protocol.
///
/// PREFETCH: the full-span spray survives unchanged in spirit — a
/// persistent (gid, line) cursor walks THIS lane's chunk sequence ahead
/// of the eval cursor, bounded by `ready` (entries beyond it are
/// unwritten) and the PfCfg tunables.
fn lane_worker_rxdesc(
    lane: Arc<HydraLane>,
    shutdown: Arc<AtomicBool>,
    kernel: CrcKernel,
    stats: Arc<WorkerStats>,
    rx: Arc<RxdescState>,
    lane_idx: u64,
    n_lanes: u64,
) {
    stats
        .cpu
        .store(crate::affinity::current_cpu() as u64, Ordering::Relaxed);
    let null = null_mode();
    let pf = PfCfg::detect(kernel);
    // The drain batch (read ONCE at worker start — outside every window;
    // worker_batch() parses an env var, which ALLOCATES. The first 11u
    // shard caught the per-iteration call as an ALLOC_DELTA violation).
    let wbatch = worker_batch();
    // Result cursor — per-LANE lifetime, continuing across generations
    // (the fresh sink's fold starts from the lane's res_tail, exactly as
    // the ring protocol's continuation).
    let mut rhead: u64 = 0;
    // Eval cursor — gen-relative span id of this lane's next span.
    let mut eval: u64 = 0;
    // Generation + pass-record resolution state.
    let mut gen: u64 = 0; // the pre-first-sink state (ready packs gen 0)
    let mut rec_slot: u8 = 0;
    let mut rec_base: u64 = 0;
    let mut blob_base: u64 = 0;
    // Prefetch cursor (this lane's sequence).
    let mut pf_gid: u64 = 0;
    let mut pf_line: usize = 0;
    // Spray-local record state (MUST be independent of the eval's — the
    // spray walks ahead and would corrupt the eval's resolution).
    let mut pf_slot: u8 = 0;
    let mut pf_base: u64 = 0;
    let mut backoff: u32 = 0;

    // Per-span pass-record resolution: probe the next record slot every
    // span (one cached load + 3 compares; the line is worker-shared
    // read-only). The sink publishes a window's record BEFORE any of its
    // spans, so every span below `ready` is covered by a published
    // record — the probe cannot miss.
    #[inline(always)]
    fn resolve_rec(rx: &RxdescState, gen: u64, slot: &mut u8, base: &mut u64, gid: u64) {
        loop {
            let s = (*slot + 1) % RX_NARR as u8;
            let (rg, rb) = rx.read_record(s);
            if rg == gen && rb > *base && rb <= gid {
                *slot = s;
                *base = rb;
            } else {
                return;
            }
        }
    }

    loop {
        let (g, ready) = rx.load_ready();
        if g != gen {
            // Fresh generation: re-anchor. The gen's first window has
            // base 0 — find its record slot (published before any of the
            // gen's spans, hence before the ready store we just loaded).
            gen = g;
            eval = lane_idx * CHUNK;
            rec_base = 0;
            blob_base = 0;
            let mut found = false;
            for s in 0..RX_NARR as u8 {
                let (rg, rb) = rx.read_record(s);
                if rg == gen && rb == 0 {
                    rec_slot = s;
                    found = true;
                    break;
                }
            }
            if !found {
                // A (gen, ready>0) publication implies the base-0 record
                // exists; ready==0 with a new gen means the sink has not
                // opened a window yet — wait for it (the record lands
                // before any span).
                if ready == 0 {
                    if shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    stats.idle_iters.fetch_add(1, Ordering::Relaxed);
                    crate::affinity::polite_spin(&mut backoff);
                    continue;
                }
                // ready > 0 without the record is a protocol violation —
                // fail loud, never fold wrong descriptors.
                panic!("rxdesc worker: generation {gen} has spans but no base-0 record");
            }
            pf_gid = eval;
            pf_line = 0;
            pf_slot = rec_slot;
            pf_base = 0;
            backoff = 0;
        }
        if blob_base == 0 {
            // Ordered before the first ready Release of this generation
            // (the sink captures before it publishes) — but NEVER
            // dereference a null base: spin until it lands.
            blob_base = rx.blob_base();
            if blob_base == 0 {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                stats.idle_iters.fetch_add(1, Ordering::Relaxed);
                crate::affinity::polite_spin(&mut backoff);
                continue;
            }
        }
        if eval >= ready {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            stats.idle_iters.fetch_add(1, Ordering::Relaxed);
            // R16b: the DEEP pause spin (the chunk-granule publication
            // cadence makes the fast-yield escalation a runqueue storm).
            crate::affinity::polite_spin_deep(&mut backoff);
            continue;
        }
        backoff = 0;
        // R16b: drain up to `wbatch` spans across THIS lane's chunks in
        // one iteration — the ring protocol's batch shape. One poll + one
        // result publish per wake amortizes the spans_ready/res-ring
        // coherence traffic (the first 8370C draw measured the per-chunk
        // wake shape at a 30% regression: the polled line ping-ponged at
        // the publication rate).
        let mut n_total: u64 = 0;
        let t_eval = std::time::Instant::now();
        // Result-space check: once per batch (n_total ≤ wbatch ≤ 4*CHUNK;
        // the same bound class as the ring worker — the fold's drain
        // frees space).
        {
            let mut rb = 0u32;
            loop {
                let rt = lane.res_tail.load(Ordering::Acquire);
                if rhead.saturating_sub(rt) + wbatch <= (RES_CAP as u64) - CHUNK {
                    break;
                }
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                stats.res_waits.fetch_add(1, Ordering::Relaxed);
                crate::affinity::polite_spin(&mut rb);
            }
        }
        // Evaluate + buffer the batch's results, then publish with ONE
        // Release store (slot writes stay invisible until the store).
        // SAFETY: res slots in [rhead, rhead+n_total) are owned by this
        // worker (the space check above bounded the batch).
        let res_slots = lane.res_slots();
        let mut nres: u64 = 0;
        while n_total < wbatch && eval < ready {
            // The chunk containing `eval` (mid-chunk resume: a partially
            // published chunk is continued, not restarted).
            let chunk = eval / CHUNK;
            debug_assert_eq!(chunk % n_lanes, lane_idx, "eval cursor left the lane grid");
            // Inline-claimed chunk? The claim is decided at the chunk's
            // open, before any of its spans are published — by the time
            // we can see the chunk, its state byte is final.
            if eval == chunk * CHUNK && rx.is_inline(chunk) {
                eval = (chunk + n_lanes) * CHUNK;
                continue;
            }
            let chunk_hi = ((chunk + 1) * CHUNK).min(ready);
            let mut gid = eval;
            while gid < chunk_hi && n_total < wbatch {
                // Prefetch spray — THE RING PROTOCOL'S CADENCE: up to
                // pf.burst lines issued PER EVALUATED SPAN, keeping the
                // cursor pf.ahead spans beyond the eval position (the
                // first 8370C draw's batch-level spray was 128x too slow
                // — the bodies arrived as demand loads and the eval ran
                // at ~45% of the kernel ceiling, memory-stalled).
                if pf.lines > 0 && pf.burst > 0 {
                    if pf_gid < gid {
                        pf_gid = gid;
                        pf_line = 0;
                        pf_slot = rec_slot;
                        pf_base = rec_base;
                    }
                    let target = gid + pf.ahead + 1;
                    let mut issued = 0usize;
                    while pf_gid < target && pf_gid < ready && issued < pf.burst {
                        let c = pf_gid / CHUNK;
                        if c % n_lanes != lane_idx {
                            // Jump to this lane's next chunk in the grid.
                            let skip = n_lanes - ((c % n_lanes) + n_lanes - lane_idx) % n_lanes;
                            pf_gid = (c + skip) * CHUNK;
                            pf_line = 0;
                            continue;
                        }
                        if rx.is_inline(c) {
                            pf_gid = (c + n_lanes) * CHUNK;
                            pf_line = 0;
                            continue;
                        }
                        resolve_rec(&rx, gen, &mut pf_slot, &mut pf_base, pf_gid);
                        let w = rx.get_arr(pf_slot, (pf_gid - pf_base) as usize);
                        let (off, dlen) = rxdesc_unpack_span(w);
                        let dptr = blob_base.wrapping_add(off as u64);
                        let span_lines = (((dlen as usize) + 63) >> 6).min(pf.lines);
                        let end = span_lines.min(pf_line + (pf.burst - issued));
                        // SAFETY: prefetch never faults and never
                        // dereferences; the entry is published (below
                        // ready).
                        for l in pf_line..end {
                            prefetch_line(dptr as *const u8, l);
                        }
                        issued += end - pf_line;
                        if end >= span_lines {
                            pf_gid += 1;
                            pf_line = 0;
                        } else {
                            pf_line = end;
                        }
                    }
                }
                resolve_rec(&rx, gen, &mut rec_slot, &mut rec_base, gid);
                let w = rx.get_arr(rec_slot, (gid - rec_base) as usize);
                let (off, dlen) = rxdesc_unpack_span(w);
                // SAFETY: the entry was published by the sink's ready
                // store (Acquire above); the body slice per the
                // HydraLane contract — immutable bytes, valid until the
                // owning pass's drain.
                let value = if null {
                    // Diagnostic: constant work, wrong value by design.
                    (dlen as u64) | (gid << 32)
                } else {
                    let body = unsafe {
                        std::slice::from_raw_parts(
                            blob_base.wrapping_add(off as u64) as *const u8,
                            dlen as usize,
                        )
                    };
                    // SAFETY: feature contract verified at spawn.
                    unsafe { kernel.eval(body) }
                };
                debug_assert!(gid <= u32::MAX as u64, "span id exceeds u32");
                res_slots[((rhead + nres) & RES_MASK) as usize] = Res {
                    span_id: gid as u32,
                    _pad: 0,
                    value,
                };
                nres += 1;
                gid += 1;
                n_total += 1;
            }
            // Advance: the resume position is `gid` — a batch-limited
            // stop mid-chunk resumes IN PLACE (advancing to chunk_hi
            // here skipped the chunk's tail: the off-by-one the fold
            // caught). A fully-consumed chunk moves to this lane's next
            // chunk; a ready-limited partial chunk resumes at chunk_hi
            // (== ready, which grows before the next pass over it).
            if gid < chunk_hi {
                eval = gid;
            } else if chunk_hi == (chunk + 1) * CHUNK {
                eval = (chunk + n_lanes) * CHUNK;
            } else {
                eval = chunk_hi;
            }
        }
        std::hint::black_box(&res_slots[(rhead & RES_MASK) as usize]);
        stats.spans.fetch_add(n_total, Ordering::Relaxed);
        stats.batches.fetch_add(1, Ordering::Relaxed);
        stats
            .eval_ns
            .fetch_add(t_eval.elapsed().as_nanos() as u64, Ordering::Relaxed);
        // SAFETY: slots [rhead, rhead+nres) fully written before this
        // Release store; the fold Acquires it and owns them after.
        lane.res_head.store(rhead + nres, Ordering::Release);
        rhead += nres;
    }
}

/// R16e: the RX DESC DIET worker — the draw-11 decomposition's fix (docs/29
/// §5.5). Draw 11 priced the array protocol at -20.6% vs the ring on the
/// healthy class (-21.2% record-class, draw 10) and decomposed the gap
/// 50/50: WAKE-CADENCE IDLE (the batched drain exiting at the publication
/// frontier — 2.7x the ring's batch iterations, 620-806K idle iters, the
/// deep-pause escalation paying scheduler wake latency on every
/// publication gap) and PER-SPAN EVAL DILUTION (~+31 cyc/span: the
/// per-span record probe, the per-span division/is_inline in the spray,
/// the chunk-grid walk). Three strands, ALL worker-side (the sink is
/// untouched — the attribution stays clean):
///
/// * STRAND A — THE DEPTH BATCH: the batch no longer terminates at the
///   frontier. A frontier hit PUBLISHES the partial run first (liveness:
///   the sink's pending pace and the window-reuse gate spin on the FOLD,
///   and the fold cannot pass results still buffered in this worker),
///   then WAITS AT THE FRONTIER in bounded pause laps — hot: no
///   outer-loop re-entry (no gen walk, no res-space re-check), no yield
///   escalation until the bound expires (see FRONTIER_LAPS). The wait is
///   counted (`frontier_waits`) and excluded from eval_ns — busy% stays
///   honest. A fresh generation landing mid-wait bails to the outer
///   loop's re-anchor (the partial results are old-gen; the old sink's
///   finish() drain consumes them — the standing contract).
/// * STRAND B — PER-CHUNK RECORD RESOLUTION: the per-span pass-record
///   probe becomes a NEXT-BOUNDARY cache (`next_wb`); the per-span cost
///   is one register compare (`gid >= next_wb`). Soundness: the record
///   for a window base rb is published (Release on records[slot]) BEFORE
///   the window's first span is submitted, hence before any spans_ready
///   store exceeding rb — the worker's ready Acquire that exposes spans
///   ≥ rb also exposes the record (release sequencing). The cache is
///   refreshed at EVERY ready advance (the outer load and each
///   frontier-wait break), so a boundary below the current ready is in
///   the cache by the time the span loop reaches it; the compare then
///   fires the resolve (which walks any number of windows in one pass)
///   exactly at the boundary. An 8-window overwrite of the probed slot
///   is unreachable while the boundary matters: the reuse gate requires
///   the fold to have drained that window, and the fold cannot pass
///   results this worker has not yet evaluated.
/// * STRAND C — THE DIVISION-FREE GRID: the chunk walk (eval + spray)
///   tracks `(chunk_id, chunk_lo)` by addition (the lane grid's own
///   stride, n_lanes*CHUNK) — the per-chunk idiv is gone. The spray's
///   per-SPAN division/modulo/is_inline/record-probe moves to per-chunk
///   sections: one is_inline + (at most) one resolve per 64 spans, no
///   idiv anywhere; the per-span residue in BOTH loops is the single
///   `>= *_wb` compare. The spray keeps its own resolution state (it
///   walks ahead and would corrupt the eval's).
///
/// Rollback: `HFT_RXDIET=0` selects the pre-diet worker verbatim (the
/// spawn-time per-run constant; the read is outside every window —
/// law #9).
fn lane_worker_rxdesc_diet(
    lane: Arc<HydraLane>,
    shutdown: Arc<AtomicBool>,
    kernel: CrcKernel,
    stats: Arc<WorkerStats>,
    rx: Arc<RxdescState>,
    lane_idx: u64,
    n_lanes: u64,
) {
    stats
        .cpu
        .store(crate::affinity::current_cpu() as u64, Ordering::Relaxed);
    let null = null_mode();
    let pf = PfCfg::detect(kernel);
    // The drain batch (read ONCE at worker start — outside every window;
    // worker_batch() parses an env var, which ALLOCATES).
    let wbatch = worker_batch();
    // R16e: the strand-A wait depth (HFT_FRONTIER_LAPS; 0 = strand A
    // off — the pre-diet wake cadence with B+C armed). Read once here,
    // outside every window (law #9).
    let flaps = frontier_laps();
    // Result cursor — per-LANE lifetime, continuing across generations
    // (the fresh sink's fold starts from the lane's res_tail).
    let mut rhead: u64 = 0;
    // Eval cursor — gen-relative span id of this lane's next span.
    let mut eval: u64 = 0;
    // Generation + pass-record resolution state.
    let mut gen: u64 = 0; // the pre-first-sink state (ready packs gen 0)
    let mut rec_slot: u8 = 0;
    let mut rec_base: u64 = 0;
    let mut blob_base: u64 = 0;
    // STRAND B: the next window boundary — the span id where the current
    // record goes stale (u64::MAX = none visible). Carried across chunks;
    // refreshed at every ready advance and every resolve (the refresh runs
    // unconditionally before every batch — the initializer and the
    // re-anchor need no assignment of their own).
    let mut next_wb: u64;
    // STRAND C: the division-free grid cursors — the chunk containing
    // `eval` (chunk_lo == chunk_id * CHUNK, maintained by addition).
    let mut chunk_id: u64 = 0;
    let mut chunk_lo: u64 = 0;
    // The spray's own grid + resolution state (MUST stay independent of
    // the eval's — the spray walks ahead and would corrupt the eval's).
    let mut pf_gid: u64 = 0;
    let mut pf_line: usize = 0;
    let mut pf_chunk_id: u64 = 0;
    let mut pf_chunk_lo: u64 = 0;
    let mut pf_slot: u8 = 0;
    let mut pf_base: u64 = 0;
    let mut pf_wb: u64;
    let mut backoff: u32 = 0;

    // Per-span pass-record resolution (identical semantics to the
    // pre-diet worker's probe; the diet calls it per boundary crossing
    // instead of per span).
    #[inline(always)]
    fn resolve_rec(rx: &RxdescState, gen: u64, slot: &mut u8, base: &mut u64, gid: u64) {
        loop {
            let s = (*slot + 1) % RX_NARR as u8;
            let (rg, rb) = rx.read_record(s);
            if rg == gen && rb > *base && rb <= gid {
                *slot = s;
                *base = rb;
            } else {
                return;
            }
        }
    }

    /// STRAND B: the next boundary beyond the CURRENT record — the next
    /// window's base if its record is visible, u64::MAX otherwise (the
    /// soundness proof is in the function doc above).
    #[inline(always)]
    fn probe_wb(rx: &RxdescState, gen: u64, slot: u8, base: u64) -> u64 {
        let s = (slot + 1) % RX_NARR as u8;
        let (rg, rb) = rx.read_record(s);
        if rg == gen && rb > base {
            rb
        } else {
            u64::MAX
        }
    }

    loop {
        let (g, mut ready) = rx.load_ready();
        if g != gen {
            // Fresh generation: re-anchor (the pre-diet worker's protocol,
            // plus the diet's grid/boundary cursors).
            gen = g;
            eval = lane_idx * CHUNK;
            rec_base = 0;
            blob_base = 0;
            chunk_id = lane_idx;
            chunk_lo = eval;
            // (next_wb / pf_wb need no reset: the ready-advance refresh
            // below overwrites both before any read.)
            let mut found = false;
            for s in 0..RX_NARR as u8 {
                let (rg, rb) = rx.read_record(s);
                if rg == gen && rb == 0 {
                    rec_slot = s;
                    found = true;
                    break;
                }
            }
            if !found {
                // A (gen, ready>0) publication implies the base-0 record
                // exists; ready==0 with a new gen means the sink has not
                // opened a window yet — wait for it.
                if ready == 0 {
                    if shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    stats.idle_iters.fetch_add(1, Ordering::Relaxed);
                    crate::affinity::polite_spin(&mut backoff);
                    continue;
                }
                // ready > 0 without the record is a protocol violation —
                // fail loud, never fold wrong descriptors.
                panic!("rxdesc worker: generation {gen} has spans but no base-0 record");
            }
            pf_gid = eval;
            pf_line = 0;
            pf_chunk_id = lane_idx;
            pf_chunk_lo = eval;
            pf_slot = rec_slot;
            pf_base = 0;
            backoff = 0;
        }
        if blob_base == 0 {
            // Ordered before the first ready Release of this generation —
            // but NEVER dereference a null base: spin until it lands.
            blob_base = rx.blob_base();
            if blob_base == 0 {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                stats.idle_iters.fetch_add(1, Ordering::Relaxed);
                crate::affinity::polite_spin(&mut backoff);
                continue;
            }
        }
        if eval >= ready {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            stats.idle_iters.fetch_add(1, Ordering::Relaxed);
            // The deep pause now serves ONLY true idleness (the batch's
            // bounded frontier wait handles publication gaps hot).
            crate::affinity::polite_spin_deep(&mut backoff);
            continue;
        }
        backoff = 0;
        // STRAND B: refresh the boundary caches at every ready advance —
        // a boundary below `ready` was published before the ready store
        // that exposed it, so the probes see it (the function-doc proof).
        next_wb = probe_wb(&rx, gen, rec_slot, rec_base);
        pf_wb = probe_wb(&rx, gen, pf_slot, pf_base);
        let mut n_total: u64 = 0;
        let t_eval = std::time::Instant::now();
        let mut wait_ns: u64 = 0;
        // Result-space check: once per batch for the FULL wbatch (the
        // same bound class as the pre-diet worker; partial publishes
        // mid-batch only shrink the outstanding reservation).
        {
            let mut rb = 0u32;
            loop {
                let rt = lane.res_tail.load(Ordering::Acquire);
                if rhead.saturating_sub(rt) + wbatch <= (RES_CAP as u64) - CHUNK {
                    break;
                }
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                stats.res_waits.fetch_add(1, Ordering::Relaxed);
                crate::affinity::polite_spin(&mut rb);
            }
        }
        // Evaluate + buffer the batch's results, then publish with ONE
        // Release store (slot writes stay invisible until the store).
        // SAFETY: res slots in [rhead, rhead+n_total) are owned by this
        // worker (the space check above bounded the batch).
        let res_slots = lane.res_slots();
        let mut nres: u64 = 0;
        'batch: while n_total < wbatch {
            if eval >= ready {
                // STRAND A — the frontier hit: publish the partial run
                // FIRST (liveness), then wait at the frontier hot.
                if nres > 0 {
                    std::hint::black_box(&res_slots[(rhead & RES_MASK) as usize]);
                    // SAFETY: slots [rhead, rhead+nres) fully written
                    // before this Release store; the fold Acquires it.
                    lane.res_head.store(rhead + nres, Ordering::Release);
                    rhead += nres;
                    nres = 0;
                }
                stats.frontier_waits.fetch_add(1, Ordering::Relaxed);
                let t_wait = std::time::Instant::now();
                let mut fw: u32 = 0;
                let mut bailed = false;
                while eval >= ready {
                    let (g2, r2) = rx.load_ready();
                    if g2 != gen {
                        // A fresh generation landed mid-batch: bail to the
                        // outer loop's re-anchor. The partial results are
                        // old-gen — the old sink's finish() drain consumes
                        // them (the standing fabric contract).
                        bailed = true;
                        break;
                    }
                    if r2 > eval {
                        ready = r2;
                        // STRAND B: the ready-advance refresh.
                        next_wb = probe_wb(&rx, gen, rec_slot, rec_base);
                        pf_wb = probe_wb(&rx, gen, pf_slot, pf_base);
                        break;
                    }
                    if shutdown.load(Ordering::Acquire) {
                        return; // nothing unpublished (published above)
                    }
                    fw += 1;
                    if fw > flaps {
                        // True idleness (a stalled RX, an EOS tail) — or
                        // strand A disarmed (flaps == 0): hand the wait to
                        // the outer loop's deep pause.
                        bailed = true;
                        break;
                    }
                    let n = 1u32 << fw.min(7);
                    for _ in 0..n {
                        std::hint::spin_loop();
                    }
                }
                wait_ns = wait_ns.saturating_add(t_wait.elapsed().as_nanos() as u64);
                if bailed {
                    break 'batch;
                }
                continue 'batch;
            }
            // The chunk section — grid-aligned FIRST entry only (a
            // mid-chunk resume after a frontier hit skips it: the record
            // state is carried and stays valid for the rest of the chunk
            // up to the next boundary, which the per-span compare guards).
            if eval == chunk_lo {
                debug_assert_eq!(
                    chunk_id % n_lanes,
                    lane_idx,
                    "eval cursor left the lane grid"
                );
                // The inline-claim check at the chunk's first entry (the
                // claim store precedes the exposing spans_ready Release).
                // The claim is decided at the chunk's open, before any of
                // its spans are published — by the time we can see the
                // chunk, its state byte is final.
                if rx.is_inline(chunk_id) {
                    eval = chunk_lo + n_lanes * CHUNK;
                    chunk_id += n_lanes;
                    chunk_lo += n_lanes * CHUNK;
                    continue 'batch;
                }
            }
            let chunk_hi = (chunk_lo + CHUNK).min(ready);
            let mut gid = eval;
            let mut idx = (eval - rec_base) as usize;
            while gid < chunk_hi && n_total < wbatch {
                // STRAND B: the boundary crossing — one register compare
                // per span; the resolve (with re-probe) only at crossings.
                if gid >= next_wb {
                    resolve_rec(&rx, gen, &mut rec_slot, &mut rec_base, gid);
                    next_wb = probe_wb(&rx, gen, rec_slot, rec_base);
                    idx = (gid - rec_base) as usize;
                }
                // STRAND C: the spray — per-chunk sections, division-free.
                if pf.lines > 0 && pf.burst > 0 {
                    if pf_gid < gid {
                        // Rare realignment (the spray fell behind the eval).
                        pf_gid = gid;
                        pf_line = 0;
                        pf_slot = rec_slot;
                        pf_base = rec_base;
                        pf_wb = next_wb;
                        pf_chunk_id = chunk_id;
                        pf_chunk_lo = chunk_lo;
                    }
                    let target = gid + pf.ahead + 1;
                    let mut issued = 0usize;
                    while pf_gid < target && pf_gid < ready && issued < pf.burst {
                        // The spray's grid advance (a `while` — the inline
                        // JUMP below also lands exactly on a this-lane
                        // chunk start; the advance is idempotent).
                        while pf_gid >= pf_chunk_lo + CHUNK {
                            pf_chunk_id += n_lanes;
                            pf_chunk_lo += n_lanes * CHUNK;
                        }
                        if pf_gid == pf_chunk_lo {
                            // First touch of this pf chunk: the inline
                            // claim, once per chunk (the claim store
                            // precedes the exposing spans_ready Release).
                            if rx.is_inline(pf_chunk_id) {
                                pf_gid = pf_chunk_lo + n_lanes * CHUNK;
                                pf_line = 0;
                                continue;
                            }
                        }
                        if pf_gid >= pf_wb {
                            resolve_rec(&rx, gen, &mut pf_slot, &mut pf_base, pf_gid);
                            pf_wb = probe_wb(&rx, gen, pf_slot, pf_base);
                        }
                        let w = rx.get_arr(pf_slot, (pf_gid - pf_base) as usize);
                        let (off, dlen) = rxdesc_unpack_span(w);
                        let dptr = blob_base.wrapping_add(off as u64);
                        let span_lines = (((dlen as usize) + 63) >> 6).min(pf.lines);
                        let end = span_lines.min(pf_line + (pf.burst - issued));
                        // SAFETY: prefetch never faults and never
                        // dereferences; the entry is published (below
                        // ready).
                        for l in pf_line..end {
                            prefetch_line(dptr as *const u8, l);
                        }
                        issued += end - pf_line;
                        if end >= span_lines {
                            pf_gid += 1;
                            pf_line = 0;
                        } else {
                            pf_line = end;
                        }
                    }
                }
                let w = rx.get_arr(rec_slot, idx);
                let (off, dlen) = rxdesc_unpack_span(w);
                // SAFETY: the entry was published by the sink's ready
                // store (Acquire above); the body slice per the
                // HydraLane contract — immutable bytes, valid until the
                // owning pass's drain.
                let value = if null {
                    // Diagnostic: constant work, wrong value by design.
                    (dlen as u64) | (gid << 32)
                } else {
                    let body = unsafe {
                        std::slice::from_raw_parts(
                            blob_base.wrapping_add(off as u64) as *const u8,
                            dlen as usize,
                        )
                    };
                    // SAFETY: feature contract verified at spawn.
                    unsafe { kernel.eval(body) }
                };
                debug_assert!(gid <= u32::MAX as u64, "span id exceeds u32");
                res_slots[((rhead + nres) & RES_MASK) as usize] = Res {
                    span_id: gid as u32,
                    _pad: 0,
                    value,
                };
                nres += 1;
                gid += 1;
                n_total += 1;
                idx += 1;
            }
            // Advance: a batch-limited stop mid-chunk resumes IN PLACE; a
            // fully-consumed chunk moves to this lane's NEXT grid chunk
            // (the other lanes' chunks in between are never this worker's
            // to evaluate); a ready-limited partial chunk resumes at
            // chunk_hi (== ready, which grows before the next pass over
            // it). The cursors advance only on full consumption.
            if gid < chunk_hi {
                eval = gid;
            } else if chunk_hi == chunk_lo + CHUNK {
                eval = chunk_lo + n_lanes * CHUNK;
                chunk_id += n_lanes;
                chunk_lo += n_lanes * CHUNK;
            } else {
                eval = chunk_hi;
            }
        }
        std::hint::black_box(&res_slots[(rhead & RES_MASK) as usize]);
        stats.spans.fetch_add(n_total, Ordering::Relaxed);
        stats.batches.fetch_add(1, Ordering::Relaxed);
        stats.eval_ns.fetch_add(
            (t_eval.elapsed().as_nanos() as u64).saturating_sub(wait_ns),
            Ordering::Relaxed,
        );
        stats.frontier_ns.fetch_add(wait_ns, Ordering::Relaxed);
        // SAFETY: slots [rhead, rhead+nres) fully written before this
        // Release store; the fold Acquires it and owns them after.
        if nres > 0 {
            lane.res_head.store(rhead + nres, Ordering::Release);
            rhead += nres;
        }
    }
}

/// The parallel verification fabric: N lanes + N worker threads + a
/// shutdown flag. Construct ONCE (startup), reused across every benchmark
/// pass; dropped (joining workers) after the last pass.
pub struct HydraFabric {
    lanes: Vec<Arc<HydraLane>>,
    shutdown: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    /// R8 phase-2: per-worker telemetry (parallel to lanes).
    wstats: Vec<Arc<WorkerStats>>,
    /// Worker cpu assignments (diagnostics).
    worker_cpus: Vec<usize>,
    pub workers: usize,
    /// GIGAHFT Lever 1: the span-CRC kernel the workers evaluate (detected
    /// ONCE here — outside every measurement window; values are bit-exact
    /// across kernels by D11, so this choice affects speed only).
    pub kernel: CrcKernel,
    /// R12: the descriptor format for this fabric's life — compact 8-byte
    /// Desc8 (default; `HFT_DESC8=0` is the rollback, CI arm 11n). The
    /// submitting sink and every worker read THIS flag, so one run never
    /// mixes formats.
    pub desc8: bool,
    /// R16b/R17: the shared rxdesc state (the array-driven submission
    /// path — `HFT_RXDESC=1` ARMS it, CI arm 11w; None (the default) is
    /// the per-lane descriptor rings). R17 ruling (senior roadmaps 1–3,
    /// unanimous): the fleet priced rxdesc at −10…−21% vs the ring across
    /// both silicon classes (draws 10–15; residue supply-coupled), so the
    /// sustained default is the RING + distinct placement; rxdesc stays
    /// merged as the armed attribution arm. Per-run constant, like `desc8`.
    pub rxdesc: Option<Arc<RxdescState>>,
}

impl HydraFabric {
    /// Spawn a fabric with `workers` lanes (0 → degenerate: callers then use
    /// [`HydraSpanSink::new_inline`], which is the sequential-equivalent
    /// code path). All allocation and thread spawn happens here — outside
    /// every measurement window.
    pub fn spawn(workers: usize) -> Box<Self> {
        Self::spawn_pinned(workers, &[])
    }

    /// R8: `spawn` + worker affinity — worker `i` pins itself to the
    /// ABSOLUTE cpu `worker_cpus[i % len]`. The caller captures the
    /// topology order BEFORE pinning its own thread (threads inherit the
    /// creator's restricted mask). Failures leave the thread unpinned,
    /// gracefully. The doc-21 "worker core affinity" lever: unpinned
    /// workers migrate and stack on SMT siblings on shared cloud runners.
    pub fn spawn_pinned(workers: usize, worker_cpus: &[usize]) -> Box<Self> {
        // R12: the descriptor format is a per-RUN constant (the sink and the
        // workers must agree; the env is read once, here, outside every
        // window). Default: compact Desc8 ON.
        let desc8 = std::env::var("HFT_DESC8").as_deref() != Ok("0");
        // R16b/R17: the array-driven submission path (requires desc8 — the
        // HFT_DESC8=0 rollback implies the pre-R12 ring world). R17: the
        // default is the RING (the fleet verdict — draws 10–15 priced the
        // arrays at −10…−21% on both classes, supply-coupled residue);
        // `HFT_RXDESC=1` arms the arrays (CI arm 11w, the attribution
        // instrument).
        let rxdesc = desc8 && std::env::var("HFT_RXDESC").as_deref() == Ok("1");
        // R16e: the RX Desc Diet — the draw-11 worker-eval fix. The default
        // IS the diet; `HFT_RXDIET=0` is the rollback (CI arm 11x — the
        // pre-diet rxdesc worker verbatim). Read once here, outside every
        // window (law #9: env parsing is allocation).
        let diet = std::env::var("HFT_RXDIET").as_deref() != Ok("0");
        Self::spawn_pinned_full(workers, worker_cpus, desc8, rxdesc, diet)
    }

    /// R12: `spawn_pinned` with an explicit descriptor format (the
    /// in-process form serves the both-format parity tests; the env form
    /// serves CI sweeps). The R16b rxdesc path stays OFF in this form —
    /// the legacy ring path is what the existing parity suite pins.
    pub fn spawn_pinned_desc8(workers: usize, worker_cpus: &[usize], desc8: bool) -> Box<Self> {
        Self::spawn_pinned_full(workers, worker_cpus, desc8, false, true)
    }

    /// R16b: the full-control spawn (descriptor format + submission path).
    /// R16e: + the diet flag (the rxdesc worker-eval shape; `false` = the
    /// pre-diet worker verbatim — the parity suite pins both shapes).
    /// All allocation (rings, arrays, threads) happens here — outside
    /// every measurement window.
    pub fn spawn_pinned_full(
        workers: usize,
        worker_cpus: &[usize],
        desc8: bool,
        rxdesc: bool,
        diet: bool,
    ) -> Box<Self> {
        let kernel = CrcKernel::detect();
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(workers);
        let mut lanes: Vec<Arc<HydraLane>> = Vec::with_capacity(workers);
        let mut wstats: Vec<Arc<WorkerStats>> = Vec::with_capacity(workers);
        for _ in 0..workers {
            // Box→Arc: single heap object, ownership moved (leak-free).
            lanes.push(Arc::from(HydraLane::new()));
            wstats.push(Arc::new(WorkerStats::new()));
        }
        let rxdesc_state = rxdesc.then(|| {
            Arc::new(RxdescState::new(
                std::env::var("HFT_RXDESC_CAP")
                    .ok()
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(nf_transport::rxdesc::RX_CAP_DEFAULT)
                    .clamp(1024, 1 << 24),
            ))
        });
        for i in 0..lanes.len() {
            let lane = lanes[i].clone();
            let sd = shutdown.clone();
            let kern = kernel;
            let stats = wstats[i].clone();
            let cpu = worker_cpus.get(i % worker_cpus.len().max(1)).copied();
            let fmt8 = desc8;
            let rxs = rxdesc_state.clone();
            let lane_idx = i as u64;
            let n_lanes = lanes.len() as u64;
            let h = std::thread::Builder::new()
                .stack_size(512 * 1024)
                .name("hydra-worker".to_string())
                .spawn(move || {
                    if let Some(c) = cpu {
                        let _ = crate::affinity::pin_current_to(c);
                    }
                    match rxs {
                        Some(rx) => {
                            if diet {
                                lane_worker_rxdesc_diet(
                                    lane, sd, kern, stats, rx, lane_idx, n_lanes,
                                )
                            } else {
                                lane_worker_rxdesc(lane, sd, kern, stats, rx, lane_idx, n_lanes)
                            }
                        }
                        None => lane_worker(lane, sd, kern, stats, fmt8),
                    }
                })
                .expect("hydra worker spawn");
            handles.push(h);
        }
        Box::new(Self {
            lanes,
            shutdown,
            handles,
            wstats,
            worker_cpus: worker_cpus.to_vec(),
            workers,
            kernel,
            desc8,
            rxdesc: rxdesc_state,
        })
    }

    /// R16b: the fabric's rxdesc state (attach to the pipelined transport
    /// via `set_rxdesc` to arm the RX-side prefill; None in ring mode).
    pub fn rxdesc_state(&self) -> Option<Arc<RxdescState>> {
        self.rxdesc.clone()
    }

    /// R8 phase-2 diagnostics: one always-on telemetry line per run covering
    /// the fabric's life (worker utilization, CRC time, idle spins, pins).
    /// Read-only, post-run — never inside a measurement window's decision
    /// path.
    pub fn diag_summary(&self, label: &str) {
        for (i, ws) in self.wstats.iter().enumerate() {
            let cpu = ws.cpu.load(Ordering::Relaxed);
            let cpu_disp = if cpu == u64::MAX {
                "?".to_string()
            } else {
                cpu.to_string()
            };
            let want = self.worker_cpus.get(i).copied();
            let pin = match want {
                Some(w) if w as u64 == cpu => format!("cpu{cpu_disp}(pinned)"),
                Some(w) => format!("cpu{cpu_disp}(WANTED {w})"),
                None => format!("cpu{cpu_disp}(unpinned)"),
            };
            let placement = &self.worker_cpus;
            eprintln!(
                "DIAG worker[{i}] {label}: {pin} placement={placement:?} batches={} spans={} eval_ms={:.1} idle_iters={} res_waits={} fw={} fw_ms={:.1}",
                ws.batches.load(Ordering::Relaxed),
                ws.spans.load(Ordering::Relaxed),
                ws.eval_ns.load(Ordering::Relaxed) as f64 / 1e6,
                ws.idle_iters.load(Ordering::Relaxed),
                ws.res_waits.load(Ordering::Relaxed),
                ws.frontier_waits.load(Ordering::Relaxed),
                ws.frontier_ns.load(Ordering::Relaxed) as f64 / 1e6,
            );
        }
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
///
/// R8 phase-5 adds the main-core WORK-ASSIST to fabric mode: when the
/// lane rings are full (workers saturated), the submitting core evaluates
/// the chunk's spans itself instead of spinning (see InlineChunk).

/// One inline-assist chunk buffer (see the sink doc above). R10: the span
/// ids are implicit (`first_span + i`) — the ids array's store/load traffic
/// left the assist path with it.
#[derive(Clone, Copy)]
struct InlineChunk {
    /// First span id of the chunk (u64::MAX = free slot).
    first_span: u64,
    /// Span count in this (possibly pass-tail partial) chunk.
    len: u32,
    /// Sealed by flush_pending — an INCOMPLETE chunk's first_span is
    /// already set while its spans are still arriving, so the ordered fold
    /// must never match it (the mid-chunk drain_ready race that produced
    /// the original fold-order violation: the fold applied a partial len,
    /// freed the slot, and the chunk's remaining spans were folded by
    /// nobody).
    sealed: bool,
    /// Values, in emission order.
    vals: [u64; CHUNK as usize],
}

impl InlineChunk {
    const fn free() -> Self {
        Self {
            first_span: u64::MAX,
            len: 0,
            sealed: false,
            vals: [0; CHUNK as usize],
        }
    }
}

/// R10 — the deep assist ring. The R8 assist buffered a mere 4 chunks:
/// inline chunks sit at the SUBMIT point, far ahead of the fold cursor,
/// and can only fold once every worker-owned chunk before them has
/// returned — so a 4-deep ring clogs after ~256 assisted spans and the
/// submitting core falls back to the backpressure spin, burning exactly
/// the cycles the assist exists to convert. The measured equilibrium on
/// the Zen3 draw (CI 37005861779): main's work_ms=86% against a ~20%
/// pure-ingest duty — the difference was spin time behind saturated
/// workers, with assist_chunks at 5.4% and the workers' SMT sibling (RX)
/// starved 3x by the spin traffic. The deep ring (default 64 chunks =
/// 4096 assisted spans of lead) lets the submitting core keep converting
/// lane-full backpressure into in-window CRC for as long as the fold
/// lags, and `HFT_ASSIST_SLOTS` sweeps the depth per runner class.
const DEFAULT_ASSIST_SLOTS: usize = 64;

fn assist_slots_from_env() -> usize {
    std::env::var("HFT_ASSIST_SLOTS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_ASSIST_SLOTS)
        .clamp(4, 4096)
}

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
    /// GIGAHFT Lever 2: descriptors are written IN PLACE into the lane's
    /// ring slot at on_span time (single 128-bit store — no staging buffer,
    /// no flush copy loop). `pending_len` counts slots of the current
    /// chunk already written but not yet published.
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
    // ── R8 phase-5: main-core work-assist ──
    /// The kernel the inline evaluations run (the fabric's — bit-exact
    /// across kernels by D11, so main and workers can mix freely).
    kernel: CrcKernel,
    /// R10: the deep assist ring — `inline_ring[i % assist_slots]` holds
    /// inline chunk #i (claim order == submission order == fold order, so
    /// the ring is indexed by two monotone counters and the fold's lookup
    /// is O(1)).
    inline_ring: Vec<InlineChunk>,
    /// Assist ring depth (HFT_ASSIST_SLOTS; construction-time).
    assist_slots: usize,
    /// Inline chunks claimed so far (next claim's sequence number).
    inline_seq: u64,
    /// Inline chunks folded so far (oldest unfolded chunk's sequence).
    inline_fold_seq: u64,
    /// Ring slot of the chunk CURRENTLY being filled (None = lane mode).
    cur_inline: Option<usize>,
    /// Force every chunk inline (diagnostics + parity tests: a fully
    /// deterministic inline-mode run must produce the identical tuple).
    force_inline: bool,
    /// Chunks taken through the assist path (telemetry).
    assist_chunks: u64,
    // ── R12: compact Desc8 state ──
    /// The fabric's descriptor format (per-run constant).
    desc8: bool,
    /// This sink's blob base — the FIRST submitted body's pointer (0 =
    /// unset). All span bodies of a run live in one contiguous blob, so
    /// every later body's Desc8 offset is `ptr - blob_base` (u32-ranged).
    blob_base: usize,
    // ── GIGAHFT Lever 4: cross-pass double buffering ──
    /// Span ids are GLOBAL across the sink's life; each pass records its
    /// boundary so the ordered fold snapshots the pass's hash exactly at
    /// the boundary and resets the chain to SPAN_SEED. Pass N+1's
    /// submission overlaps pass N's residual worker tail — the fabric
    /// never drains mid-run; only the harness's final `finish()` blocks.
    passes: [PassRec; PASS_RING],
    /// Next record slot (written by begin_pass/end_pass).
    pass_head: usize,
    /// Oldest record not yet folded past its boundary.
    pass_tail: usize,
    /// Oldest record not yet harvested by the harness.
    harvest_pos: usize,
    /// A pass is open (begin_pass called, end_pass pending).
    pass_open: bool,
    // ── R16b: the rxdesc submission state ──
    /// The fabric's rxdesc state (None = the legacy ring path — the
    /// per-run constant from the fabric).
    rx: Option<Arc<RxdescState>>,
    /// This sink's generation (workers re-anchor spans to 0 on change).
    rx_gen: u64,
    /// A submission window is open (its pass record is published).
    rx_win_open: bool,
    /// The open window's array slot.
    rx_win_slot: u8,
    /// The open window's first span id (global within the generation).
    rx_win_base: u64,
    /// This sink's per-slot window-end spans (u64::MAX = never used by
    /// this sink) — the array reuse gate.
    rx_slot_end: [u64; RX_NARR],
    /// The inline-claim marks were cleared for this sink's generation
    /// (at its FIRST window open — the activation; see
    /// `RxdescState::clear_chunk_states`).
    rx_marks_cleared: bool,
    /// Check-and-fix corrections applied (telemetry).
    rx_fixes: u64,
    /// The assist watermark: chunks go inline when pending spans exceed
    /// this (HFT_ASSIST_WATERMARK; 0 = never; force_inline overrides).
    rx_assist_wm: u64,
}

impl<'a> HydraSpanSink<'a> {
    pub const SPAN_SEED: u64 = 0xcbf29ce484222325;

    /// R16b: the hard pending pace (spans). The ring protocol's combined
    /// ring capacities bounded its in-flight lead at ~6k spans/lane; the
    /// array protocol's equivalent (the res rings + the assist ring) is
    /// ~8k, and the 8-window array gate sits far beyond it — this pace
    /// restores the ring's flow-control shape (fold near the submit
    /// point) without restoring the per-span submission cost.
    const RX_PACE: u64 = 8192;

    /// R16b: the assist watermark default. The ring protocol's assist
    /// fired on lane-ring fullness (deep saturation); the array protocol
    /// has no rings to fill, so the trigger is the submission lead
    /// (pending spans = submitted − folded). 2048 sits just under the
    /// res-ring stall regime (~8k results across 2 lanes) — the assist
    /// engages only when the workers are truly saturated, converting the
    /// submitting core's otherwise-idle cycles into in-window CRC
    /// (displacement of the sibling worker's p5 ports is the cost — the
    /// fleet prices it per draw via HFT_ASSIST_WATERMARK sweeps).
    fn assist_watermark() -> u64 {
        std::env::var("HFT_ASSIST_WATERMARK")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(2048)
    }

    fn blank(fabric: Option<&'a HydraFabric>) -> Self {
        // R16b: the submission path + generation are per-SINK constants
        // (construction time — outside every window; a fresh generation
        // re-anchors the workers' span grid to 0).
        let rx = fabric.and_then(|f| f.rxdesc.clone());
        let rx_gen = rx.as_ref().map_or(0, |st| st.next_gen());
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
            pending_len: 0,
            pending_lane: 0,
            pending_head: 0,
            n_lanes: fabric.map(|f| f.lanes.len()).unwrap_or(0),
            // Chunk 0 → lane 0; advance on chunk completion (after-use).
            submit_lane: 0,
            submit_rem: CHUNK,
            fold_lane: 0,
            fold_rem: CHUNK,
            kernel: fabric.map(|f| f.kernel).unwrap_or(CrcKernel::Scalar),
            // R10: the deep assist ring (env-sized at construction — sinks
            // are built outside every measurement window).
            assist_slots: assist_slots_from_env(),
            inline_ring: (0..assist_slots_from_env())
                .map(|_| InlineChunk::free())
                .collect(),
            inline_seq: 0,
            inline_fold_seq: 0,
            cur_inline: None,
            assist_chunks: 0,
            desc8: fabric.map(|f| f.desc8).unwrap_or(false),
            blob_base: 0,
            force_inline: std::env::var("HFT_INLINE_FORCE").as_deref() == Ok("1"),
            passes: [PassRec {
                end_span: 0,
                count: 0,
                msg_hash: 0,
                hash: 0,
                done: false,
            }; PASS_RING],
            pass_head: 0,
            pass_tail: 0,
            harvest_pos: 0,
            pass_open: false,
            rx,
            rx_gen,
            rx_win_open: false,
            rx_win_slot: 0,
            rx_win_base: 0,
            rx_slot_end: [u64::MAX; RX_NARR],
            rx_marks_cleared: false,
            rx_fixes: 0,
            rx_assist_wm: Self::assist_watermark(),
        }
    }

    pub fn new(fabric: &'a HydraFabric) -> Self {
        Self::blank(Some(fabric))
    }

    /// R8 phase-5: force every chunk through the inline (main-core) path —
    /// the deterministic assist-mode parity configuration (the env var
    //  form serves CI diagnostics; this form serves in-process tests).
    pub fn force_inline_mode(&mut self) {
        self.force_inline = true;
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
        // R8 phase-5: the inline ring must be fully folded (fold_pos ==
        // next_span proves it); clear the window state defensively.
        // R10: rebuild the deep ring's slots (depth is fixed at
        // construction; only the contents reset).
        self.inline_seq = 0;
        self.inline_fold_seq = 0;
        for c in self.inline_ring.iter_mut() {
            *c = InlineChunk::free();
        }
        self.cur_inline = None;
        // R12 Desc8: no reset handshake needed — the fresh sink's first
        // grid-aligned chunk-open writes an ANCHOR desc (first span 0),
        // re-anchoring every worker's derivation absolutely.
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
        // R16b: reset = a fresh generation (spans restart at 0; the
        // workers re-anchor). The old generation drained (asserted above),
        // so every array slot is free for the new one.
        if let Some(st) = &self.rx {
            self.rx_gen = st.next_gen();
        }
        self.rx_win_open = false;
        self.rx_marks_cleared = false;
        self.rx_slot_end = [u64::MAX; RX_NARR];
        // Chunk 0 → lane 0; advance on chunk completion (after-use).
        self.submit_rem = CHUNK;
        self.fold_rem = CHUNK;
        self.submit_lane = 0;
        self.fold_lane = 0;
        self.pass_head = 0;
        self.pass_tail = 0;
        self.harvest_pos = 0;
        self.pass_open = false;
    }

    /// GIGAHFT Lever 4: open a new pass on the SAME fabric connection —
    /// span ids stay GLOBAL, the fold chain keeps draining the previous
    /// pass's residual tail, and per-pass observable state resets. NEVER
    /// blocks on workers (that is the point: pass N+1's submission
    /// overlaps pass N's tail fold).
    pub fn begin_pass(&mut self) {
        assert!(!self.pass_open, "begin_pass while a pass is open");
        // Ring capacity: one slot must remain free for this record.
        assert_ne!(
            (self.pass_head + 1) % PASS_RING,
            self.harvest_pos,
            "pass record ring exhausted (harness not harvesting)"
        );
        // NOTE: `self.hash` (the in-flight fold chain) is intentionally NOT
        // reset here — the previous pass's residual tail is still folding
        // into it; the boundary crossing snapshots it and resets to
        // SPAN_SEED at exactly the right value. Per-pass hashes come from
        // `harvest_completed`; the field is only directly observable after
        // a full `finish()` (no boundaries pending).
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
        // NOTE: next_span / fold_pos / chunk trackers / fold chain are
        // intentionally NOT reset — global continuity is what lets the
        // fold cross the pass boundary late without stalling submission.
        // (Inline mode has fold_pos == next_span; the chain is already at
        // SPAN_SEED after the previous end_pass.)
        self.passes[self.pass_head] = PassRec {
            end_span: u64::MAX,
            count: 0,
            msg_hash: 0,
            hash: 0,
            done: false,
        };
        self.pass_head = (self.pass_head + 1) % PASS_RING;
        self.pass_open = true;
        // R16b: open the rxdesc submission window EAGERLY — the window's
        // base span id is known here, and publishing the record before
        // any span means the workers never wait on one. (The burst shape
        // — no begin_pass — opens lazily at the first submit.)
        if self.rx.is_some() && !self.rx_win_open {
            self.rx_open_window();
        }
    }

    /// Close the current pass: publish any partial chunk (one non-blocking
    /// Release store — the workers can start the tail while the main core
    /// arbitrates the next pass) and record the boundary. The pass's
    /// (count, hash, msg_hash) tuple becomes harvestable once the ordered
    /// fold crosses the boundary — typically during the NEXT pass.
    pub fn end_pass(&mut self) {
        assert!(self.pass_open, "end_pass without begin_pass");
        if self.fabric.is_some() {
            self.flush_pending();
        } else {
            // Inline mode folds eagerly: complete the record right now.
            debug_assert_eq!(self.fold_pos, self.next_span);
        }
        let idx = (self.pass_head + PASS_RING - 1) % PASS_RING;
        let rec = &mut self.passes[idx];
        rec.end_span = self.next_span;
        rec.count = self.count;
        rec.msg_hash = self.msg_hash;
        self.pass_open = false;
        // R16b: close the window (record its end for the reuse gate and
        // its span count for the next window's warm start) and publish
        // the tail's spans — the workers can finish the partial chunk
        // while the main core arbitrates the next pass.
        if self.rx_win_open {
            self.rx_slot_end[self.rx_win_slot as usize] = self.next_span;
            if let Some(st) = &self.rx {
                st.set_last_window_count(self.next_span - self.rx_win_base);
            }
            self.rx_win_open = false;
        }
        self.rx_publish_ready();
        self.complete_boundaries();
    }

    /// Snapshot + reset at every pass boundary the fold has crossed.
    /// The chain value at the crossing IS the completed pass's hash (the
    /// fold applies values in strict global span order, and the boundary
    /// sits exactly at the pass's final span + 1).
    fn complete_boundaries(&mut self) {
        while self.pass_tail != self.pass_head {
            let rec = &self.passes[self.pass_tail];
            if rec.done || rec.end_span == u64::MAX {
                break;
            }
            if rec.end_span <= self.fold_pos {
                let h = self.hash;
                let rec = &mut self.passes[self.pass_tail];
                rec.hash = h;
                rec.done = true;
                // The next pass's chain starts from a fresh seed.
                self.hash = Self::SPAN_SEED;
                self.pass_tail = (self.pass_tail + 1) % PASS_RING;
            } else {
                break;
            }
        }
    }

    /// Harvest completed passes in order (oldest first). Returns the
    /// number of (count, hash, msg_hash) tuples written to `out`.
    pub fn harvest_completed(&mut self, out: &mut [(u64, u64, u64); PASS_RING]) -> usize {
        let mut k = 0usize;
        while self.harvest_pos != self.pass_head && k < out.len() {
            let rec = &self.passes[self.harvest_pos];
            if !rec.done {
                break;
            }
            out[k] = (rec.count, rec.hash, rec.msg_hash);
            self.harvest_pos = (self.harvest_pos + 1) % PASS_RING;
            k += 1;
        }
        k
    }

    /// The per-span fold — EXACTLY `SpanConformanceSink::on_span`'s mix.
    #[inline(always)]
    fn fold_value(&mut self, v: u64) {
        self.hash = self.hash.rotate_left(13) ^ v;
        self.hash = self.hash.wrapping_mul(0x9e3779b97f4a7c15);
    }

    /// R16b: publish the span-count cursor — the ONE Release store that
    /// makes the window's array entries (RX prefill + this sink's fixes)
    /// and inline-chunk claims visible to the workers. Called at
    /// submission-batch granularity (on_span_batch / on_span flushes,
    /// end_pass, finish).
    #[inline]
    fn rx_publish_ready(&mut self) {
        if let Some(st) = &self.rx {
            st.publish_ready(self.rx_gen, self.next_span);
        }
    }

    /// R16b: open a submission window — derive the array slot
    /// (`last_slot + 1`, the single sink-driven source), gate on this
    /// sink's earlier use of the slot (its spans must be folded before
    /// the slot is overwritten), WARM-START the slot from the previous
    /// window's entries (the schedule is deterministic — the same span
    /// sequence replays every pass; the copy makes the check find every
    /// entry already correct), then publish the pass record.
    #[inline(never)]
    fn rx_open_window(&mut self) {
        let st = self
            .rx
            .clone()
            .expect("rx_open_window without rxdesc state");
        let slot = st.next_slot();
        // Generation activation: chunk ids restart at 0, so the previous
        // generation's inline marks would alias this one's chunks — clear
        // them NOW (the first window open of the generation; the previous
        // generation drained, so no live claim is erased). The clear
        // precedes the record publish and every spans_ready store of this
        // generation, which order it into the workers' gen-change
        // Acquire.
        if !self.rx_marks_cleared {
            st.clear_chunk_states();
            self.rx_marks_cleared = true;
        }
        // Reuse gate: this sink's earlier window on `slot` must be folded
        // (cross-sink reuse is free — a fresh sink implies the previous
        // one drained, the standing fabric contract).
        let end = self.rx_slot_end[slot as usize];
        if end != u64::MAX {
            let mut sb = 0u32;
            while self.fold_pos < end {
                self.fold_available();
                crate::affinity::polite_spin(&mut sb);
            }
        }
        // Warm start: copy the previous window's span-indexed entries
        // into this slot. Purely an optimization — the per-span
        // check-and-fix below still verifies EVERY entry against the
        // actual body; a wrong copy costs stores, never correctness.
        let prev_slot = (slot as usize + RX_NARR - 1) % RX_NARR;
        st.copy_arr(prev_slot as u8, slot, st.last_window_count());
        st.publish_record(slot, self.rx_gen, self.next_span);
        self.rx_win_slot = slot;
        self.rx_win_base = self.next_span;
        self.rx_win_open = true;
        // An inline-claimed chunk STRADDLING the window boundary: the
        // sealed partial inline entry covered the pre-boundary spans, but
        // the whole-chunk mark makes the worker skip the remainder too —
        // the continuation MUST go inline as well (a fresh inline-ring
        // entry continuing at next_span; the fold consumes it in claim
        // order, right after the sealed partial). The ring protocol
        // transferred such chunks back to the lane; the array protocol's
        // mark is whole-chunk, so it re-claims instead.
        if self.submit_rem != CHUNK {
            let chunk_id = self.next_span / CHUNK;
            if st.is_inline(chunk_id) {
                let mut sb = 0u32;
                while !self.try_claim_inline() {
                    self.fold_available();
                    crate::affinity::polite_spin(&mut sb);
                }
                self.assist_chunks += 1;
            }
        }
    }

    /// Publish the in-place-written descriptors: ONE Release store (Lever 2
    /// — the slots were already written by `submit_span` directly into the
    /// lane's ring, kept L1-local on this core). Full chunks (CHUNK spans)
    /// keep the cursors chunk-aligned; the pass tail publishes a partial
    /// run — all slot indexing is ring-masked, so unaligned publishes are
    /// safe. The space check happened at chunk start (see `submit_span`).
    #[inline]
    fn flush_pending(&mut self) {
        let n = self.pending_len;
        if n == 0 {
            return;
        }
        // R8 phase-5: an inline chunk is already fully evaluated — its
        // values sit in the inline ring awaiting the ordered fold. Sealing
        // marks the chunk complete (the fold must never touch a chunk
        // whose spans are still arriving).
        if let Some(slot) = self.cur_inline.take() {
            self.inline_ring[slot].sealed = true;
            self.pending_len = 0;
            return;
        }
        let fabric = match self.fabric {
            Some(f) => f,
            None => unreachable!("flush_pending in inline mode"),
        };
        let lane = &fabric.lanes[self.pending_lane];
        // SAFETY: slots [h0 & MASK, +n) were producer-owned (space checked
        // at chunk start) and fully written before this Release store.
        let h0 = self.pending_head;
        lane.desc_head.store(h0 + n, Ordering::Release);
        self.pending_len = 0;
    }

    /// Submit a span descriptor: write the 16-byte (ptr, len, span_id)
    /// descriptor DIRECTLY into its ring slot with one 128-bit store
    /// (GIGAHFT Lever 2 — zero-copy, zero staging; the descriptor ring
    /// stays mapped in this core's L1). The chunk is published with ONE
    /// Release store when full. H6: lane tracking is incremental — no
    /// division in the hot path.
    ///
    /// R9 DSB discipline: the two cold paths — the chunk-open (once per
    /// CHUNK spans: space check, assist decision, backpressure spin with
    /// its fold_available call chain) and the assist evaluation (the
    /// inline kernel dispatch) — live in #[inline(never)] helpers so the
    /// steady loop's µop-cache footprint stays minimal. The R8 campaign
    /// measured a 25% scan regression from exactly this class of bloat
    /// (the cold_apply out-of-lining fixed the worst of it); the per-span
    /// hot path (the 128-bit store + tracker advance) is all that stays
    /// inlined.
    #[inline]
    fn submit_span(&mut self, body: &[u8]) {
        let fabric = match self.fabric {
            Some(f) => f,
            None => unreachable!("submit_span called in inline mode"),
        };
        // R12 Desc8: the run's blob base — the first submitted body's
        // pointer (write-once; every body lives in the same contiguous
        // THP-backed blob, so later offsets are positive and u32-ranged).
        // R16b: the capture ALSO publishes the base to the rxdesc state
        // (the workers resolve body pointers from it — MUST happen before
        // the first spans_ready Release, which this precedes).
        if self.blob_base == 0 {
            self.blob_base = body.as_ptr() as usize;
            if let Some(st) = &self.rx {
                st.capture_blob_base(self.blob_base);
            }
        }
        // R16b: the array-driven path (per-run constant; the branch is
        // predicted cold-taken or never).
        if self.rx.is_some() {
            self.submit_span_rx(body);
            return;
        }
        if self.pending_len == 0 {
            self.open_chunk(fabric);
        }
        if let Some(slot) = self.cur_inline {
            self.submit_inline_span(slot, body);
        } else {
            debug_assert_eq!(
                ((self.next_span / CHUNK) % self.n_lanes as u64) as usize,
                self.pending_lane,
                "hydra pending buffer crossed a lane boundary"
            );
            let mut pos = self.pending_head + self.pending_len;
            let lane = &fabric.lanes[self.pending_lane];
            if self.desc8 {
                // Chunk-open (always): ONE anchor desc first — the chunk's
                // absolute first span id; the space check reserved the +1
                // slot. Every lane chunk re-anchors the worker's derivation
                // (see open_chunk).
                if self.pending_len == 0 {
                    lane.desc_words()[(pos & DESC_MASK) as usize] =
                        desc8_pack_anchor(self.next_span as u32);
                    self.pending_len += 1;
                    pos += 1;
                }
                // Compact 8-byte span desc: offset | len. One aligned u64
                // store — 8 descs per L1 line.
                let off = (body.as_ptr() as usize).wrapping_sub(self.blob_base);
                debug_assert!(off <= u32::MAX as usize, "Desc8 offset overflows u32");
                debug_assert!(body.len() <= u16::MAX as usize, "Desc8 len overflows u16");
                // The word at pos & DESC_MASK is producer-owned (space
                // checked at chunk start) and unread by the worker until
                // the Release publish below.
                lane.desc_words()[(pos & DESC_MASK) as usize] =
                    desc8_pack_span(off as u32, body.len() as u16);
            } else {
                // Legacy 16-byte desc at words [2i, 2i+1]: one unaligned
                // 128-bit store (ptr | len<<64 | span_id<<96).
                // SAFETY: the slot at pos & DESC_MASK is producer-owned
                // (space checked at chunk start for the full chunk) and
                // unread by the worker until the Release publish below.
                let packed = (body.as_ptr() as u128)
                    | ((body.len() as u128) << 64)
                    | ((self.next_span as u32 as u128) << 96);
                unsafe {
                    let words = lane.desc_words();
                    let base = words.as_mut_ptr();
                    std::ptr::write_unaligned(
                        base.add(((pos & DESC_MASK) * 2) as usize) as *mut u128,
                        packed,
                    );
                }
            }
            self.pending_len += 1;
        }
        // Advance the division-free submit-chunk tracker (after-use: the
        // lane advances when the chunk it belongs to is complete).
        self.submit_rem -= 1;
        let chunk_done = self.submit_rem == 0;
        if chunk_done {
            self.submit_rem = CHUNK;
            self.submit_lane = if self.submit_lane + 1 == self.n_lanes {
                0
            } else {
                self.submit_lane + 1
            };
        }
        // GIGAHFT Lever 4: flush on GLOBAL chunk completion, not on
        // `pending_len == CHUNK` — a chunk split across a pass boundary
        // (partial publish at end_pass + continuation in the next pass)
        // has a short window, and the next chunk must open its own window
        // on its own lane with its own space reservation.
        if chunk_done {
            self.flush_pending();
        }
    }

    /// R16b: the ARRAY-DRIVEN submission (see nf_transport::rxdesc). The
    /// steady-state per-span cost is ONE 8-byte load + compare — the RX's
    /// prefill already wrote the correct descriptor, and the compare
    /// PROVES it (any divergence — duplicates, cold frames, gaps — fixes
    /// the entry in place, which is exactly the ring protocol's cost and
    /// semantics; the prefill is only the fast path, the check is the
    /// correctness). The chunk machinery shrinks to the assist decision
    /// and the tracker advance; descriptors, anchors and space checks
    /// leave the submitting core entirely.
    #[inline]
    fn submit_span_rx(&mut self, body: &[u8]) {
        let st = self
            .rx
            .clone()
            .expect("submit_span_rx without rxdesc state");
        // (The blob base was captured by submit_span's preamble — the
        // single write-once point, shared with the RX's capture.)
        // Lazy window open (the burst shape has no begin_pass; begin_pass
        // already opened eagerly in the sustained shape).
        if !self.rx_win_open {
            self.rx_open_window();
        }
        // R16b FLOW CONTROL — the ring protocol's desc-ring backpressure
        // kept the fold within ~one pass of the submit point (the R7
        // fabric-efficiency invariance); the array protocol has no rings,
        // so the pace is an explicit pending watermark. Beyond it, the
        // submitting core folds and waits (productive: the drain frees
        // the workers' res rings) — without this, the fold lags to the
        // 8-window array gate and every window open pays the gate spin
        // (measured: 61% of the run in reset_pass on the first local
        // shape). The assist watermark (lower) fires FIRST — the lead
        // converts to in-window CRC before the hard pace binds.
        {
            let mut sb = 0u32;
            while self.rx_assist_wm > 0 && self.pending() > Self::RX_PACE {
                self.fold_available();
                crate::affinity::polite_spin_deep(&mut sb);
            }
        }
        // Chunk-open: the assist decision (the ring protocol decided at
        // lane-fullness; here the trigger is the submission lead).
        if self.submit_rem == CHUNK {
            let chunk_id = self.next_span / CHUNK;
            let take_inline = if self.force_inline {
                // Parity-test mode: every chunk inline, fold until an
                // inline-ring slot frees (the existing force semantics).
                let mut sb = 0u32;
                while !self.try_claim_inline() {
                    self.fold_available();
                    crate::affinity::polite_spin(&mut sb);
                }
                true
            } else {
                self.rx_assist_wm > 0
                    && self.pending() > self.rx_assist_wm
                    && self.try_claim_inline()
            };
            if take_inline {
                self.assist_chunks += 1;
                // The claim MUST be visible before the spans_ready store
                // that exposes this chunk's spans (workers read it after
                // their ready Acquire).
                st.mark_inline(chunk_id);
            }
        }
        if let Some(slot) = self.cur_inline {
            // Inline chunk: evaluate NOW from the actual body (no array
            // involvement), buffer for the ordered fold — the existing
            // assist path verbatim.
            self.submit_inline_span(slot, body);
        } else {
            // Check-and-fix: prove the array entry equals the actual
            // span body's descriptor; fix on mismatch (divergent
            // schedules drift the frame index past the span index).
            let idx = (self.next_span - self.rx_win_base) as usize;
            assert!(
                idx < st.cap(),
                "rxdesc window exceeds array cap (HFT_RXDESC_CAP)"
            );
            debug_assert!(body.len() <= u16::MAX as usize, "Desc8 len overflows u16");
            let off = (body.as_ptr() as usize).wrapping_sub(self.blob_base);
            debug_assert!(off <= u32::MAX as usize, "Desc8 offset overflows u32");
            let w = desc8_pack_span(off as u32, body.len() as u16);
            if st.get_arr(self.rx_win_slot, idx) != w {
                st.set_arr(self.rx_win_slot, idx, w);
                self.rx_fixes += 1;
            }
        }
        // NOTE: next_span's increment stays with the CALLER (on_span /
        // on_span_batch) — the single increment point, exactly as the
        // ring path.
        // Division-free submit-chunk tracker (after-use advance).
        self.submit_rem -= 1;
        if self.submit_rem == 0 {
            self.submit_rem = CHUNK;
            self.submit_lane = if self.submit_lane + 1 == self.n_lanes {
                0
            } else {
                self.submit_lane + 1
            };
            // Chunk completion: seal an inline chunk (the ring protocol's
            // flush point; non-inline chunks have nothing to flush) and
            // PUBLISH — one spans_ready Release per chunk (the ring
            // protocol's per-chunk head store cadence: the worker's
            // consumption unit IS the chunk, so a finer publish only
            // bounces the polled line between the cores — the measured
            // coherence storm on the first 8370C draw).
            if self.cur_inline.is_some() {
                self.flush_pending();
            }
            self.rx_publish_ready();
        }
    }

    /// The chunk-open cold path (runs once per CHUNK spans): pick the lane,
    /// check space, and decide the work-assist. #[inline(never)] keeps the
    /// backpressure spin (and its fold_available call chain) out of the
    /// steady scan's µop-cache window.
    #[inline(never)]
    fn open_chunk(&mut self, fabric: &HydraFabric) {
        // Start a new chunk on the lane that owns this span id.
        self.pending_lane = self.submit_lane;
        let lane = &fabric.lanes[self.pending_lane];
        // R12 Desc8: EVERY lane chunk-open writes an anchor as its first
        // desc. Grid alignment is NOT sufficient: a mid-grid continuation
        // (after an end_pass partial flush) can take the lane path while
        // the interleaved spans went INLINE (assist) — the worker's running
        // derivation would continue from its stale position and drift by
        // exactly the diverted span count (the fold-order assert caught
        // this on the first 8573C draw: drift 32/112 = inline-taken spans).
        // An unconditional anchor means the worker NEVER extrapolates
        // across any chunk boundary.
        if self.desc8 && self.blob_base != 0 {
            // Write-once-per-lane blob base (ordered before this chunk's
            // publish; the worker reads it after its desc_head Acquire).
            if lane.base.load(Ordering::Relaxed) == 0 {
                lane.base.store(self.blob_base as u64, Ordering::Release);
            }
        }
        let h0 = lane.desc_head.load(Ordering::Relaxed);
        // R8 phase-5 / R10 — WORK-ASSIST on the deep ring: check space
        // once; if the lane is saturated, claim an assist-ring slot and
        // take the chunk inline (evaluate on THIS core when the workers
        // cannot keep up — the cycles come out of the backpressure spin
        // they would otherwise burn). The deep ring keeps the conversion
        // sustainable for as long as the fold lags the submit point.
        // Forced-inline mode (parity tests) takes this path
        // unconditionally — and NEVER falls back to a lane (the ring
        // drains by folding, so the wait is bounded and deadlock-free).
        let t = lane.desc_tail.load(Ordering::Acquire);
        // R12: the desc8 format reserves one extra slot per chunk-open
        // (the anchor desc).
        let chunk_need = CHUNK + if self.desc8 { 1 } else { 0 };
        let lane_full = h0.saturating_sub(t) + chunk_need > DESC_CAP;
        let take_inline = if self.force_inline {
            let mut sb = 0u32;
            while !self.try_claim_inline() {
                self.fold_available();
                crate::affinity::polite_spin(&mut sb);
            }
            true
        } else {
            lane_full && self.try_claim_inline()
        };
        if take_inline {
            self.assist_chunks += 1;
        } else {
            // Space check for the WHOLE chunk up front (the in-place
            // writes below must never touch slots the worker still
            // owns). Backpressure: fold what's ready (keeps result
            // rings flowing), then re-check — deadlock-free by the
            // lane-balance proof.
            let mut sb = 0u32;
            loop {
                let t = lane.desc_tail.load(Ordering::Acquire);
                if h0.saturating_sub(t) + chunk_need <= DESC_CAP {
                    break;
                }
                self.pending_head = h0;
                self.fold_available();
                // R8: SMT-polite backpressure spin.
                crate::affinity::polite_spin(&mut sb);
            }
            self.pending_head = h0;
        }
    }

    /// R10: claim the next assist-ring slot for the chunk starting at
    /// `next_span` (O(1) — the ring is indexed by the claim counter).
    /// Returns false when the ring is at capacity (in-flight chunks fill
    /// all but one slot); the caller then falls back to the lane path or,
    /// in force mode, folds until a slot frees.
    #[inline]
    fn try_claim_inline(&mut self) -> bool {
        if self.inline_seq - self.inline_fold_seq < self.assist_slots as u64 - 1 {
            let slot = (self.inline_seq % self.assist_slots as u64) as usize;
            self.inline_seq += 1;
            let c = &mut self.inline_ring[slot];
            c.first_span = self.next_span;
            c.len = 0;
            c.sealed = false;
            self.cur_inline = Some(slot);
            true
        } else {
            false
        }
    }

    /// The assist evaluation (the inline kernel dispatch + inline-ring
    /// store). #[inline(never)] keeps the kernel-dispatch machinery out of
    /// the steady scan's µop-cache window; the assist fires only when the
    /// workers cannot keep up, so its per-span call overhead is paid from
    /// the budget the spin was burning anyway.
    #[inline(never)]
    fn submit_inline_span(&mut self, slot: usize, body: &[u8]) {
        // Inline path: evaluate NOW, buffer the value for the ordered
        // fold. SAFETY: same feature contract as the workers (the
        // fabric's detected kernel).
        let value = unsafe { self.kernel.eval(body) };
        let c = &mut self.inline_ring[slot];
        c.vals[self.pending_len as usize] = value;
        c.len = self.pending_len as u32 + 1;
        self.pending_len += 1;
    }

    /// R10: the oldest unfolded inline chunk's ring slot (None when every
    /// claimed chunk has folded). Inline chunks are claimed in submission
    /// order and consumed by the ordered fold in the same order, so this
    /// is the ONLY slot the fold ever needs to look at — the R8 linear
    /// scan left with the deep ring.
    #[inline]
    fn oldest_inline_slot(&self) -> Option<usize> {
        if self.inline_fold_seq < self.inline_seq {
            Some((self.inline_fold_seq % self.assist_slots as u64) as usize)
        } else {
            None
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
        self.complete_boundaries();
        while self.fold_pos < self.next_span {
            // R10 — WORK-ASSIST fold, O(1): inline chunks are claimed in
            // submission order, so the oldest unfolded one is the ONLY
            // candidate sitting at the fold cursor (the R8 4-slot scan
            // generalized to the deep ring). An exact first_span match is
            // unambiguous: inline chunks start at chunk-window starts.
            if let Some(slot) = self.oldest_inline_slot() {
                let (first_span, len, sealed) = {
                    let c = &self.inline_ring[slot];
                    (c.first_span, c.len, c.sealed)
                };
                if sealed && first_span == self.fold_pos {
                    let n = len as u64;
                    debug_assert!(n <= self.fold_rem, "inline chunk exceeds its window");
                    // Fail-stop ordering discipline (same law as the lane
                    // path): span ids are first_span + i by construction —
                    // the claim-order invariant IS the order proof, and the
                    // exact-match above is its guard.
                    for i in 0..n as usize {
                        let v = self.inline_ring[slot].vals[i];
                        self.fold_value(v);
                    }
                    self.inline_fold_seq += 1;
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
                    self.complete_boundaries();
                    continue;
                }
            }
            // R16b: fold_pos inside an INLINE-CLAIMED chunk — its values
            // come from the inline ring (sealed when the chunk completes;
            // the straddling case seals a partial and the next window's
            // open re-claims the continuation). The lane's ring holds
            // only LATER chunks' results here (the worker skips claimed
            // chunks), so draining it would read out-of-order spans —
            // wait for the inline path instead (the ring protocol never
            // had this shape: the lane never contained another chunk's
            // results ahead of the cursor).
            if let Some(st) = &self.rx {
                if st.is_inline(self.fold_pos / CHUNK) {
                    break;
                }
            }
            // H6: `fold_lane` tracks the chunk containing `fold_pos` — no
            // division in the hot path (advance after chunk completion).
            let lane = &fabric.lanes[self.fold_lane];
            let head = lane.res_head.load(Ordering::Acquire);
            let tail = lane.res_tail.load(Ordering::Relaxed); // main-owned cursor
            if tail == head {
                break; // this lane's next batch isn't ready — order is strict
            }
            // Never fold past the end of fold_pos's chunk (lane changes there).
            let mut n = (head - tail).min(self.fold_rem);
            // GIGAHFT Lever 4: never fold past the oldest pending pass
            // boundary — the chain must snapshot + reset exactly there.
            if self.pass_tail != self.pass_head {
                let rec = &self.passes[self.pass_tail];
                if !rec.done && rec.end_span != u64::MAX {
                    n = n.min(rec.end_span - self.fold_pos);
                }
            }
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
            // A batch capped at a boundary just crossed it — snapshot now.
            self.complete_boundaries();
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
            // R16b: close any still-open window (the burst shape never
            // calls end_pass) — its span count feeds the next window's
            // warm start — then publish the final count BEFORE the drain
            // spin (the workers must see the tail's spans and any inline
            // claims to finish their queues).
            if self.rx_win_open {
                self.rx_slot_end[self.rx_win_slot as usize] = self.next_span;
                if let Some(st) = &self.rx {
                    st.set_last_window_count(self.next_span - self.rx_win_base);
                }
                self.rx_win_open = false;
            }
            self.rx_publish_ready();
            let mut fb = 0u32;
            while self.fold_pos < self.next_span {
                if self.fold_available_once_or_spin() {
                    continue;
                }
                // R8: SMT-polite final drain.
                crate::affinity::polite_spin(&mut fb);
            }
            self.complete_boundaries();
        } else {
            // Inline mode folds eagerly; nothing to drain.
            debug_assert_eq!(self.fold_pos, self.next_span);
            self.complete_boundaries();
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

    /// R8 phase-5 telemetry: chunks taken through the assist path.
    #[inline]
    pub fn assist_chunks(&self) -> u64 {
        self.assist_chunks
    }

    /// R16b telemetry: check-and-fix corrections applied (0 on a clean
    /// schedule — every prefill entry was already correct; > 0 under
    /// divergence — duplicates/colds/gaps drift the frame index).
    #[inline]
    pub fn rx_fixes(&self) -> u64 {
        self.rx_fixes
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
        self.msg_hash =
            crate::sink::fast_hash_bytes(self.msg_hash, &(msg.len() as u16).to_le_bytes());
        self.msg_hash = crate::sink::fast_hash_bytes(self.msg_hash, msg);
        self.count += 1;
    }

    fn on_event(&mut self, ev: &Event) {
        match ev {
            Event::GapOpened {
                from,
                ahead: _,
                gen,
            } => {
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
            Event::SessionBoundary {
                prev: _,
                next: _,
                gen,
            } => {
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
                self.rx_publish_ready();
            }
        }
    }

    /// R8: batched span emission — the G-INV assert runs once per batch (one
    /// proof era for all recs), the continuity chain is checked head-to-tail
    /// across the recs (exactly as strong as per-span: recs[0] must continue
    /// the previous emission and each rec[i] must continue recs[i-1]), and
    /// the per-span submit/evaluate work runs in one tight loop. Values,
    /// order and invariants are identical to the per-span path.
    #[inline(always)]
    fn on_span_batch(&mut self, proof: &LiveFeedProof, recs: &[SpanRec<'_>]) {
        assert!(
            proof.gen() >= self.last_gen,
            "G-INV violation: proof gen {} is older than sink last_gen {}",
            proof.gen(),
            self.last_gen
        );
        let mut sum = 0u64;
        for r in recs {
            if self.last_seq != 0 {
                assert_eq!(
                    r.first_seq,
                    self.last_seq + 1,
                    "Non-monotonic span batch: expected {}, got {} (count={})",
                    self.last_seq + 1,
                    r.first_seq,
                    r.count
                );
            }
            self.last_seq = r.first_seq + r.count as u64 - 1;
            sum += r.count as u64;
        }
        self.count += sum;
        match self.fabric {
            None => {
                // Inline mode: evaluate + fold NOW — sequential-equivalent.
                for r in recs {
                    let v = span_crc32c_8lane(r.body);
                    self.fold_value(v);
                    self.next_span += 1;
                    self.fold_pos += 1;
                }
            }
            Some(_) => {
                for r in recs {
                    self.submit_span(r.body);
                    self.next_span += 1;
                }
                // R16b: NO publish here — the chunk-completion boundary
                // inside submit_span_rx owns the cadence (per-CHUNK, the
                // ring protocol's shape); end_pass/finish publish the tail.
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
    fn hydra_pass(
        transport: &mut ReplayTransport,
        sess: [u8; 10],
        fabric: &HydraFabric,
    ) -> (u64, u64, u64) {
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
    /// R12: both descriptor formats (compact Desc8 + legacy) must be
    /// bit-exact — the fold-order assert pins the Desc8 derivation.
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
        for fmt in [true, false] {
            let fabric = HydraFabric::spawn_pinned_desc8(2, &[], fmt);
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
            // R8 phase-5: FORCED work-assist — every chunk evaluated on the
            // submitting core through the inline ring, folded in strict span
            // order. Must produce the identical tuple (the assist path is a
            // scheduling decision, never a semantic one).
            let got_assist = assist_pass(&mut t2, sess, &fabric);
            assert_eq!(got_assist, want, "forced work-assist diverged");
            // Mixed mode after a forced run (the ring resets cleanly).
            let got_fabric3 = hydra_pass(&mut t2, sess, &fabric);
            assert_eq!(got_fabric3, want, "fabric-after-assist diverged");
        } // R12 both formats
    }

    /// R8 phase-5: a fabric pass with the work-assist FORCED on — every
    /// chunk takes the inline path (deterministic; no dependence on
    /// backpressure timing).
    fn assist_pass(
        transport: &mut ReplayTransport,
        sess: [u8; 10],
        fabric: &HydraFabric,
    ) -> (u64, u64, u64) {
        transport.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new(fabric);
        sink.force_inline_mode();
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
                DelayModel::GaussianApprox {
                    mean_ns: 30_000,
                    sigma_ns: 8_000,
                },
                DelayModel::GaussianApprox {
                    mean_ns: 30_000,
                    sigma_ns: 8_000,
                },
            ],
            guarantee_coverage: true,
            session_change_at_msg: Some(300_000),
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"CHAOSHYDRA";
        // R12: both descriptor formats (compact Desc8 + legacy) must be
        // bit-exact — the fold-order assert pins the anchor-based derivation.
        for fmt in [true, false] {
            let fabric = HydraFabric::spawn_pinned_desc8(2, &[], fmt);
            let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
            let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);

            let want = seq_pass(&mut t1, sess);
            assert_eq!(
                want.0, 505_849,
                "chaos must still cover the full population"
            );
            let got = hydra_pass(&mut t2, sess, &fabric);
            assert_eq!(got, want, "chaos fabric mode diverged");
        } // R12 both formats
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
        // R12: both descriptor formats (compact Desc8 + legacy) must be
        // bit-exact — the fold-order assert pins the anchor-based derivation.
        for fmt in [true, false] {
            let fabric = HydraFabric::spawn_pinned_desc8(2, &[], fmt);
            let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
            let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);

            let want = seq_pass(&mut t1, sess);
            let got = hydra_pass(&mut t2, sess, &fabric);
            assert_eq!(got, want, "fixed(1) fabric mode diverged");
        } // R12 both formats
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
        // R12: both descriptor formats (compact Desc8 + legacy) must be
        // bit-exact — the fold-order assert pins the anchor-based derivation.
        for fmt in [true, false] {
            let fabric = HydraFabric::spawn_pinned_desc8(2, &[], fmt);
            let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
            let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);

            let want = seq_pass(&mut t1, sess);
            let got = hydra_pass(&mut t2, sess, &fabric);
            assert_eq!(got, want, "seeded-range fabric mode diverged");
        } // R12 both formats
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
        // R12: both descriptor formats (compact Desc8 + legacy) must be
        // bit-exact — the fold-order assert pins the anchor-based derivation.
        for fmt in [true, false] {
            let fabric = HydraFabric::spawn_pinned_desc8(1, &[], fmt);
            let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
            let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);

            let want = seq_pass(&mut t1, sess);
            let got = hydra_pass(&mut t2, sess, &fabric);
            assert_eq!(got, want, "wraparound fabric mode diverged");
        } // R12 both formats
    }

    /// GIGAHFT Lever 4: cross-pass double-buffered overlap bit-parity.
    /// Multiple passes on ONE sink (global span ids, per-pass boundary
    /// capture, non-blocking end_pass) must produce, for EVERY pass, the
    /// exact tuple a fresh sequential SpanConformanceSink produces —
    /// including when the fold drains pass N's tail during pass N+1.
    #[test]
    fn t_hydra_crosspass_overlap_bitparity() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        // Reference: one sequential pass per session id.
        let sessions: [[u8; 10]; 3] = [*b"OVLAPAAA01", *b"OVLAPBBB02", *b"OVLAPCCC03"];
        let mut want = Vec::new();
        for sess in sessions {
            let mut t = ReplayTransport::new(&gt, sched.clone(), sess);
            want.push(seq_pass(&mut t, sess));
        }

        // Overlapped fabric passes on one sink. Pass N+1's submission
        // overlaps pass N's residual tail (the whole point of Lever 4).
        // R12: both descriptor formats (compact Desc8 + legacy) must be
        // bit-exact — the fold-order assert pins the anchor-based derivation.
        for fmt in [true, false] {
            let fabric = HydraFabric::spawn_pinned_desc8(2, &[], fmt);
            let mut t = ReplayTransport::new(&gt, sched.clone(), sessions[0]);
            let mut sink = HydraSpanSink::new(&fabric);
            let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
            let mut got = Vec::new();
            for (pi, sess) in sessions.iter().enumerate() {
                t.reset(*sess);
                let mut seq = Sequencer::new();
                sink.begin_pass();
                let mut batch = FrameBatch::new();
                let mut poll_no: u32 = 0;
                while t.poll(&mut batch) > 0 {
                    let now = t.now_ns();
                    for (pos, frame) in batch.frames().iter().enumerate() {
                        seq.ingest_auto(
                            frame.bytes(),
                            frame.feed,
                            now,
                            &mut sink,
                            t.batch_blocks(pos),
                            t.batch_memo(pos),
                        );
                    }
                    poll_no = poll_no.wrapping_add(1);
                    if poll_no % 2 == 0 {
                        sink.drain_ready();
                    }
                }
                sink.end_pass();
                // Harvest whatever completed (older passes close during this
                // pass's polling); the LAST pass closes at finish().
                let n = sink.harvest_completed(&mut harvested);
                for rec in &harvested[..n] {
                    got.push(*rec);
                }
                let _ = pi;
            }
            sink.finish();
            let n = sink.harvest_completed(&mut harvested);
            for rec in &harvested[..n] {
                got.push(*rec);
            }
            assert_eq!(got, want, "cross-pass overlap diverged from sequential");
            // And the same structure once more with inline mode (no threads).
            let mut t = ReplayTransport::new(&gt, sched.clone(), sessions[0]);
            let mut sink = HydraSpanSink::new_inline();
            let mut got2 = Vec::new();
            for sess in sessions {
                t.reset(sess);
                let mut seq = Sequencer::new();
                sink.begin_pass();
                let mut batch = FrameBatch::new();
                while t.poll(&mut batch) > 0 {
                    let now = t.now_ns();
                    for (pos, frame) in batch.frames().iter().enumerate() {
                        seq.ingest_auto(
                            frame.bytes(),
                            frame.feed,
                            now,
                            &mut sink,
                            t.batch_blocks(pos),
                            t.batch_memo(pos),
                        );
                    }
                }
                sink.end_pass();
                let n = sink.harvest_completed(&mut harvested);
                for rec in &harvested[..n] {
                    got2.push(*rec);
                }
            }
            sink.finish();
            let n = sink.harvest_completed(&mut harvested);
            for rec in &harvested[..n] {
                got2.push(*rec);
            }
            assert_eq!(got2, want, "inline cross-pass diverged");
        } // R12 both formats
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
            // R12: both descriptor formats at every worker count.
            for fmt in [true, false] {
                let fabric = HydraFabric::spawn_pinned_desc8(w, &[], fmt);
                let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);
                let got = hydra_pass(&mut t2, sess, &fabric);
                assert_eq!(got, want, "worker count {} fmt8={} diverged", w, fmt);
            }
        }
    }

    /// R12: the RX-PIPELINED transport + the SoA vectorized scan + the
    /// fabric — the exact sustained-arm stack (auto-advance program,
    /// cross-pass double buffering) under CHAOS (loss + jitter + session
    /// change), pinned bit-exactly against the sequential reference. This
    /// is the end-to-end law for the 8-entry vector ladder: every group it
    /// proves must observably equal the scalar ladder's eight steps.
    #[test]
    fn t_hydra_pipeline_soa_chaos_bitparity() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            seed_a: 0xCAFE_0000_1111_2222,
            seed_b: 0xBEEF_3333_4444_5555,
            msgs_per_packet: Packetize::MtuBound(1200),
            loss: [
                crate::sched::LossModel::Bernoulli { p_pm: 80 },
                crate::sched::LossModel::Bernoulli { p_pm: 140 },
            ],
            delay: [
                crate::sched::DelayModel::GaussianApprox {
                    mean_ns: 20_000,
                    sigma_ns: 6_000,
                },
                crate::sched::DelayModel::GaussianApprox {
                    mean_ns: 45_000,
                    sigma_ns: 18_000,
                },
            ],
            guarantee_coverage: true,
            session_change_at_msg: Some(200_000),
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sessions: [[u8; 10]; 3] = [*b"PIPECHAOS1", *b"PIPECHAOS2", *b"PIPECHAOS3"];
        let ladder = crate::soa::ladder8_best();

        // Reference: one sequential pass per session.
        let mut want = Vec::new();
        for sess in sessions {
            let mut t = ReplayTransport::new(&gt, sched.clone(), sess);
            want.push(seq_pass(&mut t, sess));
        }

        // The fabric + pipelined + SoA stack, one pass per session — under
        // BOTH descriptor formats.
        for fmt in [true, false] {
            let fabric = HydraFabric::spawn_pinned_desc8(2, &[], fmt);
            let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                &gt,
                sched.clone(),
                sessions[0],
                128,
            );
            let mut sink = HydraSpanSink::new(&fabric);
            let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
            let mut got = Vec::new();
            for sess in sessions {
                t.reset(sess);
                let mut seq = Sequencer::new();
                sink.begin_pass();
                while t.next_batch() {
                    seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut sink, ladder);
                    sink.drain_ready();
                }
                sink.end_pass();
                let n = sink.harvest_completed(&mut harvested);
                for rec in &harvested[..n] {
                    got.push(*rec);
                }
            }
            sink.finish();
            let n = sink.harvest_completed(&mut harvested);
            for rec in &harvested[..n] {
                got.push(*rec);
            }
            assert_eq!(
                got, want,
                "pipeline+soa chaos fabric diverged from sequential"
            );
        } // R12 both formats
    }

    // ══════════════════════════════════════════════════════════════════
    // R16b rxdesc parity suite (docs/29 §5 — the array-driven submission)
    // ══════════════════════════════════════════════════════════════════

    /// One pipelined pass over an ATTACHED fabric (the RX prefill armed,
    /// the array-driven submission, the SoA ladder) — the sustained arm's
    /// exact stack shape. Returns the tuple + the check-and-fix/assist
    /// telemetry.
    fn hydra_pass_piped(
        transport: &mut nf_transport::pipeline::PipelinedReplayTransport,
        fabric: &HydraFabric,
    ) -> ((u64, u64, u64), u64, u64) {
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new(fabric);
        let ladder = crate::soa::ladder8_best();
        while transport.next_batch() {
            seq.ingest_entries_ladder(transport.entries(), transport.now_ns(), &mut sink, ladder);
            sink.drain_ready();
        }
        sink.finish();
        (
            (sink.count, sink.hash, sink.msg_hash),
            sink.rx_fixes(),
            sink.assist_chunks(),
        )
    }

    /// The rxdesc parity matrix: steady + chaos schedules × {check+fix
    /// path (no prefill), forced-inline, pipelined+prefill} × worker
    /// counts. Every cell must be bit-exact vs the sequential reference;
    /// the STEADY pipelined cell must additionally need ZERO fixes (the
    /// RX prefill landed every entry exactly — the fast path is real).
    /// R16e: the diet flag sweeps BOTH worker shapes — the default (diet)
    /// runs the full chaos × w1-3 × (a)(b)(c) matrix; the pre-diet pin
    /// (the HFT_RXDIET=0 rollback arm's parity) runs the essential shape:
    /// w=2, steady + chaos, all three cells. Both must reproduce the
    /// sequential reference bit-exact — the diet restructures the worker's
    /// record resolution (per-boundary, not per-span) and its wake cadence
    /// (depth batches with hot frontier waits); any drift is a fail-stop.
    #[test]
    fn t_rxdesc_parity_matrix() {
        let gt = load_mini();
        for diet in [true, false] {
            for chaos in [false, true] {
                let cfg = if chaos {
                    ReplayConfig {
                        seed_a: 0xCAFE_0000_1111_2222,
                        seed_b: 0xBEEF_3333_4444_5555,
                        msgs_per_packet: Packetize::MtuBound(1200),
                        loss: [
                            LossModel::Bernoulli { p_pm: 80 },
                            LossModel::Bernoulli { p_pm: 140 },
                        ],
                        delay: [
                            DelayModel::GaussianApprox {
                                mean_ns: 20_000,
                                sigma_ns: 6_000,
                            },
                            DelayModel::GaussianApprox {
                                mean_ns: 45_000,
                                sigma_ns: 18_000,
                            },
                        ],
                        guarantee_coverage: true,
                        session_change_at_msg: Some(200_000),
                        ..Default::default()
                    }
                } else {
                    ReplayConfig {
                        msgs_per_packet: Packetize::MtuBound(1400),
                        guarantee_coverage: true,
                        ..Default::default()
                    }
                };
                let sched = build_schedule(&gt, &cfg);
                let sess = if chaos {
                    *b"RXDESCHAOS"
                } else {
                    *b"RXDESCSTED"
                };

                let mut t0 = ReplayTransport::new(&gt, sched.clone(), sess);
                let want = seq_pass(&mut t0, sess);
                assert_eq!(want.0, 505_849, "coverage must hold (chaos={chaos})");

                for w in 1..=3usize {
                    if !diet && w != 2 {
                        continue; // the pre-diet pin covers the essential shape only
                    }
                    let fabric = HydraFabric::spawn_pinned_full(w, &[], true, true, diet);

                    // (a) the check+fix path (single-threaded transport: no
                    // prefill — every array entry written by the sink itself).
                    let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
                    let got = hydra_pass(&mut t1, sess, &fabric);
                    assert_eq!(
                        got, want,
                        "rxdesc check+fix diverged (chaos={chaos}, w={w}, diet={diet})"
                    );

                    // (b) forced-inline (every chunk on the submitting core;
                    // the workers' inline-skip path under the array protocol).
                    let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);
                    let got_force = assist_pass(&mut t2, sess, &fabric);
                    assert_eq!(
                        got_force, want,
                        "rxdesc forced-inline diverged (chaos={chaos}, w={w}, diet={diet})"
                    );

                    // (c) the full stack: pipelined transport + the
                    // array-driven submission (the sustained arm's shape).
                    // The FIRST pipelined pass may still pay check-and-fix
                    // stores (its warm-start source is the previous cells'
                    // windows); the SECOND must find every entry already
                    // correct via the warm start — zero fixes is the FAST
                    // PATH PROVEN (the deterministic schedule's span
                    // sequence carries across sinks).
                    let mut t3 = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                        &gt,
                        sched.clone(),
                        sess,
                        128,
                    );
                    t3.reset(sess);
                    let (got3, _fixes, assists1) = hydra_pass_piped(&mut t3, &fabric);
                    assert_eq!(
                        got3, want,
                        "rxdesc pipelined diverged (chaos={chaos}, w={w}, diet={diet})"
                    );
                    // Determinism + the warm start's fast path (a fresh sink
                    // — the generation re-anchor — warm-started from the
                    // previous pass's entries).
                    t3.reset(sess);
                    let (got4, fixes2, assists2) = hydra_pass_piped(&mut t3, &fabric);
                    assert_eq!(
                        got4, want,
                        "rxdesc pipelined determinism diverged (chaos={chaos}, w={w}, diet={diet})"
                    );
                    if !chaos && assists1 + assists2 == 0 {
                        // The warm start's FAST PATH: with no assist chunks
                        // (inline evaluations leave array holes by design —
                        // the assist trades array writes for in-window CRC),
                        // the second pass must find EVERY entry already
                        // correct: zero stores on the submitting core.
                        assert_eq!(
                        fixes2, 0,
                        "steady second pass needed check-and-fix corrections (w={w}, diet={diet}) — \
                         the warm start missed entries"
                    );
                    }
                }
            }
        }
    }

    /// The sustained shape: pipelined + AUTO-ADVANCE + ONE sink wrapping
    /// TEN passes in begin/end_pass — the record ring (8 slots) wraps,
    /// the array reuse gate binds, chunks straddle pass boundaries, and
    /// the fold overlaps across passes. Every pass must reproduce the
    /// sequential reference for its session.
    #[test]
    fn t_rxdesc_sustained_multipass_parity() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sessions: [[u8; 10]; 4] = [
            *b"RXDMULTI01",
            *b"RXDMULTI02",
            *b"RXDMULTI03",
            *b"RXDMULTI04",
        ];
        // The auto-advance program: pass 0 = construction (sessions[0]);
        // pass k >= 1 = sessions[(k-1) % 4].
        fn prog(pass: u64) -> [u8; 10] {
            const S: [[u8; 10]; 4] = [
                *b"RXDMULTI01",
                *b"RXDMULTI02",
                *b"RXDMULTI03",
                *b"RXDMULTI04",
            ];
            if pass == 0 {
                S[0]
            } else {
                S[((pass - 1) % 4) as usize]
            }
        }
        let sess_of = |pass: u64| -> [u8; 10] {
            if pass == 0 {
                sessions[0]
            } else {
                sessions[((pass - 1) % 4) as usize]
            }
        };

        // Per-session sequential references.
        let mut want = Vec::new();
        for sess in sessions {
            let mut t = ReplayTransport::new(&gt, sched.clone(), sess);
            want.push(seq_pass(&mut t, sess));
        }

        // R16e: BOTH worker shapes survive the soak — the diet default and
        // the pre-diet rollback (the 11x arm's parity at the 400-pass
        // slot-cycling scale: the reuse gate, the straddling boundaries,
        // the frontier waits' partial publishes and the boundary cache's
        // mid-chunk crossings all under load).
        for diet in [true, false] {
            let label = if diet { "diet" } else { "pre-diet" };
            let fabric = HydraFabric::spawn_pinned_full(1, &[], true, true, diet);
            let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce_cpu_auto(
                &gt,
                sched.clone(),
                sessions[0],
                128,
                None,
                Some(prog),
            );
            // The ref pass (pass 1, sessions[0]) — a separate sink, drained
            // (the sustained bench's shape; also the generation boundary).
            t.reset_pass(1, sess_of(1));
            {
                let mut seq = Sequencer::new();
                let mut ref_sink = HydraSpanSink::new(&fabric);
                let ladder = crate::soa::ladder8_best();
                while t.next_batch() {
                    seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut ref_sink, ladder);
                    ref_sink.drain_ready();
                }
                ref_sink.finish();
                assert_eq!(
                    (ref_sink.count, ref_sink.hash, ref_sink.msg_hash),
                    want[0],
                    "rxdesc multipass ref diverged ({label})"
                );
            }

            // MANY passes on ONE sink (the record ring wraps at 8; hundreds
            // of passes stress the slot cycling at the sustained arm's scale).
            let mut seq = Sequencer::new();
            let mut sink = HydraSpanSink::new(&fabric);
            let ladder = crate::soa::ladder8_best();
            let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
            let mut got = Vec::new();
            for pass in 2..=400u64 {
                t.reset_pass(pass, sess_of(pass));
                *seq = Sequencer::new_unboxed();
                sink.begin_pass();
                while t.next_batch() {
                    seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut sink, ladder);
                    sink.drain_ready();
                }
                sink.end_pass();
                let n = sink.harvest_completed(&mut harvested);
                for rec in &harvested[..n] {
                    got.push(*rec);
                }
            }
            sink.finish();
            let n = sink.harvest_completed(&mut harvested);
            for rec in &harvested[..n] {
                got.push(*rec);
            }
            // In-order harvest (harvest order == pass order).
            let expect: Vec<(u64, u64, u64)> =
                (2..=400u64).map(|p| want[((p - 1) % 4) as usize]).collect();
            assert_eq!(got, expect, "rxdesc sustained multipass diverged ({label})");
        }
    }
}
