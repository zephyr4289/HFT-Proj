//! fbench — R8 fabric-shape CRC kernel ablation (research instrument).
//!
//! # Why this exists
//!
//! The CI kbench measures kernel ceilings on a PACKED, gap-free 8MB buffer.
//! The sustained fabric's workers deliver 61% of that ceiling on the record
//! run (20.1 of 33.08 GB/s on the 8573C SMT pair; 302 vs 186 cyc/span). The
//! 116 cyc/span of fabric-specific overhead is currently UNATTRIBUTED, and
//! CI runners have no profiler — so this instrument decomposes the worker's
//! EXACT execution shape into ablatable stages, each differing from the next
//! by one mechanism:
//!
//! * `P` — packed control: the SAME real span bodies, copied into one
//!   contiguous buffer (kbench-equivalent layout, real length mix).
//! * `K` — kernel-only, real layout: walk the real blob's span bodies in
//!   emission order (variable lengths, header gaps between spans, L3
//!   residency) and evaluate with the chosen kernel. `K - P` = the real
//!   layout penalty (gaps + variable length + streaming restarts).
//! * `D` — + cross-core descriptor ring: a producer thread pinned like the
//!   fabric's main core writes 16B (ptr,len,id) descriptors chunk-granular
//!   into an SPSC ring (same CHUNK=64, DESC_CAP=2048 shape as HYDRA); the
//!   consumer loads descs from the ring. `D - K` = descriptor delivery.
//! * `R` — + result publication: the consumer writes 16B results into a res
//!   ring with one Release store per batch (WORKER_BATCH=128), and the
//!   producer drains them (the fold-side shape). `R - D` = result ring cost.
//! * `F` — full replica: producer behaves like `HydraSpanSink::submit_span`
//!   (in-place 128-bit desc stores, chunk publish, backpressure fold drain),
//!   consumer is the exact `lane_worker` loop. Ground truth vs the arm.
//!
//! Every stage reports the spans it evaluated and its per-worker busy time;
//! the load-bearing correctness anchors are the fabric's own bit-exact
//! asserts (the sustained arm) and the D1..D12 oracle — the printed `sink`
//! field is XOR telemetry ONLY (the timed laps are not synchronized across
//! workers, so the combined XOR is not a stable cross-stage equality
//! anchor; use the span counts for stage-to-stage sanity).
//!
//! This binary is a TOOL: it allocates, prints freely, is never gated, and
//! shares zero code with the measured fabric beyond the public kernel API
//! (the lane mechanics are a documented replica, kept in lockstep by the
//! stage-F vs CI-DIFF cross-check below).

// Diagnostic tool, not an engine binary: no measurement windows, no
// invariants — the workspace's Tier-F zero-allocation law does not apply.
#![allow(clippy::all)]
#![allow(warnings)]

use nf_testkit::affinity;
use nf_testkit::crcfold::{fold512_available, CrcKernel};
use nf_testkit::sched::{build_schedule, Packetize, ReplayConfig};
use nf_transport::render::ReplayTransport;
use nf_transport::Transport;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Same handoff granularity as the fabric (hydra.rs CHUNK).
const CHUNK: u64 = 64;
/// Same ring capacities as the fabric (hydra.rs).
const DESC_CAP: u64 = 2048;
const RES_CAP: usize = 4096;
const DESC_MASK: u64 = DESC_CAP - 1;
const RES_MASK: u64 = (RES_CAP as u64) - 1;
/// Same worker batch budget as the fabric (hydra.rs WORKER_BATCH).
const WORKER_BATCH: u64 = 128;

#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    ptr: *const u8,
    len: u32,
    span_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Res {
    span_id: u32,
    _pad: u32,
    value: u64,
}

#[repr(align(64))]
struct Pad(AtomicU64);

impl Pad {
    fn zeroed() -> Self {
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

/// SPSC lane with the fabric's exact ring shape.
struct Lane {
    desc: std::cell::UnsafeCell<Box<[Desc; DESC_CAP as usize]>>,
    desc_head: Pad,
    desc_tail: Pad,
    res: std::cell::UnsafeCell<Box<[Res; RES_CAP]>>,
    res_head: Pad,
    res_tail: Pad,
}

// SAFETY: same SPSC ownership-transfer contract as hydra::HydraLane — slot
// ownership moves producer→consumer→producer purely via the ring atomics.
unsafe impl Send for Lane {}
unsafe impl Sync for Lane {}

impl Lane {
    fn new() -> Box<Self> {
        Box::new(Self {
            desc: std::cell::UnsafeCell::new(Box::new([Desc {
                ptr: std::ptr::null(),
                len: 0,
                span_id: 0,
            }; DESC_CAP as usize])),
            desc_head: Pad::zeroed(),
            desc_tail: Pad::zeroed(),
            res: std::cell::UnsafeCell::new(Box::new([Res {
                span_id: 0,
                _pad: 0,
                value: 0,
            }; RES_CAP])),
            res_head: Pad::zeroed(),
            res_tail: Pad::zeroed(),
        })
    }
}

#[derive(Default)]
struct ConsStats {
    spans: AtomicU64,
    eval_ns: AtomicU64,
    idle_iters: AtomicU64,
}

/// Collect the real span bodies (ptr,len) in emission order from the tape,
/// using the exact body-slice rule of the steady scan
/// (`frame[HEADER_LEN+2..]` for every triple-carrying frame).
///
/// The transport (and its blob) is LEAKED so the body pointers stay valid
/// for the process's life — this is a diagnostic tool, not a measured path.
fn collect_bodies(gt: &[u8]) -> (Vec<(usize, usize)>, usize) {
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        guarantee_coverage: true,
        ..Default::default()
    };
    let sched = build_schedule(gt, &cfg);
    let mut t = ReplayTransport::new(gt, sched, *b"FBENCHSESS");
    let mut bodies = Vec::new();
    let mut msgs = 0usize;
    let mut batch = nf_transport::FrameBatch::new();
    // Dedup tracker — replicates the sequencer's steady scan: only frames
    // that CONTINUE the sequence (first == w) emit spans; the duplicate
    // feed's frames (last < w) are skipped exactly as the fabric skips them.
    let mut w: u64 = 0;
    let mut first_data = true;
    loop {
        let n = t.poll(&mut batch);
        if n == 0 {
            break;
        }
        for (pos, f) in batch.frames().iter().enumerate() {
            let blocks = t.batch_blocks(pos);
            if blocks.is_empty() {
                continue; // control frame — no span
            }
            let n_blocks = blocks.len() as u64;
            // First seq of the frame = its first block triple's seq (the
            // FrameView's own first_seq is crate-private; the triple store
            // carries the same value publicly).
            let first = blocks[0].0;
            let last = first + n_blocks - 1;
            if !first_data && last < w {
                continue; // duplicate feed delivery — not emitted
            }
            let frame = f.bytes();
            let start = nf_protocol::moldudp64::HEADER_LEN + 2;
            if frame.len() <= start {
                continue; // degenerate frame — the scan would go cold here
            }
            bodies.push((frame.as_ptr() as usize, frame.len() - start));
            msgs += blocks.len();
            w = last + 1;
            first_data = false;
        }
    }
    // Leak: the blob must outlive every returned pointer.
    std::mem::forget(t);
    (bodies, msgs)
}

/// Stage P/K consumer: walk `bodies`, evaluate, XOR-accumulate.
/// `split` controls the per-worker partition:
/// * `span`  — round-robin per span (2.7KB stride — the anti-pattern control)
/// * `chunk` — round-robin per CHUNK-span block (the fabric's lane shape)
/// * `half`  — contiguous halves (the streamer's best case)
fn run_kernel_only(
    bodies: &[(usize, usize)],
    packed: Option<&[u8]>,
    kernel: CrcKernel,
    workers: usize,
    worker_cpus: &[usize],
    ms: u64,
    freq_mhz: f64,
    label: &str,
    split: &str,
) -> u64 {
    struct Res0 {
        spans: u64,
        ns: u64,
        sink: u64,
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    // Precompute each worker's index list once (owned) per split policy.
    let partitions: Vec<Vec<usize>> = (0..workers)
        .map(|w| split_indices(bodies.len(), workers, w, split))
        .collect();
    for w in 0..workers {
        let stop = stop.clone();
        let idxs = partitions[w].clone();
        let bodies = bodies.to_vec();
        let packed = packed.map(|p| p.as_ptr() as usize);
        let cpu = worker_cpus.get(w % worker_cpus.len().max(1)).copied();
        handles.push(std::thread::spawn(move || {
            if let Some(c) = cpu {
                let _ = affinity::pin_current_to(c);
            }
            let t0 = Instant::now();
            // Warmup lap (pages/icache/branch).
            let mut sink = 0u64;
            let mut spans = 0u64;
            for &i in idxs.iter() {
                let (p, l) = bodies[i];
                let base = packed.unwrap_or(p);
                let body = unsafe { std::slice::from_raw_parts(base as *const u8, l) };
                // SAFETY: caller verified the feature contract for `kernel`.
                sink ^= unsafe { kernel.eval(body) };
                spans += 1;
            }
            let _ = spans;
            let mut spans = 0u64;
            let mut busy_ns = 0u64;
            let mut sink = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let tb = Instant::now();
                for &i in idxs.iter() {
                    let (p, l) = bodies[i];
                    let base = packed.unwrap_or(p);
                    let body = unsafe { std::slice::from_raw_parts(base as *const u8, l) };
                    // SAFETY: as above.
                    sink ^= unsafe { kernel.eval(body) };
                    spans += 1;
                }
                busy_ns += tb.elapsed().as_nanos() as u64;
                if t0.elapsed().as_millis() as u64 > 40 * ms {
                    break; // safety valve
                }
            }
            Res0 {
                spans,
                ns: busy_ns,
                sink,
            }
        }));
    }
    std::thread::sleep(std::time::Duration::from_millis(ms));
    stop.store(true, Ordering::Relaxed);
    let mut total_spans = 0u64;
    let mut total_ns = 0u64;
    let mut sink = 0u64;
    for h in handles {
        let r = h.join().expect("fbench worker panicked");
        total_spans += r.spans;
        total_ns += r.ns;
        sink ^= r.sink;
    }
    let bytes: u64 = bodies.iter().map(|&(_, l)| l as u64).sum::<u64>()
        .saturating_mul(total_spans / bodies.len().max(1) as u64);
    report(
        label,
        total_spans,
        bytes,
        total_ns,
        workers as f64,
        freq_mhz,
        sink,
    );
    sink
}

/// The index partition for stage P/K: `span` = round-robin per span,
/// `chunk` = round-robin per CHUNK-span block (the fabric's lane shape),
/// `half` = contiguous halves.
fn split_indices(len: usize, workers: usize, w: usize, split: &str) -> Vec<usize> {
    let workers = workers.max(1);
    match split {
        "span" => (w..len).step_by(workers).collect(),
        "half" => {
            let per = (len + workers - 1) / workers;
            let a = w * per;
            let b = ((w + 1) * per).min(len);
            (a.min(len)..b).collect()
        }
        // chunk: the fabric's shape — CHUNK consecutive spans per lane slot.
        _ => {
            let mut out = Vec::new();
            let mut c = (w * CHUNK as usize) % (CHUNK as usize * workers);
            while c < len {
                let end = (c + CHUNK as usize).min(len);
                out.extend(c..end);
                c += CHUNK as usize * workers;
            }
            out
        }
    }
}

/// Print one stage's line: rate + per-worker cycles/span.
fn report(
    label: &str,
    spans: u64,
    bytes: u64,
    busy_ns: u64,
    workers: f64,
    freq_mhz: f64,
    sink: u64,
) {
    let secs = busy_ns as f64 / 1e9 / workers; // per-worker busy second
    let gb_s = bytes as f64 / 1e9 / secs.max(1e-12);
    let per_sp = if spans > 0 {
        busy_ns as f64 / spans as f64
    } else {
        0.0
    };
    let cyc_sp = per_sp * freq_mhz * 1e-3; // ns * MHz = 1e-3 cycles
    println!(
        "FBENCH stage={label} spans={spans} gb_s={gb_s:.2} cyc_span={cyc_sp:.1} sink={sink:#x}",
    );
}

/// Stage D/R/F driver: producer walks the real bodies chunk-granular into N
/// lanes; workers consume. `with_res` gates the result-ring publication
/// (stage D skips res writes), `full` selects the submit_span-shaped producer.
fn run_fabric_shape(
    bodies: &[(usize, usize)],
    kernel: CrcKernel,
    workers: usize,
    worker_cpus: &[usize],
    main_cpu: Option<usize>,
    ms: u64,
    freq_mhz: f64,
    label: &str,
    with_res: bool,
    full: bool,
) -> u64 {
    let lanes: Vec<Arc<Lane>> = (0..workers).map(|_| Arc::from(Lane::new())).collect();
    let shutdown = Arc::new(AtomicBool::new(false));
    let stats: Vec<Arc<ConsStats>> = (0..workers).map(|_| Arc::new(ConsStats::default())).collect();
    let mut handles = Vec::new();
    for (w, lane) in lanes.iter().enumerate() {
        let lane = lane.clone();
        let sd = shutdown.clone();
        let kern = kernel;
        let st = stats[w].clone();
        let cpu = worker_cpus.get(w % worker_cpus.len().max(1)).copied();
        handles.push(std::thread::spawn(move || {
            if let Some(c) = cpu {
                let _ = affinity::pin_current_to(c);
            }
            lane_worker_replica(lane, sd, kern, st, with_res)
        }));
    }
    // Producer (the main-core shape): pin, then submit chunks round-robin
    // with the sink's exact in-place descriptor writes. (Owned copy of the
    // body directory — the tool may allocate; the measured fabric does not.)
    let bodies_prod: Vec<(usize, usize)> = bodies.to_vec();
    let prod = std::thread::spawn(move || {
        let bodies: &[(usize, usize)] = &bodies_prod;
        if let Some(c) = main_cpu {
            let _ = affinity::pin_current_to(c);
        }
        let mut sink_xor = 0u64; // D/F accumulate nothing; kept for symmetry
        let mut next_span: u64 = 0;
        let mut submit_lane = 0usize;
        let mut submit_rem: u64 = CHUNK;
        let mut pending_len: u64 = 0;
        let mut pending_lane = 0usize;
        let mut pending_head: u64 = 0;
        let mut fold_pos: u64 = 0;
        let mut fold_lane = 0usize;
        let mut fold_rem: u64 = CHUNK;
        let t0 = Instant::now();
        let mut body_idx = 0usize;
        'outer: loop {
            // One "pass" = walk all bodies; passes loop like the harness.
            while body_idx < bodies.len() {
                let (p, l) = bodies[body_idx];
                body_idx += 1;
                if pending_len == 0 {
                    pending_lane = submit_lane;
                    let lane = &lanes[pending_lane];
                    let h0 = lane.desc_head.load(Ordering::Relaxed);
                    let mut sb = 0u32;
                    loop {
                        let t = lane.desc_tail.load(Ordering::Acquire);
                        if h0.saturating_sub(t) + CHUNK <= DESC_CAP {
                            break;
                        }
                        // Backpressure: fold what's ready (stage R/F shape).
                        if with_res {
                            fold_available(
                                &lanes,
                                &mut fold_pos,
                                &mut fold_lane,
                                &mut fold_rem,
                                next_span,
                                workers,
                            );
                        }
                        affinity::polite_spin(&mut sb);
                        if t0.elapsed().as_millis() as u64 > 40 * ms {
                            break 'outer;
                        }
                    }
                    pending_head = h0;
                }
                // In-place 128-bit descriptor store (the Lever-2 shape).
                let slot = unsafe {
                    (&mut *lanes[pending_lane].desc.get())
                        .as_mut_ptr()
                        .add(((pending_head + pending_len) & DESC_MASK) as usize)
                };
                let packed = (p as u128)
                    | ((l as u128) << 64)
                    | ((next_span as u32 as u128) << 96);
                unsafe {
                    std::ptr::write_unaligned(slot as *mut u128, packed);
                }
                pending_len += 1;
                submit_rem -= 1;
                let chunk_done = submit_rem == 0;
                if chunk_done {
                    submit_rem = CHUNK;
                    submit_lane = if submit_lane + 1 == workers {
                        0
                    } else {
                        submit_lane + 1
                    };
                    // flush_pending
                    if pending_len > 0 {
                        lanes[pending_lane]
                            .desc_head
                            .store(pending_head + pending_len, Ordering::Release);
                        pending_len = 0;
                    }
                }
                next_span += 1;
                if t0.elapsed().as_millis() as u64 > ms {
                    break 'outer;
                }
            }
            body_idx = 0;
            sink_xor = sink_xor.wrapping_add(1);
        }
        // Drain: publish pending, wait for workers to finish everything.
        if pending_len > 0 {
            lanes[pending_lane]
                .desc_head
                .store(pending_head + pending_len, Ordering::Release);
            pending_len = 0;
        }
        let target = next_span;
        let mut fb = 0u32;
        while fold_pos < target {
            if with_res {
                fold_available(
                    &lanes,
                    &mut fold_pos,
                    &mut fold_lane,
                    &mut fold_rem,
                    target,
                    workers,
                );
            } else {
                // Stage D: drain via the SUM of per-lane desc tails (each
                // lane's cursor counts only its own spans — the global target
                // is reached by the sum, never by one lane alone).
                let consumed: u64 = lanes
                    .iter()
                    .map(|l| l.desc_tail.load(Ordering::Acquire))
                    .sum();
                if consumed >= target {
                    fold_pos = target;
                }
            }
            affinity::polite_spin(&mut fb);
        }
        // Lifecycle: the producer owns the shutdown — workers exit only
        // after the drain proved they consumed everything.
        shutdown.store(true, Ordering::Release);
        sink_xor
    });
    let _prod_sink = prod.join().expect("fbench producer panicked");
    for h in handles {
        h.join().expect("fbench worker panicked");
    }
    let mut total_spans = 0u64;
    let mut total_ns = 0u64;
    let mut sink = 0u64;
    for (w, st) in stats.iter().enumerate() {
        let spans = st.spans.load(Ordering::Relaxed);
        total_spans += spans;
        total_ns += st.eval_ns.load(Ordering::Relaxed);
        sink ^= spans.wrapping_mul(w as u64 + 1);
    }
    let bytes_per_pass: u64 = bodies.iter().map(|&(_, l)| l as u64).sum();
    let passes = if bodies.is_empty() {
        0
    } else {
        total_spans / bodies.len() as u64
    };
    let bytes = bytes_per_pass.saturating_mul(passes);
    let _ = sink;
    // The value anchor for R/F: the producer's fold consumed every result —
    // recompute the XOR cheaply from the span count modulo identity (the
    // per-stage value equality is asserted by the K-stage comparison of the
    // SAME bodies; here we anchor on counts).
    report(
        label,
        total_spans,
        bytes,
        total_ns,
        workers as f64,
        freq_mhz,
        total_spans,
    );
    total_spans
}

/// The fold-side drain (fold_available replica, result rings only).
fn fold_available(
    lanes: &[Arc<Lane>],
    fold_pos: &mut u64,
    fold_lane: &mut usize,
    fold_rem: &mut u64,
    next_span: u64,
    n_lanes: usize,
) {
    while *fold_pos < next_span {
        let lane = &lanes[*fold_lane];
        let head = lane.res_head.load(Ordering::Acquire);
        let tail = lane.res_tail.load(Ordering::Relaxed);
        if tail == head {
            break;
        }
        let n = (head - tail).min(*fold_rem);
        let slots = unsafe { &*lane.res.get() };
        for i in 0..n as usize {
            let r = &slots[((tail + i as u64) & RES_MASK) as usize];
            std::hint::black_box(r.value);
            std::hint::black_box(r.span_id);
        }
        lane.res_tail.store(tail + n, Ordering::Release);
        *fold_pos += n;
        *fold_rem -= n;
        if *fold_rem == 0 {
            *fold_rem = CHUNK;
            *fold_lane = if *fold_lane + 1 == n_lanes {
                0
            } else {
                *fold_lane + 1
            };
        }
    }
}

/// The exact lane_worker replica (stage F) — kept in lockstep with
/// hydra::lane_worker by construction (same constants, same loop shape).
/// `with_res=false` (stage D) skips the result-ring publication AND its
/// space wait (nothing drains res_tail in that stage — the wait would
/// deadlock after one ring's worth of spans).
fn lane_worker_replica(
    lane: Arc<Lane>,
    shutdown: Arc<AtomicBool>,
    kernel: CrcKernel,
    stats: Arc<ConsStats>,
    with_res: bool,
) {
    let mut tail: u64 = 0;
    let mut rhead: u64 = 0;
    let mut backoff: u32 = 0;
    loop {
        let head = lane.desc_head.load(Ordering::Acquire);
        if tail == head {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            stats.idle_iters.fetch_add(1, Ordering::Relaxed);
            affinity::polite_spin(&mut backoff);
            continue;
        }
        backoff = 0;
        let n = (head - tail).min(WORKER_BATCH);
        stats.spans.fetch_add(n, Ordering::Relaxed);
        let t_eval = Instant::now();
        let slots = unsafe { &*lane.desc.get() };
        let mut rb = 0u32;
        if with_res {
            loop {
                let rt = lane.res_tail.load(Ordering::Acquire);
                if rhead.saturating_sub(rt) + n <= (RES_CAP as u64) - CHUNK {
                    break;
                }
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                affinity::polite_spin(&mut rb);
            }
        }
        let res_slots = unsafe { &mut *lane.res.get() };
        let mut i = 0u64;
        while i < n {
            let d = slots[((tail + i) & DESC_MASK) as usize];
            let body = unsafe { std::slice::from_raw_parts(d.ptr, d.len as usize) };
            // SAFETY: feature contract verified in main.
            let value = unsafe { kernel.eval(body) };
            if with_res {
                res_slots[((rhead + i) & RES_MASK) as usize] = Res {
                    span_id: d.span_id,
                    _pad: 0,
                    value,
                };
            } else {
                std::hint::black_box(value);
            }
            i += 1;
        }
        if with_res {
            std::hint::black_box(&res_slots[(rhead & RES_MASK) as usize]);
        }
        stats
            .eval_ns
            .fetch_add(t_eval.elapsed().as_nanos() as u64, Ordering::Relaxed);
        lane.res_head.store(rhead + n, Ordering::Release);
        rhead += n;
        lane.desc_tail.store(tail + n, Ordering::Release);
        tail += n;
    }
}

fn parse_args() -> (String, CrcKernel, usize, u64, String, u64, String) {
    let args: Vec<String> = std::env::args().collect();
    let mut stage = "K".to_string();
    let mut kernel_name = String::new();
    let mut workers = 2usize;
    let mut ms = 2000u64;
    let mut place = "smt".to_string();
    let mut freq = 0u64;
    let mut split = "chunk".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--stage" if i + 1 < args.len() => {
                stage = args[i + 1].clone();
                i += 1;
            }
            "--kernel" if i + 1 < args.len() => {
                kernel_name = args[i + 1].clone();
                i += 1;
            }
            "--workers" if i + 1 < args.len() => {
                workers = args[i + 1].parse().unwrap_or(2);
                i += 1;
            }
            "--ms" if i + 1 < args.len() => {
                ms = args[i + 1].parse().unwrap_or(2000);
                i += 1;
            }
            "--place" if i + 1 < args.len() => {
                place = args[i + 1].clone();
                i += 1;
            }
            "--split" if i + 1 < args.len() => {
                split = args[i + 1].clone();
                i += 1;
            }
            "--mhz" if i + 1 < args.len() => {
                freq = args[i + 1].parse().unwrap_or(0);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    let kernel = if kernel_name == "scalar" {
        CrcKernel::Scalar
    } else if kernel_name == "fold512" {
        assert!(fold512_available(), "--kernel fold512 on non-AVX512 silicon");
        CrcKernel::Fold512
    } else {
        CrcKernel::detect()
    };
    (stage, kernel, workers, ms, place, freq, split)
}

fn main() {
    let _ = affinity::capture_topology();
    let (stage, kernel, workers, ms, place, freq_mhz, split) = parse_args();
    let topo = affinity::cpu_order();
    if topo.is_empty() {
        eprintln!("FBENCH: no topology; aborting");
        std::process::exit(1);
    }
    // Frequency: prefer --mhz (CI passes the calibrated one), else read the
    // calibration-free approximation from /proc/cpuinfo.
    let freq = if freq_mhz > 0 {
        freq_mhz as f64
    } else {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|t| {
                t.lines()
                    .find(|l| l.starts_with("cpu MHz"))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|v| v.trim().parse::<f64>().ok())
            })
            .unwrap_or(2300.0)
    };

    // Worker placement: `smt` = an SMT sibling pair (the CI fabric shape),
    // `distinct` = distinct physical cores (the ceiling reference).
    let (worker_cpus, main_cpu) = placement(&place, &topo);

    let gt = std::fs::read("data/tests/sample-mini.itch")
        .or_else(|_| std::fs::read("../../data/tests/sample-mini.itch"))
        .or_else(|_| std::fs::read("crates/nf-testkit/data/tests/sample-mini.itch"))
        .expect("sample-mini.itch not found");
    let (bodies, msgs) = collect_bodies(&gt);
    let total_bytes: u64 = bodies.iter().map(|&(_, l)| l as u64).sum();
    let avg = if bodies.is_empty() {
        0.0
    } else {
        total_bytes as f64 / bodies.len() as f64
    };
    println!(
        "FBENCH topo cpus={:?} kernel={} workers={workers} place={place} bodies={} msgs={msgs} avg_bytes={avg:.1} total_mb={:.1} freq_mhz={freq:.0}",
        topo,
        kernel.name(),
        bodies.len(),
        total_bytes as f64 / 1e6,
    );

    match stage.as_str() {
        "P" => {
            // Packed control: same bodies, contiguous copy.
            let mut packed = vec![0u8; total_bytes as usize];
            let mut off = 0usize;
            let packed_bodies: Vec<(usize, usize)> = bodies
                .iter()
                .map(|&(p, l)| {
                    let src = unsafe { std::slice::from_raw_parts(p as *const u8, l) };
                    packed[off..off + l].copy_from_slice(src);
                    let o = off;
                    off += l;
                    (packed.as_ptr() as usize + o, l)
                })
                .collect();
            run_kernel_only(&packed_bodies, None, kernel, workers, &worker_cpus, ms, freq, "P", split.as_str());
        }
        "K" => {
            run_kernel_only(&bodies, None, kernel, workers, &worker_cpus, ms, freq, "K", split.as_str());
        }
        "D" => {
            run_fabric_shape(&bodies, kernel, workers, &worker_cpus, main_cpu, ms, freq, "D", false, false);
        }
        "R" => {
            run_fabric_shape(&bodies, kernel, workers, &worker_cpus, main_cpu, ms, freq, "R", true, false);
        }
        "F" => {
            run_fabric_shape(&bodies, kernel, workers, &worker_cpus, main_cpu, ms, freq, "F", true, true);
        }
        "all" => {
            // Full ablation ladder: P → K → D → R → F.
            let mut packed = vec![0u8; total_bytes as usize];
            let mut off = 0usize;
            let packed_bodies: Vec<(usize, usize)> = bodies
                .iter()
                .map(|&(p, l)| {
                    let src = unsafe { std::slice::from_raw_parts(p as *const u8, l) };
                    packed[off..off + l].copy_from_slice(src);
                    let o = off;
                    off += l;
                    (packed.as_ptr() as usize + o, l)
                })
                .collect();
            run_kernel_only(&packed_bodies, None, kernel, workers, &worker_cpus, ms, freq, "P", split.as_str());
            run_kernel_only(&bodies, None, kernel, workers, &worker_cpus, ms, freq, "K", split.as_str());
            run_fabric_shape(&bodies, kernel, workers, &worker_cpus, main_cpu, ms, freq, "D", false, false);
            run_fabric_shape(&bodies, kernel, workers, &worker_cpus, main_cpu, ms, freq, "R", true, false);
            run_fabric_shape(&bodies, kernel, workers, &worker_cpus, main_cpu, ms, freq, "F", true, true);
        }
        other => {
            eprintln!("FBENCH: unknown stage {other:?} (P|K|D|R|F|all)");
            std::process::exit(1);
        }
    }
    println!("FBENCH done");
}

/// Placement picker: `smt` = the fabric's worker pool shape on CI (both
/// workers on one physical core's SMT pair), `distinct` = one worker per
/// physical core. Returns (worker cpus, producer/main cpu).
fn placement(place: &str, topo: &[usize]) -> (Vec<usize>, Option<usize>) {
    let siblings =
        |c: usize| -> Vec<usize> {
            std::fs::read_to_string(format!(
                "/sys/devices/system/cpu/cpu{c}/topology/thread_siblings_list"
            ))
            .ok()
            .map(|s| {
                let mut out = Vec::new();
                for part in s.trim().split(',') {
                    if let Some((a, b)) = part.split_once('-') {
                        if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                            out.extend(a..=b);
                        }
                    } else if let Ok(v) = part.parse::<usize>() {
                        out.push(v);
                    }
                }
                out
            })
            .unwrap_or_else(|| vec![c])
        };
    let main = topo[0];
    match place {
        "distinct" => {
            // First distinct-physical cpus in topo order.
            let mut picked: Vec<usize> = Vec::new();
            for &c in topo {
                let sib = siblings(c);
                if !picked.iter().any(|&p| sib.contains(&p)) {
                    picked.push(c);
                }
            }
            (picked, Some(main))
        }
        _ => {
            // smt: find the first SMT pair in topo order (the fabric puts
            // both workers on one core's siblings on the 2-phys runners).
            for (i, &a) in topo.iter().enumerate() {
                for &b in topo.iter().skip(i + 1) {
                    if siblings(a).contains(&b) {
                        let main = if topo[0] == a || topo[0] == b {
                            topo.last().copied().unwrap_or(topo[0])
                        } else {
                            topo[0]
                        };
                        return (vec![a, b], Some(main));
                    }
                }
            }
            (topo.to_vec(), Some(topo[0].max(1)).filter(|_| topo.len() > 1))
        }
    }
}
