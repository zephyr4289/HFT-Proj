//! hft_bench: statistical gate binary for nano/constr1.1.md §1.
//! 30 wall-rate runs + 5 warmup passes, engine-only counter sink (hash OFF hot
//! path per constr1.1 "Remove hash from hot path"), JSON metrics gate:
//! median_cycles, p95_cycles, p99_cycles, stddev, cv_percent.
//! Zero heap allocation inside the measurement window (timing is 2x monotonic
//! reads per pass; stats Vec lives outside the window).

#![allow(warnings)]
#![allow(clippy::all)]

use nf_arbitrator::types::{Event, LiveFeedProof};
use nf_arbitrator::{Sequencer, Sink};
use nf_engine::clock::{calibrate_clock, read_monotonic_raw_ns};
use nf_testkit::sched::{build_schedule, Packetize, ReplayConfig};
use nf_transport::replay::ReplayTransport;
use nf_transport::{FrameBatch, Transport};
use std::env;
use std::fs;

/// Engine-only counter sink: identical emit path + proof pass, zero hash math.
/// Same shape as bench.rs FastCountSink — this IS the hot path under test.
struct CountSink {
    count: u64,
}

impl Sink for CountSink {
    #[inline(always)]
    fn on_msg(&mut self, proof: &LiveFeedProof, _seq: u64, msg: &[u8]) {
        self.count += 1;
        std::hint::black_box(proof);
        std::hint::black_box(msg.len());
    }
    fn on_event(&mut self, _e: &Event) {}
}

fn get_cpu_model() -> String {
    if let Ok(content) = fs::read_to_string("/proc/cpuinfo") {
        for line in content.lines() {
            if line.starts_with("model name") {
                if let Some(pos) = line.find(':') {
                    return line[pos + 1..].trim().to_string();
                }
            }
        }
    }
    "Generic x86_64 CPU".to_string()
}

/// One measured wall-rate pass over a REUSED transport (reset per pass).
/// Reuse matters: a fresh 15MB pre-rendered blob per pass means 35x
/// mmap/munmap + page-fault churn, which showed up as a 12c↔20c sawtooth
/// across runs (host compaction/THP dance). reset() only rewinds event_idx /
/// clock with an identical session, so frames are byte-identical and pages
/// stay faulted and warm — steady-state measurement.
/// Returns messages/sec. Panics on zero messages or zero-duration pass.
fn wall_pass(transport: &mut ReplayTransport, sess: [u8; 10]) -> u64 {
    transport.reset(sess);
    let mut seq = Sequencer::new();
    let mut sink = CountSink { count: 0 };
    let mut batch = FrameBatch::new();
    let t0 = read_monotonic_raw_ns();
    while transport.poll(&mut batch) > 0 {
        let now = transport.now_ns();
        for f in batch.frames() {
            seq.ingest(f.bytes(), f.feed, now, &mut sink);
        }
    }
    let dt = read_monotonic_raw_ns().saturating_sub(t0);
    assert!(sink.count > 0, "hft_bench: zero messages emitted");
    assert!(dt > 0, "hft_bench: zero-duration pass");
    ((sink.count as f64) / (dt as f64) * 1e9) as u64
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut runs: usize = 30;
    let mut warmup: usize = 5;
    let mut sample_path = "data/tests/sample-mini.itch".to_string();
    let mut output_format = "json".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--runs" => {
                if let Some(v) = args.get(i + 1) {
                    runs = v.parse().unwrap_or(30);
                }
                i += 1;
            }
            "--warmup" => {
                if let Some(v) = args.get(i + 1) {
                    warmup = v.parse().unwrap_or(5);
                }
                i += 1;
            }
            "--sample" => {
                if let Some(v) = args.get(i + 1) {
                    sample_path = v.clone();
                }
                i += 1;
            }
            "--output-format" => {
                if let Some(v) = args.get(i + 1) {
                    output_format = v.clone();
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    let runs = runs.max(1);

    let gt = fs::read(&sample_path).unwrap_or_else(|_| {
        fs::read("../../data/tests/sample-mini.itch")
            .unwrap_or_else(|_| fs::read("../data/tests/sample-mini.itch").expect("Failed to load sample"))
    });

    let cal = calibrate_clock();
    let freq = cal.freq_mhz * 1e6;
    eprintln!(
        "HFT_BENCH_CALIBRATION invariant_tsc={} freq_mhz={:.2} mark_overhead_cycles={} target_env={}",
        cal.has_invariant_tsc,
        cal.freq_mhz,
        cal.overhead_cycles,
        if cfg!(target_env = "musl") { "musl" } else { "gnu" }
    );

    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        guarantee_coverage: true,
        ..Default::default()
    };
    let sched = build_schedule(&gt, &cfg);
    let sess = *b"HFTBENCH01";
    // Single transport for all passes (see wall_pass): identical bytes, warm pages.
    let mut transport = ReplayTransport::new(&gt, sched, sess);

    for w in 0..warmup {
        let r = wall_pass(&mut transport, sess);
        eprintln!("HFT_BENCH_WARMUP {}/{} rate={}", w + 1, warmup, r);
    }

    let mut cs: Vec<f64> = Vec::with_capacity(runs);
    for run in 0..runs {
        let rate = wall_pass(&mut transport, sess);
        let cyc = freq / rate.max(1) as f64;
        eprintln!("HFT_BENCH_RUN {}/{} rate={} cyc={:.2}", run + 1, runs, rate, cyc);
        cs.push(cyc);
    }

    cs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = cs.len();
    let median = if n % 2 == 1 {
        cs[n / 2]
    } else {
        (cs[n / 2 - 1] + cs[n / 2]) / 2.0
    };
    let p95 = cs[(0.95 * n as f64).ceil() as usize - 1];
    let p99 = cs[(0.99 * n as f64).ceil() as usize - 1];
    let mean = cs.iter().sum::<f64>() / n as f64;
    let var = cs.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / n as f64;
    let stddev = var.sqrt();
    let cv = stddev / median.max(1e-9) * 100.0;

    if output_format == "json" {
        let cpu = get_cpu_model().replace('"', " ");
        let sample = sample_path.replace('"', " ");
        let target = if cfg!(target_env = "musl") {
            "x86_64-unknown-linux-musl"
        } else {
            "x86_64-unknown-linux-gnu"
        };
        println!(
            "{{\n  \"median_cycles\": {:.4},\n  \"p95_cycles\": {:.4},\n  \"p99_cycles\": {:.4},\n  \"stddev\": {:.4},\n  \"cv_percent\": {:.4},\n  \"runs\": {},\n  \"warmup\": {},\n  \"cpu_model\": \"{}\",\n  \"freq_mhz\": {:.2},\n  \"target\": \"{}\",\n  \"sink\": \"count\",\n  \"sample\": \"{}\"\n}}",
            median, p95, p99, stddev, cv, n, warmup, cpu, cal.freq_mhz, target, sample
        );
    } else {
        println!(
            "HFT_BENCH_RESULT median={:.2} p95={:.2} p99={:.2} stddev={:.4} cv={:.2}% runs={} warmup={}",
            median, p95, p99, stddev, cv, n, warmup
        );
    }
}
