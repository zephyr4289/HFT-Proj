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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

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
const NBUF: u64 = 16;
const NBUF_MASK: u64 = NBUF - 1;
/// R8 phase-6: frames per publication (and the EntryBuf slot count —
/// they are ONE constant: the accumulate loop writes entries[acc..acc+n)
/// with acc bounded by this cap). 2048: half the handoffs, twice the
/// per-batch amortization; with NBUF=16 the runahead spans ~2 passes at
/// the sample's ~12.6k frames.
const ENTRY_CAP: usize = 2048;

/// RX timed-park quantum for the buffer-free wait (see futex_wait_timeout).
/// 15us: the consumer frees a buffer every ~30-40us at the achieved rates —
/// a 50us quantum made the RX's production bursty against that drain and
/// the consumer parked mid-pass (Intel 8573C run: 6,750 parks / 391ms).
const BUF_PARK_NS: u64 = 15_000;

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
/// SAFETY-of-lifetime: the entries' slices point into the RX transport's
/// blob/triples (which outlive the pipeline — the RX thread is joined in
/// `Drop`) and are never dereferenced after the consumer frees the buffer
/// (the harness consumes each batch fully before the next `next_batch`).
struct EntryBuf {
    entries: Box<[FrameEntry<'static>; ENTRY_CAP]>,
    len: u32,
    clock: u64,
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
            })),
            len: 0,
            clock: 0,
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

/// The deep entry mailbox + command channel (NBUF buffers; see NBUF).
struct Mailbox {
    /// RX-built entry buffers; ownership transfers by the turn/use counters
    /// (SPSC: RX writes, consumer reads).
    bufs: [UnsafeCell<EntryBuf>; NBUF as usize],
    /// RX -> consumer: buffer `i` holds turn `T` (Release after the entry
    /// writes; the consumer's Acquire orders all reads). Initialized to
    /// NEVER; the RX publishes turns in order and is bounded by the
    /// freed-count protocol below, so an exact `== turn` match is
    /// unambiguous.
    filled: [Pad; NBUF as usize],
    /// Consumer -> RX: how many times buffer `i` has been freed (one per
    /// consumed OR skipped publication). The RX may write buffer `i` for
    /// turn `T` (its `T/NBUF + 1`-th use) once `freed[i] >= T/NBUF`.
    freed: [Pad; NBUF as usize],
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
    /// Turn of the most recent EOS marker publication (Release after the
    /// marker's filled store; the consumer's reset() uses it to free an
    /// abandoned stream's publications).
    auto_eos_turn: AtomicU64,
    /// The RX's live publication cursor (turns published so far; one
    /// Release store per publication). The consumer's auto-reset uses it
    /// to free publications it will never consume.
    rx_turn: AtomicU64,
    /// R8 phase-2: telemetry (see RxStats/ConsStats).
    rx_stats: RxStats,
    cons_stats: ConsStats,
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

/// RX thread main loop: poll ahead into free buffers, serve resets.
/// `pin_cpu_id` pins the RX thread to an absolute CPU (None = unpinned).
#[allow(clippy::disallowed_types)]
fn rx_thread(mut inner: ReplayTransport, mb: Arc<Mailbox>, pin_cpu_id: Option<usize>) {
    if let Some(cpu) = pin_cpu_id {
        let _ = pin_cpu(cpu);
    }
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
    let blob_base = inner.blob_base();
    // R8 phase-6: the end-offset ring is decoupled from NBUF (32 slots —
    // over 4 passes of publications at ENTRY_CAP=2048) so the prepatch's
    // frontier mapping stays valid at the deeper runahead.
    const TOFF_RING: u64 = 32;
    const TOFF_MASK: u64 = TOFF_RING - 1;
    let mut turn_end_off: [usize; TOFF_RING as usize] = [0; TOFF_RING as usize];
    let mut pp_idx: usize = 0;
    // RX-local scratch batch (poll writes slots here; the transform below
    // re-reads them from this core's L1).
    let mut scratch = FrameBatch::new();
    let mut turn: u64 = 0;
    let mut served_resets: u64 = 0;
    // R8 phase-3 (auto-advance): the pass currently baked into the blob —
    // construction = pass 0 with the construction session.
    let mut pass: u64 = 0;
    // R8 phase-3b: the consumed-frontier → patch-range advance. `freed`
    // counts are monotone and frees happen in turn order (SPSC), so the
    // max over the per-buffer last-freed turns IS the global frontier.
    // Only called when armed — unarmed transports keep the blocking
    // reset's full synchronous patch.
    let prepatch_step = |inner: &mut ReplayTransport,
                          next_sess: &[u8; 10],
                          pp_idx: &mut usize,
                          cur_turn: u64,
                          turn_end_off: &[usize; 32]| {
        let mut frontier: Option<u64> = None;
        for i in 0..NBUF as usize {
            let c = mb.freed[i].load(Ordering::Acquire);
            if c > 0 {
                let t = NBUF * (c - 1) + i as u64;
                if frontier.is_none_or(|f| t > f) {
                    frontier = Some(t);
                }
            }
        }
        if let Some(t) = frontier {
            // OVERWRITE GUARD: the end-offset ring holds only the last NBUF
            // turns. If the frontier is so old that its slot may already
            // hold a NEWER turn's offset (which could be LARGER — an
            // over-patch would re-bake frames the consumer has not freed),
            // skip the incremental step entirely; the synchronous tail at
            // the advance point stays correct.
            if cur_turn - t > TOFF_RING {
                return;
            }
            let upto = turn_end_off[(t & TOFF_MASK) as usize];
            if upto == usize::MAX {
                // The frontier is an EOS marker: the whole pass is consumed.
                *pp_idx = inner.patch_range(next_sess, *pp_idx, usize::MAX - 10);
            } else if upto > 10 {
                *pp_idx = inner.patch_range(next_sess, *pp_idx, upto);
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
        let i = (turn & NBUF_MASK) as usize;
        // Buffer i is free for `turn` (its (turn/NBUF + 1)-th use) once the
        // consumer freed it turn/NBUF times.
        if turn >= NBUF {
            let needed = turn / NBUF;
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
        // R8 phase-3b: the publication's last-frame blob end offset (the
        // prepatch frontier mapping). usize::MAX for empty publications
        // (EOS markers — the whole pass is consumed when they free).
        let mut end_off: usize = usize::MAX;
        let t_prod = std::time::Instant::now();
        while acc + 256 <= ENTRY_CAP {
            let tp0 = if diag {
                Some(std::time::Instant::now())
            } else {
                None
            };
            let n = inner.poll(&mut scratch);
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
            // Build entries from THIS thread's locally-hot lines (scratch
            // slots + the blob's first lines). SAFETY: (a) RX owns buffer i
            // for this turn until the Release store to filled[i] below;
            // (b) the entries' slices are re-built at 'static from raw
            // parts — the target bytes (the RX transport's blob and the
            // shared triple store) outlive the pipeline (the RX thread is
            // joined in Drop) and are never read after the consumer frees
            // the buffer (see EntryBuf's contract). The re-slice ends the
            // scratch borrow within this block.
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
                    buf.entries[acc + k] = FrameEntry {
                        bytes,
                        feed: f.feed,
                        blocks,
                        memo: (blk_count != 0)
                            .then_some(nf_protocol::packet::FrameMemo { valid_count: valid }),
                        first_seq: f.first_seq,
                        sess_lo: f.sess_lo,
                        sess_hi: f.sess_hi,
                    };
                    // R8 phase-3b: track the last frame's blob end for the
                    // prepatch frontier mapping.
                    end_off = (b.as_ptr() as usize) + b.len() - blob_base;
                }
                buf.clock = inner.now_ns();
                acc += n;
            }
        }
        {
            // SAFETY: RX owns buffer i for this turn until the Release
            // store below.
            let buf = unsafe { &mut *mb.bufs[i].get() };
            buf.len = acc as u32;
        }
        turn_end_off[(turn & TOFF_MASK) as usize] = end_off;
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
        if let Some(sess_fn) = mb.auto_fn {
            let next_sess = sess_fn(pass + 1);
            prepatch_step(&mut inner, &next_sess, &mut pp_idx, turn, &turn_end_off);
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
            let j = (turn & NBUF_MASK) as usize;
            if turn >= NBUF {
                let needed = turn / NBUF;
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
            turn_end_off[(turn & TOFF_MASK) as usize] = usize::MAX;
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
                let j = ((turn - 1) & NBUF_MASK) as usize;
                let need = (turn - 1) / NBUF + 1;
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
                    let next_sess = sess_fn(pass + 1);
                    prepatch_step(&mut inner, &next_sess, &mut pp_idx, turn, &turn_end_off);
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
                // R8 phase-3b: only the UNPREPATCHED tail bakes synchronously
                // (during the pass the RX already re-baked every frame whose
                // publication the consumer freed); then the cursor restarts
                // for the new pass's consumption.
                inner.reset_prepatched(sess, pp_idx);
                pp_idx = 0;
                pass = next_pass;
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
        let mut inner = ReplayTransport::new(gt, schedule, session);
        inner.set_poll_coalesce(coalesce);
        let triples = inner.shared_triples();
        let _ = triples;
        #[allow(clippy::disallowed_types)]
        let mb: Arc<Mailbox> = Arc::new(Mailbox {
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
            auto_eos_turn: AtomicU64::new(0),
            rx_turn: AtomicU64::new(0),
            rx_stats: RxStats::zeroed(),
            cons_stats: ConsStats::zeroed(),
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
                move || rx_thread(inner, mb, rx_cpu)
            })
            .expect("r8 rx thread spawn");
        Self {
            mb,
            rx: Some(rx),
            turn: 0,
            cur: None,
            resets: 0,
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
            self.mb.freed[(t & NBUF_MASK) as usize].fetch_add(1, Ordering::Release);
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
            self.mb.freed[(_t & NBUF_MASK) as usize].fetch_add(1, Ordering::Release);
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
            self.mb.freed[(t & NBUF_MASK) as usize].fetch_add(1, Ordering::Release);
        }
        self.resets += 1;
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
            // Unstick: free every published-but-unconsumed turn. In the
            // steady shape this range is empty — the consumer's turn
            // already sits past the EOS marker. In a mid-pass abandon it
            // grows as the RX publishes the rest of the abandoned pass,
            // releasing the RX's advance wait the moment its EOS marker
            // lands. The park below is TIMED: the RX's in-flight
            // publications bump `pub_wake`, not `wake`, so a plain park
            // here could sleep through the frees the RX is waiting for
            // (lost-wakeup deadlock — the multi-pass test caught it).
            let rt = self.mb.rx_turn.load(Ordering::Acquire);
            while self.turn < rt {
                self.mb.freed[(self.turn & NBUF_MASK) as usize]
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
            self.mb.freed[(t & NBUF_MASK) as usize].fetch_add(1, Ordering::Release);
        }
        let i = (self.turn & NBUF_MASK) as usize;
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
            return false;
        }
        self.cur = Some(t);
        true
    }

    /// The current batch's entries (valid after next_batch() returned true,
    /// until the next next_batch/reset call). Ready to scan — built by the
    /// RX thread from its locally-hot lines.
    #[inline]
    pub fn entries(&self) -> &[FrameEntry<'_>] {
        let t = self.cur.expect("r8 pipeline: no current batch");
        // SAFETY: consumer-owned for this turn; the entries' target bytes
        // outlive the pipeline (joined in Drop) and are not read after the
        // buffer is freed.
        unsafe {
            let buf = &*self.mb.bufs[(t & NBUF_MASK) as usize].get();
            let n = buf.len as usize;
            &buf.entries[..n]
        }
    }

    /// The current batch's virtual clock (the RX driver's now_ns at
    /// publication).
    #[inline]
    pub fn now_ns(&self) -> u64 {
        let t = self.cur.expect("r8 pipeline: no current batch");
        // SAFETY: same ownership + publication ordering as `entries`.
        unsafe { (*self.mb.bufs[(t & NBUF_MASK) as usize].get()).clock }
    }

    /// R8 phase-2 diagnostics: one always-on telemetry line per run covering
    /// the transport's whole life (RX production cost, buffer starvation,
    /// consumer parks). Read-only, post-run.
    pub fn diag_summary(&self, label: &str) {
        let rx = &self.mb.rx_stats;
        let cs = &self.mb.cons_stats;
        eprintln!(
            "DIAG rx {label}: publications={} polls={} prod_ms={:.1} bufwait_laps={} resets={} eos_parks={}",
            rx.publications.load(Ordering::Relaxed),
            rx.polls.load(Ordering::Relaxed),
            rx.prod_ns.load(Ordering::Relaxed) as f64 / 1e6,
            rx.bufwait_laps.load(Ordering::Relaxed),
            rx.resets.load(Ordering::Relaxed),
            rx.eos_parks.load(Ordering::Relaxed),
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
}
