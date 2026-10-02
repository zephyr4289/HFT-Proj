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
//!   Release store (buffer `i` serves turns `i, i+2, i+4, ...`);
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
    entries: Box<[FrameEntry<'static>; 1024]>,
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

/// The four-buffer entry mailbox + command channel.
struct Mailbox {
    /// RX-built entry buffers; ownership transfers by the turn/use counters
    /// (SPSC: RX writes, consumer reads).
    bufs: [UnsafeCell<EntryBuf>; 4],
    /// RX -> consumer: buffer `i` holds turn `T` (Release after the entry
    /// writes; the consumer's Acquire orders all reads). Initialized to
    /// NEVER; the RX publishes turns in order and is bounded by the
    /// freed-count protocol below, so an exact `== turn` match is
    /// unambiguous.
    filled: [Pad; 4],
    /// Consumer -> RX: how many times buffer `i` has been freed (one per
    /// consumed OR skipped publication). The RX may write buffer `i` for
    /// turn `T` (its `T/4 + 1`-th use) once `freed[i] >= T/4`.
    freed: [Pad; 4],
    cmd: AtomicU8,
    shutdown: AtomicBool,
    /// Reset payload + ack channel (count first, then turn — see doc).
    reset_session: UnsafeCell<[u8; 10]>,
    reset_cnt: AtomicU64,
    reset_ack_turn: AtomicU64,
    reset_ack_cnt: AtomicU64,
}

// SAFETY: the mailbox is the SPSC handoff described in the module doc —
// each buffer is owned by exactly one side at a time, transfers are
// ordered by the Release/Acquire turn counters, and the command/reset
// cells follow the documented handshake.
unsafe impl Send for Mailbox {}
unsafe impl Sync for Mailbox {}

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
    // RX-local scratch batch (poll writes slots here; the transform below
    // re-reads them from this core's L1).
    let mut scratch = FrameBatch::new();
    let mut turn: u64 = 0;
    let mut served_resets: u64 = 0;
    loop {
        match mb.cmd.load(Ordering::Acquire) {
            CMD_SHUTDOWN => return,
            CMD_RESET => {
                // SAFETY: the consumer is parked in reset() holding no
                // entries; the RX thread owns the transport and blob.
                let sess = unsafe { *mb.reset_session.get() };
                inner.reset(sess);
                served_resets += 1;
                mb.reset_ack_turn.store(turn, Ordering::Release);
                mb.reset_ack_cnt.store(served_resets, Ordering::Release);
                mb.cmd.store(CMD_RUN, Ordering::Release);
            }
            _ => {}
        }
        if mb.shutdown.load(Ordering::Acquire) {
            return;
        }
        let i = (turn & 3) as usize;
        // Buffer i is free for `turn` (its (turn/4 + 1)-th use) once the
        // consumer freed it turn/4 times.
        if turn >= 4 {
            let needed = turn / 4;
            let mut backoff = 0u32;
            while mb.freed[i].load(Ordering::Acquire) < needed {
                if mb.shutdown.load(Ordering::Acquire) {
                    return;
                }
                if mb.cmd.load(Ordering::Acquire) == CMD_RESET {
                    break; // serve the reset first (consumer is waiting)
                }
                spin(&mut backoff);
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
        const ENTRY_CAP: usize = 1024;
        let mut acc = 0usize;
        let mut eos = false;
        while acc + 256 <= ENTRY_CAP {
            let tp0 = if diag { Some(std::time::Instant::now()) } else { None };
            let n = inner.poll(&mut scratch);
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
                        memo: (blk_count != 0).then_some(nf_protocol::packet::FrameMemo {
                            valid_count: valid,
                        }),
                        first_seq: f.first_seq,
                        sess_lo: f.sess_lo,
                        sess_hi: f.sess_hi,
                    };
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
        mb.filled[i].store(turn, Ordering::Release);
        turn += 1;
        if eos {
            // The end-of-stream marker is its OWN (empty) publication —
            // the consumer's next_batch returns false on it. Without it
            // the consumer would spin on a turn that never comes.
            let j = (turn & 3) as usize;
            if turn >= 4 {
                let needed = turn / 4;
                let mut backoff = 0u32;
                while mb.freed[j].load(Ordering::Acquire) < needed {
                    if mb.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    spin(&mut backoff);
                }
            }
            // SAFETY: RX owns buffer j for this turn until the Release
            // store below; len = 0 needs no entry writes.
            unsafe {
                (*mb.bufs[j].get()).len = 0;
            }
            mb.filled[j].store(turn, Ordering::Release);
            turn += 1;
        }
        let n = if eos { 0 } else { acc };
        if n == 0 {
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
            // End of stream: keep serving resets until one arrives (the
            // consumer's next action after EOS is reset or drop).
            let mut backoff = 0u32;
            loop {
                match mb.cmd.load(Ordering::Acquire) {
                    CMD_SHUTDOWN => return,
                    CMD_RESET => {
                        let sess = unsafe { *mb.reset_session.get() };
                        inner.reset(sess);
                        served_resets += 1;
                        mb.reset_ack_turn.store(turn, Ordering::Release);
                        mb.reset_ack_cnt.store(served_resets, Ordering::Release);
                        mb.cmd.store(CMD_RUN, Ordering::Release);
                        break; // resume polling the fresh pass
                    }
                    _ => {}
                }
                if mb.shutdown.load(Ordering::Acquire) {
                    return;
                }
                spin(&mut backoff);
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
        let mut inner = ReplayTransport::new(gt, schedule, session);
        inner.set_poll_coalesce(coalesce);
        let triples = inner.shared_triples();
        let _ = triples;
        #[allow(clippy::disallowed_types)]
        let mb: Arc<Mailbox> = Arc::new(Mailbox {
            bufs: [
                UnsafeCell::new(EntryBuf::new()),
                UnsafeCell::new(EntryBuf::new()),
                UnsafeCell::new(EntryBuf::new()),
                UnsafeCell::new(EntryBuf::new()),
            ],
            filled: [Pad::never(), Pad::never(), Pad::never(), Pad::never()],
            freed: [Pad::zeroed(), Pad::zeroed(), Pad::zeroed(), Pad::zeroed()],
            cmd: AtomicU8::new(CMD_RUN),
            shutdown: AtomicBool::new(false),
            reset_session: UnsafeCell::new([0u8; 10]),
            reset_cnt: AtomicU64::new(0),
            reset_ack_turn: AtomicU64::new(0),
            reset_ack_cnt: AtomicU64::new(0),
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
    pub fn reset(&mut self, session: [u8; 10]) {
        if let Some(t) = self.cur.take() {
            self.mb.freed[(t & 3) as usize].fetch_add(1, Ordering::Release);
        }
        self.resets += 1;
        let n = self.resets;
        // SAFETY: consumer-exclusive cell; the RX thread reads it only
        // after our Release store of the command.
        unsafe {
            *self.mb.reset_session.get() = session;
        }
        self.mb.reset_cnt.store(n, Ordering::Release);
        self.mb.cmd.store(CMD_RESET, Ordering::Release);
        let mut backoff = 0u32;
        loop {
            if self.mb.reset_ack_cnt.load(Ordering::Acquire) >= n {
                break;
            }
            spin(&mut backoff);
        }
        // The RX's turn AFTER the reset (it already published its in-flight
        // batch before serving the command).
        let ack_turn = self.mb.reset_ack_turn.load(Ordering::Acquire);
        // Free every publication the RX made but we never consumed
        // (pre-reset frames): turns [self.turn, ack_turn) were ALL
        // published (the RX acks only after its in-flight publication
        // lands, and it never skips turns).
        for _t in self.turn..ack_turn {
            self.mb.freed[(_t & 3) as usize].fetch_add(1, Ordering::Release);
        }
        self.turn = ack_turn;
    }

    /// Advance to the next batch. Returns false at end of stream (the
    /// harness's poll()==0 equivalent). The entry slice + clock stay valid
    /// until the next call.
    pub fn next_batch(&mut self) -> bool {
        if let Some(t) = self.cur.take() {
            self.mb.freed[(t & 3) as usize].fetch_add(1, Ordering::Release);
        }
        let i = (self.turn & 3) as usize;
        // Tight acquire spin: the consumer is the pipeline's critical
        // thread — a paused-backoff here measurably lags a fast producer.
        while self.mb.filled[i].load(Ordering::Acquire) != self.turn {
            std::hint::spin_loop();
        }
        let t = self.turn;
        self.turn += 1;
        // SAFETY: consumer owns buffer i for turn t; len was published by
        // the RX's Release store (ordered by the Acquire above).
        let len = unsafe { (*self.mb.bufs[i].get()).len };
        if len == 0 {
            // EOS: the empty publication occupies a turn — free it to keep
            // the use counts symmetric for the next pass.
            self.mb.freed[i].fetch_add(1, Ordering::Release);
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
            let buf = &*self.mb.bufs[(t & 3) as usize].get();
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
        unsafe { (*self.mb.bufs[(t & 3) as usize].get()).clock }
    }
}

impl Drop for PipelinedReplayTransport {
    fn drop(&mut self) {
        self.mb.cmd.store(CMD_SHUTDOWN, Ordering::Release);
        self.mb.shutdown.store(true, Ordering::Release);
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
}
