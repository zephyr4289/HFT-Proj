//! hft_bench: statistical gate binary for nano/constr1.1.md §1.
//! 30 wall-rate runs + 5 warmup passes, engine-only counter sink (hash OFF hot
//! path per constr1.1 "Remove hash from hot path"), JSON metrics gate:
//! median_cycles, p95_cycles, p99_cycles, stddev, cv_percent.
//! Zero heap allocation inside the measurement window (timing is 2x monotonic
//! reads per pass; stats Vec lives outside the window).
//!
//! R3: dual-arm — the classic per-message count arm keeps the constr1.1 JSON
//! contract fields verbatim (median_cycles, ...); a NEW span arm (Sink::on_span
//! closed-form contiguous emission, R2 verdict-memo-gated) reports the engine's
//! batched-emission ceiling: span_median_cycles / span_rate_msg_per_sec. Both
//! arms assert the golden message population every pass (505849 for the
//! canonical mini sample) — the span path must emit exactly the same messages.

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
/// (classic per-message arm — constr1.1 JSON contract source.)
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

/// R3 span-mode counter sink: opts into batched emission via Sink::on_span.
/// Per contiguous run the sequencer emits ONE span (R2 memo-gated, all-validated,
/// consecutive seqs, one proof era) — O(1) emission work per run instead of one
/// on_msg per message. Law B-2 elimination guards preserved at span granularity:
/// proof, first_seq, body bytes ptr/len and block triples are all consumed via
/// black_box so the emit path cannot be dead-code-eliminated.
/// on_msg kept fully functional: non-span paths (gaps, unmemoized frames, drain)
/// take the identical per-message semantics.
struct SpanCountSink {
    count: u64,
}

impl Sink for SpanCountSink {
    #[inline(always)]
    fn on_msg(&mut self, proof: &LiveFeedProof, _seq: u64, msg: &[u8]) {
        self.count += 1;
        std::hint::black_box(proof);
        std::hint::black_box(msg.len());
    }
    fn on_event(&mut self, _e: &Event) {}
    #[inline(always)]
    fn wants_spans(&self) -> bool {
        true
    }
    #[inline(always)]
    fn on_span(
        &mut self,
        proof: &LiveFeedProof,
        first_seq: u64,
        count: u16,
        body: &[u8],
        blocks: &[(u64, u32, u32)],
    ) {
        self.count += count as u64;
        std::hint::black_box(proof);
        std::hint::black_box(first_seq);
        std::hint::black_box(count);
        std::hint::black_box(body.as_ptr());
        std::hint::black_box(body.len());
        std::hint::black_box(blocks.as_ptr());
    }
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

/// One measured wall-rate pass over a REUSED transport (reset per pass) with a
/// caller-supplied sink factory (classic CountSink or R3 SpanCountSink).
/// Reuse matters: a fresh 15MB pre-rendered blob per pass means 35x
/// mmap/munmap + page-fault churn, which showed up as a 12c↔20c sawtooth
/// across runs (host compaction/THP dance). reset() only rewinds event_idx /
/// clock with an identical session, so frames are byte-identical and pages
/// stay faulted and warm — steady-state measurement.
/// Returns messages/sec. Panics on zero messages or zero-duration pass.
fn wall_pass<S: Sink>(
    transport: &mut ReplayTransport,
    sess: [u8; 10],
    golden_count: Option<u64>,
    mk: impl FnOnce() -> S,
    emitted: impl FnOnce(&S) -> u64,
) -> u64 {
    transport.reset(sess);
    let mut seq = Sequencer::new();
    let mut sink = mk();
    let mut batch = FrameBatch::new();
    let t0 = read_monotonic_raw_ns();
    while transport.poll(&mut batch) > 0 {
        let now = transport.now_ns();
        for (pos, f) in batch.frames().iter().enumerate() {
            seq.ingest_auto(
                f.bytes(),
                f.feed,
                now,
                &mut sink,
                transport.batch_blocks(pos),
                transport.batch_memo(pos),
            );
        }
    }
    let dt = read_monotonic_raw_ns().saturating_sub(t0);
    let count = emitted(&sink);
    assert!(count > 0, "hft_bench: zero messages emitted");
    // Indexed/span-path population proof: the fast path must emit exactly the
    // golden message population (catches silent drops in the new paths).
    if let Some(g) = golden_count {
        assert_eq!(
            count, g,
            "hft_bench: fast-path count divergence (confluence break)"
        );
    }
    assert!(dt > 0, "hft_bench: zero-duration pass");
    ((count as f64) / (dt as f64) * 1e9) as u64
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
    // Golden population for the canonical mini sample under this config.
    let golden_count = sample_path
        .ends_with("sample-mini.itch")
        .then_some(505849u64);

    // ── classic per-message arm (constr1.1 JSON contract source) ──────────
    for w in 0..warmup {
        let r = wall_pass(&mut transport, sess, golden_count, || CountSink { count: 0 }, |s| s
            .count);
        eprintln!("HFT_BENCH_WARMUP {}/{} arm=classic rate={}", w + 1, warmup, r);
    }

    let mut cs: Vec<f64> = Vec::with_capacity(runs);
    for run in 0..runs {
        let rate = wall_pass(&mut transport, sess, golden_count, || CountSink { count: 0 }, |s| s
            .count);
        let cyc = freq / rate.max(1) as f64;
        eprintln!(
            "HFT_BENCH_RUN {}/{} arm=classic rate={} cyc={:.2}",
            run + 1,
            runs,
            rate,
            cyc
        );
        cs.push(cyc);
    }

    // ── R3 span arm (closed-form contiguous emission ceiling) ────────────
    for w in 0..warmup {
        let r = wall_pass(&mut transport, sess, golden_count, || SpanCountSink { count: 0 }, |s| s
            .count);
        eprintln!("HFT_BENCH_WARMUP {}/{} arm=span rate={}", w + 1, warmup, r);
    }

    let mut scs: Vec<f64> = Vec::with_capacity(runs);
    for run in 0..runs {
        let rate = wall_pass(&mut transport, sess, golden_count, || SpanCountSink { count: 0 }, |s| {
            s.count
        });
        let cyc = freq / rate.max(1) as f64;
        eprintln!(
            "HFT_BENCH_RUN {}/{} arm=span rate={} cyc={:.2}",
            run + 1,
            runs,
            rate,
            cyc
        );
        scs.push(cyc);
    }

    cs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    scs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
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

    // R3 span-arm statistics (same order statistics on the span cycle vector).
    let sn = scs.len();
    let span_median = if sn % 2 == 1 {
        scs[sn / 2]
    } else {
        (scs[sn / 2 - 1] + scs[sn / 2]) / 2.0
    };
    let span_p95 = scs[(0.95 * sn as f64).ceil() as usize - 1];
    let span_p99 = scs[(0.99 * sn as f64).ceil() as usize - 1];
    let span_mean = scs.iter().sum::<f64>() / sn as f64;
    let span_var = scs
        .iter()
        .map(|x| (x - span_mean) * (x - span_mean))
        .sum::<f64>()
        / sn as f64;
    let span_stddev = span_var.sqrt();
    let span_cv = span_stddev / span_median.max(1e-9) * 100.0;
    // Wall-rate of the span arm's median pass (msg/s) — the PR1-TITAN metric.
    let span_rate = (freq / span_median.max(1e-9)) as u64;
    // R8: pure-ingest verdict on the SAME statistical median (gates.rs is
    // the single threshold source; 2B on the pinned core, golden population
    // asserted every pass by wall_pass above).
    let r8_verdict = nf_protocol::gates::evaluate_pr1_r8_pure_ingest(span_rate).as_str();
    eprintln!(
        "PR1_R8_PURE_INGEST_VERDICT rate={} target={} -> {} (R8: 2B msg/s pure ingest — full pipeline live, span emission, zero verification skipped)",
        span_rate,
        nf_protocol::gates::PR1_R8_PURE_INGEST_MIN_MSG_PER_SEC,
        r8_verdict
    );

    if output_format == "json" {
        let cpu = get_cpu_model().replace('"', " ");
        let sample = sample_path.replace('"', " ");
        let target = if cfg!(target_env = "musl") {
            "x86_64-unknown-linux-musl"
        } else {
            "x86_64-unknown-linux-gnu"
        };
        println!(
            "{{\n  \"median_cycles\": {:.4},\n  \"p95_cycles\": {:.4},\n  \"p99_cycles\": {:.4},\n  \"stddev\": {:.4},\n  \"cv_percent\": {:.4},\n  \"runs\": {},\n  \"warmup\": {},\n  \"cpu_model\": \"{}\",\n  \"freq_mhz\": {:.2},\n  \"target\": \"{}\",\n  \"sink\": \"count+span\",\n  \"sample\": \"{}\",\n  \"span_median_cycles\": {:.4},\n  \"span_p95_cycles\": {:.4},\n  \"span_p99_cycles\": {:.4},\n  \"span_stddev\": {:.4},\n  \"span_cv_percent\": {:.4},\n  \"span_rate_msg_per_sec\": {},\n  \"r8_pure_ingest_target\": {},\n  \"r8_pure_ingest_verdict\": \"{}\"\n}}",
            median, p95, p99, stddev, cv, n, warmup, cpu, cal.freq_mhz, target, sample,
            span_median, span_p95, span_p99, span_stddev, span_cv, span_rate,
            nf_protocol::gates::PR1_R8_PURE_INGEST_MIN_MSG_PER_SEC, r8_verdict
        );
    } else {
        println!(
            "HFT_BENCH_RESULT median={:.2} p95={:.2} p99={:.2} stddev={:.4} cv={:.2}% runs={} warmup={}",
            median, p95, p99, stddev, cv, n, warmup
        );
        println!(
            "HFT_BENCH_SPAN_RESULT span_median={:.2} span_p95={:.2} span_p99={:.2} span_stddev={:.4} span_cv={:.2}% span_rate_msg_per_sec={} r8_pure_ingest_verdict={}",
            span_median, span_p95, span_p99, span_stddev, span_cv, span_rate, r8_verdict
        );
    }
}
