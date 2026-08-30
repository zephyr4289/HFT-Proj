//! Benchmarking binary for PR-1 (throughput) and PR-2 (latency) evaluation (doc 11).
//! Zero heap allocation during measurement window (PR-3 / doc 07).

#![allow(warnings)]
#![allow(clippy::all)]

use nf_arbitrator::Sequencer;
use nf_engine::alloc::GLOBAL;
use nf_engine::clock::{calibrate_clock, read_monotonic_raw_ns, read_tsc_serialized_end, read_tsc_serialized_start};
use nf_engine::histogram::StaticHistogram;
use nf_testkit::sched::{build_schedule, Packetize, ReplayConfig};
use nf_testkit::sink::ConformanceSink;
use nf_transport::replay::ReplayTransport;
use nf_transport::{FrameBatch, Transport};
use std::env;
use std::fs;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut sample_path = "data/tests/sample-mini.itch".to_string();
    let mut runs = 5usize;

    let mut i = 1;
    while i < args.len() {
        if args[i] == "--sample" && i + 1 < args.len() {
            sample_path = args[i + 1].clone();
            i += 1;
        } else if args[i] == "--runs" && i + 1 < args.len() {
            runs = args[i + 1].parse().unwrap_or(5);
            i += 1;
        }
        i += 1;
    }

    let gt = fs::read(&sample_path).unwrap_or_else(|_| {
        fs::read("../../data/tests/sample-mini.itch")
            .unwrap_or_else(|_| fs::read("../data/tests/sample-mini.itch").expect("Failed to load sample"))
    });

    let cal = calibrate_clock();
    println!(
        "BENCH_CALIBRATION invariant_tsc={} freq_mhz={:.2} mark_overhead_cycles={}",
        cal.has_invariant_tsc, cal.freq_mhz, cal.overhead_cycles
    );

    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        guarantee_coverage: true,
        ..Default::default()
    };
    let sched = build_schedule(&gt, &cfg);
    let sess = *b"BENCHSESS1";

    let mut rates = Vec::with_capacity(runs);
    let mut p50s = Vec::with_capacity(runs);
    let mut p99s = Vec::with_capacity(runs);
    let mut p9999s = Vec::with_capacity(runs);
    let mut maxs = Vec::with_capacity(runs);

    for run_id in 1..=runs {
        let mut transport = ReplayTransport::new(&gt, sched.clone(), sess);
        let mut seq = Sequencer::new();
        let mut sink = ConformanceSink::new();
        let mut batch = FrameBatch::new();
        let mut hist = StaticHistogram::new();

        // Snapshot allocation counter before window
        let (a1, d1) = GLOBAL.snapshot();

        let t_start_mono = read_monotonic_raw_ns();

        while transport.poll(&mut batch) > 0 {
            let now = transport.now_ns();
            for frame in batch.frames() {
                let t0 = read_tsc_serialized_start();
                seq.ingest(frame.bytes(), frame.feed, now, &mut sink);
                let t1 = read_tsc_serialized_end();

                let dt = t1.saturating_sub(t0).saturating_sub(cal.overhead_cycles);
                hist.record(dt);
            }
        }

        let t_end_mono = read_monotonic_raw_ns();
        let dt_ns = t_end_mono.saturating_sub(t_start_mono);
        let (a2, d2) = GLOBAL.snapshot();
        let alloc_delta = (a2 - a1) + (d2 - d1);

        let msg_count = sink.count();
        let rate = if dt_ns > 0 {
            ((msg_count as f64) / (dt_ns as f64) * 1e9) as u64
        } else {
            0
        };

        let p50 = hist.percentile(50.0);
        let p99 = hist.percentile(99.0);
        let p9999 = hist.percentile(99.99);
        let max = hist.max();

        println!(
            "BENCH mode=replay-core msgs={} rate={} p50={} p99={} p99.99={} max={} allocs={} freq={:.2}MHz run={}",
            msg_count, rate, p50, p99, p9999, max, alloc_delta, cal.freq_mhz, run_id
        );

        assert_eq!(alloc_delta, 0, "Benchmark must assert ALLOC_DELTA=0 (PR-3)");

        rates.push(rate);
        p50s.push(p50);
        p99s.push(p99);
        p9999s.push(p9999);
        maxs.push(max);
    }

    rates.sort();
    p50s.sort();
    p99s.sort();
    p9999s.sort();
    maxs.sort();

    let median_idx = runs / 2;
    println!(
        "BENCH_MEDIAN mode=replay-core rate={} p50={} p99={} p99.99={} max={}",
        rates[median_idx], p50s[median_idx], p99s[median_idx], p9999s[median_idx], maxs[median_idx]
    );
}
