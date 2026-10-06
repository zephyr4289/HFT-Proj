//! R8: RX-pipelined replay transport — poll on a dedicated core (docs/22).
//!
//! # Why
//!
//! The single-threaded ingest pipeline interleaves two unrelated jobs on one
//! core: transport staging (directory walk, clock pacing, session-baked
//! frame slicing, slot publication — ~12-15 cycles/frame) and arbitration
//! (the sequencer's steady ladder — ~10-12 cycles/frame). They share no
//! state except the batch handoff, and the 2B/s pure-ingest target needs
//! both halves' work overlapped: on the 4-vCPU runners (~2.3-2.6 GHz) the
//! serialized sum caps below 2B msg/s no matter how each half is tuned.
//!
//! # What
//!
//! [`PipelinedReplayTransport`] moves the ENTIRE transport driver onto a
//! dedicated RX thread that polls ahead into a double-buffered batch
//! mailbox; the consumer thread (the sequencer's owner — the single-writer
//! law is intact) drains batches through the same `FrameEntry` surface the
//! single-threaded harness uses. This is the feed-handler shape real
//! deployments run (RX/processing split), executed as an SPSC
//! ownership-transfer protocol identical in spirit to the HYDRA lanes:
//!
//! * the RX thread owns the [`ReplayTransport`] (directory, pacing, blob)
//!   and writes batch slots into mailbox buffer `i` for turn `T`, then
//!   publishes with ONE Release store;
//! * the consumer Acquires, scans the batch, and frees the buffer with ONE
//!   Release store (buffer `i` serves turns `i, i+NBUF, i+2*NBUF, ...`);
//! * the slot's raw frame pointers point into the RX thread's blob — valid
//!   for as long as the pipeline lives (the RX thread is joined in `Drop`)
//!   and never dereferenced after the consumer frees the buffer.
//!
//! # Reset handshake
//!
//! `reset(session)` rewrites the blob's session prefixes — mutating bytes
//! the consumer may otherwise dereference. The protocol:
//! 1. the consumer parks (it holds no entries: `ingest_batch` fully
//!    consumes every batch) and stores the new session + a monotonically
//!    increasing reset count, then sets the RESET command (Release);
//! 2. the RX thread finishes publishing its in-flight poll, applies
//!    `ReplayTransport::reset` (baking the new session), then stores its
//!    current turn and acks the reset count (Release);
//! 3. the consumer (Acquire on the count, then the turn) aligns its turn
//!    counter to the RX's and frees every buffer the RX filled but the
//!    consumer skipped (pre-reset frames) — keeping the turn arithmetic
//!    deadlock-free across the boundary.
//!
//! No locks anywhere near the hot path; all spins use the HYDRA-style
//! capped exponential backoff.
//!
//! # Semantics
//!
//! Identical frames, identical order, identical pacing decisions, identical
//! `now_ns` values per batch (the mailbox carries the driver's virtual
//! clock with each buffer) — the same transport, observed from two threads.
//! The parity tests in `nf-testkit::batch_parity` pin the pipelined
//! consumer's observable stream (counters, watermark, count, hash) to the
//! single-threaded transport's, including multi-pass reset cycles.

use crate::render::ReplayTransport;
use crate::sched_types::ReplaySchedule;
use crate::{FrameBatch, Transport};
use nf_protocol::packet::FrameEntry;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};

/// R8 phase-3: mailbox depth. The 4-buffer mailbox kept the RX pinned to
/// the consumer's elbow — every buffer free had to be noticed and repaid
/// with a fresh publication before the consumer's next next_batch, and the
/// wake-latency chain (yield-storm → park → schedule → wake) cost ~42µs
/// per batch on the 2-core runners (66% of the sustained arm's wall). A
/// 16-deep mailbox gives the RX a full pass of runahead: mid-pass the
/// consumer's next_batch is a cache-hot ring hit, the RX parks in timed
/// futex slots instead of churning the runqueue, and the only serialized
/// point left is the per-pass reset handshake. Power of two (mask + shift
/// protocols below); a pass is ~12.3 publications, so 16 buffers ≈ 1.3
/// passes of slack.
///
/// F-4 (CHECKLIST F-4 / ROADMAP2 §6.3): the depth is now RUNTIME —
/// `HFT_NBUF=32` doubles the runahead (at 5B the publication cadence
/// rises ~4.3x over the sustained shape and the 16-deep ring prices
/// buffer-reuse stalls); 16 stays the default (the R8 phase-3 measured
/// shape). The arrays are sized at the maximum and the protocol values
/// (mask/shift/ring) are precomputed ONCE at construction — the hot
/// paths read register-cached locals, never the env. The I-7 turn-event
/// ring is sized at 2×NBUF so the overwrite-guard window keeps its
/// 2x-max-lag headroom at every depth (at 16: 32 slots — the R9 shape
/// verbatim; at 32: 64).
const NBUF_DEFAULT: u64 = 16;
const NBUF_MAX: u64 = 32;
/// The static array bound for the Mailbox's per-buffer fields (bufs /
/// filled / freed) and the RX's turn-event ring (2×NBUF_MAX).
const NBUF_SLOTS: usize = NBUF_MAX as usize;
const TOFF_SLOTS: usize = (2 * NBUF_MAX) as usize;
/// Frames per publication (and the EntryBuf slot count — they are ONE
/// constant: the accumulate loop writes entries[acc..acc+n) with acc
/// bounded by this cap). 1024 measured best on the runner pool: the
/// 2048 experiment (a193be5) drew identical ~516M caps on two different
/// machines — a net 25% regression against the 1024-shape's 664-675M.
const ENTRY_CAP: usize = 1024;

/// RX timed-park quantum for the buffer-free wait (see futex_wait_timeout).
/// 50us measured best on the pool (the 15us experiment regressed with the
/// 2048-publication change it shipped with; restored to the measured shape).
const BUF_PARK_NS: u64 = 50_000;

// R8: construction-time shared handles (mailbox + triple store) — the hot
// path dereferences plain references; the Arcs exist only to cross the
// thread spawn boundary. Tier-F allow mirrors render.rs's construction
// allows.
#[allow(clippy::disallowed_types)]
use std::sync::Arc;
use std::thread::JoinHandle;

const CMD_RUN: u8 = 0;
const CMD_RESET: u8 = 1;
const CMD_SHUTDOWN: u8 = 2;

/// One RX-built batch: a ready-to-scan FrameEntry array + the driver's
/// virtual clock. The RX thread builds the entries from ITS locally-hot
/// frame lines and slot data (the consumer never constructs entries and
/// never touches the cross-core frame lines on the steady path), then
/// publishes the whole buffer with one Release store.
///
/// R12b: the steady scan's vectorized watermark ladder gathers its facts
/// CONSUMER-side from this AoS array (the entries are L1-hot in the scan
/// anyway) — the RX's per-frame cost is EXACTLY the R11 shape. The first
/// 8370C draw refuted the RX-published SoA sidecar design: the RX is the
/// co-bottleneck on both Intel classes (86% busy sustained), and the
/// sidecar's extra stores/uops per frame collapsed Front A 41% (2.574B ->
/// 1.509B) — cycles were being traded on the wrong core.
///
/// SAFETY-of-lifetime: the entries' slices point into the RX transport's
/// blob/triples (which outlive the pipeline — the RX thread is joined in
/// `Drop`) and are never dereferenced after the consumer frees the buffer
/// (the harness consumes each batch fully before the next `next_batch`).
struct EntryBuf {
    entries: Box<[FrameEntry<'static>; ENTRY_CAP]>,
    len: u32,
    clock: u64,
    /// F-2 (HFT_RXBUILD): the publication's master slice start (the
    /// pass-local frame index at the publication's first frame; `len`
    /// frames follow). Read by the consumer only in rxbuild mode — the
    /// per-turn entry copy is REPLACED by this 4-byte reference, which is
    /// the lever's whole point (the mailbox traffic collapses from
    /// ~64KB/batch to ~16B/batch).
    rx_start: u32,
}

impl EntryBuf {
    fn new() -> Self {
        Self {
            // Construction-time init (once per buffer): valid empty
            // entries; every published slot is overwritten by the RX
            // before the Release publication, and the consumer reads only
            // [..len] entries published for the current turn.
            entries: Box::new(std::array::from_fn(|_| FrameEntry {
                bytes: &[],
                feed: 0,
                blocks: &[],
                memo: None,
                first_seq: 0,
                sess_lo: 0,
                sess_hi: 0,
                elig: 0,
            })),
            len: 0,
            clock: 0,
            rx_start: 0,
        }
    }
}

/// Cache-line-padded cursor (no false sharing between RX and consumer).
#[repr(align(64))]
struct Pad(AtomicU64);

impl Pad {
    fn zeroed() -> Self {
        Self(AtomicU64::new(0))
    }
    fn never() -> Self {
        Self(AtomicU64::new(u64::MAX))
    }
}

impl std::ops::Deref for Pad {
    type Target = AtomicU64;
    #[inline(always)]
    fn deref(&self) -> &AtomicU64 {
        &self.0
    }
}

/// R8 phase-2 diagnostics: always-on pipeline telemetry. Two padded
/// groups — RX-side written only by the RX thread, consumer-side only by
/// the consumer — so the counters never false-share with each other or
/// with the hot ring cursors. Relaxed ordering: monotonic counters, read
/// once per run by the harness.
#[repr(align(64))]
struct RxStats {
    /// Publications (data + EOS markers) completed.
    publications: AtomicU64,
    /// inner.poll() invocations.
    polls: AtomicU64,
    /// Nanoseconds spent producing publications (the accumulate loop,
    /// including its polls — the RX thread's real work).
    prod_ns: AtomicU64,
    /// Buffer-free wait laps (RX blocked because the consumer holds all 4
    /// buffers — deep-runahead starvation visibility).
    bufwait_laps: AtomicU64,
    /// Resets served (handshakes).
    resets: AtomicU64,
    /// EOS parks (futex waits at end-of-stream).
    eos_parks: AtomicU64,
    /// F-1 (HFT_RXWARM): cumulative warm-start check-and-fix divergences
    /// (the rxdesc-law telemetry; the kill rule reads the steady state:
    /// fixes == 0 on pass >= 2).
    warm_fixes: AtomicU64,
    /// F-1: frames emitted beyond the warm array (a schedule that grew
    /// past its construction size — the live-derived fallback published;
    /// telemetry only).
    warm_uncovered: AtomicU64,
    /// F-1: fixes in the last ENDED pass (the EOS-marker flush's window;
    /// the kill rule's steady-state reading).
    warm_last_pass_fixes: AtomicU64,
    /// F-2 (HFT_RXBUILD): cumulative master session/elig patches (the
    /// per-pass bake's telemetry; the steady-state law: every patchable
    /// entry exactly once per pass boundary).
    rxbuild_patches: AtomicU64,
    /// F-2: patches in the last ENDED pass (the EOS-marker flush's
    /// window).
    rxbuild_last_pass: AtomicU64,
    /// F-5 (CHECKLIST F-5 / ROADMAP1 §6-I2): nanoseconds spent in the
    /// AUTO-ADVANCE's synchronous bake (the blob tail `reset_prepatched`
    /// and the master tail `master_patch_range` — everything the
    /// incremental prepatch left below the frontier). The steady-window
    /// law: this is the pass boundary's exposed cost on the sustained
    /// critical path (the consumer's reset_pass waits on the whole bake
    /// before pass k+1's first batch); the per-publication prepatch
    /// budget drains it during the pass instead.
    advance_ns: AtomicU64,
    /// F-5: auto-advances served (the advance_ns denominator).
    advances: AtomicU64,
}

#[repr(align(64))]
struct ConsStats {
    /// next_batch futex parks (publication not ready within the polite
    /// spin window).
    parks: AtomicU64,
    /// Nanoseconds spent in those parks.
    park_ns: AtomicU64,
    /// Publication waits that exceeded the spin window (spin + park
    /// boundary events).
    slow_waits: AtomicU64,
    /// Total nanoseconds in slow waits (spin tail + parks).
    slow_ns: AtomicU64,
}

impl RxStats {
    fn zeroed() -> Self {
        Self {
            publications: AtomicU64::new(0),
            polls: AtomicU64::new(0),
            prod_ns: AtomicU64::new(0),
            bufwait_laps: AtomicU64::new(0),
            resets: AtomicU64::new(0),
            eos_parks: AtomicU64::new(0),
            warm_fixes: AtomicU64::new(0),
            warm_uncovered: AtomicU64::new(0),
            warm_last_pass_fixes: AtomicU64::new(0),
            rxbuild_patches: AtomicU64::new(0),
            rxbuild_last_pass: AtomicU64::new(0),
            advance_ns: AtomicU64::new(0),
            advances: AtomicU64::new(0),
        }
    }
}

impl ConsStats {
    fn zeroed() -> Self {
        Self {
            parks: AtomicU64::new(0),
            park_ns: AtomicU64::new(0),
            slow_waits: AtomicU64::new(0),
            slow_ns: AtomicU64::new(0),
        }
    }
}

/// The deep entry mailbox + command channel (NBUF buffers; see
/// NBUF_DEFAULT — F-4 made the depth runtime, the arrays are sized at
/// NBUF_MAX and the protocol values below select the live depth).
struct Mailbox {
    /// F-4: the live mailbox depth (16 default / 32 armed; power of two,
    /// written ONCE at construction before the spawn — the hot paths read
    /// it through the precomputed mask/shift below).
    nbuf: u64,
    /// F-4: NBUF-1 (the buffer-index mask — `turn & nbuf_mask`).
    nbuf_mask: u64,
    /// F-4: log2(NBUF) (the use-count shift — `turn >> nbuf_shift`
    /// replaces the compile-time `turn / NBUF`).
    nbuf_shift: u32,
    /// F-4: the I-7 turn-event ring depth (2×NBUF — the overwrite-guard
    /// window keeps its 2x-max-lag headroom at every depth).
    toff_ring: u64,
    /// F-4: toff_ring-1.
    toff_mask: u64,
    /// RX-built entry buffers; ownership transfers by the turn/use counters
    /// (SPSC: RX writes, consumer reads). Sized at NBUF_MAX; slots >= nbuf
    /// are never touched (construction-initialized, no per-pass cost).
    bufs: [UnsafeCell<EntryBuf>; NBUF_SLOTS],
    /// RX -> consumer: buffer `i` holds turn `T` (Release after the entry
    /// writes; the consumer's Acquire orders all reads). Initialized to
    /// NEVER; the RX publishes turns in order and is bounded by the
    /// freed-count protocol below, so an exact `== turn` match is
    /// unambiguous.
    filled: [Pad; NBUF_SLOTS],
    /// Consumer -> RX: how many times buffer `i` has been freed (one per
    /// consumed OR skipped publication). The RX may write buffer `i` for
    /// turn `T` (its `T/NBUF + 1`-th use) once `freed[i] >= T/NBUF`.
    freed: [Pad; NBUF_SLOTS],
    cmd: AtomicU8,
    shutdown: AtomicBool,
    /// R8: futex wake word for the cold-path handshake — the consumer's
    /// reset() must not wait a scheduler quantum for a cpu-shared RX
    /// thread's spin-loop to notice the command (measured: 15-30ms per
    /// reset, 140 resets per sustained run). The RX's EOS-park futex-waits
    /// on this word; the consumer futex-wakes it after storing the
    /// command.
    wake: AtomicU64,
    /// R8: publication wake word — the consumer's next_batch polite-spins
    /// then futex-parks; the RX wakes it after each publication. An
    /// uncapped consumer PAUSE-spin measurably starved the RX on its SMT
    /// sibling (Zen3's weak PAUSE hint: the 0.3%-busy consumer's spin
    /// held the RX to ~0.2% of the core — 2.6ms per 6us batch).
    pub_wake: AtomicU64,
    /// Reset payload + ack channel (count first, then turn — see doc).
    reset_session: UnsafeCell<[u8; 10]>,
    reset_cnt: AtomicU64,
    reset_ack_turn: AtomicU64,
    reset_ack_cnt: AtomicU64,
    /// R8 phase-3 (auto-advance): session program — `Some(f)` arms the RX
    /// to re-bake and re-render the next pass by itself at every EOS
    /// (pass `k`'s session is `f(k)`; the construction session is pass 0).
    /// Set once at construction, read-only afterwards.
    auto_fn: Option<fn(u64) -> [u8; 10]>,
    /// The pass the RX has baked (advanced to). Written by the RX (Release)
    /// after `auto_session`; read by the consumer's reset() (Acquire).
    auto_pass: AtomicU64,
    /// The session the RX baked for `auto_pass` (ordering: see auto_pass).
    auto_session: UnsafeCell<[u8; 10]>,
    /// Turn of the most recent EOS marker publication (u64::MAX before
    /// the first marker lands; Release after the marker's filled store
    /// and BEFORE the rx_turn store). reset_pass() loads the pair
    /// rx_turn-THEN-auto_eos_turn, so any rx_turn that includes the
    /// marker is observed together with the marker itself — the unstick
    /// ceiling can never lag the frees it is about to issue.
    auto_eos_turn: AtomicU64,
    /// The RX's live publication cursor (turns published so far; one
    /// Release store per publication). The consumer's auto-reset uses it
    /// to free publications it will never consume.
    rx_turn: AtomicU64,
    /// R8 phase-2: telemetry (see RxStats/ConsStats).
    rx_stats: RxStats,
    cons_stats: ConsStats,
    /// F-1 (HFT_RXWARM): whether the RX thread runs the frame-entry warm
    /// start (write-once at construction, BEFORE the spawn — the spawn's
    /// happens-before makes the read race-free; the diagnostics and the
    /// CI arm key on it).
    warm_enabled: bool,
    /// F-2 (HFT_RXBUILD / CHECKLIST F-2 / ROADMAP1 §6-I1): the
    /// publish-by-reference master entry array — event-ordered over the
    /// pass's emitted frames (tombstone-free), built ONCE at construction
    /// from the freshly-baked blob (`render::build_frame_master`),
    /// outside every measured window. The RX patches only its
    /// sess/elig fields (frontier-guarded, see `master_patch_range`);
    /// the consumer walks slices of it through `entries()`. Ordering:
    /// every patch precedes the filled[] Release store of any publication
    /// whose consumer read could observe it, and patches only touch
    /// entries whose publication the consumer has already freed (the
    /// blob prepatch's contract verbatim). `None` in classic/warm modes.
    /// Drop: freeing the Box never dereferences the entries (no Drop on
    /// FrameEntry) — the EntryBuf lifetime contract extends to it.
    master: UnsafeCell<Option<Box<[FrameEntry<'static>]>>>,
    /// F-2: per-master-entry event index (the patch frontier's mapping —
    /// the same consumed-event index the blob prepatch keys on).
    /// Construction-built, immutable afterwards (plain shared read).
    master_evt: Box<[u32]>,
    /// F-2: per-master-entry patchability (the constructor's
    /// session-split rule mirrored). Construction-built, immutable.
    master_patchable: Box<[bool]>,
    /// F-2: whether the RX thread publishes by reference (write-once at
    /// construction, BEFORE the spawn). DEFAULT ON since the F-2 verdict
    /// (draws 23-25: 4/4 healthy 8573C readings at +21.6…+82.6% Front A,
    /// the armed sustained +4.4…+5.3%); HFT_RXBUILD=0 is the rollback
    /// (the classic path verbatim — and HFT_RXWARM keeps its own mode; if
    /// both are set, rxbuild wins and warm is ignored).
    rxbuild_enabled: bool,
    /// R21 (Task 4): the RX thread's ACTUAL landing cpu (`sched_getcpu`
    /// recorded right after the pin attempt; -1 before the first record).
    /// The topology verifier's ACTUAL-side fact — a silent pin failure
    /// (the `let _ = pin_cpu` pattern) can never hide again.
    rx_actual_cpu: AtomicI64,
}

// SAFETY: the mailbox is the SPSC handoff described in the module doc —
// each buffer is owned by exactly one side at a time, transfers are
// ordered by the Release/Acquire turn counters, and the command/reset
// cells follow the documented handshake.
unsafe impl Send for Mailbox {}
unsafe impl Sync for Mailbox {}

/// Futex wait on `wake` (expects the observed value; spurious wakes are
/// fine — callers re-check their condition).
#[inline(always)]
fn futex_wait(wake: &AtomicU64, expected: u64) {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            wake as *const AtomicU64 as *const u32,
            libc::FUTEX_WAIT,
            expected as u32,
            std::ptr::null::<libc::timespec>(),
        );
    }
}

/// R8 phase-3: futex wait with a RELATIVE timeout. The RX thread's
/// buffer-free wait parks here instead of escalating into a `sched_yield`
/// storm: on the 2-physical-core runners a yield-looping RX churns the
/// runqueue and taxes every futex wake-up latency in the pipeline (the
/// consumer's park chain measured ~42µs per batch). A timed park
/// self-wakes at a bounded rate (BUF_PARK_NS), needs no producer-side
/// wake syscall on the consumer's critical path, and is released
/// immediately by the reset/shutdown handshakes (which bump `wake`).
#[inline(always)]
fn futex_wait_timeout(wake: &AtomicU64, expected: u64, ns: u64) {
    let ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: ns as libc::c_long,
    };
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            wake as *const AtomicU64 as *const u32,
            libc::FUTEX_WAIT,
            expected as u32,
            &ts as *const libc::timespec,
        );
    }
}

/// Futex wake (one waiter).
#[inline(always)]
fn futex_wake(wake: &AtomicU64) {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            wake as *const AtomicU64 as *const u32,
            libc::FUTEX_WAKE,
            1i32,
        );
    }
}

#[inline(always)]
fn spin(backoff: &mut u32) {
    let spins = 1u32 << (*backoff).min(6);
    for _ in 0..spins {
        std::hint::spin_loop();
    }
    *backoff += 1;
}

/// Pin the calling thread to an ABSOLUTE cpu id (must be within the
/// process's cgroup cpuset; a thread's INHERITED mask does not restrict
/// widening — only the cgroup does).
fn pin_cpu(cpu: usize) -> bool {
    let mut target = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe { libc::CPU_SET(cpu, &mut target) };
    unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &target) == 0 }
}

/// F-1: the warm bake bookkeeping at every pass boundary (the CMD_RESET
/// serve, the EOS-park reset serve, the auto-advance): the pass-local
/// frame index restarts and W's session-derived fields are rewritten
/// from the fresh template (see render::warm_rewrite_session — the blob
/// holds the new session everywhere by the time the next pass's polls
/// run; the per-frame compare proves it).
fn warm_bake(
    warm: &mut [FrameEntry<'static>],
    warm_idx: &mut usize,
    sess_lo_tmpl: u64,
    sess_hi_tmpl: u64,
    compute_elig: bool,
) {
    *warm_idx = 0;
    crate::render::warm_rewrite_session(warm, sess_lo_tmpl, sess_hi_tmpl, compute_elig);
}

/// F-1: close the current pass's warm telemetry window.
///
/// `force` marks a true pass END (the EOS marker publication): the
/// pass's reading is ALWAYS recorded — a zero-fix steady pass must
/// overwrite the previous pass's nonzero reading, or the kill-rule
/// telemetry would go stale after the first divergent pass. The reset
/// serves pass `force = false`: a serve after a drained EOS emitted
/// nothing since the marker's flush and must not erase the drained
/// pass's reading; a serve after a MID-PASS abandon did emit frames and
/// records the abandoned partial's count.
fn warm_flush_pass_end(
    stats: &RxStats,
    fixes: u64,
    uncovered: u64,
    pass_start: &mut u64,
    force: bool,
) {
    stats.warm_fixes.store(fixes, Ordering::Relaxed);
    stats.warm_uncovered.store(uncovered, Ordering::Relaxed);
    if force || fixes != *pass_start {
        stats
            .warm_last_pass_fixes
            .store(fixes - *pass_start, Ordering::Relaxed);
        *pass_start = fixes;
    }
}

/// F-2 (HFT_RXBUILD): the master's session patch — the prepatch law
/// applied to the entry array. Walks master entries from cursor `from`,
/// patching every PATCHABLE entry whose event index is below `upto_evt`
/// (the consumed frontier — the exclusive end event index of the last
/// freed publication; entries above it belong to in-flight or future
/// publications and must keep the current pass's session). The patch
/// writes the new session's compare words and re-derives the elig
/// byte's session-derived bit from the entry's own static fields (the
/// warm_rewrite_session formula). Non-patchable entries (HB/EOS and
/// second-session frames under session_split) never change — the walk
/// still advances past them (the frontier is event-based).
/// Returns `(new cursor, entries patched this call)`; the walk is
/// bounded by `budget` entries (patched or not) — the R9c pacing law.
///
/// SAFETY CONTRACT (the blob prepatch's, verbatim): only entries whose
/// ENTIRE publication has been consumed (buffer freed) may be patched —
/// the consumer never re-reads a freed publication's entries, and the
/// next pass's walk is ordered after the advance's completion by the
/// filled[] Release/Acquire pair.
#[allow(clippy::too_many_arguments)]
fn master_patch_range(
    master: &mut [FrameEntry<'static>],
    master_evt: &[u32],
    master_patchable: &[bool],
    from: usize,
    upto_evt: usize,
    sess_lo_tmpl: u64,
    sess_hi_tmpl: u64,
    compute_elig: bool,
    budget: usize,
) -> (usize, u64) {
    debug_assert_eq!(master.len(), master_evt.len());
    debug_assert_eq!(master.len(), master_patchable.len());
    let mut idx = from;
    // budget is saturated against the remaining length FIRST (usize::MAX
    // means "unbounded" — a naive from + budget would wrap).
    let end = from + budget.min(master.len().saturating_sub(from));
    let mut patched = 0u64;
    while idx < end {
        if master_evt[idx] as usize >= upto_evt {
            break;
        }
        if master_patchable[idx] {
            let e = &mut master[idx];
            e.sess_lo = sess_lo_tmpl;
            e.sess_hi = sess_hi_tmpl;
            let static_ok = compute_elig
                && !e.blocks.is_empty()
                && e.memo
                    == Some(nf_protocol::packet::FrameMemo {
                        valid_count: e.blocks.len() as u16,
                    });
            e.elig = (e.elig & 3) | ((static_ok as u8) << 7);
            patched += 1;
        }
        idx += 1;
    }
    (idx, patched)
}

/// F-2: the master accessors (the Mailbox cell's discipline: the RX
/// patches between publications, the consumer reads through entries()
/// under the filled[] ordering — both go through the cell).
#[inline]
#[allow(clippy::mut_from_ref)] // the UnsafeCell handoff — see SAFETY
fn master_mut(mb: &Mailbox) -> &mut [FrameEntry<'static>] {
    // SAFETY: the RX thread is the only writer (the consumer's reads are
    // ordered by the filled[] Release/Acquire pair; the patch frontier
    // guarantees no in-flight entry is touched).
    unsafe {
        (*mb.master.get())
            .as_mut()
            .expect("rxbuild: master not built")
            .as_mut()
    }
}

/// F-2: the RX thread's per-pass rxbuild bookkeeping (the patch cursor
/// + the telemetry window).
struct RxBuildState {
    /// The master patch cursor (frontier-driven, restarts at every bake
    /// point — the event numbering restarts with the pass).
    mpp_idx: usize,
    /// Cumulative master patches (the telemetry).
    patches: u64,
    /// The pass-window start (the last flush's cumulative reading).
    pass_start: u64,
    /// The pass-local frame index (the next publication's start).
    idx: usize,
}

impl RxBuildState {
    fn new() -> Self {
        Self {
            mpp_idx: 0,
            patches: 0,
            pass_start: 0,
            idx: 0,
        }
    }
}

/// F-2: close the current pass's patch telemetry window (the warm
/// flush's `force` law verbatim: the EOS marker ALWAYS records — a
/// zero-patch window must overwrite a stale nonzero reading; the reset
/// serves record conditionally — a serve after a drained EOS emitted
/// nothing since the marker's flush and must not erase it).
fn rxbuild_flush_pass_end(stats: &RxStats, rxs: &mut RxBuildState, force: bool) {
    stats.rxbuild_patches.store(rxs.patches, Ordering::Relaxed);
    if force || rxs.patches != rxs.pass_start {
        stats
            .rxbuild_last_pass
            .store(rxs.patches - rxs.pass_start, Ordering::Relaxed);
        rxs.pass_start = rxs.patches;
    }
}

/// F-2: the bake point — the pass-local frame index and the patch cursor
/// restart, and (for the full synchronous re-bakes) EVERY patchable
/// entry rewritten from the fresh template (the consumer is parked —
/// nothing is in flight; `inner.reset` just did the blob's twin).
fn rxbuild_bake_full(
    mb: &Mailbox,
    rxs: &mut RxBuildState,
    sess_lo_tmpl: u64,
    sess_hi_tmpl: u64,
    compute_elig: bool,
) {
    let m = master_mut(mb);
    let (_, patched) = master_patch_range(
        m,
        &mb.master_evt,
        &mb.master_patchable,
        0,
        usize::MAX,
        sess_lo_tmpl,
        sess_hi_tmpl,
        compute_elig,
        usize::MAX,
    );
    rxs.patches += patched;
    rxs.mpp_idx = 0;
    rxs.idx = 0;
}

/// RX thread main loop: poll ahead into free buffers, serve resets.
/// `pin_cpu_id` pins the RX thread to an absolute CPU (None = unpinned).
/// `init_session` is the construction pass's session (pass 0) — the R12 SoA
/// sidecar's ok-bit session compare runs against the session the RX baked,
/// tracked here and refreshed at every bake point (reset serve, EOS-park
/// reset serve, auto-advance).
/// `warm_enabled` (F-1, HFT_RXWARM) arms the frame-entry warm start.
/// `rxbuild_enabled` (F-2, HFT_RXBUILD) selects publish-by-reference (the
/// master array in the Mailbox; DEFAULT ON since the F-2 verdict — draws
/// 23-25, 4/4 healthy readings ≥ +15%; takes precedence over `warm_enabled` —
/// both set runs rxbuild).
#[allow(clippy::disallowed_types)]
fn rx_thread(
    mut inner: ReplayTransport,
    mb: Arc<Mailbox>,
    pin_cpu_id: Option<usize>,
    init_session: [u8; 10],
    warm_enabled: bool,
    rxbuild_enabled: bool,
) {
    if let Some(cpu) = pin_cpu_id {
        let _ = pin_cpu(cpu);
    }
    // R21 (Task 4): record where the RX actually landed (the verifier's
    // ACTUAL-side fact; intended = `pin_cpu_id`).
    mb.rx_actual_cpu.store(unsafe { libc::sched_getcpu() } as i64, Ordering::Relaxed);
    // F-2: rxbuild takes precedence — a both-armed run is rxbuild (the
    // construction already normalizes; this is the defense in depth).
    let warm_enabled = warm_enabled && !rxbuild_enabled;
    // R12c: the baked-session compare template for the entries' elig
    // bytes (bit 7). The consumer proves the bit exact for a group by
    // matching the group's first entry against its OWN live template
    // (see FrameEntry::elig) — the values here key the bit at publish
    // time. Refreshed at every bake point below.
    let mut sess_lo_tmpl = u64::from_le_bytes([
        init_session[0],
        init_session[1],
        init_session[2],
        init_session[3],
        init_session[4],
        init_session[5],
        init_session[6],
        init_session[7],
    ]);
    let mut sess_hi_tmpl = u64::from_le_bytes([
        init_session[2],
        init_session[3],
        init_session[4],
        init_session[5],
        init_session[6],
        init_session[7],
        init_session[8],
        init_session[9],
    ]);
    let refresh_tmpl = |s: &[u8; 10], lo: &mut u64, hi: &mut u64| {
        *lo = u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]);
        *hi = u64::from_le_bytes([s[2], s[3], s[4], s[5], s[6], s[7], s[8], s[9]]);
    };
    // F-2: the same template words as a pure function (the prepatch
    // closure needs the NEXT pass's words before refresh_tmpl runs).
    let tmpl_words = |s: &[u8; 10]| -> (u64, u64) {
        (
            u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]),
            u64::from_le_bytes([s[2], s[3], s[4], s[5], s[6], s[7], s[8], s[9]]),
        )
    };
    // R12 verdict: the ladder is default OFF (the 8573C refutation — see
    // nf_testkit::soa::ladder8_best); the elig byte's session/memo compute
    // is only worth its µops when the consumer can use it. Read once at
    // thread start, outside every window.
    let compute_elig = std::env::var("HFT_VEC_LADDER").as_deref() == Ok("1");
    // HFT_EXP_DIAG diagnostics (never in CI): per-pass poll accounting.
    let diag = std::env::var("HFT_EXP_DIAG").is_ok();
    let mut diag_polls = 0u64;
    let mut diag_max_ns: u64 = 0;
    let mut diag_total_ns: u64 = 0;
    let triples = inner.shared_triples();
    // R8 phase-3b: PREPATCH state — per-turn blob end-offsets (the consumed
    // frontier maps to a patch-list position through them) and the patch
    // cursor itself. The prepatch bakes the NEXT pass's session bytes into
    // frames whose publication the consumer has already freed (see
    // ReplayTransport::patch_range's safety contract); the synchronous
    // patch at the advance point shrinks to the unconsumed tail.
    //
    // R11 — DEFAULT ON, re-flipped by the R10-stack evidence ledger. The
    // R9d reversal was measured on the PRE-R10 equilibrium (INLINE_SLOTS=4,
    // no deep ring): post-R10 the ordering flipped on every silicon class —
    // Zen3 5/5 draws armed-wins (+1.6% mean), 8573C +0.66%
    // (1,116.4M vs 1,109.1M, the gate-break draw), 8370C +3.2%
    // (930.5M vs 901.6M). The deep assist ring changed the mechanism the
    // R9d refutation priced: main's spin budget converts to inline CRC, so
    // the reset handshake's synchronous bake now sits on the critical path
    // the prepatch removes. HFT_PREPATCH=0 disarms (the rollback); the
    // per-pass tuple asserts stay armed either way.
    let prepatch_enabled =
        std::env::var("HFT_PREPATCH").as_deref() != Ok("0");
    // F-4: the runtime depth's protocol values — read ONCE from the
    // construction-written Mailbox (immutable; register-cached through
    // the loop below). At the default these are the R8/R9 shapes
    // verbatim (mask 15, shift 4, ring 32).
    let nbuf = mb.nbuf;
    let nbuf_mask = mb.nbuf_mask;
    let nbuf_shift = mb.nbuf_shift;
    let toff_ring = mb.toff_ring;
    let toff_mask = mb.toff_mask;
    // F-5 (CHECKLIST F-5 / docs/29 §13): the per-publication prepatch
    // budget — the R9c RFO-burst law's pacing constant, now env-tunable
    // so the drain-during-the-pass shape is priceable per draw. Default
    // 64 = the R9c measured shape (tuned for the CLASSIC render path's
    // ~24us publication period); at 1024 the ~22 publications/pass
    // absorb the full 21,996-site patchable set during the pass and the
    // advance's synchronous tail (advance_ns below) empties. Read ONCE
    // at thread start, outside every measured window.
    let prepatch_budget: usize = std::env::var("HFT_PREPATCH_BUDGET")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|b| (64..=8192).contains(b))
        .unwrap_or(64);
    // R9: the per-turn EVENT-INDEX ring — turn_evt_end[t] is the exclusive
    // end event index of turn t's publication (usize::MAX for EOS-marker
    // turns: the whole pass is consumed). The prepatch maps a freed turn
    // to the events whose frames it carried; 2×NBUF slots cover the
    // deepest runahead at ENTRY_CAP with the same overwrite guard as
    // before (F-4: the ring scales with the live depth — 32 slots at
    // NBUF=16, the R9 shape verbatim; 64 at NBUF=32).
    let mut turn_evt_end: [usize; TOFF_SLOTS] = [0; TOFF_SLOTS];
    let mut pp_idx: usize = 0;
    // RX-local scratch batch (poll writes slots here; the transform below
    // re-reads them from this core's L1).
    let mut scratch = FrameBatch::new();
    // F-1 (HFT_RXWARM — CHECKLIST F-1 / ROADMAP2 §6.1): the frame-entry
    // warm start. W holds the last pass's VERIFIED entries, frame-indexed
    // within the pass; `poll_warm` re-derives each entry in registers from
    // the walk's live facts and the COMPARE IS THE CORRECTNESS (the rxdesc
    // check-and-fix law — the proven R16b pattern, docs/29 §5). Steady
    // state the derivation matches W and the only per-frame costs are the
    // ten-field compare plus the verified-entry store — the classic slot
    // push (poll side, ~6 stores/frame) and the accumulate-loop build
    // (~9 slot loads + slice re-derivation + elig chain + construct per
    // frame) are REPLACED, never paralleled (the R12b sidecar's
    // added-store refutation is the designed-out failure mode). Sizing:
    // the schedule is construction-fixed, so the per-pass frame count is
    // exact (`rendered_frame_count`); the allocation is thread-start,
    // outside every measured window (the rxdesc arrays' pattern).
    // HFT_RXWARM unset/0 is the rollback — the classic path below is
    // verbatim.
    let mut warm: Vec<FrameEntry<'static>> = if warm_enabled {
        let empty = FrameEntry {
            bytes: &[],
            feed: 0,
            blocks: &[],
            memo: None,
            first_seq: 0,
            sess_lo: 0,
            sess_hi: 0,
            elig: 0,
        };
        vec![empty; inner.rendered_frame_count()]
    } else {
        Vec::new()
    };
    let mut warm_idx: usize = 0;
    let mut warm_fixes: u64 = 0;
    let mut warm_uncovered: u64 = 0;
    let mut warm_pass_start_fixes: u64 = 0;
    // F-2 (HFT_RXBUILD — CHECKLIST F-2 / ROADMAP1 §6-I1): the
    // publish-by-reference state. The master lives in the Mailbox
    // (construction-built from the freshly-baked blob); `rxs.idx` is the
    // pass-local frame index (the next publication's master slice start);
    // `rxs.mpp_idx` is the master patch cursor (the prepatch's frontier
    // drives it exactly as it drives the blob's `pp_idx`). HFT_RXBUILD=0
    // is the rollback — the classic path below is verbatim (the default
    // flipped ON at the F-2 verdict: draws 23-25, 4/4 healthy ≥ +15%).
    let mut rxs = RxBuildState::new();
    let mut turn: u64 = 0;
    let mut served_resets: u64 = 0;
    // R8 phase-3 (auto-advance): the pass currently baked into the blob —
    // construction = pass 0 with the construction session.
    let mut pass: u64 = 0;
    // I-7: the current pass's FIRST publication turn — the incremental
    // prepatch's floor (see prepatch_step). Freed turns below it belong to
    // a previous pass and carry no valid event index for this pass.
    let mut pass_start_turn: u64 = 0;
    // R8 phase-3b: the consumed-frontier → patch-range advance. `freed`
    // counts are monotone and frees happen in turn order (SPSC), so the
    // max over the per-buffer last-freed turns IS the global frontier.
    // R9: the frontier maps to a consumed-EVENT index (see turn_evt_end)
    // — no blob-offset inference, aliasing-compatible by construction.
    // Only called when armed — unarmed transports keep the blocking
    // reset's full synchronous patch.
    //
    // I-7 parameters:
    // * `ring_covered` — the newest turn whose event end is ALREADY
    //   recorded in `turn_evt_end`. Call site A (post-publication) passes
    //   the turn it just recorded; call site B (the advance wait) passes
    //   the marker — its own turn is not published yet. The overwrite
    //   guard below is exact only against this value.
    // * `floor_turn` — the current pass's first publication turn. A freed
    //   frontier below it belongs to a PREVIOUS pass: its event-ring slot
    //   holds that pass's event indices — or the EOS marker's usize::MAX
    //   "whole pass consumed" sentinel, whose meaning the advance's
    //   synchronous bake already consumed. Reading either here would
    //   over-patch the CURRENT pass's unconsumed head with the next
    //   pass's session: entries built after the patch carry a foreign
    //   session, the consumer's steady scan cold-paths mid-pass, and
    //   `State::Init`'s unconditional `w = first` re-anchor re-emits a
    //   straddling duplicate packet — the draw-19 +35 / R9 +39
    //   count-divergence class (docs/29 §I-7).
    let prepatch_step = |inner: &mut ReplayTransport,
                          next_sess: &[u8; 10],
                          pp_idx: &mut usize,
                          rxs: &mut RxBuildState,
                          ring_covered: u64,
                          floor_turn: u64,
                          turn_evt_end: &[usize; TOFF_SLOTS],
                          budget: usize| {
        let mut frontier: Option<u64> = None;
        for i in 0..nbuf as usize {
            let c = mb.freed[i].load(Ordering::Acquire);
            if c > 0 {
                let t = nbuf * (c - 1) + i as u64;
                if frontier.is_none_or(|f| t > f) {
                    frontier = Some(t);
                }
            }
        }
        if let Some(t) = frontier {
            // I-7: a frontier below the current pass's first publication
            // carries no valid event index for this pass — skip the
            // incremental step entirely (the advance's synchronous
            // reset_prepatched bake is the catch-all for everything
            // legitimately freeable).
            if t < floor_turn {
                return;
            }
            // OVERWRITE GUARD: the event ring holds only the last 32
            // turns. The slot for t is stale once a NEWER turn ≡ t (mod
            // TOFF_RING) has been recorded — i.e. once ring_covered − t ≥
            // TOFF_RING (I-7: ≥, not > — at exactly TOFF_RING the aliased
            // slot already holds ring_covered's own value, and reading it
            // would take a LARGER event end as the frontier: an over-patch
            // of frames the consumer has not freed). If the frontier is
            // that old, skip the incremental step entirely; the
            // synchronous tail at the advance point stays correct.
            if ring_covered.saturating_sub(t) >= toff_ring {
                return;
            }
            let upto_evt = turn_evt_end[(t & toff_mask) as usize];
            // usize::MAX (EOS-marker turn): only reachable for a marker of
            // the CURRENT pass here (the floor already excluded the
            // previous passes' markers) — the whole pass is consumed —
            // patch everything remaining below the list's end.
            *pp_idx = inner.patch_range(next_sess, *pp_idx, upto_evt, budget);
            // F-2: the master's twin patch — the SAME consumed-event
            // frontier, the master's own cursor. The I-7 floor and the
            // overwrite guard above bound BOTH patchers (a frontier below
            // the pass start carries no valid event index; a too-old
            // frontier aliases the event ring — skip both, the advance's
            // synchronous tail is the catch-all for each).
            if rxbuild_enabled {
                let (lo, hi) = tmpl_words(next_sess);
                let m = master_mut(&mb);
                let (ni, np) = master_patch_range(
                    m,
                    &mb.master_evt,
                    &mb.master_patchable,
                    rxs.mpp_idx,
                    upto_evt,
                    lo,
                    hi,
                    compute_elig,
                    budget,
                );
                rxs.mpp_idx = ni;
                rxs.patches += np;
            }
        }
    };
    loop {
        match mb.cmd.load(Ordering::Acquire) {
            CMD_SHUTDOWN => return,
            CMD_RESET => {
                if diag {
                    eprintln!("DIAG rx: serve reset turn={turn}");
                }
                // SAFETY: the consumer is parked in reset() holding no
                // entries; the RX thread owns the transport and blob.
                let sess = unsafe { *mb.reset_session.get() };
                inner.reset(sess);
                refresh_tmpl(&sess, &mut sess_lo_tmpl, &mut sess_hi_tmpl);
                // F-1: the bake point — close the (possibly abandoned,
                // possibly drained) pass's telemetry window, restart the
                // warm index, and rewrite W's session-derived fields from
                // the fresh template (the blob now holds it everywhere —
                // the full synchronous re-bake above).
                if warm_enabled {
                    warm_flush_pass_end(
                        &mb.rx_stats,
                        warm_fixes,
                        warm_uncovered,
                        &mut warm_pass_start_fixes,
                                            false,
                    );
                    warm_bake(
                        &mut warm,
                        &mut warm_idx,
                        sess_lo_tmpl,
                        sess_hi_tmpl,
                        compute_elig,
                    );
                }
                // F-2: the blocking reset's bake point — close the
                // (possibly abandoned) pass's patch window, then the FULL
                // synchronous master rewrite (the consumer is parked;
                // inner.reset just did the blob's twin — the same law).
                if rxbuild_enabled {
                    rxbuild_flush_pass_end(&mb.rx_stats, &mut rxs, false);
                    rxbuild_bake_full(&mb, &mut rxs, sess_lo_tmpl, sess_hi_tmpl, compute_elig);
                }
                // R8 phase-3b: a full synchronous re-bake invalidates the
                // prepatch cursor — restart it for the fresh pass.
                pp_idx = 0;
                served_resets += 1;
                mb.rx_stats.resets.fetch_add(1, Ordering::Relaxed);
                mb.reset_ack_turn.store(turn, Ordering::Release);
                mb.reset_ack_cnt.store(served_resets, Ordering::Release);
                mb.cmd.store(CMD_RUN, Ordering::Release);
                mb.wake.fetch_add(1, Ordering::Release); // bump before wake
                futex_wake(&mb.wake); // release the consumer's ack wait
            }
            _ => {}
        }
        if mb.shutdown.load(Ordering::Acquire) {
            return;
        }
        let i = (turn & nbuf_mask) as usize;
        // Buffer i is free for `turn` (its (turn/NBUF + 1)-th use) once the
        // consumer freed it turn/NBUF times.
        if turn >= nbuf {
            let needed = turn >> nbuf_shift;
            let mut backoff = 0u32;
            while mb.freed[i].load(Ordering::Acquire) < needed {
                if mb.shutdown.load(Ordering::Acquire) {
                    return;
                }
                if mb.cmd.load(Ordering::Acquire) == CMD_RESET {
                    break; // serve the reset first (consumer is waiting)
                }
                mb.rx_stats.bufwait_laps.fetch_add(1, Ordering::Relaxed);
                // R8 phase-3: bounded polite spin, then a TIMED futex park —
                // never a sched_yield storm. On the 2-physical-core runners
                // a yield-looping RX churns the runqueue and taxes every
                // futex wake in the pipeline (measured ~42µs per consumer
                // park). The 50µs self-wake is bounded, needs no
                // producer-side syscall, and the deep mailbox absorbs the
                // latency; the reset/shutdown handshakes still bump `wake`
                // for an immediate release.
                if backoff < 6 {
                    spin(&mut backoff);
                } else {
                    let observed = mb.wake.load(Ordering::Acquire);
                    if mb.freed[i].load(Ordering::Acquire) < needed
                        && mb.cmd.load(Ordering::Acquire) != CMD_RESET
                        && !mb.shutdown.load(Ordering::Acquire)
                    {
                        futex_wait_timeout(&mb.wake, observed, BUF_PARK_NS);
                    }
                }
            }
            if mb.cmd.load(Ordering::Acquire) == CMD_RESET {
                continue;
            }
        }
        // R8: accumulate up to ENTRY_CAP frames per published batch — the
        // handoff round-trips and the consumer's batch-boundary work
        // amortize 4x further than the 256-slot poll granularity. The RX
        // stays ahead of the consumer by construction (its per-frame cost
        // is a fraction of the scan's), so the accumulation never bubbles.
        let mut acc = 0usize;
        let mut eos = false;
        // F-2: this publication's master slice start (the pass-local frame
        // index at the first frame — rxbuild mode publishes
        // master[rx_start .. rx_start + len]).
        let rx_start = rxs.idx;
        debug_assert!(rx_start <= u32::MAX as usize);
        let t_prod = std::time::Instant::now();
        while acc + 256 <= ENTRY_CAP {
            let tp0 = if diag {
                Some(std::time::Instant::now())
            } else {
                None
            };
            let n = if rxbuild_enabled {
                // F-2: the publish-by-reference walk — poll_rxbuild only
                // advances the pacing skeleton and counts frames into
                // `rxs.idx` (no frame line read, no entry build, no
                // mailbox entry store — the publication carries the slice
                // bounds alone; the entries live in the construction-built
                // master, written once and patched at the boundaries).
                let mut wctx = crate::render::WarmEmitCtx {
                    out: &mut [],
                    warm: &mut [],
                    idx: rxs.idx,
                    fixes: 0,
                    uncovered: 0,
                    sess_lo_tmpl: 0,
                    sess_hi_tmpl: 0,
                    compute_elig: false,
                };
                let n = inner.poll_rxbuild(&mut scratch, &mut wctx);
                rxs.idx = wctx.idx;
                n
            } else if warm_enabled {
                // F-1: the warm walk — `poll_warm` derives each entry in
                // registers from the walk's own live facts (the frame meta
                // and the frame line's session words — both loaded by the
                // walk anyway), checks them against W (fixing on
                // divergence — the check IS the correctness), and stores
                // the VERIFIED entries straight into the mailbox window.
                // The scratch slot push and the accumulate-loop build are
                // the replaced cost (zero added store traffic).
                let buf = unsafe { &mut *mb.bufs[i].get() };
                let mut wctx = crate::render::WarmEmitCtx {
                    out: &mut buf.entries[acc..],
                    warm: &mut warm,
                    idx: warm_idx,
                    fixes: warm_fixes,
                    uncovered: warm_uncovered,
                    sess_lo_tmpl,
                    sess_hi_tmpl,
                    compute_elig,
                };
                let n = inner.poll_warm(&mut scratch, &mut wctx);
                warm_idx = wctx.idx;
                warm_fixes = wctx.fixes;
                warm_uncovered = wctx.uncovered;
                n
            } else {
                inner.poll(&mut scratch)
            };
            mb.rx_stats.polls.fetch_add(1, Ordering::Relaxed);
            if let Some(t0i) = tp0 {
                let tpd = t0i.elapsed().as_nanos() as u64;
                diag_polls += 1;
                diag_total_ns += tpd;
                if tpd > diag_max_ns {
                    diag_max_ns = tpd;
                }
            }
            if n == 0 {
                eos = true;
                break;
            }
            if warm_enabled {
                // F-1: the verified entries are already in the mailbox
                // window (poll_warm wrote them); only the driver clock
                // remains.
                let buf = unsafe { &mut *mb.bufs[i].get() };
                buf.clock = inner.now_ns();
                acc += n;
            } else if rxbuild_enabled {
                // F-2: nothing to copy — the frames are counted into
                // rxs.idx and the publication will carry the slice bounds;
                // only the driver clock remains (per-poll, the classic
                // cadence — the last poll's value is the publication's).
                let buf = unsafe { &mut *mb.bufs[i].get() };
                buf.clock = inner.now_ns();
                acc += n;
            } else {
                // Build entries from THIS thread's locally-hot lines (scratch
                // slots + the blob's first lines). SAFETY: (a) RX owns buffer i
                // for this turn until the Release store to filled[i] below;
                // (b) the entries' slices are re-built at 'static from raw
                // parts — the target bytes (the RX transport's blob and the
                // shared triple store) outlive the pipeline (the RX thread is
                // joined in Drop) and are never read after the consumer frees
                // the buffer (see EntryBuf's contract). The re-slice ends the
                // scratch borrow within this block.
                //
                // R12: the same loop fills the SoA sidecar (firsts/ns/lens/
                // feeds + the ok bitmask) — every value is already in
                // registers, so the sidecar costs four scalar stores and two
                // compares per frame on the RX core (which runs ~27% idle at
                // the R11 record), buying the consumer's eight-frame vector
                // ladder on the SUBMITTING core (76% busy). The ok bits
                // accumulate into a register and flush per word; the final
                // partial word is flushed after the loop (its stale high bits
                // are never read — see EntrySoA's contract).
                {
                    let buf = unsafe { &mut *mb.bufs[i].get() };
                    let tp = triples.as_ptr();
                    for k in 0..n {
                        let f = &scratch.frames()[k];
                        let b = f.bytes();
                        let bytes: &'static [u8] =
                            unsafe { std::slice::from_raw_parts(b.as_ptr(), b.len()) };
                        let (blk_base, blk_count, valid) = (f.blk_base, f.blk_count, f.valid);
                        let blocks: &'static [(u64, u32, u32)] = if blk_count == 0 {
                            &[]
                        } else {
                            // SAFETY: slot fields are construction-valid (the
                            // same contract as ReplayTransport::batch_entries).
                            unsafe {
                                std::slice::from_raw_parts(
                                    tp.add(blk_base as usize),
                                    blk_count as usize,
                                )
                            }
                        };
                        // R12c: the elig byte — the steady-eligibility facts
                        // packed into the entry's own padding line (+~4 µops,
                        // zero added line traffic). Bit 7: session == baked
                        // template AND memo proves every block valid AND a
                        // non-empty block index; bits 0..1: the feed.
                        let elig_ok = (compute_elig
                            && blk_count != 0
                            && valid == blk_count
                            && f.sess_lo == sess_lo_tmpl
                            && f.sess_hi == sess_hi_tmpl)
                            as u8;
                        buf.entries[acc + k] = FrameEntry {
                            bytes,
                            feed: f.feed,
                            blocks,
                            memo: (blk_count != 0)
                                .then_some(nf_protocol::packet::FrameMemo { valid_count: valid }),
                            first_seq: f.first_seq,
                            sess_lo: f.sess_lo,
                            sess_hi: f.sess_hi,
                            elig: (f.feed & 3) | (elig_ok << 7),
                        };
                    }
                    buf.clock = inner.now_ns();
                    acc += n;
                }
            }
        }
        // R9: the turn's events end at the transport's current cursor
        // (tombstones included — they advance the cursor without frames,
        // and their consumption tracks the publication they rode in).
        // usize::MAX is never needed here: empty publications (EOS
        // markers) record the sentinel explicitly below.
        let evt_end = inner.current_event_idx();
        {
            // SAFETY: RX owns buffer i for this turn until the Release
            // store below.
            let buf = unsafe { &mut *mb.bufs[i].get() };
            buf.len = acc as u32;
            if rxbuild_enabled {
                buf.rx_start = rx_start as u32;
            }
        }
        turn_evt_end[(turn & toff_mask) as usize] = evt_end;
        mb.rx_stats
            .prod_ns
            .fetch_add(t_prod.elapsed().as_nanos() as u64, Ordering::Relaxed);
        mb.rx_stats.publications.fetch_add(1, Ordering::Relaxed);
        mb.filled[i].store(turn, Ordering::Release);
        mb.rx_turn.store(turn + 1, Ordering::Release);
        // R8 phase-3b: bake the next pass's session bytes into everything
        // the consumer already freed while we were rendering (cheap
        // incremental work — the RX is runahead-deep, never latency-bound
        // here; unarmed transports skip it and keep the full synchronous
        // patch in their reset serve).
        if prepatch_enabled {
            if let Some(sess_fn) = mb.auto_fn {
                let next_sess = sess_fn(pass + 1);
                // R9c pacing: a SMALL budget per publication keeps the
                // RFO burst off the render critical path (the ~24us
                // publication period cannot absorb ~250 RFOs); the
                // frontier keeps advancing and later steps pick up the
                // remaining sites.
                //
                // I-7: `turn`'s own event end was recorded just above (the
                // line before the filled[] Release store), so the ring is
                // covered through `turn` itself; the floor is the current
                // pass's first publication.
                prepatch_step(
                    &mut inner,
                    &next_sess,
                    &mut pp_idx,
                    &mut rxs,
                    turn,
                    pass_start_turn,
                    &turn_evt_end,
                    prepatch_budget,
                );
            }
        }
        // Polite wake: the consumer may be futex-parked on pub_wake (it
        // parks after a bounded spin to keep this SMT sibling fed).
        mb.pub_wake.fetch_add(1, Ordering::Release);
        futex_wake(&mb.pub_wake);
        turn += 1;
        if eos {
            // The end-of-stream marker is its OWN (empty) publication —
            // the consumer's next_batch returns false on it. Without it
            // the consumer would spin on a turn that never comes.
            let j = (turn & nbuf_mask) as usize;
            if turn >= nbuf {
                let needed = turn >> nbuf_shift;
                let mut backoff = 0u32;
                while mb.freed[j].load(Ordering::Acquire) < needed {
                    if mb.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    if backoff < 6 {
                        spin(&mut backoff);
                    } else {
                        let observed = mb.wake.load(Ordering::Acquire);
                        if mb.freed[j].load(Ordering::Acquire) < needed
                            && !mb.shutdown.load(Ordering::Acquire)
                        {
                            futex_wait_timeout(&mb.wake, observed, BUF_PARK_NS);
                        }
                    }
                }
            }
            // SAFETY: RX owns buffer j for this turn until the Release
            // store below; len = 0 needs no entry writes.
            unsafe {
                (*mb.bufs[j].get()).len = 0;
            }
            mb.filled[j].store(turn, Ordering::Release);
            mb.auto_eos_turn.store(turn, Ordering::Release);
            mb.rx_turn.store(turn + 1, Ordering::Release);
            // R8 phase-3b: the marker's frontier mapping — the whole pass
            // is consumed once the marker frees.
            turn_evt_end[(turn & toff_mask) as usize] = usize::MAX;
            // F-1: the pass's frames are all published — close its warm
            // telemetry window NOW, FORCED (a zero-fix steady pass must
            // overwrite the previous reading; the bake/index restart
            // happen at the advance or reset serve that follows).
            if warm_enabled {
                warm_flush_pass_end(
                    &mb.rx_stats,
                    warm_fixes,
                    warm_uncovered,
                    &mut warm_pass_start_fixes,
                    true,
                );
            }
            // F-2: the pass's frames are all published — close its patch
            // window NOW, FORCED (the advance's tail patches below belong
            // to the NEXT pass's window by construction).
            if rxbuild_enabled {
                rxbuild_flush_pass_end(&mb.rx_stats, &mut rxs, true);
            }
            mb.pub_wake.fetch_add(1, Ordering::Release);
            futex_wake(&mb.pub_wake);
            turn += 1;
        }
        let n = if eos { 0 } else { acc };
        if n == 0 {
            if let Some(sess_fn) = mb.auto_fn {
                // R8 phase-3 — AUTO-ADVANCE. The consumer has (or will, via
                // reset()'s unstick loop) free the EOS marker's buffer;
                // every publication of this pass is consumed-safe. As soon
                // as that free lands, re-bake the blob for pass+1 and keep
                // rendering — the per-pass reset handshake (two futex
                // round-trips + a blob patch on the critical path, ~86us
                // per pass on the Zen3 runner) collapses into overlap.
                let j = ((turn - 1) & nbuf_mask) as usize;
                let need = ((turn - 1) >> nbuf_shift) + 1;
                let mut backoff = 0u32;
                loop {
                    if mb.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    if mb.freed[j].load(Ordering::Acquire) >= need {
                        break;
                    }
                    // R8 phase-3b: while waiting for the consumer to finish
                    // the pass's tail, keep baking what it HAS freed — the
                    // tail left for the synchronous advance shrinks as the
                    // consumer drains.
                    //
                    // I-7: the ring is covered through the marker (turn−1) —
                    // this turn is not published yet; the floor is the pass
                    // being drained (its own turns and marker are ≥ it).
                    if prepatch_enabled {
                        let next_sess = sess_fn(pass + 1);
                        // The EOS-drain wait: the RX is idle here BY
                        // CONSTRUCTION (every publication is out; the only
                        // pending obligation is the advance, which fires on
                        // the marker's free — after the LAST drain free).
                        // F-5: the budget is UNBOUNDED — the patch bursts
                        // can never stall a pending publication (there is
                        // none), so the R9c pacing law does not apply; each
                        // iteration patches everything below the CURRENT
                        // frontier and the advance's synchronous tail
                        // collapses to the final publication's share (the
                        // per-publication budget above keeps the mid-pass
                        // steps paced on the render critical path, where
                        // the R9c law DOES apply).
                        prepatch_step(
                            &mut inner,
                            &next_sess,
                            &mut pp_idx,
                            &mut rxs,
                            turn - 1,
                            pass_start_turn,
                            &turn_evt_end,
                            usize::MAX,
                        );
                    }
                    mb.rx_stats.bufwait_laps.fetch_add(1, Ordering::Relaxed);
                    if backoff < 6 {
                        spin(&mut backoff);
                    } else {
                        let observed = mb.wake.load(Ordering::Acquire);
                        if mb.freed[j].load(Ordering::Acquire) < need
                            && !mb.shutdown.load(Ordering::Acquire)
                        {
                            futex_wait_timeout(&mb.wake, observed, BUF_PARK_NS);
                        }
                    }
                }
                let next_pass = pass + 1;
                let sess = sess_fn(next_pass);
                refresh_tmpl(&sess, &mut sess_lo_tmpl, &mut sess_hi_tmpl);
                // F-5: the advance's synchronous bake is the pass
                // boundary's exposed cost on the sustained critical path
                // (the consumer's reset_pass waits on the whole bake) —
                // timed ALWAYS-ON (two Instant reads per PASS, ~50ns on a
                // ~400us pass period; the prod_ns pattern). The budget
                // lever above drains this tail during the pass.
                let t_adv = std::time::Instant::now();
                // F-5 (HFT_EXP_DIAG): the advance-tail attribution — where
                // the prepatch cursors stood at the boundary (the blob
                // site list and the master entry list; the residual is the
                // final publication's share, consumed with the marker).
                if diag {
                    eprintln!(
                        "DIAG advance-probe: pp_idx={}/{} rxs.mpp_idx={}/{}",
                        pp_idx,
                        inner.patch_site_count(),
                        rxs.mpp_idx,
                        mb.master_evt.len()
                    );
                }
                // R8 phase-3b (kill-switched): only the un-prepatched tail
                // bakes synchronously; with the prepatch disabled that is
                // the full blob (the pre-prepatch behavior).
                if prepatch_enabled {
                    inner.reset_prepatched(sess, pp_idx);
                    pp_idx = 0;
                } else {
                    inner.reset(sess);
                }
                // F-1: the advance's bake point — restart the warm index
                // and rewrite W's session-derived fields from the fresh
                // template (the EOS marker above already closed the
                // drained pass's telemetry window; nothing has polled
                // since, so no flush is needed here).
                if warm_enabled {
                    warm_bake(
                        &mut warm,
                        &mut warm_idx,
                        sess_lo_tmpl,
                        sess_hi_tmpl,
                        compute_elig,
                    );
                }
                // F-2: the advance's bake point — the synchronous TAIL
                // patch (everything the incremental steps left below the
                // master's end — reset_prepatched's twin), then the
                // cursor/index restart for the fresh pass. (The EOS marker
                // above already closed the drained pass's window; the
                // advance's patches land in the NEXT pass's window —
                // they bake FOR it.)
                if rxbuild_enabled {
                    let (lo, hi) = tmpl_words(&sess);
                    let m = master_mut(&mb);
                    let (_, np) = master_patch_range(
                        m,
                        &mb.master_evt,
                        &mb.master_patchable,
                        rxs.mpp_idx,
                        usize::MAX,
                        lo,
                        hi,
                        compute_elig,
                        usize::MAX,
                    );
                    rxs.patches += np;
                    rxs.mpp_idx = 0;
                    rxs.idx = 0;
                }
                mb.rx_stats
                    .advance_ns
                    .fetch_add(t_adv.elapsed().as_nanos() as u64, Ordering::Relaxed);
                mb.rx_stats.advances.fetch_add(1, Ordering::Relaxed);
                pass = next_pass;
                // I-7: the new pass's first publication lands on `turn`
                // (the marker took turn−1) — the incremental prepatch's
                // floor from here on: previous-pass turns (including the
                // marker whose sentinel the advance's bake just consumed)
                // must never map into the new pass's event numbering.
                pass_start_turn = turn;
                mb.rx_stats.resets.fetch_add(1, Ordering::Relaxed);
                // SAFETY: RX-exclusive until the Release store of auto_pass.
                unsafe {
                    *mb.auto_session.get() = sess;
                }
                mb.auto_pass.store(pass, Ordering::Release);
                // Release a consumer parked in reset()'s advance wait.
                mb.wake.fetch_add(1, Ordering::Release);
                futex_wake(&mb.wake);
                continue; // render pass k+1 into the free buffers
            }
            if diag {
                eprintln!(
                    "DIAG rx-eos: polls={} total_ms={:.1} max_poll_us={:.1} turn={}",
                    diag_polls,
                    diag_total_ns as f64 / 1e6,
                    diag_max_ns as f64 / 1e3,
                    turn
                );
                diag_polls = 0;
                diag_total_ns = 0;
                diag_max_ns = 0;
            }
            // End of stream: block until a reset/shutdown arrives. A pure
            // spin here made the reset handshake's latency a function of
            // the RX's cpu share (a scheduler quantum when shared —
            // measured 15-30ms per reset, dominating the sustained arm).
            // The futex wait is the cold path only; batch publication
            // stays spin-based.
            mb.rx_stats.eos_parks.fetch_add(1, Ordering::Relaxed);
            loop {
                match mb.cmd.load(Ordering::Acquire) {
                    CMD_SHUTDOWN => return,
                    CMD_RESET => {
                        let sess = unsafe { *mb.reset_session.get() };
                        inner.reset(sess);
                        refresh_tmpl(&sess, &mut sess_lo_tmpl, &mut sess_hi_tmpl);
                        // F-1: the bake point (the EOS-park serve — the
                        // unarmed transports' drained-pass reset; see the
                        // CMD_RESET serve above).
                        if warm_enabled {
                            warm_flush_pass_end(
                                &mb.rx_stats,
                                warm_fixes,
                                warm_uncovered,
                                &mut warm_pass_start_fixes,
                                                            false,
                            );
                            warm_bake(
                                &mut warm,
                                &mut warm_idx,
                                sess_lo_tmpl,
                                sess_hi_tmpl,
                                compute_elig,
                            );
                        }
                        // F-2: the EOS-park serve's bake point (the unarmed
                        // transports' drained-pass reset — the full
                        // synchronous rewrite, as the CMD_RESET serve).
                        if rxbuild_enabled {
                            rxbuild_flush_pass_end(&mb.rx_stats, &mut rxs, false);
                            rxbuild_bake_full(&mb, &mut rxs, sess_lo_tmpl, sess_hi_tmpl, compute_elig);
                        }
                        pp_idx = 0;
                        served_resets += 1;
                        mb.rx_stats.resets.fetch_add(1, Ordering::Relaxed);
                        mb.reset_ack_turn.store(turn, Ordering::Release);
                        mb.reset_ack_cnt.store(served_resets, Ordering::Release);
                        mb.cmd.store(CMD_RUN, Ordering::Release);
                        // Bump BEFORE waking (lost-wake discipline: a wake
                        // without a value change can fire between the
                        // waiter's load and its futex_wait).
                        mb.wake.fetch_add(1, Ordering::Release);
                        futex_wake(&mb.wake); // release the consumer's ack wait
                        break; // resume polling the fresh pass
                    }
                    _ => {}
                }
                if mb.shutdown.load(Ordering::Acquire) {
                    return;
                }
                let observed = mb.wake.load(Ordering::Acquire);
                if mb.cmd.load(Ordering::Acquire) == CMD_RUN {
                    futex_wait(&mb.wake, observed);
                }
            }
        }
    }
}

/// RX-pipelined replay transport (see the module doc).
#[allow(clippy::disallowed_types)] // construction-time Arc handles only
pub struct PipelinedReplayTransport {
    mb: Arc<Mailbox>,
    rx: Option<JoinHandle<()>>,
    /// Consumer's next turn to consume.
    turn: u64,
    /// The current batch view's turn (valid after next_batch() == true).
    cur: Option<u64>,
    /// Resets issued (matches the RX thread's served count).
    resets: u64,
    /// R16b: the consumer sits exactly at a pass boundary — the LAST
    /// next_batch() consumed an EOS marker. reset_pass() samples it to
    /// tell a clean end-of-pass (nothing of the abandoned pass remains
    /// in flight — the unstick must free NOTHING; everything published
    /// past the cursor belongs to the NEXT pass) from a mid-pass abandon
    /// (the abandoned pass's in-flight tail MUST be freed, or the RX
    /// stalls NBUF buffers in and the advance never completes).
    at_eos: bool,
}

impl PipelinedReplayTransport {
    /// Build the pipeline with RX coalescing = 1 (exact pacing). All
    /// allocation and thread spawn happen here — outside every window.
    pub fn new(gt: &[u8], schedule: ReplaySchedule, session: [u8; 10]) -> Self {
        Self::with_coalesce_cpu(gt, schedule, session, 1, None)
    }

    /// Build the pipeline with the given RX coalescing (see
    /// `ReplayTransport::set_poll_coalesce`).
    pub fn with_coalesce(
        gt: &[u8],
        schedule: ReplaySchedule,
        session: [u8; 10],
        coalesce: usize,
    ) -> Self {
        Self::with_coalesce_cpu(gt, schedule, session, coalesce, None)
    }

    /// `with_coalesce` + an ABSOLUTE RX cpu id. The caller captures the
    /// topology order BEFORE pinning its own thread (threads inherit the
    /// creator's restricted mask — pinning main first would leave the RX
    /// stuck on main's cpu) and hands the chosen cpu here.
    pub fn with_coalesce_cpu(
        gt: &[u8],
        schedule: ReplaySchedule,
        session: [u8; 10],
        coalesce: usize,
        rx_cpu: Option<usize>,
    ) -> Self {
        Self::with_coalesce_cpu_auto(gt, schedule, session, coalesce, rx_cpu, None)
    }

    /// R8 phase-3: `with_coalesce_cpu` + the AUTO-ADVANCE session program.
    /// `sess_fn(k)` is the session the RX bakes for pass `k` (pass 0 = the
    /// construction `session`; the first `reset()` corresponds to pass 1).
    /// At every end-of-stream the RX waits for the consumer to free the
    /// pass's EOS marker, re-bakes the blob for pass k+1 BY ITSELF, and
    /// keeps rendering — the per-pass reset handshake (two futex
    /// round-trips + the ~60-80us blob patch on the critical path)
    /// collapses into overlap. The consumer's `reset(expected)` then only
    /// WAITS for the bake (already in flight) and FAIL-STOPS unless the
    /// baked session equals the requested one — the session contract is
    /// load-bearing (the sequencer's session dispatch), so a divergence
    /// must never pass silently.
    ///
    /// CONTRACT (armed mode): the consumer drains each pass to EOS before
    /// resetting (the sustained loop's shape). A mid-pass abandon also
    /// works — reset()'s unstick loop frees the RX's unconsumed
    /// publications — but a blocking `reset()` never needs the CMD_RESET
    /// command path, and nothing else may issue one while the RX is in an
    /// advance wait.
    pub fn with_coalesce_cpu_auto(
        gt: &[u8],
        schedule: ReplaySchedule,
        session: [u8; 10],
        coalesce: usize,
        rx_cpu: Option<usize>,
        sess_fn: Option<fn(u64) -> [u8; 10]>,
    ) -> Self {
        // F-1 (HFT_RXWARM): the fleet path — HFT_RXWARM=1 arms the
        // frame-entry warm start. Read once at construction, outside
        // every measured window; unset/0 is the rollback (the classic
        // path verbatim).
        let warm = std::env::var("HFT_RXWARM").as_deref() == Ok("1");
        // F-2 (HFT_RXBUILD): publish-by-reference — DEFAULT ON since the
        // F-2 verdict (draws 23-25: 4/4 healthy 8573C readings at
        // +21.6…+82.6% Front A, the armed sustained +4.4…+5.3%; the
        // R9c→R9d class-evidence law — ≥ +15% over ≥ 3 draws —
        // satisfied). HFT_RXBUILD=0 is the rollback (the classic path
        // verbatim); takes precedence over the warm start (a both-armed
        // run is rxbuild).
        let rxbuild = std::env::var("HFT_RXBUILD").as_deref() != Ok("0");
        // F-4 (HFT_NBUF): the mailbox depth — 16 (the R8 phase-3 measured
        // shape, the default) or 32 (the buffer-reuse-stall pricing arm;
        // CHECKLIST F-4 / ROADMAP2 §6.3). Read once at construction;
        // anything but "16"/"32" fails safe to the default.
        let nbuf = match std::env::var("HFT_NBUF").as_deref() {
            Ok("32") => 32u64,
            Ok("16") => 16u64,
            _ => NBUF_DEFAULT,
        };
        Self::with_coalesce_cpu_auto_forced(
            gt,
            schedule,
            session,
            coalesce,
            rx_cpu,
            sess_fn,
            warm && !rxbuild,
            rxbuild,
            nbuf,
        )
    }

    /// F-1/F-2/F-4: [`Self::with_coalesce_cpu_auto`] with the warm start,
    /// the publish-by-reference master, and the mailbox depth FORCED —
    /// the tests' and local A/B's explicit path (the envs would be
    /// process-global and racy across parallel test transports).
    #[allow(clippy::too_many_arguments)]
    pub fn with_coalesce_cpu_auto_forced(
        gt: &[u8],
        schedule: ReplaySchedule,
        session: [u8; 10],
        coalesce: usize,
        rx_cpu: Option<usize>,
        sess_fn: Option<fn(u64) -> [u8; 10]>,
        warm: bool,
        rxbuild: bool,
        nbuf: u64,
    ) -> Self {
        debug_assert!(
            nbuf == 16 || nbuf == 32,
            "mailbox depth must be 16 or 32 (power-of-two protocol)"
        );
        let nbuf = if nbuf == 16 || nbuf == 32 {
            nbuf
        } else {
            NBUF_DEFAULT
        };
        let nbuf_mask = nbuf - 1;
        let nbuf_shift = nbuf.trailing_zeros();
        let toff_ring = 2 * nbuf;
        let toff_mask = toff_ring - 1;
        let mut inner = ReplayTransport::new(gt, schedule, session);
        inner.set_poll_coalesce(coalesce);
        let triples = inner.shared_triples();
        let _ = triples;
        // F-2: the master is built ONCE here — construction, outside every
        // measured window, from the freshly-baked blob (the construction
        // bake already wrote the session at every patchable site). The
        // elig compute flag mirrors the RX thread's env read.
        let compute_elig = std::env::var("HFT_VEC_LADDER").as_deref() == Ok("1");
        #[allow(clippy::disallowed_types)] // construction-time boxes only
        let (master, master_evt, master_patchable) = if rxbuild {
            let (m, e, p) = inner.build_frame_master(compute_elig);
            (Some(m), e, p)
        } else {
            (None, Box::from([]), Box::from([]))
        };
        #[allow(clippy::disallowed_types)]
        let mb: Arc<Mailbox> = Arc::new(Mailbox {
            nbuf,
            nbuf_mask,
            nbuf_shift,
            toff_ring,
            toff_mask,
            bufs: std::array::from_fn(|_| UnsafeCell::new(EntryBuf::new())),
            filled: std::array::from_fn(|_| Pad::never()),
            freed: std::array::from_fn(|_| Pad::zeroed()),
            cmd: AtomicU8::new(CMD_RUN),
            shutdown: AtomicBool::new(false),
            wake: AtomicU64::new(0),
            pub_wake: AtomicU64::new(0),
            reset_session: UnsafeCell::new([0u8; 10]),
            reset_cnt: AtomicU64::new(0),
            reset_ack_turn: AtomicU64::new(0),
            reset_ack_cnt: AtomicU64::new(0),
            auto_fn: sess_fn,
            auto_pass: AtomicU64::new(0),
            auto_session: UnsafeCell::new(session),
            auto_eos_turn: AtomicU64::new(u64::MAX),
            rx_turn: AtomicU64::new(0),
            rx_stats: RxStats::zeroed(),
            cons_stats: ConsStats::zeroed(),
            warm_enabled: warm,
            master: UnsafeCell::new(master),
            master_evt,
            master_patchable,
            rxbuild_enabled: rxbuild,
            rx_actual_cpu: AtomicI64::new(-1),
        });
        let rx = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .name(
                // SAFETY-of-law: construction-time only (thread spawn);
                // the Tier-M ban targets hot loops.
                {
                    #[allow(clippy::disallowed_methods)]
                    let n = "r8-rx".to_string();
                    n
                },
            )
            .spawn({
                #[allow(clippy::disallowed_types)]
                let mb: Arc<Mailbox> = Arc::clone(&mb);
                move || rx_thread(inner, mb, rx_cpu, session, warm, rxbuild)
            })
            .expect("r8 rx thread spawn");
        Self {
            mb,
            rx: Some(rx),
            turn: 0,
            cur: None,
            resets: 0,
            at_eos: false,
        }
    }

    /// Reset for a fresh pass (handshake in the module doc). Blocks until
    /// the RX thread has baked the new session; then aligns the turn
    /// counters and frees any pre-reset buffers the consumer skipped.
    ///
    /// ARMED (auto-advance) transports must use [`Self::reset_pass`] — the
    /// plain session-only reset cannot express which program pass the
    /// caller means (a mid-pass abandon skips a pass number), so it
    /// fail-stops instead of guessing.
    pub fn reset(&mut self, session: [u8; 10]) {
        assert!(
            self.mb.auto_fn.is_none(),
            "armed (auto-advance) transport: use reset_pass(pass, session) — \
             the session alone cannot identify the program pass"
        );
        if let Some(t) = self.cur.take() {
            self.mb.freed[(t & self.mb.nbuf_mask) as usize].fetch_add(1, Ordering::Release);
        }
        self.resets += 1;
        let n = self.resets;
        // SAFETY: consumer-exclusive cell; the RX thread reads it only
        // after our Release store of the command.
        unsafe {
            *self.mb.reset_session.get() = session;
        }
        if std::env::var("HFT_EXP_DIAG").is_ok() {
            eprintln!("DIAG reset: issue n={n} turn={}", self.turn);
        }
        self.mb.reset_cnt.store(n, Ordering::Release);
        self.mb.cmd.store(CMD_RESET, Ordering::Release);
        // Wake the (possibly futex-parked) RX thread immediately — without
        // this the handshake waits for the RX's next spin iteration, which
        // on a shared cpu is a scheduler quantum.
        self.mb.wake.fetch_add(1, Ordering::Release);
        futex_wake(&self.mb.wake);
        let mut backoff = 0u32;
        loop {
            if self.mb.reset_ack_cnt.load(Ordering::Acquire) >= n {
                break;
            }
            // Cold path: block on the futex word; the RX wakes us when it
            // stores the ack (it also spins a few rounds first for the
            // common fast case).
            if backoff > 4 {
                let observed = self.mb.wake.load(Ordering::Acquire);
                if self.mb.reset_ack_cnt.load(Ordering::Acquire) < n {
                    futex_wait(&self.mb.wake, observed);
                }
                continue;
            }
            spin(&mut backoff);
        }
        // The RX's turn AFTER the reset (it already published its in-flight
        // batch before serving the command).
        let ack_turn = self.mb.reset_ack_turn.load(Ordering::Acquire);
        if std::env::var("HFT_EXP_DIAG").is_ok() {
            eprintln!("DIAG reset: ack n={n} ack_turn={ack_turn}");
        }
        // Free every publication the RX made but we never consumed
        // (pre-reset frames): turns [self.turn, ack_turn) were ALL
        // published (the RX acks only after its in-flight publication
        // lands, and it never skips turns).
        for _t in self.turn..ack_turn {
            self.mb.freed[(_t & self.mb.nbuf_mask) as usize].fetch_add(1, Ordering::Release);
        }
        self.turn = ack_turn;
    }

    /// R8 phase-3: the AUTO-ADVANCE reset. `pass` is the program pass the
    /// caller is about to consume (pass 0 = construction; the first
    /// `reset_pass` targets pass 1). The RX has already baked — or is
    /// mid-bake of — that pass at the previous EOS, so this NEVER issues a
    /// command or waits a handshake: it (a) frees any publications the
    /// consumer will never consume (empty in the drained-to-EOS shape;
    /// covers the never-consumed construction pass and mid-pass abandons,
    /// both of which also release the RX's advance wait), and (b)
    /// FAIL-STOPS unless the RX's baked pass and session are exactly the
    /// requested ones — the session contract is load-bearing (the
    /// sequencer's session dispatch), so a divergence must never pass
    /// silently.
    ///
    /// CONTRACT: pass numbers advance by exactly one per call (an abandon
    /// consumes its pass number); the publications returned afterwards
    /// belong to `pass`.
    pub fn reset_pass(&mut self, pass: u64, session: [u8; 10]) {
        assert!(
            self.mb.auto_fn.is_some(),
            "reset_pass on an unarmed transport: use reset(session)"
        );
        if let Some(t) = self.cur.take() {
            self.mb.freed[(t & self.mb.nbuf_mask) as usize].fetch_add(1, Ordering::Release);
        }
        self.resets += 1;
        // R16b: a clean end-of-pass and a mid-pass abandon need OPPOSITE
        // unstick policies, and `last_eos` alone cannot tell them apart —
        // the stale marker of the ALREADY-DRAINED pass k−1 and the marker
        // of the pass being abandoned k both read as "last EOS". The
        // consumer KNOWS which shape it is in: it consumed the abandoned
        // pass's marker iff its last next_batch returned false.
        //
        // CLEAN END: the cursor already sits past the marker; every
        // publication in flight belongs to `pass`. The unstick must free
        // NOTHING. (The R16b short-pass bug: this loop freed to rx_turn —
        // the ap-read/rx_turn-read race let it free the NEXT pass's
        // in-flight head, the consumer resumed mid-pass and the pass
        // verified short: measured one pass in ~25k at 360,068/505,849.)
        //
        // MID-PASS ABANDON (incl. the never-consumed construction pass):
        // the abandoned pass's tail is still in flight and MUST be freed
        // or the RX stalls NBUF (=16) buffers in — a pass is thousands of
        // turns, so freezing the unstick here deadlocks the advance.
        let clean_end = self.at_eos;
        self.at_eos = false;
        // The free CEILING: the abandoned pass's EOS marker turn + 1 —
        // free THROUGH the marker (its buffer free releases the RX's
        // advance wait), never beyond it (the next pass's publications
        // must survive for the consumer's exact-match walk). On a clean
        // end the marker was the last consumed turn, so the ceiling is
        // simply self.turn. On a mid-pass abandon it starts UNBOUNDED
        // (everything in flight belongs to the abandoned pass) and LOCKS
        // at the marker the moment the RX lands it: `last_eos >=
        // self.turn` proves the marker in flight is the abandoned pass's
        // own — the RX cannot publish beyond a marker whose buffer it is
        // still waiting to have freed (the advance gate), and this loop's
        // cursor has not freed it yet. The lock cannot fire late: the RX
        // stores auto_eos_turn BEFORE rx_turn, and the loads below read
        // rx_turn BEFORE auto_eos_turn — any rt that includes the marker
        // is read together with (or after) the marker, so the lock lands
        // in the very iteration whose free loop would first cross it.
        let mut cap: u64 = if clean_end { self.turn } else { u64::MAX };
        let mut backoff = 0u32;
        loop {
            let ap = self.mb.auto_pass.load(Ordering::Acquire);
            if ap >= pass {
                // SAFETY: ordered by the auto_pass Acquire above — the RX
                // wrote the session BEFORE its Release store of auto_pass.
                let baked = unsafe { *self.mb.auto_session.get() };
                assert_eq!(
                    ap, pass,
                    "auto-advance overshoot: RX at pass {ap}, reset targets {pass} \
                     (a pass number was skipped or consumed twice)"
                );
                assert_eq!(
                    baked, session,
                    "auto-advance session divergence: RX baked {baked:?}, reset \
                     requested {session:?} — the session contract is load-bearing"
                );
                break;
            }
            // Unstick: free the abandoned pass's published-but-unconsumed
            // turns, up to the ceiling. In the steady (drained-to-EOS)
            // shape the ceiling equals self.turn — the loop is empty by
            // construction and the next pass's head is untouchable. In a
            // mid-pass abandon it releases the RX's buffer-free wait turn
            // by turn; the marker's own free then releases the advance.
            // The park below is TIMED: the RX's in-flight publications
            // bump `pub_wake`, not `wake`, so a plain park here could
            // sleep through the frees the RX is waiting for (lost-wakeup
            // deadlock — the multi-pass test caught it).
            let rt = self.mb.rx_turn.load(Ordering::Acquire);
            let last_eos = self.mb.auto_eos_turn.load(Ordering::Acquire);
            if cap == u64::MAX && last_eos != u64::MAX && last_eos >= self.turn {
                // The abandoned pass's marker just landed — lock the
                // ceiling at its turn + 1. (u64::MAX is the
                // never-published sentinel: the construction pass
                // mid-render.)
                cap = last_eos + 1;
            }
            let stop = rt.min(cap);
            while self.turn < stop {
                self.mb.freed[(self.turn & self.mb.nbuf_mask) as usize]
                    .fetch_add(1, Ordering::Release);
                self.turn += 1;
            }
            if backoff > 4 {
                let observed = self.mb.wake.load(Ordering::Acquire);
                if self.mb.auto_pass.load(Ordering::Acquire) < pass {
                    futex_wait_timeout(&self.mb.wake, observed, BUF_PARK_NS);
                }
                continue;
            }
            spin(&mut backoff);
        }
    }

    /// Advance to the next batch. Returns false at end of stream (the
    /// harness's poll()==0 equivalent). The entry slice + clock stay valid
    /// until the next call.
    pub fn next_batch(&mut self) -> bool {
        if let Some(t) = self.cur.take() {
            self.mb.freed[(t & self.mb.nbuf_mask) as usize].fetch_add(1, Ordering::Release);
        }
        let i = (self.turn & self.mb.nbuf_mask) as usize;
        // Polite acquire: a bounded spin for the common fast case, then a
        // futex park. An UNCAPPED spin here starved the RX on the SMT
        // sibling (Zen3's weak PAUSE hint) — the RX's 6us batch stretched
        // to 2.6ms and the sustained arm collapsed to 8M msg/s.
        let mut spins = 0u32;
        let mut parked = false;
        let t_park = std::time::Instant::now();
        loop {
            if self.mb.filled[i].load(Ordering::Acquire) == self.turn {
                break;
            }
            spins += 1;
            if spins > 512 {
                if !parked {
                    parked = true;
                    self.mb
                        .cons_stats
                        .slow_waits
                        .fetch_add(1, Ordering::Relaxed);
                }
                let observed = self.mb.pub_wake.load(Ordering::Acquire);
                if self.mb.filled[i].load(Ordering::Acquire) != self.turn {
                    let p0 = std::time::Instant::now();
                    futex_wait(&self.mb.pub_wake, observed);
                    self.mb
                        .cons_stats
                        .park_ns
                        .fetch_add(p0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    self.mb.cons_stats.parks.fetch_add(1, Ordering::Relaxed);
                }
                spins = 0;
            } else {
                std::hint::spin_loop();
            }
        }
        if parked {
            self.mb
                .cons_stats
                .slow_ns
                .fetch_add(t_park.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        let t = self.turn;
        self.turn += 1;
        // SAFETY: consumer owns buffer i for turn t; len was published by
        // the RX's Release store (ordered by the Acquire above).
        let len = unsafe { (*self.mb.bufs[i].get()).len };
        if len == 0 {
            // EOS: the empty publication occupies a turn — free it to keep
            // the use counts symmetric for the next pass. The free is also
            // the AUTO-ADVANCE trigger: the RX is (at most) a timed-park
            // quantum away from noticing it — bump the wake word so the
            // next pass's bake starts NOW (one syscall per pass).
            self.mb.freed[i].fetch_add(1, Ordering::Release);
            self.mb.wake.fetch_add(1, Ordering::Release);
            futex_wake(&self.mb.wake);
            // R16b: the consumer now sits exactly at the pass boundary —
            // reset_pass's clean-end discriminator (see at_eos).
            self.at_eos = true;
            return false;
        }
        // A data batch — any pass-boundary position is stale (the flag
        // always reflects the LAST next_batch call, so a stray consume
        // after a false can never poison the next reset_pass).
        self.at_eos = false;
        self.cur = Some(t);
        true
    }

    /// The current batch's entries (valid after next_batch() returned true,
    /// until the next next_batch/reset call). Ready to scan — built by the
    /// RX thread from its locally-hot lines, or — in rxbuild mode — a
    /// slice of the construction-built master (publish-by-reference: the
    /// publication carried the slice bounds; the entries are L1/L2-hot
    /// on the consumer side and were patched only below the consumed
    /// frontier, so this turn's slice is stable for its whole life).
    #[inline]
    pub fn entries(&self) -> &[FrameEntry<'_>] {
        let t = self.cur.expect("r8 pipeline: no current batch");
        if self.mb.rxbuild_enabled {
            // SAFETY: consumer-owned view for this turn; the master's
            // contents for this slice were ordered by the filled[]
            // Release/Acquire pair (the patch frontier guarantees no
            // in-flight entry is touched), and the target bytes outlive
            // the pipeline (the EntryBuf contract, extended to the
            // master).
            unsafe {
                let buf = &*self.mb.bufs[(t & self.mb.nbuf_mask) as usize].get();
                let n = buf.len as usize;
                let start = buf.rx_start as usize;
                let master = (*self.mb.master.get())
                    .as_ref()
                    .expect("rxbuild: master not built");
                debug_assert!(start + n <= master.len());
                &master[start..start + n]
            }
        } else {
            // SAFETY: consumer-owned for this turn; the entries' target bytes
            // outlive the pipeline (joined in Drop) and are not read after the
            // buffer is freed.
            unsafe {
                let buf = &*self.mb.bufs[(t & self.mb.nbuf_mask) as usize].get();
                let n = buf.len as usize;
                &buf.entries[..n]
            }
        }
    }

    /// The current batch's virtual clock (the RX driver's now_ns at
    /// publication).
    #[inline]
    pub fn now_ns(&self) -> u64 {
        let t = self.cur.expect("r8 pipeline: no current batch");
        // SAFETY: same ownership + publication ordering as `entries`.
        unsafe { (*self.mb.bufs[(t & self.mb.nbuf_mask) as usize].get()).clock }
    }

    /// F-1 (HFT_RXWARM) telemetry: (cumulative fixes, uncovered frames,
    /// last-pass fixes). The steady-state law: last_pass_fixes == 0 from
    /// pass 2 (the kill rule is "fixes > 0 persistent"); pass 1's fill
    /// count is expected and recorded.
    pub fn rx_warm_stats(&self) -> (u64, u64, u64) {
        (
            self.mb.rx_stats.warm_fixes.load(Ordering::Relaxed),
            self.mb.rx_stats.warm_uncovered.load(Ordering::Relaxed),
            self.mb.rx_stats.warm_last_pass_fixes.load(Ordering::Relaxed),
        )
    }

    /// F-2 (HFT_RXBUILD) telemetry: (cumulative master patches, patches
    /// in the last ENDED pass, the master's frame count). The
    /// steady-state law: every patchable entry patched exactly once per
    /// pass boundary (the count is a multiple of the patchable frames;
    /// the content parity suite is the correctness pin).
    pub fn rx_build_stats(&self) -> (u64, u64, usize) {
        let frames = if self.mb.rxbuild_enabled {
            // SAFETY: read-only, Relaxed — the diagnostics contract.
            unsafe {
                (*self.mb.master.get())
                    .as_ref()
                    .map(|m| m.len())
                    .unwrap_or(0)
            }
        } else {
            0
        };
        (
            self.mb.rx_stats.rxbuild_patches.load(Ordering::Relaxed),
            self.mb.rx_stats.rxbuild_last_pass.load(Ordering::Relaxed),
            frames,
        )
    }

    /// R21 (Task 4): the RX thread's ACTUAL landing cpu (None before its
    /// first record — the topology verifier's ACTUAL-side fact).
    pub fn rx_actual_cpu(&self) -> Option<usize> {
        let v = self.mb.rx_actual_cpu.load(Ordering::Relaxed);
        (v >= 0).then_some(v as usize)
    }

    /// R8 phase-2 diagnostics: one always-on telemetry line per run covering
    /// the transport's whole life (RX production cost, buffer starvation,
    /// consumer parks). Read-only, post-run.
    pub fn diag_summary(&self, label: &str) {
        let rx = &self.mb.rx_stats;
        let cs = &self.mb.cons_stats;
        // R21 (Task 4): the RX's ACTUAL landing cpu (the verifier's
        // ACTUAL-side fact — intended is the constructor's rx_cpu arg;
        // a mismatch means the pin failed and the placement's premise
        // is broken; -1 = the RX has not started yet).
        let actual = self.mb.rx_actual_cpu.load(Ordering::Relaxed);
        eprintln!(
            "DIAG rx {label}: publications={} polls={} prod_ms={:.1} bufwait_laps={} resets={} eos_parks={} nbuf={} rx_actual_cpu={}",
            rx.publications.load(Ordering::Relaxed),
            rx.polls.load(Ordering::Relaxed),
            rx.prod_ns.load(Ordering::Relaxed) as f64 / 1e6,
            rx.bufwait_laps.load(Ordering::Relaxed),
            rx.resets.load(Ordering::Relaxed),
            rx.eos_parks.load(Ordering::Relaxed),
            self.mb.nbuf,
            actual,
        );
        // F-5: the auto-advance telemetry line (always printed — the
        // pass boundary's exposed bake cost on the sustained critical
        // path; the steady-window law reads advance_ms/advances against
        // the pass period, and the prepatch budget lever drains it).
        eprintln!(
            "ADVANCE_DIAGNOSTIC {label}: advances={} advance_ms={:.1} us_per_advance={:.1} (F-5 reset/auto-advance hygiene — the synchronous bake tail the per-publication prepatch budget drains; HFT_PREPATCH_BUDGET prices it)",
            rx.advances.load(Ordering::Relaxed),
            rx.advance_ns.load(Ordering::Relaxed) as f64 / 1e6,
            if rx.advances.load(Ordering::Relaxed) > 0 {
                rx.advance_ns.load(Ordering::Relaxed) as f64 / rx.advances.load(Ordering::Relaxed) as f64 / 1e3
            } else {
                0.0
            },
        );
        // F-1: the warm-start telemetry line (always printed — the CI arm
        // greps it; enabled=false fixes=0 is the classic path's reading).
        eprintln!(
            "RXWARM_DIAGNOSTIC {label}: enabled={} fixes={} uncovered={} last_pass_fixes={} (F-1 frame-entry warm start; the steady-state law is fixes==0 on pass>=2 — the kill rule reads persistent fixes)",
            self.mb.warm_enabled,
            rx.warm_fixes.load(Ordering::Relaxed),
            rx.warm_uncovered.load(Ordering::Relaxed),
            rx.warm_last_pass_fixes.load(Ordering::Relaxed),
        );
        // F-2: the publish-by-reference telemetry line (always printed —
        // the CI arm greps it; enabled=false patches=0 is the classic
        // path's reading).
        let frames = if self.mb.rxbuild_enabled {
            // SAFETY: read-only, Relaxed — the diagnostics contract.
            unsafe {
                (*self.mb.master.get())
                    .as_ref()
                    .map(|m| m.len())
                    .unwrap_or(0)
            }
        } else {
            0
        };
        eprintln!(
            "RXBUILD_DIAGNOSTIC {label}: enabled={} patches={} last_pass_patches={} frames={} (F-2 publish-by-reference master; the steady-state law is every patchable entry patched exactly once per pass boundary)",
            self.mb.rxbuild_enabled,
            rx.rxbuild_patches.load(Ordering::Relaxed),
            rx.rxbuild_last_pass.load(Ordering::Relaxed),
            frames,
        );
        eprintln!(
            "DIAG cons {label}: parks={} park_ms={:.1} slow_waits={} slow_ms={:.1}",
            cs.parks.load(Ordering::Relaxed),
            cs.park_ns.load(Ordering::Relaxed) as f64 / 1e6,
            cs.slow_waits.load(Ordering::Relaxed),
            cs.slow_ns.load(Ordering::Relaxed) as f64 / 1e6,
        );
    }
}

impl Drop for PipelinedReplayTransport {
    fn drop(&mut self) {
        self.mb.cmd.store(CMD_SHUTDOWN, Ordering::Release);
        self.mb.shutdown.store(true, Ordering::Release);
        // Wake the (possibly futex-parked) RX thread — without this the
        // join below blocks until an unrelated wake.
        self.mb.wake.fetch_add(1, Ordering::Release);
        futex_wake(&self.mb.wake);
        if let Some(h) = self.rx.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::sched_types::{ReplaySchedule, SchedEvent, SchedKind};

    fn mini_sched(n_msgs: u64) -> ReplaySchedule {
        let mut events = Vec::new();
        let mut first_msg = 0u64;
        while first_msg < n_msgs {
            let count = 10u16.min((n_msgs - first_msg) as u16);
            events.push(SchedEvent {
                release_vt: first_msg * 1000,
                feed: 0,
                kind: SchedKind::Packet {
                    first_seq: first_msg + 1,
                    first_msg,
                    count,
                },
            });
            events.push(SchedEvent {
                release_vt: first_msg * 1000,
                feed: 1,
                kind: SchedKind::Packet {
                    first_seq: first_msg + 1,
                    first_msg,
                    count,
                },
            });
            first_msg += count as u64;
        }
        ReplaySchedule {
            events,
            session_split: None,
        }
    }

    fn mini_gt(count: u64) -> Vec<u8> {
        let mut gt = Vec::new();
        for i in 0..count {
            let mut msg = [b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O'];
            msg[1..9].copy_from_slice(&(i + 1).to_be_bytes());
            gt.extend_from_slice(&(msg.len() as u16).to_be_bytes());
            gt.extend_from_slice(&msg);
        }
        gt
    }

    /// F-1 test helper: CONTENT compare for entries from two DIFFERENT
    /// transport instances (their blobs/triples are separate mappings —
    /// pointer identity is meaningless across instances; the in-transport
    /// warm check itself uses pointer identity, which is exact there).
    fn entry_content_neq(a: &FrameEntry<'_>, b: &FrameEntry<'_>) -> bool {
        a.bytes != b.bytes
            || a.blocks != b.blocks
            || a.feed != b.feed
            || a.memo != b.memo
            || a.first_seq != b.first_seq
            || a.sess_lo != b.sess_lo
            || a.sess_hi != b.sess_hi
            || a.elig != b.elig
    }

    fn run_pass(t: &mut PipelinedReplayTransport) -> usize {
        let mut frames = 0usize;
        while t.next_batch() {
            frames += t.entries().len();
            let _ = t.now_ns();
            for e in t.entries() {
                std::hint::black_box(e.bytes.as_ptr());
            }
        }
        frames
    }

    #[test]
    fn t_pipeline_single_pass_coalesce1() {
        let gt = mini_gt(500);
        let sched = mini_sched(500);
        let mut t = PipelinedReplayTransport::with_coalesce(&gt, sched, *b"PIPETEST01", 1);
        assert_eq!(run_pass(&mut t), 100); // dual feed
    }

    #[test]
    fn t_pipeline_single_pass_coalesce8() {
        let gt = mini_gt(500);
        let sched = mini_sched(500);
        let mut t = PipelinedReplayTransport::with_coalesce(&gt, sched, *b"PIPETEST01", 8);
        assert_eq!(run_pass(&mut t), 100);
    }

    #[test]
    fn t_pipeline_coalesce128() {
        let gt = mini_gt(2000);
        let sched = mini_sched(2000);
        let mut t = PipelinedReplayTransport::with_coalesce(&gt, sched, *b"PIPETEST01", 128);
        assert_eq!(run_pass(&mut t), 400);
    }

    #[test]
    fn t_pipeline_multi_reset() {
        let gt = mini_gt(300);
        let sched = mini_sched(300);
        let mut t = PipelinedReplayTransport::with_coalesce(&gt, sched, *b"PIPETEST01", 8);
        for p in 0..3u64 {
            let mut sess = *b"PIPETEST01";
            sess[7..10].copy_from_slice(&(p + 1).to_be_bytes()[5..8]);
            t.reset(sess);
            assert_eq!(run_pass(&mut t), 60, "pass {p}");
        }
    }

    /// R8 phase-3: the auto-advance session program. The RX bakes pass k's
    /// session by itself at every EOS; reset() must (a) never need the
    /// command handshake, (b) fail-stop on a baked-vs-requested divergence,
    /// and (c) produce the same per-pass frame streams as the blocking
    /// path. Includes the never-consumed construction pass (reset before
    /// ANY consumption) and a mid-pass abandon (reset without draining).
    fn sess_program(pass: u64) -> [u8; 10] {
        let mut s = *b"PIPETEST01";
        if pass >= 1 {
            s[7..10].copy_from_slice(&pass.to_be_bytes()[5..8]);
        }
        s
    }

    #[test]
    fn t_pipeline_auto_advance_multi_pass() {
        let gt = mini_gt(300);
        let sched = mini_sched(300);
        let mut t = PipelinedReplayTransport::with_coalesce_cpu_auto(
            &gt,
            sched,
            *b"PIPETEST01",
            8,
            None,
            Some(sess_program),
        );
        // Pass 1..=3: drain-to-EOS resets (the sustained shape). Pass 0
        // (construction) is never consumed — the first reset's unstick
        // loop frees it and the RX advances to pass 1.
        for p in 1..=3u64 {
            t.reset_pass(p, sess_program(p));
            assert_eq!(run_pass(&mut t), 60, "drained pass {p}");
        }
        // Mid-pass abandon: consume one batch, then reset. The auto
        // program advances one pass per reset — abandoning pass 4 however
        // much of it was consumed means targeting pass 5; the unstick loop
        // frees pass 4's unconsumed publications and the RX advances past
        // pass 4's EOS marker to bake pass 5.
        assert!(t.next_batch());
        t.reset_pass(5, sess_program(5));
        assert_eq!(run_pass(&mut t), 60, "abandoned-into pass 5");
        // And a clean drained pass after the abandon.
        t.reset_pass(6, sess_program(6));
        assert_eq!(run_pass(&mut t), 60, "drained pass 6");
    }

    #[test]
    fn t_pipeline_auto_advance_session_divergence_failstops() {
        let gt = mini_gt(120);
        let sched = mini_sched(120);
        let mut t = PipelinedReplayTransport::with_coalesce_cpu_auto(
            &gt,
            sched,
            *b"PIPETEST01",
            8,
            None,
            Some(sess_program),
        );
        // Request a session the program will NOT bake for pass 1 — the
        // transport must fail-stop rather than silently replaying the
        // wrong session bytes.
        let wrong = *b"PIPEDIVER1";
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            t.reset_pass(1, wrong);
        }));
        assert!(
            r.is_err(),
            "auto-advance must fail-stop on session divergence"
        );
    }

    #[test]
    fn t_pipeline_armed_rejects_plain_reset() {
        let gt = mini_gt(60);
        let sched = mini_sched(60);
        let mut t = PipelinedReplayTransport::with_coalesce_cpu_auto(
            &gt,
            sched,
            *b"PIPETEST01",
            8,
            None,
            Some(sess_program),
        );
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            t.reset(*b"PIPETEST01");
        }));
        assert!(r.is_err(), "armed transport must reject plain reset");
    }

    // ─── I-7: the prepatch-race hardening pins (docs/29 §I-7) ──────────

    /// I-7 session program: a distinct session per pass (the flip the
    /// over-patch induces is only observable when consecutive passes carry
    /// different sessions — the sustained harness's shape).
    fn i7_sess(pass: u64) -> [u8; 10] {
        let mut s = *b"PIPEI7S000";
        s[6..10].copy_from_slice(&pass.to_be_bytes()[4..8]);
        s
    }

    /// I-7: the engineered STRADDLE schedule — a dual-feed tape whose 64
    /// earliest-gated regions (the incremental prepatch's first budget
    /// window) each have their PRIMARY inside publication #1 and their
    /// DUPLICATE inside publication #2. Phase-separated feeds (all feed-0
    /// packets, then all feed-1 duplicates) make the earliest gates the
    /// first 64 duplicate events, which land in publication #2 while their
    /// primaries rode publication #1 — exactly the boundary straddle the
    /// real corpus exhibits rarely (and the adjacent-pair test schedules
    /// never do, which is why the class survived every existing test).
    fn i7_straddle_sched(pairs: u64) -> ReplaySchedule {
        let mut events = Vec::new();
        for i in 0..pairs {
            events.push(SchedEvent {
                release_vt: i * 1000,
                feed: 0,
                kind: SchedKind::Packet {
                    first_seq: i * 10 + 1,
                    first_msg: i * 10,
                    count: 10,
                },
            });
        }
        for i in 0..pairs {
            events.push(SchedEvent {
                release_vt: 1_000_000 + i * 1000,
                feed: 1,
                kind: SchedKind::Packet {
                    first_seq: i * 10 + 1,
                    first_msg: i * 10,
                    count: 10,
                },
            });
        }
        ReplaySchedule {
            events,
            session_split: None,
        }
    }

    /// I-7 — the draw-19 +35 / R9 +39 count-divergence class, pinned at
    /// the transport level. The previous pass's EOS marker parks the freed
    /// frontier on a turn whose event-ring slot holds the usize::MAX
    /// "whole pass consumed" sentinel. At the CURRENT pass's first
    /// publication the consumer cannot have freed anything yet (it is
    /// still between passes), so the incremental prepatch reads that
    /// sentinel, treats the entire blob as consumed, and bakes the NEXT
    /// pass's session into the CURRENT pass's unconsumed head —
    /// publication #2's entries are then built from the patched bytes and
    /// carry a foreign session. Downstream (pinned end-to-end by the
    /// nf-testkit chaos soak): the consumer's steady scan cold-paths on
    /// the session mismatch, `session_dispatch` opens a boundary,
    /// `State::Init` re-anchors `w = first` UNCONDITIONALLY, and a
    /// straddling duplicate of an already-emitted packet re-emits its
    /// messages — the +N pass-count divergence.
    ///
    /// THE PIN: through the pass boundary (with a tardy consumer parked
    /// long enough for the RX's full NBUF runahead), every entry of every
    /// batch must carry THIS pass's session — in the frame bytes AND in
    /// the inline sess words.
    #[test]
    fn t_prepatch_marker_sentinel_head_invariant() {
        let pairs: u64 = 832;
        let gt = mini_gt(pairs * 10);
        let sched = i7_straddle_sched(pairs);
        let mut t = PipelinedReplayTransport::with_coalesce_cpu_auto(
            &gt,
            sched,
            i7_sess(0),
            8,
            None,
            Some(i7_sess),
        );
        for pass in 1..=4u64 {
            let sess = i7_sess(pass);
            t.reset_pass(pass, sess);
            // The tardy consumer: park past the RX's full runahead so the
            // freed frontier sits on the previous pass's EOS marker
            // through several publications — the fleet flake's window,
            // forced deterministically.
            std::thread::sleep(std::time::Duration::from_millis(25));
            let mut batches = 0usize;
            let mut frames = 0usize;
            while t.next_batch() {
                batches += 1;
                for e in t.entries() {
                    frames += 1;
                    assert_eq!(
                        &e.bytes[..10],
                        &sess[..],
                        "pass {pass} batch {batches}: foreign session in frame bytes — \
                         the EOS-marker sentinel over-patched the pass head (the I-7 class)"
                    );
                    let lo = u64::from_le_bytes(e.bytes[..8].try_into().unwrap());
                    let hi = u64::from_le_bytes(e.bytes[2..10].try_into().unwrap());
                    assert_eq!(
                        (e.sess_lo, e.sess_hi),
                        (lo, hi),
                        "pass {pass} batch {batches}: inline sess words diverged \
                         from the frame bytes"
                    );
                }
            }
            assert_eq!(frames, (pairs * 2) as usize, "pass {pass} frame count");
            assert!(
                batches >= 3,
                "the corpus must span >= 3 publications per pass (got {batches})"
            );
        }
    }
    // ─── F-1: the frame-entry warm start pins (CHECKLIST F-1) ──────────

    /// F-1: warm vs classic parity — the SAME schedule drained side by
    /// side through both instantiations of the shared poll skeleton,
    /// every entry of every batch compared over ALL ten payload fields
    /// (the warm compare's own definition — the strongest pin available),
    /// across BOTH pacing modes (exact 1 + the throughput shape 128) and
    /// multi-pass ROTATING sessions (the auto program — the template
    /// rewrite's path). Steady state (pass >= 2): warm fixes == 0 — the
    /// pass-invariance law the kill rule reads.
    #[test]
    fn t_rxwarm_parity_vs_classic() {
        for coalesce in [1usize, 128] {
            let gt = mini_gt(300);
            let sched = mini_sched(300);
            let mut classic = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
                &gt,
                sched.clone(),
                *b"PIPETEST01",
                coalesce,
                None,
                Some(sess_program),
                false,
                false,
                16,
            );
            let mut warm = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
                &gt,
                sched,
                *b"PIPETEST01",
                coalesce,
                None,
                Some(sess_program),
                true,
                false,
                16,
            );
            for p in 1..=4u64 {
                let sess = sess_program(p);
                classic.reset_pass(p, sess);
                warm.reset_pass(p, sess);
                let mut frames = 0usize;
                loop {
                    let c_ok = classic.next_batch();
                    let w_ok = warm.next_batch();
                    assert_eq!(c_ok, w_ok, "coalesce {coalesce} pass {p}: EOS mismatch");
                    if !c_ok {
                        break;
                    }
                    let ce = classic.entries();
                    let we = warm.entries();
                    assert_eq!(
                        ce.len(),
                        we.len(),
                        "coalesce {coalesce} pass {p}: batch length mismatch"
                    );
                    for (c, w) in ce.iter().zip(we.iter()) {
                        assert!(
                            !entry_content_neq(c, w),
                            "coalesce {coalesce} pass {p}: warm entry diverged from classic"
                        );
                    }
                    frames += ce.len();
                }
                assert_eq!(frames, 60, "coalesce {coalesce} pass {p}: frame count");
            }
            let (fixes, uncovered, last_pass) = warm.rx_warm_stats();
            assert_eq!(uncovered, 0, "coalesce {coalesce}: uncovered frames");
            assert_eq!(
                last_pass, 0,
                "coalesce {coalesce}: steady-state warm fixes (pass>=2) must be 0"
            );
            // Pass 1's fill is expected and recorded (every frame diverged
            // from the empty initialization) — the documented behavior.
            assert!(
                fixes >= 60,
                "coalesce {coalesce}: pass-1 fill expected (fixes {fixes})"
            );
            let (c_fixes, _, _) = classic.rx_warm_stats();
            assert_eq!(c_fixes, 0, "coalesce {coalesce}: classic path never fixes");
        }
    }

    /// F-1: the mid-pass abandon shape — an abandoned pass leaves a
    /// partial warm state; the next pass must still publish verified
    /// entries with zero fixes (the bake's rewrite + the static-field
    /// check self-correct; the schedule restart is the same frame
    /// sequence). Pinned against the classic transport side by side.
    #[test]
    fn t_rxwarm_mid_pass_abandon() {
        let gt = mini_gt(300);
        let sched = mini_sched(300);
        let mut classic = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
            &gt,
            sched.clone(),
            *b"PIPETEST01",
            8,
            None,
            Some(sess_program),
            false,
            false,
            16,
        );
        let mut warm = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
            &gt,
            sched,
            *b"PIPETEST01",
            8,
            None,
            Some(sess_program),
            true,
            false,
            16,
        );
        // Drain pass 1 fully (the fill), then abandon pass 2 mid-flight.
        for t in [&mut classic, &mut warm] {
            t.reset_pass(1, sess_program(1));
            let mut frames = 0usize;
            while t.next_batch() {
                frames += t.entries().len();
            }
            assert_eq!(frames, 60);
        }
        for t in [&mut classic, &mut warm] {
            t.reset_pass(2, sess_program(2));
            assert!(t.next_batch());
            let _ = t.entries();
        }
        // The abandon: both transports target pass 3 next.
        classic.reset_pass(3, sess_program(3));
        warm.reset_pass(3, sess_program(3));
        let mut frames = 0usize;
        loop {
            let c_ok = classic.next_batch();
            let w_ok = warm.next_batch();
            assert_eq!(c_ok, w_ok);
            if !c_ok {
                break;
            }
            for (c, w) in classic.entries().iter().zip(warm.entries().iter()) {
                assert!(!entry_content_neq(c, w), "post-abandon divergence");
            }
            frames += warm.entries().len();
        }
        assert_eq!(frames, 60, "post-abandon pass frame count");
        let (_, uncovered, _) = warm.rx_warm_stats();
        assert_eq!(uncovered, 0);
    }

    /// F-1: the unarmed plain-reset shape (hft_bench's span arm — one
    /// constant session, reset() per pass). The same-session rewrite is
    /// a no-op value-wise; steady state must be zero fixes from pass 2
    /// and the entries must match the classic transport's exactly.
    #[test]
    fn t_rxwarm_unarmed_constant_session() {
        let gt = mini_gt(300);
        let sched = mini_sched(300);
        let mut classic = PipelinedReplayTransport::with_coalesce(&gt, sched.clone(), *b"PIPETEST01", 128);
        let mut warm = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
            &gt,
            sched,
            *b"PIPETEST01",
            128,
            None,
            None,
            true,
            false,
            16,
        );
        for p in 0..4u64 {
            classic.reset(*b"PIPETEST01");
            warm.reset(*b"PIPETEST01");
            let mut frames = 0usize;
            loop {
                let c_ok = classic.next_batch();
                let w_ok = warm.next_batch();
                assert_eq!(c_ok, w_ok, "pass {p}: EOS mismatch");
                if !c_ok {
                    break;
                }
                for (c, w) in classic.entries().iter().zip(warm.entries().iter()) {
                    assert!(!entry_content_neq(c, w), "pass {p}: divergence");
                }
                frames += warm.entries().len();
            }
            assert_eq!(frames, 60, "pass {p}");
        }
        let (_, uncovered, last_pass) = warm.rx_warm_stats();
        assert_eq!(uncovered, 0);
        assert_eq!(last_pass, 0, "constant-session steady state must be zero-fix");
    }

    /// F-2: publish-by-reference vs classic parity — the SAME schedule
    /// drained side by side, the classic transport building every entry
    /// per publication and the rxbuild transport publishing master
    /// slices, every entry of every batch compared over ALL ten payload
    /// fields, across BOTH pacing modes and multi-pass ROTATING sessions
    /// (the prepatch-extended master patch's path). Steady-state law:
    /// cumulative patches are a multiple of the patchable frame count
    /// (every patchable entry exactly once per pass boundary).
    #[test]
    fn t_rxbuild_parity_vs_classic() {
        for coalesce in [1usize, 128] {
            let gt = mini_gt(300);
            let sched = mini_sched(300);
            let mut classic = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
                &gt,
                sched.clone(),
                *b"PIPETEST01",
                coalesce,
                None,
                Some(sess_program),
                false,
                false,
                16,
            );
            let mut rxbuild = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
                &gt,
                sched,
                *b"PIPETEST01",
                coalesce,
                None,
                Some(sess_program),
                false,
                true,
                16,
            );
            for p in 1..=4u64 {
                let sess = sess_program(p);
                classic.reset_pass(p, sess);
                rxbuild.reset_pass(p, sess);
                let mut frames = 0usize;
                loop {
                    let c_ok = classic.next_batch();
                    let r_ok = rxbuild.next_batch();
                    assert_eq!(c_ok, r_ok, "coalesce {coalesce} pass {p}: EOS mismatch");
                    if !c_ok {
                        break;
                    }
                    let ce = classic.entries();
                    let re = rxbuild.entries();
                    assert_eq!(
                        ce.len(),
                        re.len(),
                        "coalesce {coalesce} pass {p}: batch length mismatch"
                    );
                    for (c, r) in ce.iter().zip(re.iter()) {
                        assert!(
                            !entry_content_neq(c, r),
                            "coalesce {coalesce} pass {p}: rxbuild entry diverged from classic"
                        );
                    }
                    frames += ce.len();
                }
                assert_eq!(frames, 60, "coalesce {coalesce} pass {p}: frame count");
            }
            let (patches, _, master_frames) = rxbuild.rx_build_stats();
            assert_eq!(master_frames, 60, "coalesce {coalesce}: master frame count");
            // Every pass boundary patches every patchable entry exactly
            // once (60 patchable frames here; the trailing construction
            // -> pass 1 boundary included; the post-pass-4 advance races
            // shutdown and may or may not complete). F-5 lesson: the
            // final read also races the last in-flight pass's
            // INCREMENTAL prepatch absorption — the count is
            // boundary-aligned only AT boundaries, so the pin is the
            // floor/ceiling pair (the advance's tail guarantees the
            // floor; the ceiling forbids any double-patch).
            assert!(
                (240..=300).contains(&patches),
                "coalesce {coalesce}: patch law broken (patches {patches})"
            );
            let (c_patches, _, c_frames) = classic.rx_build_stats();
            assert_eq!(c_patches, 0, "coalesce {coalesce}: classic path never patches");
            assert_eq!(c_frames, 0, "coalesce {coalesce}: classic path builds no master");
        }
    }

    /// F-4 (HFT_NBUF=32): the deep-mailbox parity pin — the SAME schedule,
    /// BOTH pacing modes, multi-pass ROTATING sessions, drained side by
    /// side through the depth-32 classic pipeline and the depth-16 classic
    /// pipeline (the default's observables are the reference: identical
    /// batches, identical entry contents, identical EOS alignment — the
    /// ring protocol must be depth-transparent), plus the depth-32 rxbuild
    /// twin against the depth-32 classic (the F-2 lever rides the same
    /// ring; the master slice bounds and the patch law must hold at the
    /// doubled runahead, where the consumer's frees lag up to 32 turns —
    /// the I-7 overwrite-guard window scales to 64 with the ring).
    #[test]
    fn t_nbuf32_parity_vs_depth16() {
        for coalesce in [1usize, 128] {
            let gt = mini_gt(300);
            let sched = mini_sched(300);
            let mut d16 = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
                &gt,
                sched.clone(),
                *b"PIPETEST01",
                coalesce,
                None,
                Some(sess_program),
                false,
                false,
                16,
            );
            let mut d32 = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
                &gt,
                sched.clone(),
                *b"PIPETEST01",
                coalesce,
                None,
                Some(sess_program),
                false,
                false,
                32,
            );
            let mut d32r = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
                &gt,
                sched,
                *b"PIPETEST01",
                coalesce,
                None,
                Some(sess_program),
                false,
                true,
                32,
            );
            for p in 1..=4u64 {
                let sess = sess_program(p);
                d16.reset_pass(p, sess);
                d32.reset_pass(p, sess);
                d32r.reset_pass(p, sess);
                let mut frames = 0usize;
                loop {
                    let a_ok = d16.next_batch();
                    let b_ok = d32.next_batch();
                    let r_ok = d32r.next_batch();
                    assert_eq!(a_ok, b_ok, "coalesce {coalesce} pass {p}: d16/d32 EOS mismatch");
                    assert_eq!(a_ok, r_ok, "coalesce {coalesce} pass {p}: d32 classic/rxbuild EOS mismatch");
                    if !a_ok {
                        break;
                    }
                    let ae = d16.entries();
                    let be = d32.entries();
                    let re = d32r.entries();
                    assert_eq!(
                        ae.len(),
                        be.len(),
                        "coalesce {coalesce} pass {p}: d16/d32 batch length mismatch"
                    );
                    assert_eq!(
                        be.len(),
                        re.len(),
                        "coalesce {coalesce} pass {p}: d32 classic/rxbuild batch length mismatch"
                    );
                    for (a, b) in ae.iter().zip(be.iter()) {
                        assert!(
                            !entry_content_neq(a, b),
                            "coalesce {coalesce} pass {p}: depth-32 entry diverged from depth-16"
                        );
                    }
                    for (b, r) in be.iter().zip(re.iter()) {
                        assert!(
                            !entry_content_neq(b, r),
                            "coalesce {coalesce} pass {p}: depth-32 rxbuild entry diverged"
                        );
                    }
                    frames += ae.len();
                }
                assert_eq!(frames, 60, "coalesce {coalesce} pass {p}: frame count");
            }
            let (patches, _, master_frames) = d32r.rx_build_stats();
            assert_eq!(master_frames, 60, "coalesce {coalesce}: d32 master frame count");
            assert!(
                (240..=300).contains(&patches),
                "coalesce {coalesce}: d32 patch law broken (patches {patches})"
            );
        }
    }

    /// F-2: the mid-pass abandon shape — an abandoned pass leaves a
    /// partially-patched master; the next pass must still publish the
    /// correct entries (the reset serve's full rewrite is the
    /// catch-all). Pinned against the classic transport side by side.
    #[test]
    fn t_rxbuild_mid_pass_abandon() {
        let gt = mini_gt(300);
        let sched = mini_sched(300);
        let mut classic = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
            &gt,
            sched.clone(),
            *b"PIPETEST01",
            8,
            None,
            Some(sess_program),
            false,
            false,
            16,
        );
        let mut rxbuild = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
            &gt,
            sched,
            *b"PIPETEST01",
            8,
            None,
            Some(sess_program),
            false,
            true,
            16,
        );
        // Drain pass 1 fully, then abandon pass 2 mid-flight.
        for t in [&mut classic, &mut rxbuild] {
            t.reset_pass(1, sess_program(1));
            let mut frames = 0usize;
            while t.next_batch() {
                frames += t.entries().len();
            }
            assert_eq!(frames, 60);
        }
        for t in [&mut classic, &mut rxbuild] {
            t.reset_pass(2, sess_program(2));
            assert!(t.next_batch());
            let _ = t.entries();
        }
        // The abandon: both transports target pass 3 next.
        classic.reset_pass(3, sess_program(3));
        rxbuild.reset_pass(3, sess_program(3));
        let mut frames = 0usize;
        loop {
            let c_ok = classic.next_batch();
            let r_ok = rxbuild.next_batch();
            assert_eq!(c_ok, r_ok);
            if !c_ok {
                break;
            }
            for (c, r) in classic.entries().iter().zip(rxbuild.entries().iter()) {
                assert!(!entry_content_neq(c, r), "post-abandon divergence");
            }
            frames += rxbuild.entries().len();
        }
        assert_eq!(frames, 60, "post-abandon pass frame count");
        let (patches, _, master_frames) = rxbuild.rx_build_stats();
        assert_eq!(master_frames, 60);
        assert!(patches >= 180 && patches % 60 == 0, "patch law after abandon (patches {patches})");
    }

    /// F-2: the unarmed plain-reset shape (hft_bench's span arm — one
    /// constant session, reset() per pass). The master patch is
    /// value-identical per pass; the entries must match the classic
    /// transport's exactly, forever.
    #[test]
    fn t_rxbuild_constant_session() {
        let gt = mini_gt(300);
        let sched = mini_sched(300);
        let mut classic = PipelinedReplayTransport::with_coalesce(&gt, sched.clone(), *b"PIPETEST01", 128);
        let mut rxbuild = PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
            &gt,
            sched,
            *b"PIPETEST01",
            128,
            None,
            None,
            false,
            true,
            16,
        );
        for p in 0..4u64 {
            classic.reset(*b"PIPETEST01");
            rxbuild.reset(*b"PIPETEST01");
            let mut frames = 0usize;
            loop {
                let c_ok = classic.next_batch();
                let r_ok = rxbuild.next_batch();
                assert_eq!(c_ok, r_ok, "pass {p}: EOS mismatch");
                if !c_ok {
                    break;
                }
                for (c, r) in classic.entries().iter().zip(rxbuild.entries().iter()) {
                    assert!(!entry_content_neq(c, r), "pass {p}: divergence");
                }
                frames += rxbuild.entries().len();
            }
            assert_eq!(frames, 60, "pass {p}");
        }
        // The blocking reset serves patch all 60 patchable entries each
        // (4 passes drained + the armed-mode drain's own boundaries).
        let (patches, _, master_frames) = rxbuild.rx_build_stats();
        assert_eq!(master_frames, 60);
        assert!(patches >= 240 && patches % 60 == 0, "patch law (patches {patches})");
    }
}
