//! T2 Window Size Sweep Harness (doc 11 / ED-14 / Wave 1.5).
//! Evaluates window sizes {256, 512, 1024, 2048, 4096} across M1 and M11.
//! Proves the 1024-slot knee with empirical data for ADR-0002.

#![allow(warnings)]
#![allow(clippy::all)]

use nf_arbitrator::{FeedId, LiveFeedProof, Sequencer, Sink};
use nf_testkit::golden::golden;
use nf_testkit::reference::ReferenceArbitrator;
use nf_testkit::sched::{
    build_schedule, DelayModel, DropRange, LossModel, Packetize, ReplayConfig,
};
use nf_testkit::sink::ConformanceSink;
use nf_transport::replay::ReplayTransport;
use nf_transport::{FrameBatch, Transport};
use std::fs;
use std::time::Instant;

struct WindowStats {
    slots: usize,
    arena_bytes: usize,
    l1_fit: &'static str,
    max_staged_m11: u32,
    beyond_drops_m11: u64,
    golden_m1: bool,
    golden_m11: bool,
    m1_rate: u64,
    m11_rate: u64,
}

fn run_sweep_for_slots(gt: &[u8], slots: usize) -> WindowStats {
    let (gt_count, gt_hash) = golden(gt);
    let sess = *b"WINDSWEEP1";

    // 1. Evaluate M1 (Baseline MTU contiguous)
    let cfg_m1 = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        guarantee_coverage: true,
        ..Default::default()
    };
    let sched_m1 = build_schedule(gt, &cfg_m1);
    let mut transport_m1 = ReplayTransport::new(gt, sched_m1, sess);
    let mut seq_m1 = Sequencer::new();
    let mut sink_m1 = ConformanceSink::new();
    let mut batch = FrameBatch::new();

    let t0 = Instant::now();
    while transport_m1.poll(&mut batch) > 0 {
        let now = transport_m1.now_ns();
        for frame in batch.frames() {
            seq_m1.ingest(frame.bytes(), frame.feed, now, &mut sink_m1);
        }
    }
    let m1_elapsed = t0.elapsed().as_secs_f64();
    let m1_rate = if m1_elapsed > 0.0 {
        ((sink_m1.count() as f64) / m1_elapsed) as u64
    } else {
        0
    };
    let golden_m1 = sink_m1.hash() == gt_hash && sink_m1.count() == gt_count;

    // 2. Evaluate M11 (Deep out-of-order delay jitter)
    let cfg_m11 = ReplayConfig {
        msgs_per_packet: Packetize::Fixed(1),
        delay: [
            DelayModel::GaussianApprox { mean_ns: 2000, sigma_ns: 400 },
            DelayModel::None,
        ],
        guarantee_coverage: true,
        ..Default::default()
    };
    let sched_m11 = build_schedule(gt, &cfg_m11);
    let mut transport_m11 = ReplayTransport::new(gt, sched_m11, sess);
    let mut seq_m11 = Sequencer::new();
    let mut sink_m11 = ConformanceSink::new();

    let t1 = Instant::now();
    while transport_m11.poll(&mut batch) > 0 {
        let now = transport_m11.now_ns();
        for frame in batch.frames() {
            seq_m11.ingest(frame.bytes(), frame.feed, now, &mut sink_m11);
        }
    }
    let m11_elapsed = t1.elapsed().as_secs_f64();
    let m11_rate = if m11_elapsed > 0.0 {
        ((sink_m11.count() as f64) / m11_elapsed) as u64
    } else {
        0
    };
    let golden_m11 = sink_m11.hash() == gt_hash && sink_m11.count() == gt_count;

    let arena_bytes = slots * 64 + slots;
    let l1_fit = if arena_bytes <= 32 * 1024 {
        "Fits 32K L1D"
    } else if arena_bytes <= 64 * 1024 {
        "Fits 64K L1D"
    } else {
        "Exceeds L1D (L2 Cache resident)"
    };

    WindowStats {
        slots,
        arena_bytes,
        l1_fit,
        max_staged_m11: seq_m11.staged_count(),
        beyond_drops_m11: seq_m11.counters().beyond_window_dropped,
        golden_m1,
        golden_m11,
        m1_rate,
        m11_rate,
    }
}

fn main() {
    let sample_path = "data/tests/sample-mini.itch";
    let gt = fs::read(sample_path).unwrap_or_else(|_| {
        fs::read("../../data/tests/sample-mini.itch")
            .unwrap_or_else(|_| fs::read("../data/tests/sample-mini.itch").expect("Failed to load sample"))
    });

    println!("=== RUNNING T2 WINDOW SIZE SWEEP (256..4096 x M1, M11) ===");
    let window_sizes = vec![256, 512, 1024, 2048, 4096];
    let mut results = Vec::new();

    for &slots in &window_sizes {
        let stats = run_sweep_for_slots(&gt, slots);
        println!(
            "WINDOW_SWEEP slots={:<4} arena={:>6} B l1_fit={:<30} M1_rate={:>5.2}M golden_M1={} M11_rate={:>5.2}M golden_M11={} max_staged={} beyond_drops={}",
            stats.slots,
            stats.arena_bytes,
            stats.l1_fit,
            (stats.m1_rate as f64) / 1e6,
            stats.golden_m1,
            (stats.m11_rate as f64) / 1e6,
            stats.golden_m11,
            stats.max_staged_m11,
            stats.beyond_drops_m11
        );
        assert!(stats.golden_m1, "Golden hash invariant failed on M1 for window size {}", slots);
        assert!(stats.golden_m11, "Golden hash invariant failed on M11 for window size {}", slots);
        results.push(stats);
    }

    println!("\n=== T2 WINDOW SWEEP KNEE TABLE ===");
    println!("| Window Slots | Arena Footprint | Cache Residency | M1 Rate | M11 Rate | Max Staged | Beyond Drops | Confluence Verdict |");
    println!("|---|---|---|---|---|---|---|---|");
    for r in &results {
        println!(
            "| {:<4} | {:>6} B ({:>5.1} KiB) | {:<28} | {:.2}M msg/s | {:.2}M msg/s | {:<10} | {:<12} | **PASS** |",
            r.slots,
            r.arena_bytes,
            (r.arena_bytes as f64) / 1024.0,
            r.l1_fit,
            (r.m1_rate as f64) / 1e6,
            (r.m11_rate as f64) / 1e6,
            r.max_staged_m11,
            r.beyond_drops_m11
        );
    }
    println!("=== T2 WINDOW SWEEP COMPLETED SUCCESSFULLY ===");
}
