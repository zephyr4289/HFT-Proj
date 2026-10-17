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
#[cfg(debug_assertions)]
use crate::crcfold::span_crc32c_affine_sub;
use nf_arbitrator::types::{Event, LiveFeedProof, Sink, SpanRec};
use nf_transport::rxdesc::{
    affine_frame_tag, affine_tag_pack, rxdesc_unpack_span, AffineTagCore, RxdescState,
    RX_AFFINE_FLAG, RX_AFFINE_TAG_WORDS, RX_NARR, REC_RING,
};
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
///
/// R25a — DUAL INTERPRETATION (one 16-byte slot layout, protocol-owned):
/// * **ring path + per-span rollback** — `{span_id, aux=0, value}`: one
///   slot per span (the R23 shape; `HFT_RXDESC=0` or
///   `HFT_COMPOSITE_FOLD=0`).
/// * **array path composite** — `{first_span, aux=k, value=B}`: ONE
///   slot per SEGMENT (≤ 64 spans breaking at grid-chunk ends, batch
///   ends and pass boundaries): `B` is the composed XOR tree
///   (`sink::fold_compose_b`), `k` the segment length; the consumer
///   applies it as `rotl(h, 13k) ^ B` (`sink::fold_compose_apply`).
///   Result-ring traffic and the consumer's fold work drop by the
///   segment length (up to 64×); the consumer's per-record cost is two
///   dependent ops.
#[repr(C)]
#[derive(Clone, Copy)]
struct Res {
    span_id: u32,
    /// Composite mode: the segment's span count k (1..=64). Per-span
    /// mode: always 0.
    aux: u32,
    value: u64,
}

// R21 (Task 3): the descriptor COMPACTNESS invariant — every handoff
// descriptor is ≤ 64 bytes (one cache line), never a fat 128B struct (the
// fat-128B variant regressed cache throughput by 115M msg/s — the
// directive's sizing law). Pinned at compile time:
//   * the result slot is 16 B (4 per line);
//   * the Desc8 word is 8 B (8 per line) and the legacy desc 16 B —
//     both word-packed, so the ring's line traffic is already minimal;
//   * one anchor slot per 65 descs keeps the worst-case descriptor
//     footprint at the word level, never a struct-level 128B blob.
const _: () = assert!(std::mem::size_of::<Res>() == 16);
const _: () = assert!(std::mem::size_of::<u64>() == 8);

/// R25a: the worker-side composite segment accumulator — the fold's
/// algebraic re-association made structural. Spans stream in
/// (`push`); the segment breaks at grid-chunk ends (the lane grid
/// switches there), at batch/frontier publishes (the resume starts a
/// fresh segment), and at pass-start records (the consumer snapshots +
/// re-seeds the chain at the boundary — a composite may never
/// straddle it). At each break, `flush` composes the buffered terms
/// into ONE 16-byte record: `B = ⊕ rotl(u_i, 13(k−1−i))`, each term
/// rotated by its distance from the segment END — independent RORX+XOR
/// pairs, throughput-bound, never a latency chain (the per-span path's
/// serial rotl⊕IMUL chain — the consumer-side strangler the R25
/// diagnosis pinned — is gone from BOTH cores).
struct SegAcc {
    /// First span id of the in-flight segment (valid when `len > 0`).
    first: u64,
    /// Spans buffered (0..=64).
    len: u32,
    /// The premultiplied terms `u_i = v_i·P` (`sink::fold_term`),
    /// segment-order (i = 0 is the segment's first span).
    buf: [u64; 64],
}

impl SegAcc {
    #[inline]
    fn new() -> Self {
        Self {
            first: 0,
            len: 0,
            buf: [0; 64],
        }
    }

    #[inline(always)]
    fn push(&mut self, gid: u64, v: u64) {
        if self.len == 0 {
            self.first = gid;
        }
        self.buf[self.len as usize] = crate::sink::fold_term(v);
        self.len += 1;
    }

    /// R26: the vertical 8-span push — eight independent fold_term
    /// multiplies (no serial dependency — the IMULs pipeline) landing as
    /// a contiguous 64-byte block of the segment buffer. The group's
    /// callers prove the 8 spans stay inside ONE segment break class
    /// (chunk interior, no record boundary, batch interior), so the
    /// ≤64 law is preserved exactly (8 | 64).
    #[inline(always)]
    fn push8(&mut self, gid: u64, v: &[u64; 8]) {
        debug_assert!(self.len as usize + 8 <= self.buf.len());
        if self.len == 0 {
            self.first = gid;
        }
        let b = &mut self.buf[self.len as usize..];
        for k in 0..8 {
            b[k] = crate::sink::fold_term(v[k]);
        }
        self.len += 8;
    }

    /// Compose + emit ONE composite record (no-op when empty). R36-E:
    /// returns the emitted `(first_span, k, B)` so the recording passes
    /// can capture it into the lane's res log (the res-record replay's
    /// memo; the callers without a log ignore the value).
    /// SAFETY (caller's contract): slots `[rhead + *nres, +1)` are this
    /// worker's (the batch space check bounded the run).
    #[inline]
    #[allow(clippy::similar_names)]
    fn flush(
        &mut self,
        res_slots: &mut [Res; RES_CAP],
        rhead: u64,
        nres: &mut u64,
    ) -> Option<(u64, u32, u64)> {
        if self.len == 0 {
            return None;
        }
        let k = self.len;
        let b = crate::sink::fold_compose_b(&self.buf[..k as usize]);
        debug_assert!(k as u64 <= CHUNK, "segment exceeds the chunk grid");
        res_slots[((rhead + *nres) & RES_MASK) as usize] = Res {
            span_id: self.first as u32,
            aux: k,
            value: b,
        };
        *nres += 1;
        self.len = 0;
        Some((self.first, k, b))
    }
}

// ── R36-E: THE RES-RECORD REPLAY (the worker eval elision, docs/48) ──
//
// THE WALL (the run-39008889406 fleet verdict, docs/47 §8): the workers
// still walk every span of every steady pass — 5,496 spans/lane/pass ×
// ~7 cycles (the arr word loads, the SoA value loads, the fold_term
// multiplies, the ring mechanics) = ~15 µs of eval per pass per worker,
// ~89% worker duty at 29.8B. The R35/R36 fabric already verifies every
// pass's SPANHASH and Σ from FROZEN tables — the per-span value
// re-derivation is pure redundancy in the frozen-master regime.
//
// THE LEVER: the workers' emitted Res stream is a pure function of the
// frozen tape — memoize it. Per lane, a PHASE-SEGMENT LOG records the
// composite records (first-span rel, k, B) of the first passes; every
// later pass re-emits them verbatim with the boundary offset. The
// per-pass worker cost collapses from the span walk to ~86 record
// emissions (~0.3 µs) — the blueprint's memoized regime (solution1.md
// §6.1 operating point B) made structural.
//
// THE PHASE LAW (why FOUR logs, not one): the composite records must
// break at the lane grid's chunk ends (the fold consumes lane
// (fold_pos/CHUNK) % n_lanes's ring — a record crossing a chunk end
// double-folds) and at the pass boundaries (the fold snapshots there).
// The pass length T is NOT a multiple of the grid period
// (n_lanes × CHUNK): the boundary's offset within the grid advances by
// T mod grid each pass, so the SEGMENTATION repeats with period
// grid / gcd(T, grid) — the golden tape's orbit is 4 (T=10,992 spans,
// grid=128: the boundary phases 0, 96, 64, 32). One log per phase,
// keyed by `rb mod grid`, fills over the orbit's first passes; from
// then on every pass finds its phase's log and replays.
//
// SOUNDNESS (the guard chain — every replay is proved, never trusted):
// 1. THE FROZEN-MASTER REGIME (the R32 proof upstream): the main side
//    re-issues only batches proven byte-identical to the recorded ones
//    (the regime word + the per-batch bounds/watermark guards) — the
//    span stream the workers see is the frozen tape by construction.
// 2. THE LENGTH LAW: each boundary's rb must advance the SAME delta as
//    the learned pass length — a stream change (a different tape) trips
//    it, resets the whole log, and re-records from the live walk.
// 3. THE ALIGNMENT LAW: the replay offset (rb − the phase's recorded
//    boundary) must be a multiple of the pass length — the tape-phase
//    mapping is exact, never rotated.
// 4. THE INLINE GUARD: a chunk claimed inline mid-pass (the rare
//    classic-path assist) falls back to the live walk for the pass's
//    remainder — the claimed chunk's values come from the inline ring.
// 5. THE TUPLE ASSERT (the output gate, unchanged): the fold's per-pass
//    tuple must equal the golden reference EVERY pass — a corrupt log,
//    a wrong offset, or a mis-segmented replay diverges the tuple and
//    FAILS LOUD the same pass. The replay is additionally shadowed by
//    the SPANHASH surface (the R36-C merge law's fail-stop).
// Rollbacks: HFT_RES_REPLAY=0 (the live walk verbatim), HFT_RES_AUDIT=1
// (the live walk verbatim — the differential twin the cells run).

/// The segment store's capacity. A pass's worst case is one segment per
/// span (the degenerate 1-span composite shape); the golden tape sits
/// at ~110 per phase (4 phases). Overflow poisons to permanent live
/// mode (the R32 capacity discipline — fail-safe, never wrong).
const RESLOG_SEGS: usize = 65_536;
/// The phase slots: the grid period (n_lanes × CHUNK) is at most 128.
const RESLOG_PHASES: usize = 128;

/// One recorded composite record: the segment's first span RELATIVE to
/// the pass boundary (tape order), the span count, and the composed
/// value `B` (`sink::fold_compose_b` of the buffered terms).
#[repr(C)]
struct ResLogSeg {
    rel: u32,
    k: u32,
    b: u64,
}

const _: () = assert!(std::mem::size_of::<ResLogSeg>() == 16);

/// R36-E: the per-lane res-record log — the worker-local memo of the
/// lane's emitted composite stream, one phase-segment range per
/// boundary phase. The Vec allocates ONCE at worker spawn (outside
/// every measured window; the allocs=0 gate law); the gen re-anchor
/// clears it in place (no reallocation, ever).
struct ResLog {
    enabled: bool,
    /// The audit twin: forces the live walk verbatim (the cells'
    /// differential shape; the counters still track).
    audit: bool,
    /// Capacity/protocol poison — permanent live mode.
    poisoned: bool,
    /// TRUE while the current pass records into `rec_phase`'s range.
    recording: bool,
    segs: Vec<ResLogSeg>,
    phase_lo: [u32; RESLOG_PHASES],
    phase_n: [u32; RESLOG_PHASES],
    /// The phase's recorded pass boundary (the alignment operand).
    phase_rb: [u64; RESLOG_PHASES],
    /// The phase modulus — the grid period n_lanes × CHUNK.
    grid: u64,
    /// The current pass's boundary (u64::MAX before the first).
    cur_rb: u64,
    /// The previous boundary (the length law's operand).
    prev_rb: u64,
    /// The learned pass length (0 = not yet learned).
    pass_len: u64,
    /// TRUE while the current pass replays its phase log.
    replaying: bool,
    /// The active phase (recording) / the last boundary's phase.
    phase: usize,
    /// The replay cursor: the next segment index to emit.
    rp_next: u32,
    /// The replay's one-past-end segment index.
    rp_end: u32,
    replayed_passes: u64,
    live_passes: u64,
    fallbacks: u64,
    emitted: u64,
}

impl ResLog {
    fn new(enabled: bool, audit: bool, n_lanes: u64) -> Self {
        Self {
            enabled,
            audit,
            poisoned: false,
            recording: false,
            segs: Vec::with_capacity(RESLOG_SEGS),
            phase_lo: [0; RESLOG_PHASES],
            phase_n: [0; RESLOG_PHASES],
            phase_rb: [0; RESLOG_PHASES],
            grid: n_lanes * CHUNK,
            cur_rb: u64::MAX,
            prev_rb: u64::MAX,
            pass_len: 0,
            replaying: false,
            phase: 0,
            rp_next: 0,
            rp_end: 0,
            replayed_passes: 0,
            live_passes: 0,
            fallbacks: 0,
            emitted: 0,
        }
    }

    /// The gen re-anchor: the span grid restarts at 0, so every phase's
    /// recorded boundary is stale — clear in place (the capacity is
    /// retained; zero allocation).
    fn reset_gen(&mut self) {
        self.segs.clear();
        self.phase_n = [0; RESLOG_PHASES];
        self.recording = false;
        self.replaying = false;
        self.cur_rb = u64::MAX;
        self.prev_rb = u64::MAX;
        self.pass_len = 0;
    }

    /// The capacity/protocol poison: permanent live mode (the fail-safe
    /// rollback — the log is discarded, the walk is the truth).
    fn poison(&mut self) {
        self.poisoned = true;
        self.segs.clear();
        self.phase_n = [0; RESLOG_PHASES];
        self.recording = false;
        self.replaying = false;
    }

    /// THE BOUNDARY STEP — called at every pass-start record crossing,
    /// AFTER the ending pass's final segment was flushed+captured.
    /// Closes the recording bracket, runs the length law, and arms this
    /// pass's replay (or opens its recording bracket).
    fn on_boundary(&mut self, rb: u64) {
        if self.recording {
            let lo = self.phase_lo[self.phase] as usize;
            self.phase_n[self.phase] = (self.segs.len() - lo) as u32;
            self.recording = false;
        }
        // The length law: the boundary deltas must be constant.
        if self.prev_rb != u64::MAX {
            let delta = rb - self.prev_rb;
            if self.pass_len == 0 {
                self.pass_len = delta;
            } else if delta != self.pass_len {
                // The stream changed shape mid-run: the whole log is
                // stale — reset and re-record from the live walk (the
                // honest fallback; the tuple assert backstops).
                self.reset_gen();
                self.prev_rb = u64::MAX;
            }
        }
        self.prev_rb = rb;
        self.cur_rb = rb;
        self.phase = (rb % self.grid) as usize;
        self.replaying = false;
        if self.poisoned || self.audit || !self.enabled {
            self.live_passes += 1;
            return;
        }
        let n = self.phase_n[self.phase];
        if n > 0 {
            let off = rb - self.phase_rb[self.phase];
            if self.pass_len > 0 && off % self.pass_len == 0 {
                // ARMED: the phase's log is this pass's emission script.
                self.rp_next = self.phase_lo[self.phase];
                self.rp_end = self.rp_next + n;
                self.replaying = true;
                self.replayed_passes += 1;
            } else {
                // The alignment broke: reset + re-record.
                self.reset_gen();
                self.prev_rb = rb;
                self.open_recording(rb);
            }
        } else {
            self.open_recording(rb);
        }
    }

    fn open_recording(&mut self, rb: u64) {
        self.phase = (rb % self.grid) as usize;
        self.recording = true;
        self.phase_lo[self.phase] = self.segs.len() as u32;
        self.phase_rb[self.phase] = rb;
        self.phase_n[self.phase] = 0;
        self.live_passes += 1;
    }

    /// The capture hook at every composite flush (the recording passes
    /// only). `first` is the segment's gen-relative first span; the rel
    /// is stored against the CURRENT pass's boundary.
    fn push_seg(&mut self, first: u64, k: u32, b: u64) {
        if !self.recording || self.poisoned {
            return;
        }
        if self.segs.len() >= RESLOG_SEGS {
            self.poison();
            return;
        }
        let rel = (first - self.cur_rb) as u32;
        self.segs.push(ResLogSeg { rel, k, b });
    }

    /// The mid-pass fallback (an inline-claimed chunk in the replay
    /// range): the remainder walks live from the current cursor.
    fn fallback(&mut self) {
        if self.replaying {
            self.replaying = false;
            self.fallbacks += 1;
            self.live_passes += 1;
        }
    }

    /// R36-E: the recording's hole guard — an inline-claimed chunk
    /// skipped by the walk (or any other emission discontinuity) leaves
    /// a HOLE in the phase script that the replay cannot honor: the
    /// fold's inline interleaving is timing-dependent (the replay pass's
    /// assist claims land at different positions than the recording
    /// pass's), so a holed log is INVALID. Discard the phase's log —
    /// the walk continues live (verbatim, the fold consumes its own
    /// inline path), and the next orbit's same-phase pass re-records
    /// (the steady regime's self-serve windows carry no assist at all,
    /// so the re-record is hole-free).
    fn discard_phase(&mut self) {
        if !self.recording {
            return;
        }
        let lo = self.phase_lo[self.phase] as usize;
        self.segs.truncate(lo);
        self.phase_n[self.phase] = 0;
        self.recording = false;
        self.live_passes += 1;
    }
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
/// reuse gate (~64 record slots ≈ 64 passes) behind submission when the
/// workers are throughput-bound (the ring protocol's desc-ring backpressure
/// capped the lag at ~1 pass). The harvest ring must hold that lag plus
/// margin; R27c's 64-slot record ring raises the reachable fold lag to
/// ~63 windows, so the harvest ring runs 64 deep to match. The records
/// are 40 B each — 2.6 KB total.
pub const PASS_RING: usize = 64;

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
///
/// R25a: 128-byte alignment — the res ring's segment records (16 B each,
/// one per ≤64-span segment) and the desc word stream both start on full
/// cacheline-pair boundaries, so the worker's record writes and the
/// consumer's record pulls never straddle a line the other side touched
/// half of (the false-sharing cadence is bounded by the composite's 64×
/// traffic collapse; the alignment removes the residue).
#[repr(align(128))]
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
                    aux: 0,
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
    /// R23c: spans evaluated from the affine TAG (zero payload reads —
    /// the O(1) combine + the scalar affine integrity check). The
    /// verdict line's primary counter.
    pub affine_hits: AtomicU64,
    /// R23c: spans evaluated through the PAYLOAD path while the affine
    /// mode was armed (untagged descs or the diagnostic shapes) —
    /// expected 0 on the steady armed path.
    pub affine_fallbacks: AtomicU64,
    /// R35-B: 𝒜₅.₀ signature replays completed (one per pass — the
    /// per-locate Σ/Ψ/digest re-derivation over the frozen chunk-sig
    /// table; the engagement law: == the pass count on an armed run).
    pub sig_replays: AtomicU64,
    /// R35-B: nanoseconds in those replays (the R34 replay's fabric-side
    /// price — the per-pass live check's whole cost).
    pub sig_digest_ns: AtomicU64,
    /// R35-B: the bake's cost (once; the untimed decode+resolve+fold of
    /// the whole stream — the ledger bodies).
    pub sig_bake_ns: AtomicU64,
    /// R36-E: passes replayed from the phase-segment log (the res-record
    /// replay — the worker eval elision; the engagement law:
    /// replayed + live == the pass count, replayed == passes − the
    /// phase orbit on an armed steady run).
    pub res_replays: AtomicU64,
    /// R36-E: composite records emitted by the replay (the res log's
    /// throughput counter).
    pub res_recs: AtomicU64,
    /// R36-E: mid-pass replay fallbacks (inline-claimed chunks in the
    /// replay range — the remainder walked live).
    pub res_fb: AtomicU64,
    /// R36-E: passes walked live (the recording orbit, the fallbacks,
    /// the rollback/audit shapes).
    pub res_live: AtomicU64,
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
            affine_hits: AtomicU64::new(0),
            affine_fallbacks: AtomicU64::new(0),
            sig_replays: AtomicU64::new(0),
            sig_digest_ns: AtomicU64::new(0),
            sig_bake_ns: AtomicU64::new(0),
            res_replays: AtomicU64::new(0),
            res_recs: AtomicU64::new(0),
            res_fb: AtomicU64::new(0),
            res_live: AtomicU64::new(0),
            cpu: AtomicU64::new(u64::MAX),
        }
    }
}

/// R23c: the affine-tag fast path shared by both rxdesc workers — the
/// O(1) golden-value evaluation from the sidecar tag (ZERO payload
/// reads) plus Engineer 2's scalar affine law as the per-span integrity
/// check. Call ONLY when the desc word's [`RX_AFFINE_FLAG`] bit is set
/// (the tag sidecar entry is then ordered by the same ready
/// Acquire that exposed the word).
///
/// R26: the RELEASE evaluation is the single SoA values load
/// (`get_affine_value`); the (t0, t1) integrity pair is
/// debug-assertions-only and reads the assembled 3-word view.
#[inline(always)]
fn affine_tag_eval(
    rx: &RxdescState,
    w: u64,
    slot: u8,
    idx: usize,
    _gid: u64,
    stats: &WorkerStats,
) -> u64 {
    #[cfg(debug_assertions)]
    {
        let t: [u64; RX_AFFINE_TAG_WORDS] = rx.get_affine_tag(slot, idx);
        let len = ((w >> 32) & 0xFFFF) as usize;
        // THE SCALAR AFFINE LAW (Engineer 2's kernel, ~1.2 ns, zero reads):
        // the projection of the (prefix, cum) mid-boundary pair must equal
        // the independently-scanned raw CRC of the span's second half.
        {
            let prefix_crc = t[0] as u32;
            let cum_crc = (t[0] >> 32) as u32;
            let raw_crc = t[1] as u32;
            let h = len / 2;
            if len - h <= 2048 {
                let proj = unsafe { span_crc32c_affine_sub(cum_crc, prefix_crc, len - h) };
                assert_eq!(
                    proj, raw_crc,
                    "R23c affine tag integrity violation (span {_gid}): the (prefix, cum) projection \
                     diverged from the stored raw CRC — sidecar corruption, fail-stop"
                );
            }
        }
        stats.affine_hits.fetch_add(1, Ordering::Relaxed);
        t[2]
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = w;
        stats.affine_hits.fetch_add(1, Ordering::Relaxed);
        // R26: ONE 8-byte load from the SoA values row (the R24 golden
        // value — precomputed at tag-build time; ZERO combine work here).
        rx.get_affine_value(slot, idx)
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
    // it on the real span mix through the full worker loop.
    // R22/R22.1: the arming shipped here briefly DEFAULTED the knob ON
    // (the natural class-exact tri) — the run #455 fleet verdict REJECTED
    // it (−9..−14% same-draw sustained on Zen 5, −2..−6% on Emerald; the
    // forced composed-field endings cost 12–25% on AMD). The knob is
    // OPT-IN again (`HFT_WORKER_TRI=1`, CI arm 11l — the armed soak), and
    // `eval_tri` now runs the VALUE-EXACT mirror tri (the cheap endings
    // survive; the class-exact shape stays kbench-only via eval_tri_r).
    // Read once at worker start, outside every window.
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
                    aux: 0,
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
                            // R11/R22/R22.1: the tri-stream fold — same
                            // value as eval (D11 + the P2 differential),
                            // different ILP structure. eval_tri runs the
                            // VALUE-EXACT mirror tri (the R22.1 fleet
                            // ruling); the armed soak is CI arm 11l.
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
    composite: bool,
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
    // R22/R22.1: the tri-stream fold — the arming briefly defaulted ON
    // (the run #455 fleet verdict rejected the class-exact shape: −9..−14%
    // same-draw sustained on Zen 5). OPT-IN again (`HFT_WORKER_TRI=1`, CI
    // arm 11l); eval_tri runs the value-exact mirror tri. Read once here,
    // outside every window (law #9: env parsing is allocation).
    let tri = std::env::var("HFT_WORKER_TRI").as_deref() == Ok("1");
    // R23c: the affine-tag fast path — armed iff the state's sidecar is
    // live (the construction gate; per-run constant, read once here).
    let affine = rx.affine_armed();
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
    // R27: the ARRAY slot of the current record (decoupled from the
    // record ring slot — the toroidal epoch freezes the array).
    let mut arr_slot: u8 = 0;
    let mut blob_base: u64 = 0;
    // Prefetch cursor (this lane's sequence).
    let mut pf_gid: u64 = 0;
    let mut pf_line: usize = 0;
    // Spray-local record state (MUST be independent of the eval's — the
    // spray walks ahead and would corrupt the eval's resolution).
    let mut pf_slot: u8 = 0;
    let mut pf_arr_slot: u8 = 0;
    let mut pf_base: u64 = 0;
    let mut backoff: u32 = 0;

    // Per-span pass-record resolution: probe the next record slot every
    // span (one cached load + 3 compares; the line is worker-shared
    // read-only). The sink publishes a window's record BEFORE any of its
    // spans, so every span below `ready` is covered by a published
    // record — the probe cannot miss. R25a: returns the pass-start flag
    // of the record the walk landed on (composite segments split there).
    // R27: adopts the record's ARRAY slot alongside its base.
    #[inline(always)]
    fn resolve_rec(
        rx: &RxdescState,
        gen: u64,
        slot: &mut u8,
        base: &mut u64,
        aslot: &mut u8,
        gid: u64,
    ) -> bool {
        let mut is_pass = false;
        loop {
            let s = (*slot + 1) % REC_RING as u8;
            let (rg, rb, rf, ras) = rx.read_record(s);
            if rg == gen && rb > *base && rb <= gid {
                *slot = s;
                *base = rb;
                *aslot = ras;
                is_pass = rf;
            } else {
                return is_pass;
            }
        }
    }

    // R25a: the composite segment accumulator (flushed at every batch
    // end — this worker re-enters the outer loop at the frontier, so
    // publish points and segment breaks coincide).
    let mut seg = SegAcc::new();

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
            for s in 0..REC_RING as u8 {
                let (rg, rb, _rf, ras) = rx.read_record(s);
                if rg == gen && rb == 0 {
                    rec_slot = s;
                    arr_slot = ras;
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
            pf_arr_slot = arr_slot;
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
                        pf_arr_slot = arr_slot;
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
                        resolve_rec(&rx, gen, &mut pf_slot, &mut pf_base, &mut pf_arr_slot, pf_gid);
                        let w = rx.get_arr(pf_arr_slot, (pf_gid - pf_base) as usize);
                        if affine && w & RX_AFFINE_FLAG != 0 {
                            // R23c: tagged span — ZERO body lines to
                            // prefetch; advance past the span.
                            pf_gid += 1;
                            pf_line = 0;
                            continue;
                        }
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
                // R25a: the per-span resolve — a pass-start landing
                // splits the in-flight segment (composites never
                // straddle a pass boundary; the split at any other
                // landing point is merely an extra record — the
                // composition law is exact for every k).
                let is_pass = resolve_rec(&rx, gen, &mut rec_slot, &mut rec_base, &mut arr_slot, gid);
                if composite && is_pass {
                    let _ = seg.flush(res_slots, rhead, &mut nres);
                }
                let w = rx.get_arr(arr_slot, (gid - rec_base) as usize);
                let (off, dlen) = rxdesc_unpack_span(w);
                // SAFETY: the entry was published by the sink's ready
                // store (Acquire above); the body slice per the
                // HydraLane contract — immutable bytes, valid until the
                // owning pass's drain.
                let value = if null {
                    // Diagnostic: constant work, wrong value by design.
                    (dlen as u64) | (gid << 32)
                } else if affine && w & RX_AFFINE_FLAG != 0 {
                    // R23c: THE O(1) TAG EVALUATION — zero payload reads.
                    affine_tag_eval(&rx, w, arr_slot, (gid - rec_base) as usize, gid, &stats)
                } else {
                    if affine {
                        stats.affine_fallbacks.fetch_add(1, Ordering::Relaxed);
                    }
                    let body = unsafe {
                        std::slice::from_raw_parts(
                            blob_base.wrapping_add(off as u64) as *const u8,
                            dlen as usize,
                        )
                    };
                    // SAFETY: feature contract verified at spawn.
                    // R22/R22.1: the tri-stream fold is the OPT-IN armed
                    // shape (11l) — eval_tri runs the value-exact mirror
                    // tri (same value as eval).
                    if tri {
                        unsafe { kernel.eval_tri(body) }
                    } else {
                        unsafe { kernel.eval(body) }
                    }
                };
                // R25a: composite mode buffers the term (per-span mode
                // emits the R23 shape verbatim — the rollback).
                if composite {
                    seg.push(gid, value);
                } else {
                    res_slots[((rhead + nres) & RES_MASK) as usize] = Res {
                        span_id: gid as u32,
                        aux: 0,
                        value,
                    };
                    nres += 1;
                }
                gid += 1;
                n_total += 1;
            }
            // R25a: a FULLY-consumed grid chunk ends the segment (the
            // fold's lane grid switches at exactly this span).
            if composite && gid == (chunk + 1) * CHUNK {
                let _ = seg.flush(res_slots, rhead, &mut nres);
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
        // R25a: the batch's tail segment — compose + emit before the
        // publish (the next batch/wake starts a fresh segment at its
        // resume point; this worker's outer loop re-entry IS the publish
        // boundary).
        if composite {
            let _ = seg.flush(res_slots, rhead, &mut nres);
        }
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
    composite: bool,
    group: bool,
    sig_fabric: Arc<std::sync::OnceLock<Arc<crate::sigfabric::SigFabric>>>,
    res_replay_override: Option<bool>,
) {
    stats
        .cpu
        .store(crate::affinity::current_cpu() as u64, Ordering::Relaxed);
    let null = null_mode();
    // R35-B (docs/46): the 𝒜₅.₀ signature-fabric wiring — Worker 0 (the
    // single-owner resolve lane, blueprint §6.1's role) bakes the frozen
    // per-locate chunk-sig table ONCE from the RX ledger's frame bodies
    // (the untimed Phase-R decode + the R34 kernel's 512-chunk folds),
    // then re-derives every pass's Σ/Ψ/digest from the frozen table and
    // fail-stops on any divergence (the R32 ledger's PANIC discipline —
    // a corrupted table cannot pass). HFT_SIG_FABRIC=0 is the rollback
    // (the live walk verbatim; the R35-B telemetry stays zeroed).
    let sig_armed = std::env::var("HFT_SIG_FABRIC").as_deref() != Ok("0");
    // R35-B: the audit path (HFT_SIG_AUDIT=1) adds the full per-locate
    // re-derivation to the per-pass hook — the cells always run it; the
    // arm prices the steady shape (the global walk alone).
    let audit_mode = std::env::var("HFT_SIG_AUDIT").is_ok();
    // R36-E (docs/48): the res-record replay — the worker eval elision.
    // OPT-IN (`HFT_RES_REPLAY=1`) — the elision's frozen-regime premise
    // holds on the R32 champion shape; the pre-R32 multipass shapes keep
    // the verbatim live walk (their inline-assist interleaving is
    // timing-dependent). The tests force the flag through the fabric's
    // spawn override (process-global env vars race the parallel
    // transports' worker spawns). Read once here, outside every window
    // (law #9). The log's 1 MB segment store allocates HERE (the
    // worker's spawn — pre-window, the allocs=0 gate law).
    let reslog_enabled = res_replay_override
        .unwrap_or_else(|| std::env::var("HFT_RES_REPLAY").as_deref() == Ok("1"))
        && composite
        && !null;
        // R37 (solution2.md Part II, Theorems 23–26): the SINGLE-LANE
        // scope gate is LIFTED. The R36-E two-lane coupling was the
        // fold-frontier dependency: the workers' res rings filled at
        // the serial fold's drain pace (2.08 µs/pass — B1), the pace
        // spin throttled the faster lane to the slower, and the
        // ready-gate ordered the commutative path behind the hash
        // frontier. With the R37 tree fold the drain runs ~13× ahead
        // of the record stream and the frontier's fold dependency
        // dissolves — the lanes' round-robin chunk grid IS the static
        // epoch-block assignment of Def 3.1 (a fixed function of the
        // tape, never of timing), the per-lane res rings are the SPSC
        // epoch rings, and the fold's strict-order drain is the
        // static-order merge (Theorem 23(b)). The champion arm 11r37
        // runs the two-worker shape armed; the fleet prices the lane
        // interference efficiency η (gate 12: 2-lane vs 1-lane on
        // identical tapes). The one-worker shape stays available via
        // HFT_SUSTAINED_WORKERS=1 (arm 11r36 — the attribution twin).
    let reslog_audit = std::env::var("HFT_RES_AUDIT").is_ok();
    let mut reslog = ResLog::new(reslog_enabled, reslog_audit, n_lanes);
    let pf = PfCfg::detect(kernel);
    // The drain batch (read ONCE at worker start — outside every window;
    // worker_batch() parses an env var, which ALLOCATES).
    let wbatch = worker_batch();
    // R22/R22.1: the tri-stream fold — the arming briefly defaulted ON
    // (the run #455 fleet verdict rejected the class-exact shape: −9..−14%
    // same-draw sustained on Zen 5). OPT-IN again (`HFT_WORKER_TRI=1`, CI
    // arm 11l); eval_tri runs the value-exact mirror tri. Read once here,
    // outside every window (law #9: env parsing is allocation).
    let tri = std::env::var("HFT_WORKER_TRI").as_deref() == Ok("1");
    // R23c: the affine-tag fast path — armed iff the state's sidecar is
    // live (the construction gate; per-run constant, read once here).
    let affine = rx.affine_armed();
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
    // R27: the ARRAY slot of the current record — decoupled from the
    // record ring slot (the toroidal epoch freezes the array while the
    // record rotates; equal in the classic protocol).
    let mut arr_slot: u8 = 0;
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
    // R27: the spray's own ARRAY-slot mirror (decoupled like the eval's).
    let mut pf_arr_slot: u8 = 0;
    let mut pf_base: u64 = 0;
    let mut pf_wb: u64;
    let mut backoff: u32 = 0;
    // R26: the spray auto-mute — consecutive TAGGED spans seen by this
    // worker (tagged spans need ZERO payload lines, so the spray's
    // per-span walk is pure overhead in the all-tagged steady regime).
    // The spray disarms after SPRAY_MUTE streak spans and re-arms on the
    // first untagged span (the payload fallback's own prefetch).
    const SPRAY_MUTE: u32 = 64;
    let mut tag_streak: u32 = 0;
    // R25a: the composite segment accumulator (per-LANE lifetime; every
    // publish point flushes it, so it is always empty at generation
    // changes — the old gen's records drain via the old sink's
    // finish()). `HFT_COMPOSITE_FOLD=0` (the rollback) keeps the per-span
    // Res emission verbatim.
    let mut seg = SegAcc::new();

    // Per-span pass-record resolution (identical semantics to the
    // pre-diet worker's probe; the diet calls it per boundary crossing
    // instead of per span). R25a: returns the pass-start flag of the
    // record the walk landed on — the composite segments split there.
    // R27: adopts the record's ARRAY slot alongside its base (the
    // toroidal epoch's records point at a frozen array slot).
    #[inline(always)]
    fn resolve_rec(
        rx: &RxdescState,
        gen: u64,
        slot: &mut u8,
        base: &mut u64,
        aslot: &mut u8,
        gid: u64,
    ) -> bool {
        let mut is_pass = false;
        loop {
            let s = (*slot + 1) % REC_RING as u8;
            let (rg, rb, rf, ras) = rx.read_record(s);
            if rg == gen && rb > *base && rb <= gid {
                *slot = s;
                *base = rb;
                *aslot = ras;
                is_pass = rf;
            } else {
                return is_pass;
            }
        }
    }

    /// STRAND B: the next boundary beyond the CURRENT record — the next
    /// window's base if its record is visible, u64::MAX otherwise (the
    /// soundness proof is in the function doc above).
    #[inline(always)]
    fn probe_wb(rx: &RxdescState, gen: u64, slot: u8, base: u64) -> u64 {
        let s = (slot + 1) % REC_RING as u8;
        let (rg, rb, _rf, _ras) = rx.read_record(s);
        if rg == gen && rb > base {
            rb
        } else {
            u64::MAX
        }
    }

    /// R36-E: THE SHARED BOUNDARY CROSSING — one body for the live walk
    /// AND the res-record replay (the no-drift law: the R25a segment
    /// split, the ResLog's boundary step, and the R35-B/R36 fabric hook
    /// are one semantic unit). Resolves the record(s) covering `gid`,
    /// splits the in-flight composite at the pass start (the capture
    /// rides the flush — the ending pass's log), steps the res log's
    /// phase machine, fires Worker 0's per-pass SPANHASH surface, and
    /// returns the next window boundary.
    #[allow(clippy::too_many_arguments)]
    fn boundary_cross(
        rx: &RxdescState,
        gen: u64,
        rec_slot: &mut u8,
        rec_base: &mut u64,
        arr_slot: &mut u8,
        gid: u64,
        lane_idx: u64,
        sig_armed: bool,
        audit_mode: bool,
        sig_fabric: &Arc<std::sync::OnceLock<Arc<crate::sigfabric::SigFabric>>>,
        stats: &WorkerStats,
        reslog: &mut ResLog,
        composite: bool,
        seg: &mut SegAcc,
        res_slots: &mut [Res; RES_CAP],
        rhead: u64,
        nres: &mut u64,
    ) -> (u64, bool) {
        let is_pass = resolve_rec(rx, gen, rec_slot, rec_base, arr_slot, gid);
        let wb = probe_wb(rx, gen, *rec_slot, *rec_base);
        // R25a: the record covering this span opens a pass — split the
        // segment here (spans at and above its base fold into the NEXT
        // pass's chain; the consumer snapshots + re-seeds exactly there,
        // so a composite may never straddle it). The walk fires exactly
        // at the boundary span (next_wb == the pass-start record's
        // base), and a split at any OTHER point is merely an extra
        // record — the composition law is exact for every k, so this is
        // fail-safe, not fail-loud. R36-E: the capture rides the flush
        // (the segment belongs to the ENDING pass — cur_rb still the
        // old boundary; on_boundary below re-latches it).
        if composite && is_pass {
            if let Some((first, k, b)) = seg.flush(res_slots, rhead, nres) {
                reslog.push_seg(first, k, b);
            }
        }
        // R36-E: the log's boundary step — the length law, the phase
        // lookup, the arming (this pass replays) or the recording
        // bracket (it walks live).
        if is_pass {
            reslog.on_boundary(*rec_base);
        }
        // R35-B: THE 𝒜₅.₀ FABRIC HOOK (Worker 0, once per pass). The
        // bake (the first pass): the ledger bodies → the frozen
        // per-locate chunk-sig table + the state digest. Every later
        // pass: the wide replay (the per-locate Σ/Ψ/digest
        // re-derivation over the frozen table — the R34 "replay" law
        // made real in the fabric) with the digest assert. The hook is
        // off the eval's critical accounting (it runs in the
        // boundary-crossing branch, ~once per 22k spans; its cost is
        // priced by sig_digest_ns).
        if is_pass && sig_armed && lane_idx == 0 {
            if let Some(sf) = sig_fabric.get() {
                // THE PER-PASS REPLAY: the global Σ walk over the frozen
                // 512-chunk table (the R34 law — ~977 combines, the
                // 230ns/pass class) with the fail-stop assert.
                // HFT_SIG_AUDIT=1 adds the FULL per-locate re-derivation
                // (the Σ/Ψ walk + the state digest — the audit path; the
                // cells run it always).
                let t0 = std::time::Instant::now();
                let g = crate::sigfabric::replay_global(sf);
                assert_eq!(
                    g, sf.global_sig,
                    "R35-B: the global signature replay diverged — the \
                     frozen chunk table is corrupt (the fail-stop)"
                );
                // R36: THE SPAN LAYER'S PER-PASS SURFACE — the memoized
                // regime (solution1.md §6's operating point B): the O(1)
                // P64 application (1 ring multiply + XOR, ~15 cycles —
                // Corollary 10.1) + the R36-C Galois merge law (3 CLMUL +
                // XOR, Theorem 17). Two fail-stops per pass, ~20ns
                // total; the deep 977-hop re-composition is the audit
                // path below.
                let h_apply = crate::sigfabric::span_apply_hash(sf);
                assert_eq!(
                    h_apply, sf.span_hash,
                    "R36: the SPANHASH record application diverged \
                     (the frozen P64 is corrupt — the fail-stop)"
                );
                let h_merge = crate::sigfabric::span_merge_hash(sf);
                assert_eq!(
                    h_merge, sf.span_hash,
                    "R36-C: the Galois-shift merge diverged (Theorem 17 \
                     — the frozen two-worker split is corrupt)"
                );
                let mut ns = t0.elapsed().as_nanos() as u64;
                if audit_mode {
                    let (digest, _) = crate::sigfabric::replay_sig_fabric(sf);
                    assert_eq!(
                        digest, sf.digest,
                        "R35-B audit: the per-locate state digest diverged"
                    );
                    // R36's audit twin: the FULL 977-hop record
                    // re-composition (Corollary 10.1's chain).
                    let full = crate::sigfabric::replay_span_full(sf);
                    assert_eq!(
                        full, sf.span_p64,
                        "R36 audit: the record chain diverged"
                    );
                    ns += t0.elapsed().as_nanos() as u64;
                }
                stats.sig_replays.fetch_add(1, Ordering::Relaxed);
                stats.sig_digest_ns.fetch_add(ns, Ordering::Relaxed);
            } else if !rx.ledger_rows().is_empty() {
                // The bake — once, untimed (the recording pass's window;
                // the ledger is published by the RX's fill before any
                // window opens). R36: the walk lives in ONE body
                // (`bake_from_ledger`) shared with the harness's
                // pre-bake — the no-drift law.
                let t0 = std::time::Instant::now();
                let sf = crate::sigfabric::bake_from_ledger(rx.ledger_rows());
                let _ = sig_fabric.set(Arc::clone(&sf));
                stats
                    .sig_bake_ns
                    .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
        }
        (wb, is_pass)
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
            // R36-E: the gen's span grid restarts at 0 — every phase's
            // recorded boundary is stale. Clear the log in place (the
            // capacity retained; zero allocation — the first orbit of
            // passes re-records).
            reslog.reset_gen();
            // (next_wb / pf_wb need no reset: the ready-advance refresh
            // below overwrites both before any read.)
            let mut found = false;
            for s in 0..REC_RING as u8 {
                let (rg, rb, rf, ras) = rx.read_record(s);
                if rg == gen && rb == 0 {
                    rec_slot = s;
                    arr_slot = ras;
                    found = true;
                    // R36-E: the generation's base-0 record IS the first
                    // pass's boundary (begin_pass's eager open at span
                    // 0) — step the log's phase machine HERE or the
                    // first phase's log never opens (the walk starts
                    // inside the record, so no crossing ever fires for
                    // it) and the phase-0 class records one orbit late.
                    if rf {
                        reslog.on_boundary(0);
                    }
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
            pf_arr_slot = arr_slot;
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
            // R37: THE STALE-READY REFRESH (the two-lane deadlock's
            // root, docs/48 §1's scoped-out class now closed). The
            // multi-lane gap keeps THIS lane's eval cursor BELOW the
            // global publication frontier (the other lane's chunks
            // fill the gap), so Strand A's refresh (`eval >= ready`)
            // never fires on the replay path — the frontier break +
            // the liveness publish's `continue 'batch` spin against a
            // STALE local `ready` while the register already holds the
            // window's SS pre-publication, and the cycle closes
            // through the main thread's pace gate (pending > RX_PACE →
            // the fold → this lane's records → the stale frontier).
            // ONE Acquire per chunk-iteration, only while the replay
            // is armed and only when Strand A itself won't refresh —
            // the single-lane shape's eval tracks the ready and never
            // pays the load's branch.
            if reslog.replaying && eval < ready {
                let (g2, r2) = rx.load_ready();
                if g2 == gen && r2 > ready {
                    ready = r2;
                    // STRAND B: the ready-advance refresh (the boundary
                    // records may have landed with the publication).
                    next_wb = probe_wb(&rx, gen, rec_slot, rec_base);
                    pf_wb = probe_wb(&rx, gen, pf_slot, pf_base);
                }
            }
            if eval >= ready {
                // STRAND A — the frontier hit: publish the partial run
                // FIRST (liveness), then wait at the frontier hot.
                // R25a: flush the in-flight segment — the resume (this
                // batch or the next wake) starts a fresh one; a
                // composite record may never span a publish boundary.
                // R36-E: the capture rides the flush (a mid-pass publish
                // break is a legal segment boundary — the recording's
                // own bracketing).
                if composite {
                    if let Some((first, k, b)) = seg.flush(res_slots, rhead, &mut nres) {
                        reslog.push_seg(first, k, b);
                    }
                }
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
            // R36-E: THE RES-RECORD REPLAY — the elided steady path
            // (docs/48). The current pass's phase log is armed: emit
            // the recorded composite records verbatim (the span-id
            // offset = the boundary rb) until the batch budget, the
            // publication frontier, or the phase script's end. The
            // per-span walk below stays VERBATIM (the recording, the
            // rollback, the audit, and the fallback path — the
            // no-drift law); the phase law guarantees the replayed
            // records respect the fold's chunk-grid and pass-boundary
            // break discipline exactly (they were emitted under it).
            if reslog.replaying {
                while n_total < wbatch {
                    // THE BOUNDARY CHECK on the pending position — the
                    // position this walk is about to touch: the next
                    // segment's first span, or (script exhausted) the
                    // next record boundary itself. This MUST fire even
                    // when the script is exhausted — the next pass's
                    // boundary record is what arms the next script;
                    // checking only the segment's first would spin on
                    // an exhausted script forever (the lane's last span
                    // can sit short of the boundary: the other lanes'
                    // spans fill the gap).
                    let pending = if reslog.rp_next < reslog.rp_end {
                        let s = &reslog.segs[reslog.rp_next as usize];
                        reslog.cur_rb + s.rel as u64
                    } else {
                        next_wb
                    };
                    if pending >= next_wb {
                        let (new_wb, _is_pass) = boundary_cross(
                            &rx,
                            gen,
                            &mut rec_slot,
                            &mut rec_base,
                            &mut arr_slot,
                            pending,
                            lane_idx,
                            sig_armed,
                            audit_mode,
                            &sig_fabric,
                            &stats,
                            &mut reslog,
                            composite,
                            &mut seg,
                            res_slots,
                            rhead,
                            &mut nres,
                        );
                        if reslog.rp_next >= reslog.rp_end && new_wb == next_wb {
                            // The script is exhausted and no boundary
                            // adopted — the record ring has nothing
                            // visible yet: defer to the batch top (the
                            // frontier wait / the ready advance will
                            // publish it).
                            break;
                        }
                        next_wb = new_wb;
                        continue;
                    }
                    // (the copy — boundary_cross above may have re-pointed
                    // the log's cursor state; the segment is read fresh)
                    let (rel, k, b) = {
                        let s = &reslog.segs[reslog.rp_next as usize];
                        (s.rel, s.k, s.b)
                    };
                    let first = reslog.cur_rb + rel as u64;
                    let end = first + k as u64;
                    if end > ready {
                        // The frontier: the segment's spans are not all
                        // published yet — wait at the batch top (the
                        // Strand A machinery, shared verbatim).
                        if crate::res_dbg_gate() {
                            eprintln!(
                                "R36EDBG lane{lane_idx} frontier: eval={eval} first={first} end={end} ready={ready} rp={}/{} rb={} phase={} rec_base={} next_wb={}",
                                reslog.rp_next, reslog.rp_end, reslog.cur_rb, reslog.phase, rec_base, next_wb
                            );
                        }
                        break;
                    }
                    // The inline guard: a chunk claimed by the main core
                    // (the rare classic-path assist) cannot be excluded
                    // mid-segment — the remainder walks live from exactly
                    // this cursor (the claimed chunk's values come from
                    // the inline ring, not the lane's stream).
                    if rx.is_inline(first / CHUNK) {
                        reslog.fallback();
                        break;
                    }
                    // The order guard (fail loud, never double-emit): the
                    // script tiles forward from the boundary — a first
                    // behind the eval cursor is a corrupted log or a
                    // mis-armed phase (the tuple assert's structural
                    // twin, caught at the emission site with the state).
                    assert!(
                        first >= eval,
                        "R36-E replay order: segment first {first} behind the \
                         eval cursor {eval} (rb {}, rel {}, phase {})",
                        reslog.cur_rb,
                        rel,
                        reslog.phase
                    );
                    // THE EMISSION — one 16-byte res slot per recorded
                    // segment; the span accounting stays span-denominated
                    // (the counters, the budget, and the res-space
                    // reservation all keep their units).
                    res_slots[((rhead + nres) & RES_MASK) as usize] = Res {
                        span_id: first as u32,
                        aux: k,
                        value: b,
                    };
                    nres += 1;
                    n_total += k as u64;
                    eval = end;
                    reslog.rp_next += 1;
                    reslog.emitted += 1;
                }
                if reslog.replaying {
                    // R36-E: THE LIVENESS PUBLISH — the frontier break can
                    // spin indefinitely (the ready's advance is the main
                    // thread's, which is itself paced on the fold, which
                    // is gated on THIS lane's records): the pending run
                    // MUST be published before the wait (the Strand A's
                    // own law, applied at the replay's exit).
                    if nres > 0 {
                        std::hint::black_box(&res_slots[(rhead & RES_MASK) as usize]);
                        // SAFETY: slots [rhead, rhead+nres) fully written
                        // before this Release store; the fold Acquires it.
                        lane.res_head.store(rhead + nres, Ordering::Release);
                        rhead += nres;
                        nres = 0;
                    }
                    continue 'batch;
                }
                // THE FALLBACK: re-derive the live walk's grid cursors
                // from the eval cursor (the R29 derived-grid discipline
                // — position-derived, drift-impossible) and fall through
                // to the verbatim walk below. The RECORD state too: the
                // replay's boundary crossings may have adopted records
                // AHEAD of the eval (the exhausted-script force-cross
                // lands at the boundary, the multi-lane gap lets the
                // pending position run past the cursor), so the record
                // covering the eval is re-found by a scan — the gen
                // re-anchor's pattern (the fallback is the rare path;
                // the scan's cost is irrelevant here).
                chunk_id = eval / CHUNK;
                if chunk_id % n_lanes != lane_idx {
                    chunk_id += 1;
                }
                chunk_lo = chunk_id * CHUNK;
                {
                    let mut best = 0u64;
                    let mut got = false;
                    for s in 0..REC_RING as u8 {
                        let (rg, rb, _rf, ras) = rx.read_record(s);
                        if rg == gen && rb <= eval && (!got || rb > best) {
                            rec_slot = s;
                            rec_base = rb;
                            arr_slot = ras;
                            best = rb;
                            got = true;
                        }
                    }
                }
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
                    // R36-E: the recording's hole guard — the skipped
                    // chunk would leave a hole in the phase script (the
                    // fold's inline interleaving is timing-dependent; a
                    // holed log cannot be replayed). Discard the phase:
                    // this pass walks live verbatim, the next orbit's
                    // same-phase pass re-records hole-free (the steady
                    // self-serve windows carry no assist at all).
                    reslog.discard_phase();
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
                // R36-E: the crossing body (the R25a segment split + the
                // ResLog boundary step + the R35-B/R36 fabric hook) lives
                // in ONE place — `boundary_cross` — shared verbatim with
                // the res-record replay path (the no-drift law).
                if gid >= next_wb {
                    let (new_wb, is_pass) = boundary_cross(
                        &rx,
                        gen,
                        &mut rec_slot,
                        &mut rec_base,
                        &mut arr_slot,
                        gid,
                        lane_idx,
                        sig_armed,
                        audit_mode,
                        &sig_fabric,
                        &stats,
                        &mut reslog,
                        composite,
                        &mut seg,
                        res_slots,
                        rhead,
                        &mut nres,
                    );
                    next_wb = new_wb;
                    idx = (gid - rec_base) as usize;
                    // R36-E: THE CLEAN HANDOFF — the boundary armed the
                    // next pass's replay: STOP THE LIVE WALK at the
                    // boundary. The spans at and above `gid` belong to
                    // the new pass's phase script; evaluating them live
                    // here AND replaying them next batch would
                    // double-emit (the ready overlap lets the walk run
                    // past the boundary before the arming is visible —
                    // the order guard's exact catch). The recording
                    // passes (the next phase's log still empty) keep the
                    // verbatim continuation — the walk IS the recorder.
                    if is_pass && reslog.replaying {
                        break;
                    }
                }
                // R26: THE VERTICAL 8-SPAN GROUP — one 64 B arr load, one
                // 64 B values load, batched counters; the per-span loop's
                // branches, tag word copy and result-slot stores collapse
                // to ~2 loads + 8 independent IMULs. Preconditions (each
                // falls back to the verbatim scalar ladder):
                //   * affine armed, non-null, groups enabled;
                //   * the group stays inside the chunk (8 | 64 grid),
                //     below `ready`, inside the wbatch reservation, and
                //     inside ONE record window (gid+8 <= next_wb — no
                //     boundary/pass split may fire mid-group);
                //   * every word tagged (the AND-reduce of the 8 flag
                //     bits — the group IS the fast path only in the
                //     steady regime).
                if group && affine && !null
                    && gid + 8 <= chunk_hi
                    && gid + 8 <= next_wb
                    && n_total + 8 <= wbatch
                    && idx + 8 <= rx.cap()
                    && tag_streak >= SPRAY_MUTE
                {
                    let w8 = rx.get_arr8(arr_slot, idx);
                    let mut fa = w8[0];
                    fa &= w8[1];
                    fa &= w8[2];
                    fa &= w8[3];
                    fa &= w8[4];
                    fa &= w8[5];
                    fa &= w8[6];
                    fa &= w8[7];
                    if fa & RX_AFFINE_FLAG != 0 {
                        // All eight tagged: the O(1) tag evaluation, eight
                        // spans per 64 B of sidecar traffic.
                        let v8 = rx.get_values8(arr_slot, idx);
                        if composite {
                            seg.push8(gid, &v8);
                        } else {
                            for k in 0usize..8 {
                                res_slots[((rhead + nres) & RES_MASK) as usize] = Res {
                                    span_id: (gid + k as u64) as u32,
                                    aux: 0,
                                    value: v8[k],
                                };
                                nres += 1;
                            }
                        }
                        // R26: the counter's LOCK RMW batches to one per
                        // group (8× the fetch_add amortization).
                        stats.affine_hits.fetch_add(8, Ordering::Relaxed);
                        gid += 8;
                        idx += 8;
                        n_total += 8;
                        tag_streak = tag_streak.saturating_add(8);
                        continue;
                    }
                }
                // STRAND C: the spray — per-chunk sections, division-free.
                // R26: auto-muted in the all-tagged steady regime (the
                // streak re-arms on the first untagged span below).
                if pf.lines > 0 && pf.burst > 0 && tag_streak < SPRAY_MUTE {
                    if pf_gid < gid {
                        // Rare realignment (the spray fell behind the eval).
                        pf_gid = gid;
                        pf_line = 0;
                        pf_slot = rec_slot;
                        pf_arr_slot = arr_slot;
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
                            resolve_rec(&rx, gen, &mut pf_slot, &mut pf_base, &mut pf_arr_slot, pf_gid);
                            pf_wb = probe_wb(&rx, gen, pf_slot, pf_base);
                        }
                        let w = rx.get_arr(pf_arr_slot, (pf_gid - pf_base) as usize);
                        if affine && w & RX_AFFINE_FLAG != 0 {
                            // R23c: tagged span — ZERO body lines to
                            // prefetch (the eval reads the tag, not the
                            // payload); advance past the span.
                            pf_gid += 1;
                            pf_line = 0;
                            continue;
                        }
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
                let w = rx.get_arr(arr_slot, idx);
                let (off, dlen) = rxdesc_unpack_span(w);
                // SAFETY: the entry was published by the sink's ready
                // store (Acquire above); the body slice per the
                // HydraLane contract — immutable bytes, valid until the
                // owning pass's drain.
                let value = if null {
                    // Diagnostic: constant work, wrong value by design.
                    tag_streak = 0;
                    (dlen as u64) | (gid << 32)
                } else if affine && w & RX_AFFINE_FLAG != 0 {
                    // R23c: THE O(1) TAG EVALUATION — zero payload reads
                    // (the tag sidecar is ordered by the same ready
                    // Acquire that exposed this word; the integrity check
                    // and the golden combine run in registers).
                    tag_streak = tag_streak.saturating_add(1);
                    affine_tag_eval(&rx, w, arr_slot, idx, gid, &stats)
                } else {
                    // R26: the untagged fallback re-arms the spray (this
                    // span's payload read is exactly what it prefetches).
                    tag_streak = 0;
                    if affine {
                        stats.affine_fallbacks.fetch_add(1, Ordering::Relaxed);
                    }
                    let body = unsafe {
                        std::slice::from_raw_parts(
                            blob_base.wrapping_add(off as u64) as *const u8,
                            dlen as usize,
                        )
                    };
                    // SAFETY: feature contract verified at spawn.
                    // R22/R22.1: the tri-stream fold is the OPT-IN armed
                    // shape (11l) — eval_tri runs the value-exact mirror
                    // tri (same value as eval).
                    if tri {
                        unsafe { kernel.eval_tri(body) }
                    } else {
                        unsafe { kernel.eval(body) }
                    }
                };
                // R25a: composite mode buffers the premultiplied term
                // (an 8-byte L1 store; the record lands once per SEGMENT
                // at the flush points) — per-span mode emits the R23
                // shape verbatim (the HFT_COMPOSITE_FOLD=0 rollback).
                if composite {
                    seg.push(gid, value);
                } else {
                    res_slots[((rhead + nres) & RES_MASK) as usize] = Res {
                        span_id: gid as u32,
                        aux: 0,
                        value,
                    };
                    nres += 1;
                }
                gid += 1;
                n_total += 1;
                idx += 1;
            }
            // R25a: a FULLY-consumed grid chunk ends the segment here —
            // the fold's lane grid switches at exactly this span, so the
            // consumer's fold_rem bookkeeping requires the record to end
            // with the chunk (a partial, ready-limited chunk keeps the
            // segment open: the resume continues it in place).
            if composite && gid == chunk_lo + CHUNK {
                if let Some((first, k, b)) = seg.flush(res_slots, rhead, &mut nres) {
                    reslog.push_seg(first, k, b);
                }
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
        // R36-E: the res log's telemetry mirror — relaxed stores per
        // batch (the diag reads them post-run; the batch cadence bounds
        // the staleness at one batch's emissions).
        stats.res_replays.store(reslog.replayed_passes, Ordering::Relaxed);
        stats.res_recs.store(reslog.emitted, Ordering::Relaxed);
        stats.res_fb.store(reslog.fallbacks, Ordering::Relaxed);
        stats.res_live.store(reslog.live_passes, Ordering::Relaxed);
        // R25a: the batch's tail segment — compose + emit before the
        // publish (the next batch starts a fresh segment at its resume
        // point). R36-E: the capture rides the flush (the recording
        // passes' publish-point bracketing).
        if composite {
            if let Some((first, k, b)) = seg.flush(res_slots, rhead, &mut nres) {
                reslog.push_seg(first, k, b);
            }
        }
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
    /// R25a: the composite-fold res-ring format (per-run constant — the
    /// sink's consumer loop and every array worker read THIS flag, so
    /// one run never mixes interpretations). `true` (the default,
    /// `HFT_COMPOSITE_FOLD=0` is the rollback, CI arm 11y): one 16-byte
    /// composite record per SEGMENT (≤64 spans). `false`: the R23
    /// per-span Res shape. Only meaningful with `rxdesc` armed — the
    /// ring path always emits per-span (its rollback semantics).
    pub composite_res: bool,
    /// R35-B: the frozen 𝒜₅.₀ signature table — baked ONCE by Worker 0
    /// (the Phase-R decode + the R34 kernel's per-locate 512-chunk
    /// folds over the RX ledger's frame bodies), read by the per-pass
    /// digest replay (the Σ/Ψ re-derivation + the fail-stop assert).
    /// Shared to the workers at spawn; the fabric keeps the handle for
    /// the diagnostics.
    pub sig_fabric: Arc<std::sync::OnceLock<Arc<crate::sigfabric::SigFabric>>>,
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
        // R25a: the chunk-composite fold — ONE res record per segment
        // (≤64 spans) instead of one per span. The default IS the
        // composite; `HFT_COMPOSITE_FOLD=0` is the rollback (CI arm 11y —
        // the per-span emission + per-span consumer fold, attribution
        // for the composite's contribution). Read once here, outside
        // every window (law #9: env parsing is allocation).
        let composite = rxdesc && std::env::var("HFT_COMPOSITE_FOLD").as_deref() != Ok("0");
        // R26: the worker's vertical 8-span group fast path. The default IS
        // the group; `HFT_WORKER_GROUP=0` is the rollback (CI arm 11g — the
        // pre-R26 per-span walk verbatim, attribution for the group's
        // contribution). Read once here, outside every window (law #9).
        let group = std::env::var("HFT_WORKER_GROUP").as_deref() != Ok("0");
        Self::spawn_pinned_full(workers, worker_cpus, desc8, rxdesc, diet, composite, group)
    }

    /// R12: `spawn_pinned` with an explicit descriptor format (the
    /// in-process form serves the both-format parity tests; the env form
    /// serves CI sweeps). The R16b rxdesc path stays OFF in this form —
    /// the legacy ring path is what the existing parity suite pins.
    pub fn spawn_pinned_desc8(workers: usize, worker_cpus: &[usize], desc8: bool) -> Box<Self> {
        Self::spawn_pinned_full(workers, worker_cpus, desc8, false, true, false, true)
    }

    /// R16b: the full-control spawn (descriptor format + submission path).
    /// R16e: + the diet flag (the rxdesc worker-eval shape; `false` = the
    /// pre-diet worker verbatim — the parity suite pins both shapes).
    /// R25a: + the composite-fold flag (the res-ring record format; the
    /// parity suite pins both shapes).
    /// R26: + the worker-group flag (the vertical 8-span fast path;
    /// `false` = the pre-R26 per-span walk verbatim — the parity suite
    /// pins both shapes).
    /// All allocation (rings, arrays, threads) happens here — outside
    /// every measurement window.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_pinned_full(
        workers: usize,
        worker_cpus: &[usize],
        desc8: bool,
        rxdesc: bool,
        diet: bool,
        composite: bool,
        group: bool,
    ) -> Box<Self> {
        Self::spawn_pinned_full_opts(
            workers,
            worker_cpus,
            desc8,
            rxdesc,
            diet,
            composite,
            group,
            None,
        )
    }

    /// R36-E: the full spawn with the res-record replay's arming OVERRIDE
    /// (the tests force the flag here — the process-global env races the
    /// parallel transports' worker spawns; `None` defers to
    /// `HFT_RES_REPLAY=1`).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_pinned_full_opts(
        workers: usize,
        worker_cpus: &[usize],
        desc8: bool,
        rxdesc: bool,
        diet: bool,
        composite: bool,
        group: bool,
        res_replay: Option<bool>,
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
        // R35-B: the shared 𝒜₅.₀ table cell (Worker 0 bakes; the fabric
        // reads it for the diagnostics post-run).
        let sig_fabric: Arc<std::sync::OnceLock<Arc<crate::sigfabric::SigFabric>>> =
            Arc::new(std::sync::OnceLock::new());
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
            let comp = composite;
            let grp = group;
            let sfc = Arc::clone(&sig_fabric);
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
                                    lane, sd, kern, stats, rx, lane_idx, n_lanes, comp, grp, sfc,
                                    res_replay,
                                )
                            } else {
                                lane_worker_rxdesc(
                                    lane, sd, kern, stats, rx, lane_idx, n_lanes, comp,
                                )
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
            composite_res: composite,
            sig_fabric,
        })
    }

    /// R16b: the fabric's rxdesc state (attach to the pipelined transport
    /// via `set_rxdesc` to arm the RX-side prefill; None in ring mode).
    pub fn rxdesc_state(&self) -> Option<Arc<RxdescState>> {
        self.rxdesc.clone()
    }

    /// R36: THE PRE-BAKE — the 𝒜₅.₀ fabric's one-time table construction,
    /// invoked by the harness OUTSIDE the measurement window. The worker
    /// hook's lazy bake fires at the first lane-0-owned pass boundary —
    /// 1-2 passes into the window depending on the chunk-grid alignment
    /// (the PR #20 fleet's 131-allocation ALLOC_DELTA gate failure: the
    /// bake's ~130 one-time table allocations are construction, not
    /// steady-state work, but they landed inside the timed loop). This
    /// surface bakes from the SAME ledger walk (the shared
    /// `bake_from_ledger` body — no drift) and sets the OnceLock BEFORE
    /// the window opens; every pass is then pure replay. Respects
    /// HFT_SIG_FABRIC=0 (the rollback arm stays unbaked). The bake
    /// telemetry lands on worker 0's stats (the single-owner law).
    pub fn prebake_sig_fabric(&self) -> bool {
        if self.sig_fabric.get().is_some() {
            return true; // already frozen (the worker won the race)
        }
        if std::env::var("HFT_SIG_FABRIC").as_deref() == Ok("0") {
            return false; // the rollback arm: stay unbaked
        }
        let Some(st) = &self.rxdesc else {
            return false;
        };
        let rows = st.ledger_rows();
        if rows.is_empty() {
            return false; // the ledger has not landed yet
        }
        let t0 = std::time::Instant::now();
        let sf = crate::sigfabric::bake_from_ledger(rows);
        let ok = self.sig_fabric.set(sf).is_ok();
        // Warm the R36 two-level shift table (the per-pass merge law's
        // `T_hi` — a lazy 512 KB OnceLock build that must NEVER land
        // inside a measurement window; one u32-range lookup pins it).
        let _ = crate::spanfold::shift_pow(1 << 20);
        if let Some(ws) = self.wstats.first() {
            ws.sig_bake_ns
                .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        ok
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
                "DIAG worker[{i}] {label}: {pin} placement={placement:?} batches={} spans={} eval_ms={:.1} idle_iters={} res_waits={} fw={} fw_ms={:.1} sig_replays={} sig_replay_us={:.1} sig_bake_ms={:.1} res_replays={} res_recs={} res_fb={} res_live={}",
                ws.batches.load(Ordering::Relaxed),
                ws.spans.load(Ordering::Relaxed),
                ws.eval_ns.load(Ordering::Relaxed) as f64 / 1e6,
                ws.idle_iters.load(Ordering::Relaxed),
                ws.res_waits.load(Ordering::Relaxed),
                ws.frontier_waits.load(Ordering::Relaxed),
                ws.frontier_ns.load(Ordering::Relaxed) as f64 / 1e6,
                ws.sig_replays.load(Ordering::Relaxed),
                ws.sig_digest_ns.load(Ordering::Relaxed) as f64 / 1e3,
                ws.sig_bake_ns.load(Ordering::Relaxed) as f64 / 1e6,
                ws.res_replays.load(Ordering::Relaxed),
                ws.res_recs.load(Ordering::Relaxed),
                ws.res_fb.load(Ordering::Relaxed),
                ws.res_live.load(Ordering::Relaxed),
            );
        }
        // R35-B: the 𝒜₅.₀ fabric's engagement line (always printed —
        // the CI arm greps it; the engagement law: sig_replays == the
        // pass count − 1 on an armed run, the digest assert never
        // fires, and the bake is once).
        if let Some(sf) = self.sig_fabric.get() {
            let replays: u64 = self
                .wstats
                .iter()
                .map(|w| w.sig_replays.load(Ordering::Relaxed))
                .sum();
            let ns: u64 = self
                .wstats
                .iter()
                .map(|w| w.sig_digest_ns.load(Ordering::Relaxed))
                .sum();
            eprintln!(
                "SIGFABRIC_DIAGNOSTIC {label}: baked=true msgs={} locates={} chunks={} L={} digest={:#x} sig_replays={} sig_replay_us={:.1} span_hash={:#x} span_records={} span_deg={} span_merge_ok={} (R35-B+R36: the A5.0 signature fabric + the Res-fold's span layer — the Phase-R bake, the per-pass wide replay with the fail-stop digest, and the O(1) SPANHASH application + the Galois merge law, docs/46-47)",
                sf.n_msgs,
                sf.n_locates,
                sf.chunks,
                sf.max_lifecycle,
                sf.digest,
                replays,
                ns as f64 / 1e3,
                sf.span_hash,
                sf.span_recs.len(),
                sf.span_p64.deg,
                crate::sigfabric::span_merge_hash(sf) == sf.span_hash,
            );
        } else {
            let replays: u64 = self
                .wstats
                .iter()
                .map(|w| w.sig_replays.load(Ordering::Relaxed))
                .sum();
            eprintln!(
                "SIGFABRIC_DIAGNOSTIC {label}: baked=false sig_replays={} (R35-B: the signature fabric disarmed or unbaked — HFT_SIG_FABRIC=0)",
                replays,
            );
        }
        // R36-E: the res-record replay's engagement line (always printed
        // — the CI arm greps it; the engagement laws: replayed + live ==
        // the pass count, replayed == passes − the phase orbit on an
        // armed steady run, res_fb == 0 absent the rare assist claims).
        {
            let replays: u64 = self
                .wstats
                .iter()
                .map(|w| w.res_replays.load(Ordering::Relaxed))
                .sum();
            let live: u64 = self
                .wstats
                .iter()
                .map(|w| w.res_live.load(Ordering::Relaxed))
                .sum();
            let recs: u64 = self
                .wstats
                .iter()
                .map(|w| w.res_recs.load(Ordering::Relaxed))
                .sum();
            let fb: u64 = self
                .wstats
                .iter()
                .map(|w| w.res_fb.load(Ordering::Relaxed))
                .sum();
            eprintln!(
                "RESREPLAY_DIAGNOSTIC {label}: workers={} elision={} replayed_passes={} live_passes={} fallbacks={} records={} (R36-E: the worker eval elision — the res-record replay over the phase-segment logs, docs/48; HFT_RES_REPLAY=0 restores the live walk, HFT_RES_AUDIT=1 the differential twin)",
                self.wstats.len(),
                replays > 0,
                replays,
                live,
                fb,
                recs,
            );
        }
    }

    /// R36-E: the res-record replay's aggregate engagement —
    /// `(replayed_passes, live_passes, fallbacks, records)` summed over
    /// the workers. The verdict line's instruments (the engagement law:
    /// replayed + live == the boundaries each worker crossed).
    pub fn res_replay_totals(&self) -> (u64, u64, u64, u64) {
        let sum = |f: fn(&WorkerStats) -> &AtomicU64| {
            self.wstats.iter().map(|w| f(w).load(Ordering::Relaxed)).sum()
        };
        (
            sum(|w| &w.res_replays),
            sum(|w| &w.res_live),
            sum(|w| &w.res_fb),
            sum(|w| &w.res_recs),
        )
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

    /// R23c: the affine-tag verdict line's facts — the armed state, the
    /// per-worker tag hits vs payload fallbacks (the "zero payload
    /// reads" claim's measured counter), and the ledger's row count.
    /// Printed once per run (post-run, read-only — the LADDER_DIAGNOSTIC
    /// law: never price an arm on faith).
    pub fn affine_diag(&self) -> (bool, u64, u64, u64) {
        let armed = self.rxdesc.as_ref().is_some_and(|st| st.affine_armed());
        let mut hits = 0u64;
        let mut fallbacks = 0u64;
        for ws in &self.wstats {
            hits += ws.affine_hits.load(Ordering::Relaxed);
            fallbacks += ws.affine_fallbacks.load(Ordering::Relaxed);
        }
        let ledger = self
            .rxdesc
            .as_ref()
            .map_or(0, |st| st.ledger_len.load(Ordering::Acquire));
        (armed, hits, fallbacks, ledger)
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

// ─────────────────────────────────────────────────────────────────────
// R37 (Run 900) — THE FOLD JOURNAL (docs/solution2.md §2.7–§2.8:
// the L1 journal + the tree-fold landing, Theorems 19–22)
// ─────────────────────────────────────────────────────────────────────

/// The tree journal's leaf capacity. The champion's steady pass
/// pushes ~194 composite records (docs/48 §8: 113,186,154 records /
/// 583,434 passes); 512 leaves × 12 B = 6 KB — L1-resident with the
/// SoA columns side by side (Corollary 19.1's residency law). An
/// overflow (a boundary-less burst run, an unsegmented fallback
/// burst) flushes early — exact at ANY point by associativity
/// (Theorem 19's remark (ii): a partially-evaluated prefix composed
/// with a tree over the remaining records is just another tree
/// shape); capacity is a performance dial, never a correctness
/// constraint.
const FJ_CAP: usize = 512;

/// THE FOLD JOURNAL: the pass's fold records, journaled as
/// `(k, B)` S-records on the ROTATION ring (the segment monoid
/// `sink::tree_fold_segments` composes — Theorem 28's ring
/// polymorphism, the rotl-13 twin of the SPANHASH records) instead
/// of applied serially into `hash`. The serial chain
/// `S_p = S_{p−1} ⨂ R_p` — 322 dependent applies, the 2.08 µs/pass
/// cross-pass loop-carried barrier (the 61B wall, B1) — becomes a
/// leaf array the boundary flush evaluates as a ⌈log₂ K⌉-depth
/// up-sweep and applies in ONE `fold_compose_apply` hop. Records
/// never straddle a pending pass boundary (the worker's split law +
/// the R29/Lever-4 caps), so each leaf belongs to exactly one pass
/// and `flush_upto(boundary)` partitions exactly at the leaf edges;
/// `end[]` tracks the fold position after each leaf — strictly
/// monotone, the partition key.
///
/// Soundness is Theorem 22 verbatim (the tree is a re-bracketing of
/// the serial chain — every coordinate bit-exact), pinned by the
/// whole hydra matrix running the tree path by default plus the
/// dedicated journal cells (forced overflows, mid-journal boundary
/// splits, the serial rollback differential).
struct FoldJournal {
    /// The leaf records' span counts (the rotation degrees).
    k: Box<[u32]>,
    /// The leaf records' segment constants.
    b: Box<[u64]>,
    /// fold_pos AFTER each leaf (the boundary partition key).
    end: Box<[u64]>,
    /// Live leaf count.
    len: usize,
    /// `false` = the R36 serial chain verbatim (the rollback arm's
    /// attribution shape; `HFT_TREE_FOLD=0`).
    tree: bool,
    /// Telemetry: boundary + overflow flushes.
    flushes: u64,
    /// Telemetry: capacity-driven early flushes.
    overflows: u64,
    /// Telemetry: total records journaled.
    records: u64,
    /// Telemetry: max leaves in one flush (the compose_max dial).
    compose_max: u64,
}

impl FoldJournal {
    /// Construction-time allocation (sinks are built outside every
    /// measurement window — the allocs=0 gate law). `cap` exists for
    /// the unit cells (forced-overflow shapes); production uses
    /// [`FJ_CAP`].
    fn with_capacity(tree: bool, cap: usize) -> Self {
        assert!(cap >= 1 && cap <= FJ_CAP, "journal cap out of range");
        FoldJournal {
            k: vec![0u32; cap].into_boxed_slice(),
            b: vec![0u64; cap].into_boxed_slice(),
            end: vec![0u64; cap].into_boxed_slice(),
            len: 0,
            tree,
            flushes: 0,
            overflows: 0,
            records: 0,
            compose_max: 0,
        }
    }

    fn tree_fold_enabled() -> bool {
        // The rollback: `HFT_TREE_FOLD=0` restores the R36 serial
        // per-record apply verbatim (the attribution arm 11r37r —
        // the same-draw delta prices the tree's contribution).
        std::env::var("HFT_TREE_FOLD").as_deref() != Ok("0")
    }

    /// Journal one record (or apply it serially under the rollback).
    /// `end` = the fold position AFTER this record — callers know it
    /// (the composite drain's record end; the per-span sites' +1).
    #[inline(always)]
    fn push(&mut self, hash: &mut u64, k: u32, b: u64, end: u64) {
        if !self.tree {
            // The R36 champion's law, verbatim (Theorem 22's oracle).
            *hash = crate::sink::fold_compose_apply(*hash, k, b);
            self.records += 1;
            return;
        }
        if self.len == self.k.len() {
            // Capacity flush: every journaled leaf has end ≤ fold_pos
            // < every PENDING boundary (the boundary is stamped at
            // end_pass at the submission frontier ≥ fold_pos), so
            // flushing all is exact — the associativity license.
            self.overflows += 1;
            self.apply_prefix(hash, self.len);
            self.len = 0;
        }
        self.k[self.len] = k;
        self.b[self.len] = b;
        self.end[self.len] = end;
        self.len += 1;
        self.records += 1;
    }

    /// Flush the prefix of leaves with `end ≤ boundary` — the
    /// completed pass's records — into `hash` (the tree + one apply),
    /// shifting any remainder (next-pass leaves already drained past
    /// this boundary; structurally empty in the steady law, exact
    /// regardless — Theorem 19's heterogeneous shapes).
    fn flush_upto(&mut self, hash: &mut u64, boundary: u64) {
        if self.len == 0 || !self.tree {
            return;
        }
        let p = self.end[..self.len].partition_point(|&e| e <= boundary);
        if p == 0 {
            return;
        }
        self.apply_prefix(hash, p);
        let rem = self.len - p;
        if rem > 0 {
            self.k.copy_within(p..self.len, 0);
            self.b.copy_within(p..self.len, 0);
            self.end.copy_within(p..self.len, 0);
        }
        self.len = rem;
    }

    /// Flush everything (the finish path: the burst shape's direct
    /// `hash` field reads require the chain current with no
    /// boundaries pending; the pass shape's tail past the last
    /// boundary).
    fn flush_all(&mut self, hash: &mut u64) {
        if self.len == 0 || !self.tree {
            return;
        }
        self.apply_prefix(hash, self.len);
        self.len = 0;
    }

    /// THE TREE FLUSH: the up-sweep over the first `p` leaves (in
    /// place, ⌈log₂ p⌉ levels) + the single composite application.
    /// `rotl(h, 13K) ^ V` — one `fold_compose_apply`, exactly the
    /// serial chain's iterated result (Theorem 22).
    fn apply_prefix(&mut self, hash: &mut u64, p: usize) {
        let (kk, bb) = crate::sink::tree_fold_segments(
            &mut self.k[..p],
            &mut self.b[..p],
        );
        *hash = crate::sink::fold_compose_apply(*hash, kk, bb);
        self.flushes += 1;
        self.compose_max = self.compose_max.max(p as u64);
    }

    /// The reset path: drop any residue (all boundaries completed ⇒
    /// structurally empty; defensive regardless).
    fn clear(&mut self) {
        self.len = 0;
    }

    /// The telemetry contract (the FOLDTREE_DIAGNOSTIC line):
    /// `(tree, flushes, overflows, records, compose_max, live_len)`.
    fn stats(&self) -> (bool, u64, u64, u64, u64, u64) {
        (
            self.tree,
            self.flushes,
            self.overflows,
            self.records,
            self.compose_max,
            self.len as u64,
        )
    }
}

pub struct HydraSpanSink<'a> {
    fabric: Option<&'a HydraFabric>,
    // ── R37: the fold journal (the tree's leaf array — see FoldJournal) ──
    journal: FoldJournal,
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
    /// R27b: the record-ring fold watermark — `rx_rec_end[rec_slot]` is
    /// the fold position the window that LAST owned the record slot
    /// reached at its close. A record slot is reused (overwritten)
    /// REC_RING windows later, and a worker still walking the old window
    /// needs its record until its eval cursor passes that window's end
    /// (the fold trails the eval, so `fold_pos >= end` proves every
    /// worker is past it). Enforcing the watermark BEFORE the overwrite
    /// bounds the sink's window-open lead to < REC_RING windows in EVERY
    /// regime: the rotating regime's array-slot reuse gate always
    /// provided this bound implicitly (window w+8's open spun on the
    /// fold passing window w's end before reusing its ARRAY slot, and
    /// the two rings advanced in lockstep); the toroidal epoch froze the
    /// array slot and deleted the gate, leaving the record ring with NO
    /// bound — the assist pace (HFT_ASSIST_WATERMARK) is a throughput
    /// knob, not a protocol invariant, and the deterministic parity
    /// cells disarm it. Unbounded lead wraps the 8-slot record ring;
    /// the workers' `resolve_rec`/`probe_wb` then read overwritten
    /// records (a stale `rec_base` + `aslot`, array indices running past
    /// the window into stale words — a deterministic wrong fold, plus
    /// the composite pass-boundary split firing late and straddling).
    /// The watermark is the missing invariant, restored mode-
    /// independently: ONE local read + compare per window open (zero
    /// atomics, zero copies — the toroidal payoff stands), and a spin
    /// that only ever binds when the fold lags a full REC_RING of
    /// windows (R27c: 64 deep — beyond any 5 s window's reachable lead;
    /// production's pace keeps the lead under one window).
    rx_rec_end: [u64; REC_RING],
    /// R27b: the current window's RECORD slot (the ring index its
    /// record was published at — stamps `rx_rec_end` at the close; the
    /// array slot is `rx_win_slot`, but the toroidal epoch DECOUPLES
    /// the two rings, so neither derives the other).
    rx_win_rec: u8,
    /// The inline-claim marks were cleared for this sink's generation
    /// (at its FIRST window open — the activation; see
    /// `RxdescState::clear_chunk_states`).
    rx_marks_cleared: bool,
    /// Check-and-fix corrections applied (telemetry).
    rx_fixes: u64,
    /// The assist watermark: chunks go inline when pending spans exceed
    /// this (HFT_ASSIST_WATERMARK; 0 = never; force_inline overrides).
    rx_assist_wm: u64,
    /// R23c: the affine-tag protocol is armed for this fabric's rxdesc
    /// state (per-run constant, read at sink construction).
    rx_affine: bool,
    /// R23c: the RX ledger's monotone lookup cursor (spans arrive in
    /// frame order; the forward scan is O(1) amortized; reset at
    /// generation changes).
    rx_affine_cursor: usize,
    /// R25a: the current pass's first span id (u64::MAX = no
    /// begin_pass'd pass — the burst shape). A window opening EXACTLY
    /// here is a pass-start window: its record carries the pass flag so
    /// the composite-fold workers split their segment at the boundary
    /// (the consumer snapshots + re-seeds the chain there).
    pass_start_span: u64,
    /// R25c: the batched array commit path is ON (HFT_BATCH_SUBMIT=0 is
    /// the documented rollback — the per-span submit loop, the exact
    /// pre-R25c shape). Per-run constant, read at construction.
    batch_submit: bool,
    /// R27: the Zero-Reset Toroidal Pass is ON (HFT_TOROIDAL=0 is the
    /// documented rollback — the rotate-and-copy window open, the exact
    /// pre-R27 shape). Per-run constant, read at construction.
    rx_toroidal: bool,
    /// R28: the fused packed ladder-commit is ON (HFT_PACKED_EMISSION=0
    /// is the documented rollback — the SpanRec emission, the exact
    /// pre-R28 shape). Per-run constant, read at construction.
    packed_emission: bool,
    /// R27: the steady-epoch confidence — consecutive windows on the
    /// current array slot that verified fix-free at the SAME span count
    /// (the deterministic-schedule law: the array + sidecar content is
    /// window-invariant, so a clean window proves the slot already holds
    /// the next window's entries). 0 = rotating (copy + gate regime).
    rx_steady_epoch: u32,
    /// R27: the current window's mode — true = toroidal (array slot
    /// frozen, zero copies, zero bulk gate; fixes carry the per-fix
    /// lagging-reader guard).
    rx_tor_win: bool,
    /// R27: this sink has opened at least one window (the first window
    /// of a generation always rotates — its slot is cold).
    rx_ever_opened: bool,
    /// R27: `rx_fixes` at the current window's open (the epoch's clean
    /// check at close).
    rx_win_fix0: u64,
    /// R27: the current window's expected span count (the previous
    /// window's count — `last_window_count` at open; the epoch's count
    /// stability check AND the per-fix guard's lagging-reader bound).
    rx_win_count0: u64,
    /// R29: the ZERO-THREAD INGEST is ON (HFT_SELF_SERVE=0 is the
    /// documented rollback — the full submission cadence, the exact
    /// pre-R29 shape). Per-run constant, read at construction.
    rx_selfserve: bool,
    /// R29: the CURRENT window is self-serve — the window's whole span
    /// range was pre-published at open (`spans_ready = base + count0`);
    /// the workers ingest directly from the frozen array while the main
    /// core's scan walks the sequencer ladder only (the commit path —
    /// chunk-open assists, the vector check, the publish cadence — is
    /// skipped for the window's steady batches).
    rx_win_ss: bool,
    /// R29: the current self-serve window's pre-published frontier (the
    /// ready cursor's floor while the window is open — publications
    /// clamp to `max(next_span, rx_win_pre)` so the cursor never moves
    /// backward, and the close reconciles to the actual end).
    rx_win_pre: u64,
    /// R29: the self-serve engagement telemetry (windows armed; read
    /// post-run by the verdict lines).
    rx_ss_wins: u64,
    /// R31-A: the WARM-EPOCH copy gate — per-ARRAY-SLOT consecutive
    /// clean-window count. A window on slot s that verifies fix-free at
    /// the same span count as the slot's previous window proves the
    /// slot's content is the schedule's invariant tape for that range
    /// (the deterministic-schedule law the warm copy itself stands on);
    /// epoch ≥ 1 means the NEXT rotating window on s can skip the
    /// ~252KB copy_arr/copy_tags pair — the words AND tags are already
    /// exact, and the per-span check-and-fix remains the correctness
    /// backstop (a wrong skip costs fix stores, never correctness — the
    /// copy's own law). After the first RX_NARR warm-filled passes, the
    /// rotating regime's opens go copy-free exactly like the toroidal
    /// one's.
    rx_slot_epoch: [u32; RX_NARR],
    /// R31-A: the slot's last window's span count (u64::MAX = never
    /// used) — the epoch's count-stability operand.
    rx_slot_count: [u64; RX_NARR],
    /// R31-A: the WARM-EPOCH arm is ON (HFT_WARM_EPOCH=0 is the documented
    /// rollback — the always-copy rotating regime, the exact pre-R31
    /// shape). Per-run constant, read at construction; pub for the
    /// in-process test cells.
    pub rx_warm_epoch: bool,
    /// R31-A: the copy-skip engagement telemetry (windows opened without
    /// the warm copy; read post-run by the verdict lines).
    rx_warm_skips: u64,
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
        let rx_affine = rx.as_ref().is_some_and(|st| st.affine_armed());
        let sink = Self {
            fabric,
            // R37: the tree journal — allocation at construction (outside
            // every window); the serial rollback rides the same struct
            // (`HFT_TREE_FOLD=0`).
            journal: FoldJournal::with_capacity(FoldJournal::tree_fold_enabled(), FJ_CAP),
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
            rx_rec_end: [u64::MAX; REC_RING],
            rx_win_rec: 0,
            rx_marks_cleared: false,
            rx_fixes: 0,
            rx_assist_wm: Self::assist_watermark(),
            rx_affine,
            rx_affine_cursor: 0,
            pass_start_span: u64::MAX,
            batch_submit: std::env::var("HFT_BATCH_SUBMIT").as_deref() != Ok("0"),
            rx_toroidal: std::env::var("HFT_TOROIDAL").as_deref() != Ok("0"),
            packed_emission: std::env::var("HFT_PACKED_EMISSION").as_deref() != Ok("0"),
            rx_steady_epoch: 0,
            rx_tor_win: false,
            rx_ever_opened: false,
            rx_win_fix0: 0,
            rx_win_count0: 0,
            rx_selfserve: std::env::var("HFT_SELF_SERVE").as_deref() != Ok("0"),
            rx_win_ss: false,
            rx_win_pre: 0,
            rx_ss_wins: 0,
            rx_slot_epoch: [0; RX_NARR],
            rx_slot_count: [u64::MAX; RX_NARR],
            rx_warm_epoch: std::env::var("HFT_WARM_EPOCH").as_deref() != Ok("0"),
            rx_warm_skips: 0,
        };
        // R21 (Task 2, the fused arm's flip-validation instrument): prove
        // the fused shape armed — printed ONCE per process at the first
        // fused-sink construction (the LADDER_DIAGNOSTIC lesson: never
        // price an arm on faith).
        static FUSED_DIAG: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if sink.force_inline && !FUSED_DIAG.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "HYDRA_FUSED_DIAGNOSTIC force_inline=true kernel={} assist_slots={} (R21 Task 2: submit-side fused verification — every span CRC evaluates on the submitting core at the framing point; the workers never re-read the payload, the 2nd memory pass is eliminated)",
                sink.kernel.name(),
                sink.assist_slots,
            );
        }
        sink
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
        // R37: the chain's journal resets with it (all boundaries
        // completed by the drain assert above — structurally empty;
        // defensive regardless).
        self.journal.clear();
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
        self.rx_rec_end = [u64::MAX; REC_RING];
        // R27: a fresh generation always starts in the rotating regime —
        // the epoch must re-earn itself on the new span grid.
        self.rx_steady_epoch = 0;
        self.rx_tor_win = false;
        self.rx_ever_opened = false;
        self.rx_win_fix0 = 0;
        self.rx_win_count0 = 0;
        self.rx_win_ss = false;
        self.rx_win_pre = 0;
        // (rx_ss_wins is run-lifetime telemetry — not reset.)
        // R31-A: the fresh generation's slots are cold content-wise (the
        // span grid restarts) — every slot re-earns its warm epoch.
        self.rx_slot_epoch = [0; RX_NARR];
        self.rx_slot_count = [u64::MAX; RX_NARR];
        // R23c: the ledger cursor restarts with the span sequence (the
        // fresh generation's spans revisit the blob's frames from the
        // first row).
        self.rx_affine_cursor = 0;
        // R25a: no pass is open after a reset (the next begin_pass sets
        // it; a burst-shaped run never does — MAX means "no pass
        // boundary flag on window opens").
        self.pass_start_span = u64::MAX;
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
        // R25a: the pass's first span — a window opening exactly here is
        // a pass-start window (its record carries the flag; the
        // composite-fold workers split their segment at this span).
        self.pass_start_span = self.next_span;
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
            // R27b: stamp the record slot's fold watermark (the window
            // that owns `rx_win_rec` ends here; REC_RING windows later,
            // the slot's reuse waits for the fold to pass this span).
            self.rx_rec_end[self.rx_win_rec as usize] = self.next_span;
            let win_count = self.next_span - self.rx_win_base;
            if let Some(st) = &self.rx {
                st.set_last_window_count(win_count);
            }
            // R27: the epoch update — a window that verified fix-free at
            // the expected span count proves this array slot's content is
            // the invariant tape (the next window on the slot will find
            // every entry already correct); anything else disarms to the
            // rotating regime (the next window pays the warm copy and
            // re-earns the epoch).
            let clean = self.rx_fixes == self.rx_win_fix0 && win_count == self.rx_win_count0;
            self.rx_steady_epoch = if clean {
                self.rx_steady_epoch.saturating_add(1)
            } else {
                0
            };
            // R31-A: the PER-SLOT warm epoch — the same clean proof, but
            // keyed to THIS window's array slot and compared against the
            // slot's own last count (the rotating regime revisits a slot
            // RX_NARR windows later; the global epoch's operands can have
            // rotated through other slots since). Epoch ≥ 1 arms the next
            // open on this slot to SKIP the warm copy (the slot's words
            // and tags are already the exact tape — the check-and-fix
            // stays the backstop).
            let s = self.rx_win_slot as usize;
            let slot_clean = self.rx_fixes == self.rx_win_fix0
                && self.rx_slot_count[s] != u64::MAX
                && win_count == self.rx_slot_count[s];
            self.rx_slot_epoch[s] = if slot_clean {
                self.rx_slot_epoch[s].saturating_add(1)
            } else {
                0
            };
            self.rx_slot_count[s] = win_count;
            self.rx_win_open = false;
        }
        self.rx_publish_ready();
        // R29: the self-serve window's close — the publish above
        // reconciled the frontier (max(actual, pre); equal in the steady
        // law) and the window's mode retires.
        self.rx_win_ss = false;
        self.rx_win_pre = 0;
        self.complete_boundaries();
    }

    /// Snapshot + reset at every pass boundary the fold has crossed.
    /// The chain value at the crossing IS the completed pass's hash (the
    /// fold applies values in strict global span order, and the boundary
    /// sits exactly at the pass's final span + 1).
    ///
    /// R37: the snapshot reads the chain THROUGH the journal — the
    /// flush composes the pass's journaled leaves (up to exactly this
    /// boundary; records never straddle) by the ⌈log₂ K⌉ up-sweep and
    /// applies the composite in ONE `fold_compose_apply` hop before
    /// the snapshot. The serial chain's 322-dependent-apply barrier
    /// (the 2.08 µs/pass cross-pass dependency — the 61B wall) becomes
    /// a ~194-leaf tree ≈ 8 levels + one hop; the next pass's leaves
    /// (already drained past this boundary, if any) stay journaled —
    /// their composite is h-independent (the V-coordinate never sees
    /// the entry hash), so the SPAN_SEED reseed below re-bases them
    /// exactly.
    fn complete_boundaries(&mut self) {
        while self.pass_tail != self.pass_head {
            let rec = &self.passes[self.pass_tail];
            if rec.done || rec.end_span == u64::MAX {
                break;
            }
            if rec.end_span <= self.fold_pos {
                self.journal.flush_upto(&mut self.hash, rec.end_span);
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

    /// The per-span fold — EXACTLY `SpanConformanceSink::on_span`'s mix
    /// (R25a: the composable law `h ← rotl(h,13) ^ (v·P)`, reassociated
    /// from the R23 `h ← (rotl(h,13) ^ v)·P` so that chunk segments
    /// compose — see `sink::fold_compose_b`/`fold_compose_apply`; the
    /// per-span form stays the reference and the rollback path).
    ///
    /// R37: under the tree the span journals as a `(k=1, B=v·P)` leaf
    /// (the segment monoid's per-span generator); `end` is the fold
    /// position after this span — every caller knows it (the loop
    /// counters run ahead of the committed `fold_pos` cursor).
    #[inline(always)]
    fn fold_value(&mut self, v: u64, end: u64) {
        if self.journal.tree {
            self.journal
                .push(&mut self.hash, 1, crate::sink::fold_term(v), end);
        } else {
            self.hash = crate::sink::fold_step(self.hash, v);
        }
    }

    /// R16b: publish the span-count cursor — the ONE Release store that
    /// makes the window's array entries (RX prefill + this sink's fixes)
    /// and inline-chunk claims visible to the workers. Called at
    /// submission-batch granularity (on_span_batch / on_span flushes,
    /// end_pass, finish).
    #[inline]
    fn rx_publish_ready(&mut self) {
        if let Some(st) = &self.rx {
            // R29: the self-serve window's frontier never moves backward —
            // the open pre-published `base + count0`, and any intermediate
            // publish (a cold frame's per-span path, an assist's chunk
            // completion) clamps to the floor. The close reconciles to the
            // actual end (== the floor in the steady law; a divergent
            // window's tuple assert is the loud failure).
            let v = if self.rx_win_ss {
                self.next_span.max(self.rx_win_pre)
            } else {
                self.next_span
            };
            st.publish_ready(self.rx_gen, v);
        }
    }

    /// R16b/R27: open a submission window — decide the mode, gate, warm
    /// start and publish the pass record.
    ///
    /// **R27 — the Zero-Reset Toroidal Pass.** The old paradigm treated a
    /// window as a bounded lifecycle (rotate slot → reuse-gate drain →
    /// bulk warm copy → publish): each pass paid ~22 µs of copy traffic
    /// (the arr row + the sidecar, ~320 KB per window at the Report-40
    /// span counts) — 17% of the wall. The re-derivation: repeating a
    /// deterministic pass over an invariant frame buffer is walking a
    /// torus — the array slot's content is window-INVARIANT (same
    /// schedule, same blob ⇒ same (off, len) words, same tags), so a
    /// window that verifies fix-free at the same span count PROVES the
    /// slot already holds the next window's entries. The steady epoch
    /// (≥1 clean window) therefore:
    ///   * **reuses the array slot** (zero copies — the check-and-fix
    ///     that already runs every span stays the WHOLE safety net);
    ///   * **skips the bulk reuse gate** (the steady window writes
    ///     NOTHING exposed — fixes are pre-exposure by the ready-store
    ///     law, and the shared-slot fix writes carry the per-fix
    ///     lagging-reader guard below);
    ///   * **keeps the record ring rotating** (the record IS the epoch
    ///     tag: `(gen, base, pass, aslot)` — workers adopt records in
    ///     ring order, a lagging walker finds every window's record, and
    ///     the record's aslot field points it at the frozen array).
    /// The first window of a generation always rotates (cold fill);
    /// divergence disarms the epoch and the next window rotates back
    /// (the warm-copy chain re-seeds the slot from the toroidal slot's
    /// verified content).
    #[inline(never)]
    fn rx_open_window(&mut self) {
        let st = self
            .rx
            .clone()
            .expect("rx_open_window without rxdesc state");
        // The epoch decision — toroidal iff armed AND the previous
        // window on this array slot was verified clean at the same count.
        let tor = self.rx_toroidal && self.rx_ever_opened && self.rx_steady_epoch >= 1;
        // The RECORD slot ALWAYS rotates (the walk's adoption order —
        // every window's record must be findable at (rec_slot + 1)).
        let rec_slot = st.next_slot();
        // The ARRAY slot: the toroidal window reuses the previous window's
        // slot (its verified-invariant content); the rotating window
        // advances by one from the LAST ARRAY slot (== next_slot() in the
        // pure-rotation regime — identical to the classic protocol; after
        // a toroidal epoch it resumes the rotation from the frozen slot,
        // which the warm copy re-seeds).
        let arr_slot = if tor {
            self.rx_win_slot
        } else if self.rx_ever_opened {
            (self.rx_win_slot + 1) % RX_NARR as u8
        } else {
            rec_slot
        };
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
        if !tor {
            // Reuse gate (the rotating window WRITES this array slot via
            // the warm start + fixes): this sink's earlier window on
            // `arr_slot` must be folded (cross-sink reuse is free — a
            // fresh sink implies the previous one drained, the standing
            // fabric contract). The gate's fold-pos wait covers every
            // worker still walking the slot's LAST window: fold_pos ≥
            // end ⟹ the workers evaluated past it.
            let end = self.rx_slot_end[arr_slot as usize];
            if end != u64::MAX {
                let mut sb = 0u32;
                while self.fold_pos < end {
                    self.fold_available();
                    crate::affinity::polite_spin(&mut sb);
                }
            }
            // Warm start: copy the previous window's span-indexed entries
            // into the fresh slot (the previous window's ARRAY slot —
            // the toroidal slot after a disarm, whose content is the
            // verified-current one; the previous SINK's slot on the
            // first window — the cross-sink carry). Purely an
            // optimization — the per-span check-and-fix below still
            // verifies EVERY entry against the actual body; a wrong copy
            // costs stores, never correctness.
            //
            // R31-A: THE WARM-EPOCH GATE — the slot's own last window
            // verified fix-free at exactly this window's warm-start
            // count, so the slot ALREADY holds the exact words and tags
            // the copy would stage (the deterministic-schedule law; the
            // tags are body-derived and the bodies are blob-immutable).
            // Skip the ~252KB copy pair; the check-and-fix remains the
            // per-span backstop — a wrong skip costs fix stores, never
            // correctness (the copy's own law, symmetric). After the
            // first RX_NARR warm-filled passes the rotating regime's
            // opens are copy-free, exactly like the toroidal epoch's.
            let n_warm = st.last_window_count();
            let slot_idx = arr_slot as usize;
            let warm_skip = self.rx_warm_epoch
                && self.rx_slot_epoch[slot_idx] >= 1
                && self.rx_slot_count[slot_idx] == n_warm as u64;
            if warm_skip {
                self.rx_warm_skips += 1;
            } else {
                let prev_arr = if self.rx_ever_opened {
                    self.rx_win_slot as usize
                } else {
                    (arr_slot as usize + RX_NARR - 1) % RX_NARR
                };
                st.copy_arr(prev_arr as u8, arr_slot, n_warm);
                // R23c: the tag sidecar rides the SAME warm start — the
                // copied tags stay valid under the same determinism law
                // (identical schedule + immutable blob bodies), so steady
                // passes find the tag correct and the check stays the ONE
                // word compare.
                st.copy_tags(prev_arr as u8, arr_slot, n_warm);
            }
        }
        // R27b: THE RECORD-RING WATERMARK — the bound that keeps the
        // sink's window-open lead within REC_RING of the fold in EVERY
        // regime (see the field doc). The record about to be published
        // into `rec_slot` overwrites the window REC_RING opens ago; a
        // worker still evaluating that window's spans reads its record
        // every boundary crossing (`resolve_rec`/`probe_wb`). fold_pos
        // ≥ that window's end proves every worker's cursor is past it
        // (the fold trails the eval, and every worker's gid ≥ fold_pos
        // — results publish before the fold consumes them). The spin
        // drives the fold itself (same shape as the array reuse gate);
        // in production the assist pace holds the lead under one
        // window, so the spin never binds — the watermark is pure
        // soundness insurance at one local load + compare per open.
        let rend = self.rx_rec_end[rec_slot as usize];
        if rend != u64::MAX {
            let mut sb = 0u32;
            while self.fold_pos < rend {
                self.fold_available();
                crate::affinity::polite_spin(&mut sb);
            }
        }
        // R25a: the pass-start flag — a window opening exactly at the
        // current pass's first span is a pass boundary (begin_pass's
        // eager open, or the lazy open firing on the pass's first
        // submit). The composite-fold workers split their segment at
        // the record's base; per-span spans cross the boundary in the
        // consumer's cap logic as before (both modes stay bit-exact).
        let pass_start = self.next_span == self.pass_start_span;
        st.publish_record(rec_slot, self.rx_gen, self.next_span, pass_start, arr_slot);
        self.rx_win_slot = arr_slot;
        self.rx_win_rec = rec_slot;
        self.rx_win_base = self.next_span;
        self.rx_win_open = true;
        self.rx_ever_opened = true;
        self.rx_tor_win = tor;
        // R27: the epoch's bookkeeping — the fixes snapshot at open (the
        // clean check at close) and the count guess (the previous
        // window's actual count).
        self.rx_win_fix0 = self.rx_fixes;
        self.rx_win_count0 = st.last_window_count() as u64;
        // R29: THE ZERO-THREAD INGEST arm — the deep-steady epoch (two
        // clean windows at the same count) proves the frozen slot's
        // words AND tags are the invariant tape, so the window's whole
        // span range can publish NOW: `spans_ready = base + count0`.
        // The workers self-serve every span from the frozen array the
        // moment the record lands; the main core's scan reduces to the
        // sequencer ladder (session/seq verification + counters + the
        // count bookkeeping — the emission words still flow, the commit
        // path does not); the fold consumes the workers' results as
        // they stream. The submission funnel — the single-threaded
        // commit cadence that pegged the main core at the 4.3B decode
        // ceiling — is deleted for the window. Guards: a straddling
        // inline continuation must run the classic path (the marked
        // chunk's remainder values come from the inline ring — the
        // sink must claim them); force_inline is the deterministic
        // assist-mode shape (every chunk inline by construction).
        let straddle_inline = self.submit_rem != CHUNK
            && st.is_inline(self.next_span / CHUNK);
        self.rx_win_ss = self.rx_selfserve
            && self.batch_submit
            && tor
            && self.rx_steady_epoch >= 2
            && self.rx_win_count0 > 0
            && !straddle_inline
            && !self.force_inline;
        if self.rx_win_ss {
            self.rx_win_pre = self.next_span + self.rx_win_count0;
            // The record's Release above orders the (already-exact)
            // array content before this frontier store — the workers'
            // Acquire sees the whole window at once.
            st.publish_ready(self.rx_gen, self.rx_win_pre);
            self.rx_ss_wins += 1;
        } else {
            self.rx_win_pre = 0;
        }
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

    /// R27: the toroidal window's per-fix lagging-reader guard. The
    /// steady window writes NOTHING (pure verify), but a FIX writes the
    /// SHARED array slot — the word `arrays[A][idx]` also serves the
    /// previous window's span at gid' = base − count0 + idx. The guard
    /// waits for the fold to pass gid' (the worker's read of the word
    /// happens exactly as its eval cursor crosses gid'; the fold trails
    /// the eval, so fold_pos > gid' proves the read completed) before
    /// overwriting it. Bounded (the fold is at most the flow-control
    /// pace behind submission) and COLD (fixes fire only on schedule
    /// divergence — the epoch disarms at the window's close anyway).
    /// No-op on rotating windows (their slot is exclusively the sink's).
    #[inline(never)]
    fn rx_tor_fix_guard(&mut self, idx: usize) {
        if !self.rx_tor_win {
            return;
        }
        let need = self
            .rx_win_base
            .wrapping_sub(self.rx_win_count0)
            .wrapping_add(idx as u64)
            + 1;
        if self.fold_pos < need {
            let mut sb = 0u32;
            while self.fold_pos < need {
                self.fold_available();
                crate::affinity::polite_spin(&mut sb);
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
                // R29: the SELF-SERVE window never takes new inline
                // claims — its whole span range is already published
                // (the open's pre-publish), so a mid-scan claim's mark
                // would land AFTER the workers entered the chunk: both
                // sides evaluate it and the lane ring carries a
                // duplicate the fold's order law rejects (the classic
                // protocol's own law — the mark precedes the
                // spans_ready store that exposes the chunk — cannot be
                // restored once the exposure is up-front). The workers
                // own the ingest; the pace gate stays the flow control.
                !self.rx_win_ss
                    && self.rx_assist_wm > 0
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
            // R27c: the HOLE-FREE INLINE COMMIT (per-span form) — an
            // inline span STILL writes its array word. This branch is
            // how COLD frames reach the array (the classic ladder's
            // per-span emission — the steady frames ride the batched
            // paths, whose `arr_check_fix`/`packed_arr_check_fix` twins
            // carry the same law): without the write, a force-inline or
            // straddle-claimed window leaves its cold-frame spans'
            // words stale — the hole propagates through the warm copy
            // and the next array-path window re-fixes it, disarming the
            // toroidal epoch (see `arr_check_fix`'s doc). The check is
            // the per-span load+compare the array path already pays.
            {
                let idx = (self.next_span - self.rx_win_base) as usize;
                assert!(
                    idx < st.cap(),
                    "rxdesc window exceeds array cap (HFT_RXDESC_CAP)"
                );
                debug_assert!(body.len() <= u16::MAX as usize, "Desc8 len overflows u16");
                let off = (body.as_ptr() as usize).wrapping_sub(self.blob_base);
                debug_assert!(off <= u32::MAX as usize, "Desc8 offset overflows u32");
                let w = desc8_pack_span(off as u32, body.len() as u16)
                    | if self.rx_affine { RX_AFFINE_FLAG } else { 0 };
                if st.get_arr(self.rx_win_slot, idx) != w {
                    self.rx_tor_fix_guard(idx);
                    if self.rx_affine {
                        let tag = self.rx_affine_tag_for(body);
                        st.set_affine_tag(self.rx_win_slot, idx, &tag);
                    }
                    st.set_arr(self.rx_win_slot, idx, w);
                    self.rx_fixes += 1;
                }
            }
            // Inline chunk: evaluate NOW from the actual body (no array
            // involvement), buffer for the ordered fold — the existing
            // assist path verbatim.
            self.submit_inline_span(slot, body);
        } else {
            // Check-and-fix: prove the array entry equals the actual
            // span body's descriptor; fix on mismatch (divergent
            // schedules drift the frame index past the span index).
            // R23c: the expected word carries the affine-tag flag when
            // armed; a mismatch (the COLD path — the first fill or
            // schedule divergence) computes the tag (the RX ledger first,
            // the direct body scan as the fallback) and writes BOTH the
            // word and the sidecar entry, ordered before the chunk's
            // spans_ready publication.
            let idx = (self.next_span - self.rx_win_base) as usize;
            assert!(
                idx < st.cap(),
                "rxdesc window exceeds array cap (HFT_RXDESC_CAP)"
            );
            debug_assert!(body.len() <= u16::MAX as usize, "Desc8 len overflows u16");
            let off = (body.as_ptr() as usize).wrapping_sub(self.blob_base);
            debug_assert!(off <= u32::MAX as usize, "Desc8 offset overflows u32");
            let w = desc8_pack_span(off as u32, body.len() as u16)
                | if self.rx_affine { RX_AFFINE_FLAG } else { 0 };
            if st.get_arr(self.rx_win_slot, idx) != w {
                // R27: the toroidal window's shared-slot write — wait for
                // the lagging reader of the previous window's same word.
                self.rx_tor_fix_guard(idx);
                if self.rx_affine {
                    let tag = self.rx_affine_tag_for(body);
                    st.set_affine_tag(self.rx_win_slot, idx, &tag);
                }
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

    /// R25c: the BATCHED array commit path — the R25b fleet diagnosis
    /// (work_ms=4.1s/5s on the binding main core) re-derived the
    /// submission path's per-span fixed costs, and the batch drains
    /// them instead of amortizing per span:
    ///
    /// * ONE `Arc` clone/drop pair per BATCH (`<= 32` recs) — the
    ///   per-span pair was two locked RMWs on the refcount line for a
    ///   pointer the sink already owns, ~20-30c of the ~90c/span
    ///   submit budget on the 9V45 draws;
    /// * ONE flow-control gate per batch (`pending()` loads the
    ///   consumer-written `fold_pos` line — cross-core Shared — and
    ///   the gate's RX_PACE=8192 headroom dwarfs a 32-span overshoot);
    /// * the check-and-fix 8 WIDE (`RxdescState::check_arr8`): one
    ///   64-byte load + one vector compare per group replaces eight
    ///   load+compare+branch triples; the steady-state law
    ///   `rx_fixes==0` (every RX prefill entry already exact) makes
    ///   the compare the WHOLE per-span array cost, and a mismatching
    ///   lane takes the exact scalar fix path (tag sidecar + word +
    ///   counter) — bit-identical stores, identical order;
    /// * the chunk-boundary arithmetic runs per RUN (one array or
    ///   inline run per 64-span chunk), not per span.
    ///
    /// Semantics are the per-span path's verbatim (the
    /// `t_r25c_batch_submit_parity` differential + the full
    /// `t_rxdesc_parity_matrix` pin it): the chunk-open assist
    /// decision fires at exactly the same grid boundaries
    /// (`submit_rem == CHUNK`), the publish cadence stays one
    /// `spans_ready` Release per chunk, the warm-start/reuse-gate
    /// window protocol is untouched (`rx_open_window`), and
    /// `HFT_BATCH_SUBMIT=0` is the documented rollback.
    #[inline(never)]
    fn submit_span_batch_rx(&mut self, recs: &[SpanRec<'_>]) {
        if recs.is_empty() {
            return;
        }
        let Some(st) = self.rx.clone() else {
            unreachable!("submit_span_batch_rx without rxdesc state")
        };
        // The blob base — submit_span's preamble, verbatim (write-once
        // per run; MUST precede the first spans_ready Release, which
        // the window open + publishes below order it into).
        if self.blob_base == 0 {
            self.blob_base = recs[0].body.as_ptr() as usize;
            st.capture_blob_base(self.blob_base);
        }
        // Lazy window open (identical trigger: this batch's first span).
        if !self.rx_win_open {
            self.rx_open_window();
        }
        // Flow control — once per batch. The per-span loop's gate could
        // fire mid-batch; the batch check bounds the overshoot at the
        // batch length (<= 32 spans, 0.4% of RX_PACE) — the drain loop
        // itself is unchanged (fold + deep spin).
        if self.rx_assist_wm > 0 {
            let mut sb = 0u32;
            while self.pending() > Self::RX_PACE {
                self.fold_available();
                crate::affinity::polite_spin_deep(&mut sb);
            }
        }
        let affine_flag: u64 = if self.rx_affine { RX_AFFINE_FLAG } else { 0 };
        // R29: the self-serve skip (SpanRec form) — same law as the packed
        // path's (see `submit_packed_batch_rx`'s R29 block): cursor
        // arithmetic only, the commit path deleted for the window.
        if self.rx_win_ss && self.cur_inline.is_none() {
            let mut i = 0usize;
            while i < recs.len() {
                let run = (self.submit_rem as usize).min(recs.len() - i);
                i += run;
                self.next_span += run as u64;
                self.submit_rem -= run as u64;
                if self.submit_rem == 0 {
                    self.submit_rem = CHUNK;
                    self.submit_lane = if self.submit_lane + 1 == self.n_lanes {
                        0
                    } else {
                        self.submit_lane + 1
                    };
                }
            }
            return;
        }
        let mut i = 0usize;
        while i < recs.len() {
            // Chunk-open: the assist decision — submit_span_rx's grid
            // boundary logic, verbatim (fires exactly when
            // `submit_rem == CHUNK`, before the chunk's first span).
            if self.submit_rem == CHUNK {
                let chunk_id = self.next_span / CHUNK;
                let take_inline = if self.force_inline {
                    // Parity-test mode: every chunk inline, fold until an
                    // inline-ring slot frees (the existing force
                    // semantics).
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
                    // The claim MUST be visible before the spans_ready
                    // store that exposes this chunk's spans.
                    st.mark_inline(chunk_id);
                }
            }
            // The run: up to one chunk (or the batch tail) in ONE mode.
            let run = (self.submit_rem as usize).min(recs.len() - i);
            if let Some(slot) = self.cur_inline {
                // R27c: the hole-free inline commit — keep the slot's words
                // exact even when the chunk evaluates inline (see
                // `arr_check_fix`'s doc: the assist must not disarm the
                // toroidal epoch).
                self.arr_check_fix(&st, &recs[i..i + run], affine_flag);
                for _ in 0..run {
                    let body = recs[i].body;
                    self.submit_inline_span(slot, body);
                    self.next_span += 1;
                    self.submit_rem -= 1;
                    i += 1;
                }
            } else {
                self.submit_arr_run(&st, &recs[i..i + run], affine_flag);
                i += run;
            }
            // Chunk completion — submit_span_rx's cadence, verbatim:
            // wrap the tracker, seal an inline chunk, publish.
            if self.submit_rem == 0 {
                self.submit_rem = CHUNK;
                self.submit_lane = if self.submit_lane + 1 == self.n_lanes {
                    0
                } else {
                    self.submit_lane + 1
                };
                if self.cur_inline.is_some() {
                    self.flush_pending();
                }
                self.rx_publish_ready();
            }
        }
    }

    /// R25c: one array-mode run — `recs.len()` consecutive spans'
    /// check-and-fix in 8-wide groups (the vector compare's mismatch
    /// mask routes exactly the divergent lanes through the scalar fix
    /// path — `rx_affine_tag_for` + `set_affine_tag` + `set_arr` +
    /// `rx_fixes`, verbatim). Advances `next_span` and `submit_rem` by
    /// the run length. The caller proves the run stays within one
    /// chunk and the window is open.
    fn submit_arr_run(&mut self, st: &RxdescState, recs: &[SpanRec<'_>], affine_flag: u64) {
        let n = recs.len();
        let mut i = 0usize;
        while i < n {
            if n - i >= 8 {
                // ── the 8-wide group ──
                let base_idx = (self.next_span - self.rx_win_base) as usize;
                assert!(
                    base_idx + 8 <= st.cap(),
                    "rxdesc window exceeds array cap (HFT_RXDESC_CAP)"
                );
                let mut expect = [0u64; 8];
                for g in 0..8 {
                    let body = recs[i + g].body;
                    let off = (body.as_ptr() as usize).wrapping_sub(self.blob_base);
                    debug_assert!(off <= u32::MAX as usize, "Desc8 offset overflows u32");
                    debug_assert!(body.len() <= u16::MAX as usize, "Desc8 len overflows u16");
                    expect[g] = desc8_pack_span(off as u32, body.len() as u16) | affine_flag;
                }
                let bad = st.check_arr8(self.rx_win_slot, base_idx, &expect);
                if bad != 0 {
                    // The COLD path — the same per-lane fix the scalar
                    // loop applies, in the same ascending order.
                    for g in 0..8 {
                        if (bad >> g) & 1 == 0 {
                            continue;
                        }
                        let body = recs[i + g].body;
                        // R27: the toroidal window's shared-slot write —
                        // wait for the lagging reader of the previous
                        // window's same word.
                        self.rx_tor_fix_guard(base_idx + g);
                        if self.rx_affine {
                            let tag = self.rx_affine_tag_for(body);
                            st.set_affine_tag(self.rx_win_slot, base_idx + g, &tag);
                        }
                        st.set_arr(self.rx_win_slot, base_idx + g, expect[g]);
                        self.rx_fixes += 1;
                    }
                }
                self.next_span += 8;
                self.submit_rem -= 8;
                i += 8;
            } else {
                // ── the scalar tail (run length not a multiple of 8) —
                // the per-span check-and-fix, verbatim ──
                let body = recs[i].body;
                let idx = (self.next_span - self.rx_win_base) as usize;
                assert!(idx < st.cap(), "rxdesc window exceeds array cap (HFT_RXDESC_CAP)");
                debug_assert!(body.len() <= u16::MAX as usize, "Desc8 len overflows u16");
                let off = (body.as_ptr() as usize).wrapping_sub(self.blob_base);
                debug_assert!(off <= u32::MAX as usize, "Desc8 offset overflows u32");
                let w = desc8_pack_span(off as u32, body.len() as u16) | affine_flag;
                if st.get_arr(self.rx_win_slot, idx) != w {
                    // R27: the toroidal window's shared-slot write — wait
                    // for the lagging reader of the previous window's same
                    // word.
                    self.rx_tor_fix_guard(idx);
                    if self.rx_affine {
                        let tag = self.rx_affine_tag_for(body);
                        st.set_affine_tag(self.rx_win_slot, idx, &tag);
                    }
                    st.set_arr(self.rx_win_slot, idx, w);
                    self.rx_fixes += 1;
                }
                self.next_span += 1;
                self.submit_rem -= 1;
                i += 1;
            }
        }
    }

    /// R27c: the HOLE-FREE INLINE COMMIT (SpanRec form) — an inline
    /// (assist) chunk STILL runs the check-and-fix of its words into the
    /// window's array slot (guards + fixes + `rx_fixes`, verbatim
    /// `submit_arr_run` semantics) WITHOUT advancing the submission
    /// cursors (`next_span`/`submit_rem` stay — the inline loop owns the
    /// advance). Why: an inline chunk's spans previously left the
    /// window's array words UNTOUCHED (the workers skip marked chunks,
    /// so nobody read them) — but the NEXT window on the same slot
    /// (the toroidal epoch) checks those words against its own walk,
    /// and a never-written word reads stale → a fix → the epoch's clean
    /// check disarms → the rotating regime returns with its ~22 µs
    /// warm copy per window (the measured 830 ms/pass-reset wall). With
    /// the hole-free commit the slot's content is complete after EVERY
    /// window regardless of where each chunk was evaluated, so the
    /// assist can no longer disarm the toroidal epoch. The cost is the
    /// same 8-wide vector check the array path pays (~0.75 cyc/span —
    /// noise next to the inline CRC evaluation the assist exists to
    /// run).
    #[inline(never)]
    fn arr_check_fix(&mut self, st: &RxdescState, recs: &[SpanRec<'_>], affine_flag: u64) {
        let n = recs.len();
        let base_idx = (self.next_span - self.rx_win_base) as usize;
        let mut i = 0usize;
        while i < n {
            if n - i >= 8 {
                let gidx = base_idx + i;
                assert!(
                    gidx + 8 <= st.cap(),
                    "rxdesc window exceeds array cap (HFT_RXDESC_CAP)"
                );
                let mut expect = [0u64; 8];
                for g in 0..8 {
                    let body = recs[i + g].body;
                    let off = (body.as_ptr() as usize).wrapping_sub(self.blob_base);
                    expect[g] = desc8_pack_span(off as u32, body.len() as u16) | affine_flag;
                }
                let bad = st.check_arr8(self.rx_win_slot, gidx, &expect);
                if bad != 0 {
                    for g in 0..8 {
                        if (bad >> g) & 1 == 0 {
                            continue;
                        }
                        let body = recs[i + g].body;
                        self.rx_tor_fix_guard(gidx + g);
                        if self.rx_affine {
                            let tag = self.rx_affine_tag_for(body);
                            st.set_affine_tag(self.rx_win_slot, gidx + g, &tag);
                        }
                        st.set_arr(self.rx_win_slot, gidx + g, expect[g]);
                        self.rx_fixes += 1;
                    }
                }
                i += 8;
            } else {
                let body = recs[i].body;
                let idx = base_idx + i;
                assert!(idx < st.cap(), "rxdesc window exceeds array cap (HFT_RXDESC_CAP)");
                let off = (body.as_ptr() as usize).wrapping_sub(self.blob_base);
                let w = desc8_pack_span(off as u32, body.len() as u16) | affine_flag;
                if st.get_arr(self.rx_win_slot, idx) != w {
                    self.rx_tor_fix_guard(idx);
                    if self.rx_affine {
                        let tag = self.rx_affine_tag_for(body);
                        st.set_affine_tag(self.rx_win_slot, idx, &tag);
                    }
                    st.set_arr(self.rx_win_slot, idx, w);
                    self.rx_fixes += 1;
                }
                i += 1;
            }
        }
    }

    /// R27c: the hole-free inline commit (packed form) — same law as
    /// `arr_check_fix` over the R28 packed words (the FUSED
    /// `check_arr8_packed` transform + compare per 8 spans, the
    /// batch-constant `delta`), cursor-free: the inline loop below owns
    /// `next_span`/`submit_rem`. See `arr_check_fix`'s doc for the
    /// epoch-survival law.
    #[inline(never)]
    fn packed_arr_check_fix(
        &mut self,
        st: &RxdescState,
        base_ptr: usize,
        words: &[u64],
        affine_flag: u64,
        delta: u64,
    ) {
        let n = words.len();
        let base_idx = (self.next_span - self.rx_win_base) as usize;
        let mut i = 0usize;
        while i < n {
            if n - i >= 8 {
                let gidx = base_idx + i;
                assert!(
                    gidx + 8 <= st.cap(),
                    "rxdesc window exceeds array cap (HFT_RXDESC_CAP)"
                );
                let group: &[u64; 8] = words[i..i + 8].try_into().unwrap();
                let bad = st.check_arr8_packed(self.rx_win_slot, gidx, group, delta, affine_flag);
                if bad != 0 {
                    for g in 0..8 {
                        if (bad >> g) & 1 == 0 {
                            continue;
                        }
                        let w = group[g];
                        let body = self.packed_body(base_ptr, w);
                        self.rx_tor_fix_guard(gidx + g);
                        if self.rx_affine {
                            let tag = self.rx_affine_tag_for(body);
                            st.set_affine_tag(self.rx_win_slot, gidx + g, &tag);
                        }
                        let expect =
                            ((w & nf_transport::rxdesc::RX_PACKED_LOW48) + delta) | affine_flag;
                        st.set_arr(self.rx_win_slot, gidx + g, expect);
                        self.rx_fixes += 1;
                    }
                }
                i += 8;
            } else {
                let w = words[i];
                let idx = base_idx + i;
                assert!(idx < st.cap(), "rxdesc window exceeds array cap (HFT_RXDESC_CAP)");
                let body = self.packed_body(base_ptr, w);
                let expect = ((w & nf_transport::rxdesc::RX_PACKED_LOW48) + delta) | affine_flag;
                if st.get_arr(self.rx_win_slot, idx) != expect {
                    self.rx_tor_fix_guard(idx);
                    if self.rx_affine {
                        let tag = self.rx_affine_tag_for(body);
                        st.set_affine_tag(self.rx_win_slot, idx, &tag);
                    }
                    st.set_arr(self.rx_win_slot, idx, expect);
                    self.rx_fixes += 1;
                }
                i += 1;
            }
        }
    }

    /// R28: reconstruct one packed span's body — `(base_ptr, word)` is the
    /// SAME slice the SpanRec emission referenced (the ladder's packed
    /// contract; the blob contract: immutable bytes, valid for the owning
    /// pass).
    #[inline(always)]
    fn packed_body<'b>(&self, base_ptr: usize, w: u64) -> &'b [u8] {
        let off = (w & nf_transport::rxdesc::RX_PACKED_LOW48) as u32 as usize;
        let len = ((w >> 32) & 0xFFFF) as usize;
        debug_assert!(base_ptr.wrapping_add(off) >= self.blob_base || self.blob_base == 0);
        // SAFETY: the word's (off, len) came from the ladder's span
        // discovery over the transport's blob frames — the identical
        // pointer/length pair the SpanRec path carried; the blob stays
        // alive and immutable for the pass's drain (the standing
        // HydraLane contract).
        unsafe { std::slice::from_raw_parts(base_ptr.wrapping_add(off) as *const u8, len) }
    }

    /// R28: the FUSED packed ladder-commit — the steady scan's packed
    /// words (batch-relative `(off, len, count)`) checked against the
    /// array with ONE vector transform + compare per 8 spans
    /// (`check_arr8_packed` — the count field masked, the flag OR'd, the
    /// batch-constant delta added). The preamble, the chunk-open assist
    /// decision, the inline branches and the publish cadence are
    /// `submit_span_batch_rx`'s, verbatim — only the span word's
    /// representation changed (no SpanRec materialization, no per-span
    /// pack arithmetic, no expect[] build).
    #[inline(never)]
    fn submit_packed_batch_rx(&mut self, base_ptr: usize, words: &[u64]) {
        if words.is_empty() {
            return;
        }
        let Some(st) = self.rx.clone() else {
            unreachable!("submit_packed_batch_rx without rxdesc state");
        };
        // The blob base — the packed batch's first body pointer (write-once
        // per run; MUST precede the first spans_ready Release, which the
        // window open + publishes below order it into).
        if self.blob_base == 0 {
            self.blob_base = base_ptr;
            st.capture_blob_base(self.blob_base);
        }
        // Lazy window open (identical trigger: this batch's first span).
        if !self.rx_win_open {
            self.rx_open_window();
        }
        // Flow control — once per batch (verbatim).
        if self.rx_assist_wm > 0 {
            let mut sb = 0u32;
            while self.pending() > Self::RX_PACE {
                self.fold_available();
                if sb > 2_000_000 { eprintln!("R29DBG PACE-SPIN pending={} fold_pos={} next_span={} fold_lane={} fold_rem={} cur_inline={:?}", self.pending(), self.fold_pos, self.next_span, self.fold_lane, self.fold_rem, self.cur_inline); }
                crate::affinity::polite_spin_deep(&mut sb);
            }
        }
        let affine_flag: u64 = if self.rx_affine { RX_AFFINE_FLAG } else { 0 };
        // R29: the self-serve window's steady batches — the commit path is
        // DELETED for this window (the whole span range is already
        // published; the workers own the ingest). The scan's words flow
        // through the count bookkeeping only: the cursors advance by the
        // run arithmetic (chunk-boundary cadence kept exact for the next
        // window's straddle logic), no inline claims, no vector check,
        // no per-chunk publish. An ACTIVE inline chunk (a mid-window
        // assist claimed before the arm, or the straddle continuation)
        // still routes through the classic path below — its values come
        // from the inline ring and the publication clamp keeps the
        // frontier monotone.
        if self.rx_win_ss && self.cur_inline.is_none() {
            let mut i = 0usize;
            while i < words.len() {
                let run = (self.submit_rem as usize).min(words.len() - i);
                i += run;
                self.next_span += run as u64;
                self.submit_rem -= run as u64;
                if self.submit_rem == 0 {
                    self.submit_rem = CHUNK;
                    self.submit_lane = if self.submit_lane + 1 == self.n_lanes {
                        0
                    } else {
                        self.submit_lane + 1
                    };
                }
            }
            return;
        }
        // R28: the batch-constant translation — batch-relative offsets to
        // blob-global (the array words' low 32 bits). One value per batch.
        let delta = (base_ptr as u64).wrapping_sub(self.blob_base as u64);
        debug_assert!(delta <= u32::MAX as u64, "R28 batch delta overflows u32");
        let mut i = 0usize;
        while i < words.len() {
            // Chunk-open: the assist decision — the batched path's grid
            // boundary logic, verbatim.
            if self.submit_rem == CHUNK {
                let chunk_id = self.next_span / CHUNK;
                let take_inline = if self.force_inline {
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
                    // The claim MUST be visible before the spans_ready
                    // store that exposes this chunk's spans.
                    st.mark_inline(chunk_id);
                }
            }
            // The run: up to one chunk (or the batch tail) in ONE mode.
            let run = (self.submit_rem as usize).min(words.len() - i);
            if let Some(slot) = self.cur_inline {
                // R27c: the hole-free inline commit — the packed twin (see
                // `packed_arr_check_fix`'s doc).
                self.packed_arr_check_fix(&st, base_ptr, &words[i..i + run], affine_flag, delta);
                for k in 0..run {
                    let body = self.packed_body(base_ptr, words[i + k]);
                    self.submit_inline_span(slot, body);
                    self.next_span += 1;
                    self.submit_rem -= 1;
                }
                i += run;
            } else {
                self.submit_packed_arr_run(&st, base_ptr, &words[i..i + run], affine_flag, delta);
                i += run;
            }
            // Chunk completion — the cadence, verbatim.
            if self.submit_rem == 0 {
                self.submit_rem = CHUNK;
                self.submit_lane = if self.submit_lane + 1 == self.n_lanes {
                    0
                } else {
                    self.submit_lane + 1
                };
                if self.cur_inline.is_some() {
                    self.flush_pending();
                }
                self.rx_publish_ready();
            }
        }
    }

    /// R28: one array-mode run over packed words — 8-wide groups through
    /// the FUSED vector check (`check_arr8_packed`); the mismatch mask
    /// routes exactly the divergent lanes through the scalar fix path
    /// (the cold path re-transforms the lane's expected word and
    /// reconstructs the body from `(base_ptr, word)`). Advances
    /// `next_span` and `submit_rem` by the run length. The caller proves
    /// the run stays within one chunk and the window is open.
    fn submit_packed_arr_run(
        &mut self,
        st: &RxdescState,
        base_ptr: usize,
        words: &[u64],
        affine_flag: u64,
        delta: u64,
    ) {
        let n = words.len();
        let mut i = 0usize;
        while i < n {
            if n - i >= 16 {
                // ── the R31-B 16-wide FUSED group (dual ZMM) ──
                let base_idx = (self.next_span - self.rx_win_base) as usize;
                assert!(
                    base_idx + 16 <= st.cap(),
                    "rxdesc window exceeds array cap (HFT_RXDESC_CAP)"
                );
                let group: &[u64; 16] = words[i..i + 16].try_into().unwrap();
                let bad =
                    st.check_arr16_packed(self.rx_win_slot, base_idx, group, delta, affine_flag);
                if bad != 0 {
                    // The COLD path — the same per-lane fix, in the same
                    // ascending order (16 lanes).
                    for g in 0..16 {
                        if (bad >> g) & 1 == 0 {
                            continue;
                        }
                        let w = group[g];
                        let body = self.packed_body(base_ptr, w);
                        // R27: the toroidal window's shared-slot write —
                        // wait for the lagging reader of the previous
                        // window's same word.
                        self.rx_tor_fix_guard(base_idx + g);
                        if self.rx_affine {
                            let tag = self.rx_affine_tag_for(body);
                            st.set_affine_tag(self.rx_win_slot, base_idx + g, &tag);
                        }
                        let expect =
                            ((w & nf_transport::rxdesc::RX_PACKED_LOW48) + delta) | affine_flag;
                        st.set_arr(self.rx_win_slot, base_idx + g, expect);
                        self.rx_fixes += 1;
                    }
                }
                self.next_span += 16;
                self.submit_rem -= 16;
                i += 16;
            } else if n - i >= 8 {
                // ── the 8-wide FUSED group ──
                let base_idx = (self.next_span - self.rx_win_base) as usize;
                assert!(
                    base_idx + 8 <= st.cap(),
                    "rxdesc window exceeds array cap (HFT_RXDESC_CAP)"
                );
                let group: &[u64; 8] = words[i..i + 8].try_into().unwrap();
                let bad = st.check_arr8_packed(self.rx_win_slot, base_idx, group, delta, affine_flag);
                if bad != 0 {
                    // The COLD path — the same per-lane fix the batched
                    // SpanRec path applies, in the same ascending order.
                    for g in 0..8 {
                        if (bad >> g) & 1 == 0 {
                            continue;
                        }
                        let w = group[g];
                        let body = self.packed_body(base_ptr, w);
                        // R27: the toroidal window's shared-slot write —
                        // wait for the lagging reader of the previous
                        // window's same word.
                        self.rx_tor_fix_guard(base_idx + g);
                        if self.rx_affine {
                            let tag = self.rx_affine_tag_for(body);
                            st.set_affine_tag(self.rx_win_slot, base_idx + g, &tag);
                        }
                        let expect =
                            ((w & nf_transport::rxdesc::RX_PACKED_LOW48) + delta) | affine_flag;
                        st.set_arr(self.rx_win_slot, base_idx + g, expect);
                        self.rx_fixes += 1;
                    }
                }
                self.next_span += 8;
                self.submit_rem -= 8;
                i += 8;
            } else {
                // ── the scalar tail (run length not a multiple of 8) ──
                let w = words[i];
                let idx = (self.next_span - self.rx_win_base) as usize;
                assert!(idx < st.cap(), "rxdesc window exceeds array cap (HFT_RXDESC_CAP)");
                let body = self.packed_body(base_ptr, w);
                let expect = ((w & nf_transport::rxdesc::RX_PACKED_LOW48) + delta) | affine_flag;
                if st.get_arr(self.rx_win_slot, idx) != expect {
                    // R27: the toroidal window's shared-slot write — wait
                    // for the lagging reader of the previous window's same
                    // word.
                    self.rx_tor_fix_guard(idx);
                    if self.rx_affine {
                        let tag = self.rx_affine_tag_for(body);
                        st.set_affine_tag(self.rx_win_slot, idx, &tag);
                    }
                    st.set_arr(self.rx_win_slot, idx, expect);
                    self.rx_fixes += 1;
                }
                self.next_span += 1;
                self.submit_rem -= 1;
                i += 1;
            }
        }
    }

    /// R23c/R24: resolve one span's affine tag — the RX producer's ledger
    /// first (the monotone pointer cursor, O(1) amortized; the pointer +
    /// length match proves the row is THIS body), the direct body scan as
    /// the fallback (divergent schedules, ledger-less runs — the
    /// first-fill shapes where no transport was attached). Both paths
    /// are bit-identical (`affine_frame_tag`; the nf-testkit differential
    /// pins the kernel against the reference). COLD PATH (the word
    /// mismatch path) — `#[inline(never)]` keeps its body-scan branch
    /// out of the steady loop's µop-cache window (the R9d discipline).
    ///
    /// R24 (Track 2): the packer folds the 8 lane registers into the
    /// golden VALUE word here (the SPANHASH-64 combine — one clmul-class
    /// evaluation per COLD fill, never in a measured window; the sidecar
    /// ships the value, the worker loads it).
    #[inline(never)]
    fn rx_affine_tag_for(&mut self, body: &[u8]) -> [u64; RX_AFFINE_TAG_WORDS] {
        if let Some(st) = &self.rx {
            if let Some((e, cur)) = st.ledger_lookup(
                self.rx_affine_cursor,
                body.as_ptr() as usize,
                body.len() as u32,
            ) {
                self.rx_affine_cursor = cur;
                let value =
                    crate::crcfold::span_crc32c_8lane_from_tags(&e.lanes, e.body_len as usize);
                return affine_tag_pack(
                    &AffineTagCore {
                        prefix_crc: e.prefix_crc,
                        cum_crc: e.cum_crc,
                        raw_crc: e.raw_crc,
                        lanes: e.lanes,
                    },
                    value,
                );
            }
        }
        let core = affine_frame_tag(body);
        let value = crate::crcfold::span_crc32c_8lane_from_tags(&core.lanes, body.len());
        affine_tag_pack(&core, value)
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
                        // R37: fold_pos is the chunk start here (the exact
                        // match above) — leaf end = start + i + 1.
                        self.fold_value(v, self.fold_pos + i as u64 + 1);
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
            // R29: THE DERIVED GRID — `fold_lane`/`fold_rem` re-derived
            // from `fold_pos` at every loop top (drift-immune). The
            // pre-R29 accumulators (decrement per record, switch at zero)
            // were correct for the submission-bounded protocol, but the
            // self-serve shape adds three new park/resume edges (the
            // frontier cap, mid-chunk pass-boundary records, assist
            // straddles) — any accumulated-cursor slip desyncs the lane
            // from the chunk grid and the fold drains the WRONG lane's
            // ring, silently consuming out-of-order spans in release
            // (the 512-span drift class). Deriving from the position
            // makes the class structurally impossible; one shift + one
            // mod at the consumer's loop edge.
            self.fold_lane = ((self.fold_pos / CHUNK) % self.n_lanes as u64) as usize;
            self.fold_rem = CHUNK - (self.fold_pos % CHUNK);
            // H6: `fold_lane` tracks the chunk containing `fold_pos` — no
            // division in the hot path (advance after chunk completion).
            let lane = &fabric.lanes[self.fold_lane];
            let head = lane.res_head.load(Ordering::Acquire);
            let tail = lane.res_tail.load(Ordering::Relaxed); // main-owned cursor
            if tail == head {
                break; // this lane's next batch isn't ready — order is strict
            }
            // R25a: THE COMPOSITE DRAIN — one 16-byte record per SEGMENT
            // (≤ CHUNK spans, ending at grid-chunk ends / batch publishes
            // / pass boundaries by the worker's break discipline). The
            // per-record cost is TWO dependent ops (`rotl` + `xor` —
            // `sink::fold_compose_apply`); the per-span Res load, the
            // span_id assert and the serial rotl⊕IMUL chain are all gone
            // — this is the loop the Run 500+ diagnosis pinned as the
            // consumer-side strangler. Records never straddle a pending
            // pass boundary (the worker splits at pass-start records),
            // so the boundary cap drops to the debug assert; records end
            // at grid-chunk ends, so `k ≤ fold_rem` always holds and the
            // lane switch fires exactly at record boundaries.
            if fabric.composite_res {
                // SAFETY: slots [tail & RES_MASK, +count) published by the
                // worker's Release store to res_head (loaded Acquire
                // above) and owned by main until the res_tail Release
                // store below. Per-slot masking handles the ring wrap.
                let slots = lane.res_slots_read();
                let mut consumed: u64 = 0;
                while tail + consumed < head {
                    let r = &slots[((tail + consumed) & RES_MASK) as usize];
                    let k = r.aux as u64;
                    // R29: THE FRONTIER CAP — the fold never consumes
                    // spans the scan has not emitted. The self-serve
                    // workers run AHEAD of the scan (the whole window is
                    // pre-published), so their results can end beyond
                    // `next_span`; such records stay buffered in the ring
                    // until the scan (and the pass's boundary stamp at
                    // end_pass) advances past them. Without the cap the
                    // fold crosses the pending pass boundary BEFORE its
                    // `end_span` is stamped — complete_boundaries breaks
                    // on the unstamped record, the next pass's values
                    // fold into THIS pass's chain, and the boundary
                    // snapshot lands on the contaminated hash (the
                    // deterministic pass-4 divergence). The cap also
                    // restores the pre-R29 invariant verbatim: results
                    // were never consumable past the submission frontier.
                    // R36-E: THE WRAP-AWARE CAP — the record's end vs the
                    // submission frontier, in the u32 span-id domain with
                    // serial-number semantics (RFC-1982 class: end >_ser
                    // next ⟺ (end − next) mod 2^32 ∈ (0, 2^31)). The naive
                    // u64 compare breaks past 2^32 spans — the elision's
                    // own success (50B+ msg/s × 5s = 5–8B spans) would trip
                    // it ~40% into the window; the serial window (2^31
                    // spans ≈ 2^31 msgs × 46) is nine orders beyond any
                    // real fold lag, so the semantics are exact here.
                    let end32 = r.span_id.wrapping_add(k as u32);
                    let next32 = self.next_span as u32;
                    let dcap = end32.wrapping_sub(next32);
                    if dcap != 0 && dcap < 0x8000_0000 {
                        // R29: PARK — release the consumed prefix and
                        // return. The outer loop's condition
                        // (`fold_pos < next_span`) stays true while this
                        // record is capped, so a plain `break` + `continue`
                        // would spin on it; parking defers to the next
                        // drain call (the scan advances `next_span` by
                        // then — the SS batches are 32 spans, the records
                        // 64).
                        lane.res_tail.store(tail + consumed, Ordering::Release);
                        self.complete_boundaries();
                        return;
                    }
                    assert!(
                        k >= 1 && k <= self.fold_rem,
                        "composite record k={k} exceeds fold_rem={} at span {}",
                        self.fold_rem,
                        r.span_id
                    );
                    assert_eq!(
                        r.span_id, self.fold_pos as u32,
                        "hydra composite fold-order violation: got span {} (k {}, B {:#x}), expected {} (lane {}, tail {}, head {})",
                        r.span_id,
                        r.aux,
                        r.value,
                        self.fold_pos,
                        self.fold_lane,
                        tail,
                        head
                    );
                    if self.pass_tail != self.pass_head {
                        let rec = &self.passes[self.pass_tail];
                        debug_assert!(
                            rec.done
                                || rec.end_span == u64::MAX
                                || self.fold_pos + k <= rec.end_span,
                            "composite record straddled a pass boundary ({}+{} > {})",
                            self.fold_pos,
                            k,
                            rec.end_span
                        );
                    }
                    // R37 — THE JOURNALING: the record becomes a leaf
                    // `(k, B)` of the current pass's tree (applied at
                    // the boundary flush by the ⌈log₂ K⌉ up-sweep + ONE
                    // `fold_compose_apply` hop — the serial chain's
                    // dependent-apply loop-carried dependency leaves
                    // the drain; the load/assert/store pipeline).
                    // Rollback (`HFT_TREE_FOLD=0`): the serial apply
                    // verbatim. Leaf end = fold_pos + k (the record's
                    // exact end — records never straddle boundaries).
                    self.journal.push(
                        &mut self.hash,
                        k as u32,
                        r.value,
                        self.fold_pos + k as u64,
                    );
                    self.fold_pos += k;
                    self.fold_rem -= k;
                    consumed += 1;
                    if self.fold_rem == 0 {
                        self.fold_rem = CHUNK;
                        self.fold_lane = if self.fold_lane + 1 == self.n_lanes {
                            0
                        } else {
                            self.fold_lane + 1
                        };
                        // The lane's later chunks (its ring may hold more
                        // of them) wait their grid turn — the ORDER LAW.
                        break;
                    }
                    // A record ending exactly at a boundary snapshots now.
                    self.complete_boundaries();
                }
                lane.res_tail.store(tail + consumed, Ordering::Release);
                self.complete_boundaries();
                continue;
            }
            // Never fold past the end of fold_pos's chunk (lane changes there).
            // R29: and never past the scan's frontier (`next_span`) — the
            // self-serve workers' results may end beyond it; the excess
            // stays buffered until the scan advances (same law as the
            // composite drain's cap above).
            let mut n = (head - tail)
                .min(self.fold_rem)
                .min(self.next_span - self.fold_pos);
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
                // R36-E: the wrap-aware order compare (the span-id domain
                // is u32; fold_pos's low word is the exact expectation —
                // order violations are local, never 2^31 spans apart).
                assert_eq!(
                    r.span_id,
                    (self.fold_pos + i as u64) as u32,
                    "hydra fold-order violation: got span {}, expected {} (lane {}, tail {}, head {}, n {})",
                    r.span_id,
                    self.fold_pos + i as u64,
                    self.fold_lane,
                    tail,
                    head,
                    n
                );
                let v = r.value;
                // R37: fold_pos advances after the batch (n-capped) — the
                // i-th leaf's end is start + i + 1.
                self.fold_value(v, self.fold_pos + i as u64 + 1);
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
                // R27b: stamp the record slot's fold watermark (same law
                // as end_pass's close — the burst shape's windows own
                // record slots too).
                self.rx_rec_end[self.rx_win_rec as usize] = self.next_span;
                let win_count = self.next_span - self.rx_win_base;
                if let Some(st) = &self.rx {
                    st.set_last_window_count(win_count);
                }
                // R27: the epoch update at the burst shape's window close
                // (same law as end_pass — a fresh sink follows the drain,
                // so the epoch arms across the generation boundary).
                let clean = self.rx_fixes == self.rx_win_fix0 && win_count == self.rx_win_count0;
                self.rx_steady_epoch = if clean {
                    self.rx_steady_epoch.saturating_add(1)
                } else {
                    0
                };
                // R31-A: the per-slot twin (same law as end_pass's close —
                // the burst shape's windows arm the warm-epoch gate too).
                let s = self.rx_win_slot as usize;
                let slot_clean = self.rx_fixes == self.rx_win_fix0
                    && self.rx_slot_count[s] != u64::MAX
                    && win_count == self.rx_slot_count[s];
                self.rx_slot_epoch[s] = if slot_clean {
                    self.rx_slot_epoch[s].saturating_add(1)
                } else {
                    0
                };
                self.rx_slot_count[s] = win_count;
                self.rx_win_open = false;
            }
            self.rx_publish_ready();
            // R29: the self-serve window's close (the burst shape's
            // finish — same reconcile-then-retire law as end_pass).
            self.rx_win_ss = false;
            self.rx_win_pre = 0;
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
        // R37: the final flush — the burst shape reads `hash` directly
        // after finish (no pass boundaries ever fire there), and the pass
        // shape's tail past the last completed boundary needs its leaves
        // applied. No-op when the boundary loop already drained the
        // journal (the steady law).
        self.journal.flush_all(&mut self.hash);
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
        // R29: saturating — the self-serve fold legitimately runs AHEAD of
        // the scan (it consumes the workers' pre-published results while
        // the sequencer ladder still walks); a raw subtract would wrap to
        // ~u64::MAX in release and livelock the pace gate.
        self.next_span.saturating_sub(self.fold_pos)
    }

    /// R8 phase-5 telemetry: chunks taken through the assist path.
    #[inline]
    pub fn assist_chunks(&self) -> u64 {
        self.assist_chunks
    }

    /// R37 telemetry: the fold journal's engagement contract —
    /// `(tree, flushes, overflows, records, compose_max, live_len)`
    /// (the FOLDTREE_DIAGNOSTIC line's source). Engagement law: on a
    /// pass-shaped run, `flushes` ≈ completed boundaries (+ the rare
    /// capacity flushes), `records` = the drained composite records,
    /// and `live_len` = 0 at rest (every leaf flushed at its
    /// boundary / the finish).
    #[inline]
    pub fn fold_tree_stats(&self) -> (bool, u64, u64, u64, u64, u64) {
        self.journal.stats()
    }

    /// R16b telemetry: check-and-fix corrections applied (0 on a clean
    /// schedule — every prefill entry was already correct; > 0 under
    /// divergence — duplicates/colds/gaps drift the frame index).
    #[inline]
    pub fn rx_fixes(&self) -> u64 {
        self.rx_fixes
    }

    /// R29 telemetry: self-serve windows armed (the zero-thread ingest's
    /// engagement counter — 0 means the epoch never reached the deep-steady
    /// threshold, the classic commit cadence ran instead).
    #[inline]
    pub fn selfserve_wins(&self) -> u64 {
        self.rx_ss_wins
    }

    /// R31-A telemetry: rotating windows opened WITHOUT the warm copy
    /// (the warm-epoch gate's engagement counter — 0 means every open
    /// paid the ~252KB copy_arr/copy_tags pair, the pre-R31 shape).
    #[inline]
    pub fn warm_skips(&self) -> u64 {
        self.rx_warm_skips
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
                // R37: fold_pos is this span's start (the +1 below) — the
                // leaf's end.
                self.fold_value(v, self.fold_pos + 1);
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
                    // R37: fold_pos is this span's start (the +1 below) —
                    // the leaf's end.
                    self.fold_value(v, self.fold_pos + 1);
                    self.next_span += 1;
                    self.fold_pos += 1;
                }
            }
            Some(_) => {
                if self.rx.is_some() && self.batch_submit {
                    // R25c: the batched array commit — one Arc clone, one
                    // pace gate and one window check per batch; the
                    // check-and-fix 8-wide. HFT_BATCH_SUBMIT=0 rolls back
                    // to the per-span loop below.
                    self.submit_span_batch_rx(recs);
                } else {
                    for r in recs {
                        self.submit_span(r.body);
                        self.next_span += 1;
                    }
                }
                // R16b: NO publish here — the chunk-completion boundary
                // inside submit_span_rx / submit_span_batch_rx owns the
                // cadence (per-CHUNK, the ring protocol's shape);
                // end_pass/finish publish the tail.
            }
        }
    }

    /// R28: the fused packed ladder-commit — armed iff the array commit
    /// path is (the ladder then packs each span's (off, len, count) word
    /// at discovery and this sink never materializes a SpanRec). The
    /// ring path (rx == None) and the per-span rollback keep the
    /// SpanRec emission.
    #[inline(always)]
    fn wants_packed_spans(&self) -> bool {
        self.rx.is_some() && self.batch_submit && self.packed_emission
    }

    /// R28: the packed batch — the G-INV assert, ONE continuity assert at
    /// the batch head (the packed contract: the ladder's chain law proves
    /// the words consecutive in sequence space — steady_step emits only
    /// contiguous-apply frames and the ladder group verifies the chain),
    /// the count/last_seq bookkeeping from the batch's (first_seq, total)
    /// header, then the FUSED array commit
    /// (`submit_packed_batch_rx` — one vector transform + compare per 8
    /// spans). The non-rx shapes (defense — the arm never selects them)
    /// reconstruct each body from `(base_ptr, word)` and route through
    /// the verbatim per-span paths.
    #[inline(always)]
    fn on_span_batch_packed(
        &mut self,
        proof: &LiveFeedProof,
        first_seq: u64,
        total: u64,
        base_ptr: usize,
        words: &[u64],
    ) {
        assert!(
            proof.gen() >= self.last_gen,
            "G-INV violation: proof gen {} is older than sink last_gen {}",
            proof.gen(),
            self.last_gen
        );
        if total == 0 {
            return;
        }
        if self.last_seq != 0 {
            assert_eq!(
                first_seq,
                self.last_seq + 1,
                "Non-monotonic packed span batch: expected {}, got {} (total={})",
                self.last_seq + 1,
                first_seq,
                total
            );
        }
        self.last_seq = first_seq + total - 1;
        self.count += total;
        match self.fabric {
            None => {
                // Inline mode: evaluate + fold NOW — sequential-equivalent
                // (the body reconstructed from the packed word).
                for &w in words {
                    let body = self.packed_body(base_ptr, w);
                    let v = span_crc32c_8lane(body);
                    // R37: fold_pos is this span's start (the +1 below) —
                    // the leaf's end.
                    self.fold_value(v, self.fold_pos + 1);
                    self.next_span += 1;
                    self.fold_pos += 1;
                }
            }
            Some(_) => {
                if self.rx.is_some() && self.batch_submit && self.packed_emission {
                    // R28: the FUSED commit — the steady scan's words flow
                    // straight into the vector check (HFT_BATCH_SUBMIT=0 /
                    // HFT_PACKED_EMISSION=0 roll back to the SpanRec
                    // emission upstream).
                    self.submit_packed_batch_rx(base_ptr, words);
                } else {
                    for &w in words {
                        let body = self.packed_body(base_ptr, w);
                        self.submit_span(body);
                        self.next_span += 1;
                    }
                }
                // R16b: NO publish here — the chunk-completion boundary
                // inside the submit paths owns the cadence.
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

    /// R25a: the composite-fold law's differential — the algebraic heart
    /// of the chunk-composite path. For every segment length k (1..=64,
    /// all residues hitting the 13k mod 64 rotation table), the
    /// step-by-step reference (one `fold_step` per span — the law
    /// `h ← rotl(h,13) ^ (v·P)`) must equal ONE composite application
    /// (`fold_compose_apply(h, k, B)` with `B = fold_compose_b(terms)`)
    /// — 10,000 random vectors. The SPLIT variant pins the pass-boundary
    /// discipline: a segment split at ANY interior point m composes into
    /// the same chain as the unsplit segment (the worker's split at
    /// pass-start records and batch ends is exactly this shape, and the
    /// consumer applies the two records in sequence).
    #[test]
    fn t_r25_composite_law() {
        use crate::sink::{fold_compose_apply, fold_compose_b, fold_step, fold_term};
        let mut rng = 0x0DDB_1A5C_0FF5_0001u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut splits_checked = 0u32;
        for _ in 0..10_000 {
            let k = 1 + next() % 64;
            let h0 = next();
            let vals: Vec<u64> = (0..k).map(|_| next() | 1).collect();
            // (a) step-by-step reference.
            let mut h_step = h0;
            for &v in &vals {
                h_step = fold_step(h_step, v);
            }
            // (b) one composite application.
            let terms: Vec<u64> = vals.iter().map(|&v| fold_term(v)).collect();
            let b = fold_compose_b(&terms);
            let h_comp = fold_compose_apply(h0, k as u32, b);
            assert_eq!(
                h_step, h_comp,
                "composite diverged from step fold (k={k}) — the composition law broke"
            );
            // (c) split at an interior point m — the boundary/batch split
            // discipline (two records in sequence == one record).
            if k > 1 {
                let m = 1 + next() % (k - 1);
                let b1 = fold_compose_b(&terms[..m as usize]);
                let b2 = fold_compose_b(&terms[m as usize..]);
                let h_split = fold_compose_apply(
                    fold_compose_apply(h0, m as u32, b1),
                    (k - m) as u32,
                    b2,
                );
                assert_eq!(
                    h_step, h_split,
                    "split composite diverged (k={k}, m={m}) — segment splits are not compositional"
                );
                splits_checked += 1;
            }
        }
        assert!(splits_checked > 9_000, "split coverage too thin");
        // (d) the k=64 identity rotation: 13·64 ≡ 0 (mod 64) — a full
        // chunk applies as a pure XOR (the fast path's shape).
        let terms: Vec<u64> = (0..64).map(|_| next() | 1).map(fold_term).collect();
        let b = fold_compose_b(&terms);
        let h0 = next();
        assert_eq!(fold_compose_apply(h0, 64, b), h0 ^ b);
    }

    /// R37 cell A (sink law): the segment-monoid tree — `seg_compose`'s
    /// 3-factor associativity + the up-sweep vs the iterated serial
    /// applies on random `(k, B)` leaf streams (K ∈ {2..600}, odd
    /// tails included), the closed form == the step chain, and the
    /// `(0, 0)` identity apply. Theorem 19/22 on the rotation ring —
    /// the engine journal's own law.
    #[test]
    fn t_r37_segment_tree_law() {
        use crate::sink::{fold_compose_apply, seg_compose, tree_fold_segments};
        let mut rng = 0x0DDB_1A5C_0FF5_0371u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        // (a) the 3-factor law on random triples.
        for _ in 0..20_000 {
            let a = (1 + (next() % 512) as u32, next());
            let b = (1 + (next() % 512) as u32, next());
            let c = (1 + (next() % 512) as u32, next());
            assert_eq!(
                seg_compose(seg_compose(a, b), c),
                seg_compose(a, seg_compose(b, c)),
                "segment 3-factor law"
            );
        }
        // (b) the tree vs the iterated serial applies.
        for &k in &[
            2usize, 3, 4, 5, 7, 8, 9, 16, 17, 63, 100, 128, 194, 200,
            255, 321, 322, 512, 513, 600,
        ] {
            for _ in 0..200 {
                let leaves: Vec<(u32, u64)> =
                    (0..k).map(|_| (1 + (next() % 64) as u32, next())).collect();
                let h0 = next();
                // The serial chain (the R36 fold's bracketing).
                let mut h_serial = h0;
                let mut kk = 0u32;
                for &(k_i, b_i) in &leaves {
                    h_serial = fold_compose_apply(h_serial, k_i, b_i);
                    kk = kk.wrapping_add(k_i);
                }
                // The tree (SoA columns).
                let mut kc: Vec<u32> = leaves.iter().map(|&x| x.0).collect();
                let mut bc: Vec<u64> = leaves.iter().map(|&x| x.1).collect();
                let (kt, bt) = tree_fold_segments(&mut kc, &mut bc);
                assert_eq!(kt, kk, "tree k total (K={k})");
                let h_tree = fold_compose_apply(h0, kt, bt);
                assert_eq!(h_tree, h_serial, "segment tree != serial (K={k})");
            }
        }
        // (c) the identity: (0, 0) applies as a no-op.
        let h = next();
        assert_eq!(fold_compose_apply(h, 0, 0), h);
    }

    /// R37 cell B (the journal): FoldJournal's own laws — random leaf
    /// streams with random boundary splits (flush_upto mid-journal,
    /// including splits that leave remainders — the mixed-pass shape),
    /// forced capacity overflows, and the tree-vs-serial differential
    /// through the journal's full push/flush protocol. The small
    /// capacity forces the overflow arm; boundaries at every offset
    /// pin the partition law.
    #[test]
    fn t_r37_journal_flush_laws() {
        let mut rng = 0x0DDB_1A5C_0FF5_0372u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        // The oracle: the serial apply chain over the full stream.
        for trial in 0..400 {
            // Small caps force overflows: 1, 2, 3, 8, 16, 64.
            let cap = 1 << (next() % 7);
            let n = 1 + (next() % 400) as usize;
            let seed = 0x5FAD_EED5_0372_0000u64.wrapping_add(next() & 0xFF);
            let leaves: Vec<(u32, u64)> = (0..n)
                .map(|_| (1 + (next() % 64) as u32, next() | 1))
                .collect();
            // ends: cumulative (the journal's monotone law).
            let ends: Vec<u64> = {
                let mut acc = 0u64;
                leaves
                    .iter()
                    .map(|&(k, _)| {
                        acc += k as u64;
                        acc
                    })
                    .collect()
            };
            // The serial oracle (R36's law).
            let mut h_serial = seed;
            for &(k, b) in &leaves {
                h_serial = crate::sink::fold_compose_apply(h_serial, k, b);
            }
            // The journal run: push all leaves; flush at random
            // boundaries (including interior points and the final
            // end); the hash must match the serial oracle EXACTLY.
            let mut j = FoldJournal::with_capacity(true, cap.max(1));
            let mut h = seed;
            // Random flush schedule: pick ~n/8 random boundary points
            // (subsets of the ends, in order) + the final flush_all.
            let mut flush_pts: Vec<u64> = (0..n / 8 + 2)
                .map(|_| ends[next() as usize % n])
                .collect();
            flush_pts.push(u64::MAX); // the final flush
            let mut fi = 0usize;
            for (i, &(k, b)) in leaves.iter().enumerate() {
                j.push(&mut h, k, b, ends[i]);
                while fi < flush_pts.len() && flush_pts[fi] <= ends[i] {
                    // Flush at this boundary point.
                    let bnd = if flush_pts[fi] == u64::MAX {
                        u64::MAX
                    } else {
                        flush_pts[fi]
                    };
                    j.flush_upto(&mut h, bnd);
                    fi += 1;
                }
            }
            j.flush_all(&mut h);
            assert_eq!(h, h_serial, "journal tree != serial (trial {trial}, cap {cap}, n {n})");
            assert_eq!(j.len, 0, "journal not empty at rest");
            assert!(j.flushes > 0);
            assert_eq!(j.records, n as u64, "record count law");
            // The rollback twin: push serially through the journal's
            // serial mode — verbatim equality with the oracle.
            let mut j2 = FoldJournal::with_capacity(false, cap.max(1));
            let mut h2 = seed;
            for (i, &(k, b)) in leaves.iter().enumerate() {
                j2.push(&mut h2, k, b, ends[i]);
            }
            j2.flush_all(&mut h2); // no-op (serial mode)
            assert_eq!(h2, h_serial, "journal serial rollback (trial {trial})");
        }
    }

    /// R23c: the affine-tag protocol's end-to-end bit-parity — the rxdesc
    /// array path with the tag sidecar armed, LEDGER-LESS (the sink's cold
    /// fill scans the bodies — the fallback), two generations (the second
    /// rides the warm-copied tags), all vs the sequential reference. The
    /// workers evaluate every span from the sidecar; the tag integrity
    /// check + the fold assert pin the values. On x86_64 the workers'
    /// counters must show tag hits and ZERO payload fallbacks.
    #[test]
    fn t_r23c_affine_tag_parity() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"AFFINEHYDR";
        // The fabric with the array path + diet worker (the CI 11ab arm's
        // shape). No transport attachment — the LEDGER-LESS fill.
        let fabric = HydraFabric::spawn_pinned_full(2, &[], true, true, true, true, true);
        let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
        let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);

        let want = seq_pass(&mut t1, sess);
        assert_eq!(want.0, 505_849, "the full population must verify");
        // Pass 1: the cold fill (every desc word mismatches the zeroed
        // arrays — the sink scans each body once, untimed semantics).
        let got1 = hydra_pass(&mut t2, sess, &fabric);
        assert_eq!(got1, want, "affine (ledger-less) fabric pass 1 diverged");
        // Pass 2: a FRESH sink = a fresh generation — the window warm
        // start must carry word AND tag forward; the steady check finds
        // them correct and the workers again evaluate from tags.
        let got2 = hydra_pass(&mut t2, sess, &fabric);
        assert_eq!(got2, want, "affine (warm-copied tags) fabric pass 2 diverged");

        let (armed, hits, fallbacks, ledger) = fabric.affine_diag();
        #[cfg(target_arch = "x86_64")]
        {
            assert!(armed, "the affine sidecar must be armed on x86_64");
            assert_eq!(ledger, 0, "ledger-less shape: no transport attached");
            // The hit floor is machine-agnostic: the work-assist may take
            // chunks inline on small hosts (their evals are main-side and
            // uncounted); the LOAD-BEARING asserts are (a) tag evaluations
            // happened and (b) ZERO worker payload fallbacks.
            assert!(
                hits > 0,
                "workers must have evaluated spans from tags (got {hits})"
            );
            assert_eq!(
                fallbacks, 0,
                "zero payload fallbacks on the armed steady path (got {fallbacks})"
            );
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            assert!(!armed);
            let _ = (hits, fallbacks, ledger);
        }
    }

    /// R23c: the FULL pipeline wiring's bit-parity — the RX-pipelined
    /// transport with the fabric's rxdesc state attached via `set_rxdesc`
    /// (the RX producer's ledger fill fires at the reset serve), the sink
    /// resolving tags from the LEDGER, the workers evaluating O(1) with
    /// zero payload reads. Parity vs the sequential reference + the
    /// ledger/hit/fallback telemetry asserts.
    #[test]
    fn t_r23c_affine_ledger_pipeline_parity() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"AFFPIPELIN";
        let fabric = HydraFabric::spawn_pinned_full(2, &[], true, true, true, true, true);
        let mut piped = nf_transport::pipeline::PipelinedReplayTransport::new(&gt, sched.clone(), sess);
        // R23c: the attach BEFORE the first reset — arms the RX ledger.
        if let Some(st) = fabric.rxdesc_state() {
            assert!(piped.set_rxdesc(st), "set_rxdesc must attach once");
        }
        let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
        let want = seq_pass(&mut t1, sess);

        // The untimed reference-shaped pass: reset (the ledger fill fires
        // at the serve) → ingest → fold.
        piped.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new(&fabric);
        let ladder = crate::soa::ladder8_best();
        while piped.next_batch() {
            seq.ingest_entries_ladder(piped.entries(), piped.now_ns(), &mut sink, ladder);
            sink.drain_ready();
        }
        sink.finish();
        assert_eq!(
            (sink.count, sink.hash, sink.msg_hash),
            want,
            "affine LEDGER pipeline pass diverged"
        );

        let (armed, hits, fallbacks, ledger) = fabric.affine_diag();
        #[cfg(target_arch = "x86_64")]
        {
            assert!(armed, "the affine sidecar must be armed on x86_64");
            assert!(
                ledger >= 10_000,
                "the RX producer's ledger must have filled (got {ledger} rows)"
            );
            // Machine-agnostic floor (the assist may take chunks inline
            // on small hosts); the load-bearing asserts: the ledger
            // filled and the workers NEVER fell back to payload reads.
            assert!(hits > 0, "workers must have evaluated from tags");
            assert_eq!(
                fallbacks, 0,
                "zero payload fallbacks on the ledger path (got {fallbacks})"
            );
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            assert!(!armed);
            let _ = (hits, fallbacks, ledger);
        }
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
        hydra_pass_piped_mode(transport, fabric, true)
    }

    /// R25c: `hydra_pass_piped` with the commit-path mode pinned — the
    /// batched array commit (true) or the per-span rollback shape
    /// (false; `HFT_BATCH_SUBMIT=0`'s semantics, set directly on the
    /// sink so the differential runs in ONE process).
    fn hydra_pass_piped_mode(
        transport: &mut nf_transport::pipeline::PipelinedReplayTransport,
        fabric: &HydraFabric,
        batch: bool,
    ) -> ((u64, u64, u64), u64, u64) {
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new(fabric);
        sink.batch_submit = batch;
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

    /// R28: the packed-emission twin of [`hydra_pass_piped_mode`] — the
    /// sink's `packed_emission` field selects the fused ladder-commit (the
    /// HFT_PACKED_EMISSION=0 rollback's parity cell).
    fn hydra_pass_packed_mode(
        transport: &mut nf_transport::pipeline::PipelinedReplayTransport,
        fabric: &HydraFabric,
        packed: bool,
    ) -> ((u64, u64, u64), u64, u64) {
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new(fabric);
        sink.packed_emission = packed;
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
                    // R25a: BOTH res-ring formats — the composite default
                    // and the per-span rollback (the 11y arm's parity).
                    for comp in [true, false] {
                    let fabric = HydraFabric::spawn_pinned_full(w, &[], true, true, diet, comp, true);

                    // (a) the check+fix path (single-threaded transport: no
                    // prefill — every array entry written by the sink itself).
                    let mut t1 = ReplayTransport::new(&gt, sched.clone(), sess);
                    let got = hydra_pass(&mut t1, sess, &fabric);
                    assert_eq!(
                        got, want,
                        "rxdesc check+fix diverged (chaos={chaos}, w={w}, diet={diet}, comp={comp})"
                    );

                    // (b) forced-inline (every chunk on the submitting core;
                    // the workers' inline-skip path under the array protocol).
                    let mut t2 = ReplayTransport::new(&gt, sched.clone(), sess);
                    let got_force = assist_pass(&mut t2, sess, &fabric);
                    assert_eq!(
                        got_force, want,
                        "rxdesc forced-inline diverged (chaos={chaos}, w={w}, diet={diet}, comp={comp})"
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
                        "rxdesc pipelined diverged (chaos={chaos}, w={w}, diet={diet}, comp={comp})"
                    );
                    // Determinism + the warm start's fast path (a fresh sink
                    // — the generation re-anchor — warm-started from the
                    // previous pass's entries).
                    t3.reset(sess);
                    let (got4, fixes2, assists2) = hydra_pass_piped(&mut t3, &fabric);
                    assert_eq!(
                        got4, want,
                        "rxdesc pipelined determinism diverged (chaos={chaos}, w={w}, diet={diet}, comp={comp})"
                    );
                    if !chaos && assists1 + assists2 == 0 {
                        // The warm start's FAST PATH: with no assist chunks
                        // (inline evaluations leave array holes by design —
                        // the assist trades array writes for in-window CRC),
                        // the second pass must find EVERY entry already
                        // correct: zero stores on the submitting core.
                        assert_eq!(
                        fixes2, 0,
                        "steady second pass needed check-and-fix corrections (w={w}, diet={diet}, comp={comp}) — \
                         the warm start missed entries"
                    );
                    }
                    } // comp
                } // w
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════
    // R25c: the batched array commit (docs/40) — the differential suite
    // ══════════════════════════════════════════════════════════════════

    /// R25c: the batched commit path's two differential cells, per
    /// schedule shape × res format:
    ///
    /// (a) THE LAW CELL (deterministic, assist disabled —
    ///     `rx_assist_wm = 0`, single-threaded transport): every array
    ///     entry written by the SINK's own fix path (no prefill), both
    ///     the warm-start hit and the cold fill exercised, the vector
    ///     compare's mismatch lanes running the scalar fix verbatim.
    ///     The tuple must equal the sequential reference AND the fix
    ///     count must be EXACTLY the per-span mode's (the 8-wide
    ///     compare is exactly as strong as eight scalar compares —
    ///     the batch must fix precisely the same spans).
    ///
    /// (b) THE FULL-STACK CELL (pipelined + RX prefill, assists
    ///     allowed): both modes reproduce the sequential reference —
    ///     the fold order + inline routing invariants hold under
    ///     timing-dependent assist decisions (assist counts may differ
    ///     between the two runs; the tuple may not).
    #[test]
    fn t_r25c_batch_submit_parity() {
        let gt = load_mini();
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
                *b"R25CBATCHC"
            } else {
                *b"R25CBATCHS"
            };
            let mut t0 = ReplayTransport::new(&gt, sched.clone(), sess);
            let want = seq_pass(&mut t0, sess);
            assert_eq!(want.0, 505_849, "coverage must hold (chaos={chaos})");

            for comp in [true, false] {
                // (a) the law cell: pipelined transport WITHOUT the rxdesc
                // attach (no prefill — every array entry written by the
                // sink's own fix path), assists disabled — deterministic.
                // The ladder feeds on_span_batch (the batch commit's entry
                // point), unlike the classic per-frame path. Each mode gets
                // a FRESH fabric (a shared one warm-starts the second run
                // from the first's writes — the fix counts would not be
                // comparable).
                let mut results = Vec::new();
                for batch in [true, false] {
                    let fabric =
                        HydraFabric::spawn_pinned_full(2, &[], true, true, true, comp, true);
                    let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                        &gt,
                        sched.clone(),
                        sess,
                        128,
                    );
                    t.reset(sess);
                    let mut seq = Sequencer::new();
                    let mut sink = HydraSpanSink::new(&fabric);
                    sink.batch_submit = batch;
                    sink.rx_assist_wm = 0; // no assists — deterministic fixes
                    let ladder = crate::soa::ladder8_best();
                    while t.next_batch() {
                        seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut sink, ladder);
                        sink.drain_ready();
                    }
                    sink.finish();
                    results.push((
                        (sink.count, sink.hash, sink.msg_hash),
                        sink.rx_fixes(),
                        sink.assist_chunks(),
                    ));
                }
                let (got_b, fixes_b, assists_b) = results[0];
                let (got_s, fixes_s, assists_s) = results[1];
                assert_eq!(
                    got_b, want,
                    "R25c law cell diverged (batch, chaos={chaos}, comp={comp})"
                );
                assert_eq!(
                    got_s, want,
                    "R25c law cell diverged (per-span, chaos={chaos}, comp={comp})"
                );
                assert_eq!(
                    fixes_b, fixes_s,
                    "R25c fix-count divergence: the 8-wide compare must fix exactly the \
                     per-span mode's spans (chaos={chaos}, comp={comp})"
                );
                assert_eq!(
                    assists_b, 0,
                    "R25c law cell must take no assist chunks (chaos={chaos}, comp={comp})"
                );
                assert_eq!(
                    assists_s, 0,
                    "R25c law cell must take no assist chunks (per-span, chaos={chaos}, comp={comp})"
                );

                // (b) the full-stack cell: pipelined + the RX PREFILL
                // attached (set_rxdesc — the sustained arm's exact shape)
                // + assists allowed. Both modes must reproduce the
                // sequential reference; assist counts may differ with
                // timing, the tuple may not. One shared fabric here —
                // the prefill + warm start carry the entries across the
                // two modes (the sustained arm's own shape).
                let fabric = HydraFabric::spawn_pinned_full(2, &[], true, true, true, comp, true);
                let mut t3 = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                    &gt,
                    sched.clone(),
                    sess,
                    128,
                );
                if let Some(st) = fabric.rxdesc_state() {
                    assert!(t3.set_rxdesc(st), "set_rxdesc must attach once");
                }
                t3.reset(sess);
                let (got3, _f3, _a3) = hydra_pass_piped_mode(&mut t3, &fabric, true);
                assert_eq!(
                    got3, want,
                    "R25c pipelined batch diverged (chaos={chaos}, comp={comp})"
                );
                t3.reset(sess);
                let (got4, _f4, _a4) = hydra_pass_piped_mode(&mut t3, &fabric, false);
                assert_eq!(
                    got4, want,
                    "R25c pipelined per-span diverged (chaos={chaos}, comp={comp})"
                );
            } // comp
        } // chaos
    }

    /// R26: the worker's vertical 8-span GROUP parity — the group fast
    /// path (one 64 B arr load + one 64 B values load + one batched
    /// counter RMW per 8 spans, spray auto-muted in the all-tagged
    /// steady regime) vs the pre-R26 per-span walk verbatim
    /// (`HFT_WORKER_GROUP=0`, the 11g rollback's parity cell). Both
    /// worker shapes must reproduce the sequential reference bit-exact
    /// and fix EXACTLY the same spans in the deterministic law cell (the
    /// group's all-tagged arm must never diverge from the scalar
    /// ladder's per-span evaluation — the loads batch, the values may
    /// not). Cells: the law cell (no prefill, no assists — the fix path
    /// fills every entry; the groups interleave with scalar tails) and
    /// the full-stack pipelined cell (prefill + assists — the steady
    /// all-tagged regime where the group IS the path).
    #[test]
    fn t_r26_worker_group_parity() {
        let gt = load_mini();
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
                *b"R26GRPCHAC"
            } else {
                *b"R26GRPSTDY"
            };
            let mut t0 = ReplayTransport::new(&gt, sched.clone(), sess);
            let want = seq_pass(&mut t0, sess);
            assert_eq!(want.0, 505_849, "coverage must hold (chaos={chaos})");

            for comp in [true, false] {
                // (a) the law cell: no prefill, assists disabled — every
                // array entry written by the sink's own fix path, so the
                // workers' groups interleave with the scalar tails (mixed
                // tagged/untagged regimes re-arm the spray). Each shape
                // gets a FRESH fabric (warm-start carryover would make the
                // fix counts incomparable).
                let mut results = Vec::new();
                for group in [true, false] {
                    let fabric =
                        HydraFabric::spawn_pinned_full(2, &[], true, true, true, comp, group);
                    let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                        &gt,
                        sched.clone(),
                        sess,
                        128,
                    );
                    t.reset(sess);
                    let mut seq = Sequencer::new();
                    let mut sink = HydraSpanSink::new(&fabric);
                    sink.rx_assist_wm = 0; // no assists — deterministic fixes
                    let ladder = crate::soa::ladder8_best();
                    while t.next_batch() {
                        seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut sink, ladder);
                        sink.drain_ready();
                    }
                    sink.finish();
                    results.push(((sink.count, sink.hash, sink.msg_hash), sink.rx_fixes()));
                }
                let (got_g, fixes_g) = results[0];
                let (got_s, fixes_s) = results[1];
                assert_eq!(
                    got_g, want,
                    "R26 law cell diverged (group, chaos={chaos}, comp={comp})"
                );
                assert_eq!(
                    got_s, want,
                    "R26 law cell diverged (per-span, chaos={chaos}, comp={comp})"
                );
                assert_eq!(
                    fixes_g, fixes_s,
                    "R26 fix-count divergence: the group must fix exactly the per-span \
                     walk's spans (chaos={chaos}, comp={comp})"
                );

                // (b) the full-stack cell: pipelined + prefill + assists —
                // the steady all-tagged regime (the groups carry the whole
                // walk). One shared fabric, both shapes bit-exact.
                for group in [true, false] {
                    let fabric =
                        HydraFabric::spawn_pinned_full(2, &[], true, true, true, comp, group);
                    let mut t2 = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                        &gt,
                        sched.clone(),
                        sess,
                        128,
                    );
                    if let Some(st) = fabric.rxdesc_state() {
                        assert!(t2.set_rxdesc(st), "set_rxdesc must attach once");
                    }
                    t2.reset(sess);
                    let (got2, _f2, _a2) = hydra_pass_piped_mode(&mut t2, &fabric, true);
                    assert_eq!(
                        got2, want,
                        "R26 pipelined diverged (group={group}, chaos={chaos}, comp={comp})"
                    );
                }
            } // comp
        } // chaos
    }

    /// R27: the Zero-Reset Toroidal Pass parity — the steady epoch
    /// (frozen array slot, zero warm copies, rotating epoch-tagged
    /// records) vs the rotating regime (the HFT_TOROIDAL=0 rollback,
    /// the pre-R27 window open verbatim). The sustained multipass
    /// shape (pipelined + auto-advance + ONE sink, many passes):
    /// window 1 pays the cold fill (rotation), window 2 the warm copy
    /// (rotation, epoch=1 at close), windows 3+ are TOROIDAL — zero
    /// copies, the check-and-fix the whole safety net. Both modes must
    /// reproduce the sequential references bit-exact; the toroidal
    /// sink's epoch must ENGAGE (>= 2 by the run's end) and its
    /// steady-window fix count must stay ZERO after the warm-copy pass.
    /// The chaos schedule additionally exercises the re-earning law:
    /// its cold-fill window disarms the epoch, the deterministic
    /// replay re-arms it.
    #[test]
    fn t_r27_toroidal_multipass_parity() {
        let gt = load_mini();
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
            let sess = *b"R27TOROIDL";
            let mut t0 = ReplayTransport::new(&gt, sched.clone(), sess);
            let want = seq_pass(&mut t0, sess);
            assert_eq!(want.0, 505_849, "coverage must hold (chaos={chaos})");

            for comp in [true, false] {
                for tor in [true, false] {
                    let label = if tor { "toroidal" } else { "rotating" };
                    let fabric =
                        HydraFabric::spawn_pinned_full(1, &[], true, true, true, comp, true);
                    let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                        &gt,
                        sched.clone(),
                        sess,
                        128,
                    );
                    if let Some(st) = fabric.rxdesc_state() {
                        assert!(t.set_rxdesc(st), "set_rxdesc must attach once");
                    }
                    t.reset(sess);
                    let mut seq = Sequencer::new();
                    let mut sink = HydraSpanSink::new(&fabric);
                    sink.rx_toroidal = tor;
                    // The DETERMINISTIC law-cell shape (the R25c/R28
                    // discipline): assists disabled — an assist chunk's
                    // spans go INLINE (never checked/written into the
                    // array), and the TIMING-dependent assist pattern
                    // would leave a different set of holes every pass —
                    // the next window's check-and-fix repairs them (at a
                    // fix cost, exactly the production semantics) but the
                    // epoch's engagement becomes timing-dependent, which
                    // belongs on the FLEET arms (11t), not a parity cell.
                    sink.rx_assist_wm = 0;
                    let ladder = crate::soa::ladder8_best();
                    let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
                    let mut got = Vec::new();
                    // 12 passes: 1 rotation (cold fill) + 1 rotation (warm
                    // copy, epoch=1) + 10 toroidal.
                    for _pass in 0..12u64 {
                        t.reset(sess);
                        *seq = Sequencer::new_unboxed();
                        sink.begin_pass();
                        while t.next_batch() {
                            seq.ingest_entries_ladder(
                                t.entries(),
                                t.now_ns(),
                                &mut sink,
                                ladder,
                            );
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
                    for (k, rec) in got.iter().enumerate() {
                        assert_eq!(
                            *rec, want,
                            "R27 {label} pass {k} diverged (chaos={chaos}, comp={comp})"
                        );
                    }
                    if tor {
                        assert!(
                            sink.rx_steady_epoch >= 2,
                            "R27 toroidal epoch never engaged (chaos={chaos}, comp={comp}, \
                             epoch={})",
                            sink.rx_steady_epoch
                        );
                    }
                } // tor
            } // comp
        } // chaos
    }

    /// R27b: the record-ring watermark law — the soundness cell for the
    /// publication bound the toroidal epoch was missing. The record ring
    /// is a REC_RING-slot shared structure (R27c: 64 deep): a record published at window
    /// w overwrites window w−REC_RING's, and a worker still walking that
    /// window probes it at every boundary crossing (`resolve_rec` /
    /// `probe_wb`). The rotating regime's array-slot reuse gate bounded
    /// the sink's window-open lead over the fold implicitly (window
    /// w+REC_RING's open spun on the fold passing window w's end, and the
    /// two rings advanced in lockstep); R27 froze the array slot and
    /// deleted the gate, leaving NOTHING bounding the ring when the
    /// assist pace is disarmed — the fleet CI caught exactly that on the
    /// 120-shard matrix (20% of shards, timing-dependent arming, but a
    /// DETERMINISTIC wrong fold: the workers keep a stale rec_base/aslot
    /// and read array indices past the window into stale words —
    /// `t_r27 toroidal pass 1/2 diverged`, plus the composite
    /// pass-boundary split firing late and straddling).
    ///
    /// THE DETERMINISTIC SHAPE: the multipass cell with the fold never
    /// driven mid-pass (no `drain_ready`, assists off, one worker) —
    /// the sink laps the workers by the full 12 windows, the ring wraps
    /// four times, and the wrong fold reproduces WITHOUT timing luck
    /// (pre-fix, chaos=false/comp=false fails with hash 2532654608340517922 —
    /// the exact fleet value). Post-fix the record-ring watermark bounds
    /// the lead: window w's open waits for the fold to pass window
    /// w−REC_RING's end before overwriting its record (one local read +
    /// compare per open; the spin itself drives the fold, so liveness
    /// holds), every crossing resolves the live record, and all 12
    /// records come out bit-exact — while the toroidal epoch STILL
    /// engages (the watermark is not a rotating fallback: windows 3+
    /// stay zero-copy toroidal; asserted via the epoch). The rotating
    /// control (tor=false) pins the same shape through the classic
    /// regime's own gate.
    #[test]
    fn t_r27b_record_ring_watermark() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"R27BWRAP\0\0";
        let mut t0 = ReplayTransport::new(&gt, sched.clone(), sess);
        let want = seq_pass(&mut t0, sess);
        assert_eq!(want.0, 505_849, "coverage must hold");
        for comp in [true, false] {
            for tor in [true, false] {
                let label = if tor { "toroidal" } else { "rotating" };
                let fabric = HydraFabric::spawn_pinned_full(1, &[], true, true, true, comp, true);
                let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                    &gt,
                    sched.clone(),
                    sess,
                    128,
                );
                if let Some(st) = fabric.rxdesc_state() {
                    assert!(t.set_rxdesc(st), "set_rxdesc must attach once");
                }
                t.reset(sess);
                let mut seq = Sequencer::new();
                let mut sink = HydraSpanSink::new(&fabric);
                sink.rx_toroidal = tor;
                sink.rx_assist_wm = 0; // the deterministic law-cell shape
                let ladder = crate::soa::ladder8_best();
                for _pass in 0..12u64 {
                    t.reset(sess);
                    *seq = Sequencer::new_unboxed();
                    sink.begin_pass();
                    while t.next_batch() {
                        seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut sink, ladder);
                        // NO drain_ready: the fold stays parked — the
                        // unbounded-lead regime, deterministically.
                    }
                    sink.end_pass();
                }
                sink.finish();
                let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
                let mut got = Vec::new();
                let mut n = sink.harvest_completed(&mut harvested);
                while n > 0 {
                    for rec in &harvested[..n] {
                        got.push(*rec);
                    }
                    n = sink.harvest_completed(&mut harvested);
                }
                assert_eq!(got.len(), 12, "R27b {label} records (comp={comp})");
                for (k, rec) in got.iter().enumerate() {
                    assert_eq!(
                        *rec, want,
                        "R27b {label} pass {k} diverged (comp={comp}) — the ring-wrap shape"
                    );
                }
                if tor {
                    assert!(
                        sink.rx_steady_epoch >= 2,
                        "R27b toroidal epoch never engaged (comp={comp}, epoch={})",
                        sink.rx_steady_epoch
                    );
                }
            } // tor
        } // comp
    }

    /// R27c: the deep record ring — the 60-window unbounded-lead law.
    /// t_r27b's parked-fold shape at 12 windows stops right where the OLD
    /// 8-slot ring's watermark began binding (window 9); R27c's 64-deep
    /// ring must let the same parked shape run SIXTY windows with zero
    /// binds — the pre-R27c regime deterministically wraps the record
    /// ring at window 9 and folds wrong (the t_r27b fleet hash), so this
    /// cell's bit-exact 60 records IS the depth law's proof. The actual
    /// 64-wrap is STRUCTURALLY unreachable: the sink's own harvest ring
    /// (PASS_RING=64) forces mid-run harvesting (harness discipline) at
    /// 63 windows of lead, which forces record completion, which forces
    /// the fold — the watermark's reuse bound sits beyond every lead the
    /// protocol can express (the design's own safety: the record-ring
    /// watermark becomes dead insurance, and the reset wall it priced
    /// collapses to zero).
    #[test]
    fn t_r27c_deep_record_ring_wrap() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"R27CDEEP\0\0";
        let mut t0 = ReplayTransport::new(&gt, sched.clone(), sess);
        let want = seq_pass(&mut t0, sess);
        assert_eq!(want.0, 505_849, "coverage must hold");
        const PASSES: u64 = 60;
        for comp in [true, false] {
            let fabric = HydraFabric::spawn_pinned_full(1, &[], true, true, true, comp, true);
            let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                &gt,
                sched.clone(),
                sess,
                128,
            );
            if let Some(st) = fabric.rxdesc_state() {
                assert!(t.set_rxdesc(st), "set_rxdesc must attach once");
            }
            t.reset(sess);
            let mut seq = Sequencer::new();
            let mut sink = HydraSpanSink::new(&fabric);
            sink.rx_toroidal = true;
            sink.rx_assist_wm = 0; // the deterministic law-cell shape
            let ladder = crate::soa::ladder8_best();
            for _pass in 0..PASSES {
                t.reset(sess);
                *seq = Sequencer::new_unboxed();
                sink.begin_pass();
                while t.next_batch() {
                    seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut sink, ladder);
                    // NO drain_ready: the fold stays parked — sixty
                    // windows of unbounded lead, deterministically.
                }
                sink.end_pass();
            }
            sink.finish();
            let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
            let mut got = Vec::new();
            let mut n = sink.harvest_completed(&mut harvested);
            while n > 0 {
                for rec in &harvested[..n] {
                    got.push(*rec);
                }
                n = sink.harvest_completed(&mut harvested);
            }
            assert_eq!(got.len() as u64, PASSES, "R27c records (comp={comp})");
            for (k, rec) in got.iter().enumerate() {
                assert_eq!(
                    *rec, want,
                    "R27c pass {k} diverged (comp={comp}) — the deep-ring unbounded-lead shape"
                );
            }
            assert!(
                sink.rx_steady_epoch >= 2,
                "R27c toroidal epoch never engaged (comp={comp}, epoch={})",
                sink.rx_steady_epoch
            );
        }
    }

    /// R27c: the HOLE-FREE INLINE COMMIT — the epoch-survival law. The
    /// deterministic hole shape: a COLD window (pass 1) whose every chunk
    /// goes INLINE (force_inline) never array-writes its spans the
    /// pre-R27c way — the words stay zero, the warm copy propagates the
    /// zeros, and the next ARRAY-path window (pass 2) re-fixes every
    /// never-written word (rx_fixes grows) and disarms the toroidal
    /// epoch (the assist's 830 ms/pass reset wall, reproduced
    /// deterministically). With the hole-free commit the inline run
    /// STILL checks-and-fixes its words into the slot, so pass 2 finds
    /// them exact: the fix count NEVER grows past pass 1's cold fill and
    /// the epoch engages at pass 2's close. Pinned: (a) every record
    /// bit-exact, (b) fixes after pass 3 == fixes after pass 1, (c) the
    /// toroidal epoch armed.
    #[test]
    fn t_r27c_hole_free_inline_commit() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"R27CHOLE\0\0";
        let mut t0 = ReplayTransport::new(&gt, sched.clone(), sess);
        let want = seq_pass(&mut t0, sess);
        assert_eq!(want.0, 505_849, "coverage must hold");
        let fabric = HydraFabric::spawn_pinned_full(2, &[], true, true, true, true, true);
        let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
            &gt,
            sched.clone(),
            sess,
            128,
        );
        if let Some(st) = fabric.rxdesc_state() {
            assert!(t.set_rxdesc(st), "set_rxdesc must attach once");
        }
        t.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new(&fabric);
        sink.rx_assist_wm = 0; // deterministic: force_inline is the assist
        let ladder = crate::soa::ladder8_best();
        let mut fixes_hist = Vec::new();
        for pass in 0..3u64 {
            // Pass 0: the COLD window with EVERY chunk inline — the
            // hole-free law's fill shape. Passes 1-2: the normal array
            // path (the re-verification the holes used to poison).
            sink.force_inline = pass == 0;
            t.reset(sess);
            *seq = Sequencer::new_unboxed();
            sink.begin_pass();
            while t.next_batch() {
                seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut sink, ladder);
                sink.drain_ready();
            }
            sink.end_pass();
            fixes_hist.push(sink.rx_fixes);
        }
        sink.finish();
        let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
        let mut got = Vec::new();
        let mut n = sink.harvest_completed(&mut harvested);
        while n > 0 {
            for rec in &harvested[..n] {
                got.push(*rec);
            }
            n = sink.harvest_completed(&mut harvested);
        }
        assert_eq!(got.len(), 3, "R27c hole-free records");
        for (k, rec) in got.iter().enumerate() {
            assert_eq!(*rec, want, "R27c hole-free pass {k} diverged");
        }
        // (b) THE LAW: the fix count never grows past the cold fill —
        // passes 1-2 (the array path over the inline-filled slot) find
        // every word exact. Pre-R27c this grows by the full span count.
        assert!(
            fixes_hist[0] > 0,
            "R27c hole-free: the cold inline fill must have written words (fixes={})",
            fixes_hist[0]
        );
        assert_eq!(
            fixes_hist[1], fixes_hist[0],
            "R27c hole-free law violated: pass 1 re-fixed {} words the inline \
             commit should have written (cold fill {})",
            fixes_hist[1] - fixes_hist[0],
            fixes_hist[0]
        );
        assert_eq!(fixes_hist[2], fixes_hist[0], "R27c hole-free: pass 2 grew fixes");
        // (c) the epoch survived the inline-heavy window.
        assert!(
            sink.rx_steady_epoch >= 1,
            "R27c hole-free: the toroidal epoch never engaged (epoch={})",
            sink.rx_steady_epoch
        );
    }

    /// R29: the ZERO-THREAD INGEST parity — the self-serve window's
    /// law. The deep-steady epoch (two clean windows at the same count)
    /// proves the frozen slot's words and tags are the invariant tape,
    /// so the window's open pre-publishes the WHOLE span range
    /// (`spans_ready = base + count0`) and the steady batches' commit
    /// path (chunk-open assists, the vector check, the per-chunk
    /// publish cadence) is deleted — the scan walks the sequencer
    /// ladder only, the workers ingest directly from the frozen array,
    /// the fold consumes their streamed results. The law cell: ten
    /// passes of the deterministic sustained shape (drain per batch —
    /// the harness's exact cadence) over comp × self-serve cells, every
    /// record bit-exact vs the sequential reference; the engagement
    /// cell asserts the arm fired (windows 4+: the cold fill, the first
    /// clean verify, the toroidal verify, THEN self-serve); the
    /// rollback cell (HFT_SELF_SERVE=0's in-process form) asserts zero
    /// engagement and the same bit-exact records.
    #[test]
    fn t_r29_selfserve_parity() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"R29SELF\0\0\0";
        let mut t0 = ReplayTransport::new(&gt, sched.clone(), sess);
        let want = seq_pass(&mut t0, sess);
        assert_eq!(want.0, 505_849, "coverage must hold");
        const PASSES: u64 = 10;
        for comp in [true, false] {
            for ss in [true, false] {
                let fabric =
                    HydraFabric::spawn_pinned_full(2, &[], true, true, true, comp, true);
                let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                    &gt,
                    sched.clone(),
                    sess,
                    128,
                );
                if let Some(st) = fabric.rxdesc_state() {
                    assert!(t.set_rxdesc(st), "set_rxdesc must attach once");
                }
                t.reset(sess);
                let mut seq = Sequencer::new();
                let mut sink = HydraSpanSink::new(&fabric);
                sink.rx_selfserve = ss;
                let ladder = crate::soa::ladder8_best();
                for _pass in 0..PASSES {
                    t.reset(sess);
                    *seq = Sequencer::new_unboxed();
                    sink.begin_pass();
                    while t.next_batch() {
                        seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut sink, ladder);
                        sink.drain_ready();
                    }
                    sink.end_pass();
                }
                sink.finish();
                let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
                let mut got = Vec::new();
                let mut n = sink.harvest_completed(&mut harvested);
                while n > 0 {
                    for rec in &harvested[..n] {
                        got.push(*rec);
                    }
                    n = sink.harvest_completed(&mut harvested);
                }
                assert_eq!(
                    got.len() as u64,
                    PASSES,
                    "R29 records (comp={comp}, ss={ss})"
                );
                for (k, rec) in got.iter().enumerate() {
                    assert_eq!(
                        *rec, want,
                        "R29 pass {k} diverged (comp={comp}, ss={ss}) — the self-serve window's fold-order law"
                    );
                }
                if ss {
                    // The engagement law: pass 1 cold-fills (dirty, assists
                    // mark straddling chunks), pass 2 verifies clean (epoch
                    // 1), pass 3 verifies toroidal (epoch 2) — the early
                    // passes' straddle marks disarm one extra window —
                    // passes 5+ self-serve; 6 of 10 windows here.
                    assert!(
                        sink.selfserve_wins() >= PASSES - 4,
                        "R29 self-serve never engaged (comp={comp}, wins={})",
                        sink.selfserve_wins()
                    );
                    assert!(
                        sink.rx_steady_epoch >= 4,
                        "R29 epoch collapsed under self-serve (comp={comp}, epoch={})",
                        sink.rx_steady_epoch
                    );
                } else {
                    assert_eq!(
                        sink.selfserve_wins(),
                        0,
                        "R29 rollback cell armed self-serve (comp={comp})"
                    );
                }
            } // ss
        } // comp
    }

    /// R31-A: the WARM-EPOCH copy gate — the rotating regime's opens
    /// (toroidal disarmed, the exact pre-R27 window protocol) skip the
    /// ~252KB copy_arr/copy_tags pair once every RX_NARR slot has
    /// warm-filled and verified fix-free at the stable window count
    /// (the deterministic-schedule law); the check-and-fix remains the
    /// per-span backstop, so every record must stay bit-exact vs the
    /// sequential reference. The rollback cell (rx_warm_epoch=false,
    /// the in-process form of HFT_WARM_EPOCH=0) pays the copy on every
    /// open (skips == 0) and produces the SAME records.
    #[test]
    fn t_r31a_warm_epoch_copy_gate() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let sess = *b"R31WARM\0\0\0";
        let mut t0 = ReplayTransport::new(&gt, sched.clone(), sess);
        let want = seq_pass(&mut t0, sess);
        assert_eq!(want.0, 505_849, "coverage must hold");
        // 2 full slot rotations of warm-fill + enough steady opens to
        // prove the gate engaged on every slot.
        const PASSES: u64 = 24;
        for warm_epoch in [true, false] {
            let fabric = HydraFabric::spawn_pinned_full(1, &[], true, true, true, true, true);
            let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                &gt,
                sched.clone(),
                sess,
                128,
            );
            if let Some(st) = fabric.rxdesc_state() {
                assert!(t.set_rxdesc(st), "set_rxdesc must attach once");
            }
            t.reset(sess);
            let mut seq = Sequencer::new();
            let mut sink = HydraSpanSink::new(&fabric);
            sink.rx_toroidal = false; // the ROTATING regime — the copy's path
            sink.rx_assist_wm = 0; // deterministic (the law-cell discipline)
            sink.rx_warm_epoch = warm_epoch;
            let ladder = crate::soa::ladder8_best();
            let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
            let mut got = Vec::new();
            for _pass in 0..PASSES {
                t.reset(sess);
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
            assert_eq!(
                got.len() as u64,
                PASSES,
                "R31a warm-epoch records (warm_epoch={warm_epoch})"
            );
            for (k, rec) in got.iter().enumerate() {
                assert_eq!(
                    *rec, want,
                    "R31a warm-epoch pass {k} diverged (warm_epoch={warm_epoch}) — \
                     the copy-skip must stay check-and-fix backstopped"
                );
            }
            if warm_epoch {
                assert!(
                    sink.warm_skips() >= PASSES - 2 * nf_transport::rxdesc::RX_NARR as u64,
                    "R31a warm-epoch never engaged (skips={})", sink.warm_skips()
                );
            } else {
                assert_eq!(
                    sink.warm_skips(),
                    0,
                    "R31a rollback cell armed the copy gate"
                );
            }
        }
    }

    /// R28: the fused packed ladder-commit parity — the steady ladder's
    /// SoA packed emission (one (off, len, count) word per span at
    /// discovery, the FUSED vector transform+compare per 8 spans, zero
    /// SpanRec materialization) vs the SpanRec emission (the
    /// HFT_PACKED_EMISSION=0 rollback, the exact pre-R28 shape). The
    /// law cell (no prefill, no assists — every entry filled by the
    /// sink's own fix path, so the packed groups interleave with scalar
    /// tails and the fused check's mismatch mask routes real fixes)
    /// must reproduce the sequential reference bit-exact AND fix
    /// exactly the same spans as the SpanRec mode (the fused
    /// transform+compare proves exactly as much per span as the
    /// expect[]-build compare did). The full-stack cell (prefill +
    /// assists) pins the steady all-clean regime.
    #[test]
    fn t_r28_packed_ladder_commit_parity() {
        let gt = load_mini();
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
                *b"R28PACKCHA"
            } else {
                *b"R28PACKSTD"
            };
            let mut t0 = ReplayTransport::new(&gt, sched.clone(), sess);
            let want = seq_pass(&mut t0, sess);
            assert_eq!(want.0, 505_849, "coverage must hold (chaos={chaos})");

            for comp in [true, false] {
                // (a) the law cell: pipelined WITHOUT the rxdesc attach
                // (no prefill — every array entry written by the sink's
                // own fix path), assists disabled — deterministic. Each
                // mode gets a FRESH fabric (warm-start carryover would
                // make the fix counts incomparable).
                let mut results = Vec::new();
                for packed in [true, false] {
                    let fabric =
                        HydraFabric::spawn_pinned_full(2, &[], true, true, true, comp, true);
                    let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                        &gt,
                        sched.clone(),
                        sess,
                        128,
                    );
                    t.reset(sess);
                    let mut seq = Sequencer::new();
                    let mut sink = HydraSpanSink::new(&fabric);
                    sink.packed_emission = packed;
                    sink.rx_assist_wm = 0; // no assists — deterministic fixes
                    let ladder = crate::soa::ladder8_best();
                    while t.next_batch() {
                        seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut sink, ladder);
                        sink.drain_ready();
                    }
                    sink.finish();
                    results.push(((sink.count, sink.hash, sink.msg_hash), sink.rx_fixes()));
                }
                let (got_p, fixes_p) = results[0];
                let (got_s, fixes_s) = results[1];
                assert_eq!(
                    got_p, want,
                    "R28 law cell diverged (packed, chaos={chaos}, comp={comp})"
                );
                assert_eq!(
                    got_s, want,
                    "R28 law cell diverged (SpanRec, chaos={chaos}, comp={comp})"
                );
                assert_eq!(
                    fixes_p, fixes_s,
                    "R28 fix-count divergence: the fused transform+compare must fix exactly the \
                     SpanRec mode's spans (chaos={chaos}, comp={comp})"
                );

                // (b) the full-stack cell: pipelined + the RX PREFILL
                // attached + assists allowed — the sustained arm's exact
                // shape. Both modes bit-exact.
                for packed in [true, false] {
                    let fabric =
                        HydraFabric::spawn_pinned_full(2, &[], true, true, true, comp, true);
                    let mut t2 = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
                        &gt,
                        sched.clone(),
                        sess,
                        128,
                    );
                    if let Some(st) = fabric.rxdesc_state() {
                        assert!(t2.set_rxdesc(st), "set_rxdesc must attach once");
                    }
                    t2.reset(sess);
                    let (got2, _f2, _a2) = hydra_pass_packed_mode(&mut t2, &fabric, packed);
                    assert_eq!(
                        got2, want,
                        "R28 pipelined diverged (packed={packed}, chaos={chaos}, comp={comp})"
                    );
                }
            } // comp
        } // chaos
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
        // R25a: BOTH res-ring formats under the 400-pass straddle soak —
        // the composite default and the per-span rollback (the pass-start
        // record flags + segment splits get hammered here: 7903 spans per
        // pass, 7903 mod 64 = 47, so ~every pass boundary straddles a grid
        // chunk mid-chunk).
        for diet in [true, false] {
            let label = if diet { "diet" } else { "pre-diet" };
            for comp in [true, false] {
            let label = if comp { format!("{label}+comp") } else { format!("{label}+perspan") };
            let fabric = HydraFabric::spawn_pinned_full(1, &[], true, true, diet, comp, true);
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
            } // comp
        } // diet
    }

    // ─── R32: the pass-replay ledger pins (Run 625) ───────────────────

    /// The R32 sustained shape's session program (the 4-session rotation,
    /// the elided auto-advance). Construction = sessions[0].
    fn r32_prog(pass: u64) -> [u8; 10] {
        const S: [[u8; 10]; 4] = [
            *b"R32REPLA01",
            *b"R32REPLA02",
            *b"R32REPLA03",
            *b"R32REPLA04",
        ];
        if pass == 0 {
            S[0]
        } else {
            S[((pass - 1) % 4) as usize]
        }
    }

    /// Drive the R32 sustained shape (elided auto-advance + the hydra
    /// sink + the pass-replay ledger) for `passes` measured passes and
    /// return `(harvested tuples in order, the final sequencer state
    /// (watermark, counters, progress_vt), the ledger stats)`. The
    /// ledger is FORCED on/off by the caller (the env is process-global
    /// and racy across parallel test transports).
    fn r32_drive(
        gt: &[u8],
        sched: &nf_transport::sched_types::ReplaySchedule,
        replay_on: bool,
        passes: u64,
    ) -> (
        Vec<(u64, u64, u64)>,
        (u64, nf_arbitrator::Counters, u64),
        (u64, u64, u64, u64),
    ) {
        let construction = r32_prog(0);
        let fabric = HydraFabric::spawn_pinned_full(2, &[], true, true, true, true, true);
        let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
            gt,
            sched.clone(),
            construction,
            128,
            None,
            Some(r32_prog),
            false,
            true,
            16,
            true,
            true, // R33-A: the schedule replay ON — the R32 ledger's (rx_start,len)
                  // identity guard is the replay's oracle (the champion shape).
            true, // R35-A: prearm mirrors the sched-replay arm (the champion shape)
        );
        if let Some(st) = fabric.rxdesc_state() {
            assert!(t.set_rxdesc(st), "set_rxdesc must attach once");
        }
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new(&fabric);
        let ladder = crate::soa::ladder8_best();
        let mut replay = crate::passreplay::PassReplay::new_forced(replay_on);
        let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
        let mut got = Vec::new();
        // The ref pass (pass 1) runs live with a separate sink — the
        // sustained bench's shape (also the generation boundary).
        t.reset_pass(1, r32_prog(1));
        {
            let mut rseq = Sequencer::new_unboxed();
            let mut ref_sink = HydraSpanSink::new(&fabric);
            while t.next_batch() {
                rseq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut ref_sink, ladder);
                ref_sink.drain_ready();
            }
            ref_sink.finish();
        }
        let mut final_state = (0u64, nf_arbitrator::Counters::default(), 0u64);
        for pass in 2..=passes + 1 {
            t.reset_pass(pass, r32_prog(pass));
            *seq = Sequencer::new_unboxed();
            sink.begin_pass();
            replay.begin_pass();
            while t.next_batch() {
                if !replay.try_replay(&t, &mut seq, &mut sink) {
                    replay.begin_record(&t, &seq);
                    {
                        let mut rec_sink = replay.recorder(&mut sink);
                        seq.ingest_entries_ladder(
                            t.entries(),
                            t.now_ns(),
                            &mut rec_sink,
                            ladder,
                        );
                    }
                    replay.seal_batch(&seq);
                }
                sink.drain_ready();
            }
            replay.note_pass_end();
            final_state = (seq.watermark(), seq.counters(), seq.progress_vt());
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
        (got, final_state, replay.stats())
    }

    /// R32: the pass-replay ledger's BIT-EXACTNESS law — the elided
    /// frozen-master regime replays the recorded pass surface (every
    /// sink call, the counters, the watermark, the progress clock) and
    /// the harvested tuples + the final sequencer state are IDENTICAL to
    /// the un-replayed live scan. The engagement law: exactly ONE
    /// live-recorded pass (the first measured pass) — every later pass
    /// replays (replay_passes = passes - 1, re_records = 0).
    #[test]
    fn t_r32_pass_replay_parity_vs_live() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        // Per-session sequential references (the 4-session rotation).
        let mut want = Vec::new();
        for k in 0..4u64 {
            let mut t = ReplayTransport::new(&gt, sched.clone(), r32_prog(k));
            want.push(seq_pass(&mut t, r32_prog(k)));
        }
        const PASSES: u64 = 12;
        let (got_live, state_live, stats_live) = r32_drive(&gt, &sched, false, PASSES);
        let (got_rp, state_rp, stats_rp) = r32_drive(&gt, &sched, true, PASSES);
        // The tuple law: both arms produce the identical in-order pass
        // sequence, each equal to its session's sequential reference.
        let expect: Vec<(u64, u64, u64)> = (2..=PASSES + 1)
            .map(|p| want[((p - 1) % 4) as usize])
            .collect();
        assert_eq!(got_live, expect, "R32 live arm diverged");
        assert_eq!(got_rp, expect, "R32 replay arm diverged — the verdict replay must stay bit-exact");
        // The sequencer-state law: the replay lands the sequencer in the
        // exact recorded post-batch state (watermark, counters, clock).
        assert_eq!(
            state_live, state_rp,
            "R32 sequencer state diverged — the recorded counters/watermark/clock law"
        );
        // The engagement law: one live-recorded pass, the rest replayed.
        assert_eq!(stats_live, (0, 0, 0, 0), "R32 disabled arm must be a no-op");
        let (rp_batches, rp_live, rp_passes, rp_rerecords) = stats_rp;
        assert!(rp_live > 0, "R32 never live-recorded a template");
        assert!(
            rp_batches > rp_live * 4,
            "R32 replay never dominated (batches {rp_batches}, live {rp_live})"
        );
        assert_eq!(
            rp_passes,
            PASSES - 1,
            "R32 engagement: exactly the first measured pass runs live (passes {rp_passes})"
        );
        assert_eq!(rp_rerecords, 0, "R32 re-records in the steady regime (rerecords {rp_rerecords})");
    }

    /// R32: the MASTER WRITE BARRIER's reaction — a foreign version bump
    /// (any master write's signature) must truncate the template and
    /// force the next pass back to the LIVE scan (re-record under the new
    /// version), after which the replay re-engages; the tuples stay
    /// bit-exact throughout (the barrier is the anti-stale-replay proof,
    /// the re-record is the recovery).
    #[test]
    fn t_r32_pass_replay_barrier_injection() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let mut want = Vec::new();
        for k in 0..4u64 {
            let mut t = ReplayTransport::new(&gt, sched.clone(), r32_prog(k));
            want.push(seq_pass(&mut t, r32_prog(k)));
        }
        const PASSES: u64 = 10;
        const BUMP_AFTER: u64 = 5; // passes 2..=6 normal, bump, 7..=11 after
        let construction = r32_prog(0);
        let fabric = HydraFabric::spawn_pinned_full(2, &[], true, true, true, true, true);
        let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
            &gt,
            sched.clone(),
            construction,
            128,
            None,
            Some(r32_prog),
            false,
            true,
            16,
            true,
            true, // R33-A: the schedule replay ON — the R32 ledger's (rx_start,len)
                  // identity guard is the replay's oracle (the champion shape).
            true, // R35-A: prearm mirrors the sched-replay arm (the champion shape)
        );
        if let Some(st) = fabric.rxdesc_state() {
            assert!(t.set_rxdesc(st), "set_rxdesc must attach once");
        }
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new(&fabric);
        let ladder = crate::soa::ladder8_best();
        let mut replay = crate::passreplay::PassReplay::new_forced(true);
        let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
        let mut got = Vec::new();
        // The ref pass (live, separate sink).
        t.reset_pass(1, r32_prog(1));
        {
            let mut rseq = Sequencer::new_unboxed();
            let mut ref_sink = HydraSpanSink::new(&fabric);
            while t.next_batch() {
                rseq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut ref_sink, ladder);
                ref_sink.drain_ready();
            }
            ref_sink.finish();
        }
        for pass in 2..=PASSES + 1 {
            // The foreign-master-write injection: bump AFTER a clean pass
            // boundary (the consumer is parked in reset_pass's wait — the
            // same visibility point a real RX-side bake would have).
            if pass == BUMP_AFTER + 2 {
                t.r32_debug_bump_master_version();
            }
            t.reset_pass(pass, r32_prog(pass));
            *seq = Sequencer::new_unboxed();
            sink.begin_pass();
            replay.begin_pass();
            while t.next_batch() {
                if !replay.try_replay(&t, &mut seq, &mut sink) {
                    replay.begin_record(&t, &seq);
                    {
                        let mut rec_sink = replay.recorder(&mut sink);
                        seq.ingest_entries_ladder(
                            t.entries(),
                            t.now_ns(),
                            &mut rec_sink,
                            ladder,
                        );
                    }
                    replay.seal_batch(&seq);
                }
                sink.drain_ready();
            }
            replay.note_pass_end();
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
        let expect: Vec<(u64, u64, u64)> = (2..=PASSES + 1)
            .map(|p| want[((p - 1) % 4) as usize])
            .collect();
        assert_eq!(got, expect, "R32 barrier arm diverged — a version bump must re-record, never replay stale");
        let (rp_batches, rp_live, rp_passes, rp_rerecords) = replay.stats();
        assert_eq!(
            rp_rerecords, 1,
            "the injected bump must fire exactly one re-record (rerecords {rp_rerecords})"
        );
        // Two live passes total (the first record + the post-bump
        // re-record); the rest replayed.
        assert_eq!(
            rp_passes,
            PASSES - 2,
            "engagement after the barrier recovery (passes {rp_passes}, batches {rp_batches}, live {rp_live})"
        );
    }

    use crate::sigfabric;

    /// R35-B: THE 𝒜₅.₀ FABRIC INTEGRATION CELL — the sustained
    /// multi-pass shape (the champion regime: the diet worker + the
    /// composite fold + the frozen-master transport) with the signature
    /// fabric wired: Worker 0 bakes the per-locate chunk-sig table from
    /// the RX ledger (the untimed Phase-R decode + the R34 kernel
    /// folds), and every pass's window boundary runs the global Σ
    /// replay with the fail-stop assert. THE LAWS:
    /// * the count law: the baked token count == the hydra span-fold's
    ///   per-pass message count (the same stream, both surfaces);
    /// * the engagement law: sig_replays == the pass count (every
    ///   window replayed; the digest assert never fires);
    /// * the parity law: every pass's tuple == the sequential reference
    ///   (the R32/R33 gates carry the whole load — the 𝒜₅.₀ fabric
    ///   changes nothing observable);
    /// * the audit law: the full per-locate re-derivation (the Σ/Ψ
    ///   walk) == the frozen state digest (the dual-path cell's law at
    ///   the fabric level).
    #[test]
    fn t_r35b_fabric_integration_sustained() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        fn prog(pass: u64) -> [u8; 10] {
            let mut s = *b"RXDA5FAB01";
            if pass >= 1 {
                s[8..10].copy_from_slice(&pass.to_be_bytes()[6..8]);
            }
            s
        }
        // The sequential reference (the oracle tuple).
        let mut t_ref = ReplayTransport::new(&gt, sched.clone(), prog(0));
        let want = seq_pass(&mut t_ref, prog(1));

        let fabric = HydraFabric::spawn_pinned_full(1, &[], true, true, true, true, true);
        let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce_cpu_auto(
            &gt,
            sched.clone(),
            prog(0),
            128,
            None,
            Some(prog),
        );
        if let Some(st) = fabric.rxdesc_state() {
            assert!(t.set_rxdesc(st), "set_rxdesc must attach once");
        }
        // The ref pass (pass 1): the live walk + the BAKE (the worker's
        // first window opens here — the ledger is already filled by the
        // RX's thread-start probe).
        t.reset_pass(1, prog(1));
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
                want,
                "the ref tuple (the fabric changes nothing observable)"
            );
        }
        // The sustained passes: each pass replays the signature fabric's
        // global Σ at its window boundary (the fail-stop assert), and
        // the per-pass tuple must stay the reference.
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new(&fabric);
        let ladder = crate::soa::ladder8_best();
        let mut harvested = [(0u64, u64::MAX, u64::MAX); PASS_RING];
        let mut tuples = 0usize;
        for pass in 2..=12u64 {
            t.reset_pass(pass, prog(pass));
            *seq = Sequencer::new_unboxed();
            sink.begin_pass();
            while t.next_batch() {
                seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut sink, ladder);
                sink.drain_ready();
            }
            sink.end_pass();
            let n = sink.harvest_completed(&mut harvested);
            for rec in &harvested[..n] {
                assert_eq!((rec.0, rec.1, rec.2), want, "pass {pass} diverged");
                tuples += 1;
            }
        }
        sink.finish();
        let n = sink.harvest_completed(&mut harvested);
        for rec in &harvested[..n] {
            assert_eq!((rec.0, rec.1, rec.2), want, "the tail diverged");
            tuples += 1;
        }
        assert!(tuples >= 10, "the harvest must cover the passes ({tuples})");
        // THE FABRIC LAWS.
        let sf = fabric
            .sig_fabric
            .get()
            .expect("the bake must have landed (the ref pass's window)");
        assert_eq!(sf.n_msgs, want.0, "the count law: the token stream == the hydra count");
        let replays: u64 = fabric
            .wstats
            .iter()
            .map(|w| w.sig_replays.load(Ordering::Relaxed))
            .sum();
        // The engagement law: the worker's WALK fires the replay at
        // every pass boundary it crosses — the walk can trail the last
        // passes' submissions when the sink finishes (the harvest
        // completes via the finish() drain; the tail is racy by a
        // couple of windows — the R33 engagement law's shape).
        assert!(
            (8..=12).contains(&replays),
            "the engagement law: sig_replays={replays}"
        );
        // THE AUDIT LAW: the full per-locate re-derivation == the frozen
        // digest (the dual-path cell's law, at the baked table).
        let (digest, _) = sigfabric::replay_sig_fabric(sf);
        assert_eq!(digest, sf.digest, "the audit re-derivation");
        // The global walk (the per-pass surface) == the frozen Σ.
        assert_eq!(sigfabric::replay_global(sf), sf.global_sig);
        // L: Theorem 7's tracked invariant (the corpus constant).
        assert_eq!(sf.max_lifecycle, 63, "the pinned corpus L (docs/46)");
    }


    // ─── R36-E: the res-record replay cells (Run 800, docs/48) ─────────

    /// Drive the R36-E sustained shape (the champion stack: the R32
    /// ledger + the call fusion + the worker eval elision, all defaults)
    /// for `passes` measured passes on ONE worker. One worker keeps the
    /// phase grid at 64 — the mini tape's boundary orbit is the FULL 64
    /// (7,903 spans, 7,903 mod 64 = 47, gcd(47,64)=1), the phase law's
    /// adversarial case: every pass boundary lands at a different grid
    /// offset, so the arming waits a complete orbit and the logs fill one
    /// phase per pass. Returns the in-order harvested tuples plus the
    /// elision's engagement instruments.
    fn r36e_drive(
        gt: &[u8],
        sched: &nf_transport::sched_types::ReplaySchedule,
        passes: u64,
    ) -> (
        Vec<(u64, u64, u64)>,
        (u64, u64, u64, u64),
        u64,
    ) {
        let construction = r32_prog(0);
        // R36-E: the elision is FORCED through the spawn override (the
        // env races the parallel tests' worker spawns).
        let fabric =
            HydraFabric::spawn_pinned_full_opts(1, &[], true, true, true, true, true, Some(true));
        let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
            gt,
            sched.clone(),
            construction,
            128,
            None,
            Some(r32_prog),
            false,
            true,
            16,
            true,
            true,
            true,
        );
        if let Some(st) = fabric.rxdesc_state() {
            assert!(t.set_rxdesc(st), "set_rxdesc must attach once");
        }
        let mut seq = Sequencer::new();
        let mut sink = HydraSpanSink::new(&fabric);
        let ladder = crate::soa::ladder8_best();
        let mut replay = crate::passreplay::PassReplay::new_forced(true);
        let mut harvested = [(0u64, 0u64, 0u64); PASS_RING];
        let mut got = Vec::new();
        // The ref pass (pass 1) runs live with a separate sink — the
        // sustained bench's shape (also the generation boundary).
        t.reset_pass(1, r32_prog(1));
        {
            let mut rseq = Sequencer::new_unboxed();
            let mut ref_sink = HydraSpanSink::new(&fabric);
            while t.next_batch() {
                rseq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut ref_sink, ladder);
                ref_sink.drain_ready();
            }
            ref_sink.finish();
        }
        for pass in 2..=passes + 1 {
            t.reset_pass(pass, r32_prog(pass));
            *seq = Sequencer::new_unboxed();
            sink.begin_pass();
            replay.begin_pass();
            while t.next_batch() {
                if !replay.try_replay(&t, &mut seq, &mut sink) {
                    replay.begin_record(&t, &seq);
                    {
                        let mut rec_sink = replay.recorder(&mut sink);
                        seq.ingest_entries_ladder(
                            t.entries(),
                            t.now_ns(),
                            &mut rec_sink,
                            ladder,
                        );
                    }
                    replay.seal_batch(&seq);
                }
                sink.drain_ready();
            }
            replay.note_pass_end();
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
        (
            got,
            fabric.res_replay_totals(),
            replay.fused_calls(),
        )
    }

    /// R36-E: THE ELLISION'S BIT-EXACTNESS + ENGAGEMENT LAW — under the
    /// full-64 phase orbit (the adversarial rotation), the recorded
    /// phase scripts arm after one orbit and every later pass REPLAYS:
    /// the harvested tuples are identical to the per-session sequential
    /// references (the fold consumes the replayed composite records —
    /// the tuple assert's law at the cell scale), the boundary count
    /// matches the pass count (every boundary stepped the phase machine),
    /// the live passes are exactly the recording orbit, and the fusion
    /// absorbed the emitter's per-32-span flushes.
    #[test]
    fn t_r36e_replay_parity_and_engagement() {
        let gt = load_mini();
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::MtuBound(1400),
            guarantee_coverage: true,
            ..Default::default()
        };
        let sched = build_schedule(&gt, &cfg);
        let mut want = Vec::new();
        for k in 0..4u64 {
            let mut t = ReplayTransport::new(&gt, sched.clone(), r32_prog(k));
            want.push(seq_pass(&mut t, r32_prog(k)));
        }
        // The mini's orbit is 64 (one worker's grid): 72 measured passes
        // = the recording orbit (64) + 8 replayed passes.
        const PASSES: u64 = 72;
        let (got, (replayed, live, fb, recs), fused) = r36e_drive(&gt, &sched, PASSES);
        let expect: Vec<(u64, u64, u64)> = (2..=PASSES + 1)
            .map(|p| want[((p - 1) % 4) as usize])
            .collect();
        assert_eq!(
            got, expect,
            "R36-E: the replayed tuples diverged from the sequential references"
        );
        // THE ENGAGEMENT LAWS: the worker crossed every boundary (the
        // ref pass + the measured passes + the gen re-anchor), the live
        // passes are the recording orbit, and the replays dominate.
        let crossed = replayed + live;
        assert!(
            crossed >= PASSES && crossed <= PASSES + 4,
            "R36-E boundary count: crossed {crossed}, passes {PASSES} \
             (replayed {replayed}, live {live})"
        );
        assert!(
            replayed >= 4,
            "R36-E engagement: only {replayed} replayed passes (the orbit 64 needs 72 passes)"
        );
        assert_eq!(fb, 0, "R36-E fallbacks on the steady shape (fb {fb})");
        assert!(recs > 0, "R36-E no records emitted");
        assert!(
            fused > 0,
            "R36-E the call fusion never engaged (fused {fused})"
        );
    }

    /// R36-E: THE PHASE MACHINE'S LAWS, unit-level — the orbit fill (one
    /// phase slot per distinct boundary offset), the arming exactly when
    /// a phase repeats at a length-aligned offset, the LENGTH LAW's trip
    /// (a changed pass length resets the whole log and re-records), the
    /// ALIGNMENT LAW's trip (a rotated offset resets), the hole guard's
    /// discard (an inline-skipped chunk invalidates the phase), and the
    /// poison's permanence. The pure state machine — no threads, exact
    /// arithmetic, the adversarial rotations.
    #[test]
    fn t_r36e_phase_machine_laws() {
        const T: u64 = 7_903; // the mini tape's span count (T mod 64 = 47)
        let mut log = ResLog::new(true, false, 1);
        // Pass 1: the base-0 boundary opens the phase-0 recording.
        log.on_boundary(0);
        assert!(log.recording, "the first boundary must open the recording");
        log.push_seg(0, 64, 0x1111);
        log.push_seg(64, 64, 0x2222);
        // Pass 2: the phase-0 log SEALS (2 segments) + the length is
        // learned + the phase-47 recording opens immediately (the
        // machine always leaves the walk in the recording state for a
        // fresh phase).
        log.on_boundary(T);
        assert!(log.recording, "the fresh phase re-opens the recording");
        assert_eq!(log.pass_len, T, "the length law learns the pass length");
        assert_eq!(log.phase, (T % 64) as usize);
        assert_eq!(log.phase_n[0], 2, "the phase-0 log holds 2 segments");
        log.push_seg(0, 32, 0x3333);
        // The orbit: drive boundaries 3..=64 (each a fresh phase — the
        // mini's orbit is the full 64) — no arming until a repeat.
        for k in 2..64u64 {
            log.on_boundary(k * T);
            assert!(
                !log.replaying,
                "no arming before the phase repeats (pass {})",
                k + 1
            );
            assert!(log.recording, "each fresh phase records (pass {})", k + 1);
            log.push_seg(0, 48, 0x4444);
        }
        // Pass 65: the phase 0 repeats at a length-aligned offset (64*T)
        // — ARMED. The script is the pass-1 recording (2 segments).
        log.on_boundary(64 * T);
        assert!(log.replaying, "the phase-0 repeat must arm the replay");
        assert_eq!(log.rp_next, 0);
        assert_eq!(log.rp_end, 2);
        assert_eq!(log.replayed_passes, 1);
        // THE LENGTH LAW'S TRIP: the next boundary advances a DIFFERENT
        // delta — the whole log resets and the phase re-records.
        log.on_boundary(64 * T + T + 13);
        assert!(!log.replaying, "the length trip must disarm");
        assert!(log.recording, "the length trip must re-record");
        assert_eq!(
            log.segs.len(), 0,
            "the reset truncated the whole store (the fresh bracket is empty)"
        );
        assert_eq!(log.pass_len, 0, "the length is unlearned after the reset");
        // A grid-aligned length regime (T2 = 8,192 = 64*128: the orbit is
        // exactly 1 — every boundary repeats the phase) — the arming law
        // and the alignment trip on a fresh machine.
        const T2: u64 = 8_192;
        let mut log2 = ResLog::new(true, false, 1);
        log2.on_boundary(0);
        log2.push_seg(0, 64, 0xAAAA);
        log2.on_boundary(T2);
        assert_eq!(log2.pass_len, T2);
        assert_eq!(log2.phase, 0, "T2 mod 64 == 0: the same phase");
        log2.push_seg(0, 64, 0xBBBB);
        log2.on_boundary(2 * T2);
        assert!(
            log2.replaying,
            "the grid-aligned length arms on the first repeat"
        );
        assert_eq!(log2.rp_end, 1, "the phase-0 script is the one segment");
        // THE ALIGNMENT LAW'S TRIP: a rotated offset (the phase matches,
        // the offset is not a multiple of the length) resets.
        log2.on_boundary(2 * T2 + T2 / 2);
        assert!(!log2.replaying, "the rotated offset must disarm");
        assert!(log2.recording, "the rotated offset must re-record");
        // THE HOLE GUARD: an inline-skipped chunk mid-recording discards
        // the phase's partial log (the walk continues live).
        let mut log3 = ResLog::new(true, false, 1);
        log3.on_boundary(0);
        log3.push_seg(0, 64, 0xCCCC);
        log3.discard_phase();
        assert!(!log3.recording, "the discard closes the recording");
        assert_eq!(log3.segs.len(), 0, "the discard truncates the segments");
        assert_eq!(log3.phase_n[0], 0);
        log3.on_boundary(T);
        // (the phase T%64 != 0 — a different phase; the phase-0 stays
        // empty, so the NEXT phase-0 boundary re-records.)
        log3.on_boundary(64 * T);
        assert!(
            log3.recording,
            "the discarded phase re-records on its next boundary"
        );
        // THE AUDIT TWIN: forced live, never armed.
        let mut log4 = ResLog::new(true, true, 1);
        log4.on_boundary(0);
        log4.push_seg(0, 64, 0xDDDD);
        for k in 1..130u64 {
            log4.on_boundary(k * T2);
            assert!(!log4.replaying, "the audit mode never arms");
        }
        // THE DISABLED ARM: the same.
        let mut log5 = ResLog::new(false, false, 1);
        log5.on_boundary(0);
        for k in 1..130u64 {
            log5.on_boundary(k * T2);
            assert!(!log5.replaying, "the disabled arm never arms");
        }
    }

    /// R36-E: THE SPAN-ID WRAP — the fold's order and frontier-cap
    /// compares live in the u32 span-id domain; past 2^32 spans the
    /// truncated ids must still tile exactly (the masked order assert +
    /// the RFC-1982 serial cap). Hand-driven at the drain level: seed
    /// the fold just below the wrap, emit one record crossing it and
    /// one after it, and fold — no panic, the cursor lands past the
    /// wrap, and the chain consumed both records.
    #[test]
    fn t_r36w_span_id_wrap() {
        let fabric = HydraFabric::spawn_pinned_full(1, &[], true, true, true, true, true);
        let lane = &fabric.lanes[0];
        // Seed: the fold sits 8 spans below the wrap; the submission
        // frontier sits 64 past it (the cap's serial window).
        let base: u64 = u32::MAX as u64 - 7;
        {
            let mut sink = HydraSpanSink::new(&fabric);
            sink.fold_pos = base;
            sink.next_span = base + 8 + 64;
            // Two composite records: [base, base+8) crossing the wrap and
            // [0, 64) after it (the lane's chunk grid: 64-spans).
            let slots = lane.res_slots();
            slots[0] = Res {
                span_id: base as u32,
                aux: 8,
                value: 0xAAAA_0000_1111_2222,
            };
            slots[1] = Res {
                span_id: 0,
                aux: 64,
                value: 0xBBBB_3333_4444_5555,
            };
            lane.res_head.store(2, Ordering::Release);
            sink.drain_ready();
            assert_eq!(
                sink.fold_pos,
                base + 8 + 64,
                "R36-E wrap: the fold did not cross 2^32 (fold_pos {})",
                sink.fold_pos
            );
            assert_eq!(
                lane.res_tail.load(Ordering::Relaxed),
                2,
                "R36-E wrap: the records were not consumed"
            );
        }
    }

}
