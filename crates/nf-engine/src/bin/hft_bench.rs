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

use nf_arbitrator::types::{Event, LiveFeedProof, SpanRec};
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
    /// R8: batched emission — the per-span elimination guards collapse to
    /// one guard over the rec array (the array's bytes ARE the per-span
    /// ptr/len data, so the emission path cannot be dead-code-eliminated),
    /// and the count fold becomes one add per rec with a single commit.
    #[inline(always)]
    fn on_span_batch(&mut self, proof: &LiveFeedProof, recs: &[SpanRec<'_>]) {
        let mut sum = 0u64;
        for r in recs {
            sum += r.count as u64;
        }
        self.count += sum;
        std::hint::black_box(proof);
        std::hint::black_box(recs.as_ptr());
        std::hint::black_box(recs.len());
        std::hint::black_box(sum);
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
/// R8: the per-frame index comes slot-direct (`frame_blocks_memo`) — one
/// call replacing the batch_blocks/batch_memo side-table pair — over the
/// classic ingest_auto ladder; poll() runs the RX-coalesced group release
/// (see `set_poll_coalesce`, NAPI-style receipt batching).
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
        seq.ingest_batch(transport.batch_entries(&batch), now, &mut sink);
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

/// R10: the per-batch DIAG timing flag — read ONCE at startup. The R8 arm
/// ran two `read_monotonic_raw_ns()` vDSO calls per batch and one
/// `env::var` (a heap allocation) per pass INSIDE the measured window:
/// ~44 syscalls per pass ≈ 1% of the 119us Zen3 pass at the 4.24B record.
/// Instrumentation must never tax the window it measures.
static EXP_DIAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// R8: the span arm's pipelined pass — the RX thread (transport staging:
/// directory walk, pacing, session-baked slicing, slot publication) runs
/// on its own core while THIS thread arbitrates (the sequencer's steady
/// ladder + span emission). Same bytes, same order, same emissions as the
/// single-threaded pass (pinned by the parity suite); the two halves of
/// the ingest pipeline are simply overlapped — the feed-handler shape
/// real deployments run.
fn wall_pass_pipelined<S: Sink>(
    transport: &mut nf_transport::pipeline::PipelinedReplayTransport,
    sess: [u8; 10],
    golden_count: Option<u64>,
    mk: impl FnOnce() -> S,
    emitted: impl FnOnce(&S) -> u64,
) -> u64 {
    transport.reset(sess); // rewind + session bake (RX handshake)
    let mut seq = Sequencer::new();
    let mut sink = mk();
    // R12: the vectorized watermark ladder — read once per pass (std's
    // detection caches after the first call; the env rollback is
    // HFT_VEC_LADDER=0).
    let ladder = nf_testkit::soa::ladder8_best();
    let t0 = read_monotonic_raw_ns();
    let mut nb = 0usize;
    let mut max_batch_ns: u128 = 0;
    let diag = EXP_DIAG.load(std::sync::atomic::Ordering::Relaxed);
    while transport.next_batch() {
        if diag {
            let tb = read_monotonic_raw_ns();
            seq.ingest_entries_ladder(transport.entries(), transport.now_ns(), &mut sink, ladder);
            let db = read_monotonic_raw_ns().saturating_sub(tb) as u128;
            if db > max_batch_ns {
                max_batch_ns = db;
            }
        } else {
            seq.ingest_entries_ladder(transport.entries(), transport.now_ns(), &mut sink, ladder);
        }
        nb += 1;
    }
    let dt = read_monotonic_raw_ns().saturating_sub(t0);
    if diag {
        eprintln!(
            "DIAG pass: batches={} total_ms={:.1} max_batch_us={:.1}",
            nb,
            dt as f64 / 1e6,
            max_batch_ns as f64 / 1e3
        );
    }
    let count = emitted(&sink);
    assert!(count > 0, "hft_bench: zero messages emitted (pipelined)");
    if let Some(g) = golden_count {
        assert_eq!(
            count, g,
            "hft_bench: pipelined fast-path count divergence (confluence break)"
        );
    }
    assert!(dt > 0, "hft_bench: zero-duration pass (pipelined)");
    ((count as f64) / (dt as f64) * 1e9) as u64
}

fn main() {
    // R8 phase-4: capture the topology truth BEFORE any arm pins anything
    // (the mask-pollution trap — see affinity::capture_topology).
    let _ = nf_testkit::affinity::capture_topology();
    // R10: the per-batch DIAG flag, read once (the per-pass env::var was an
    // in-window allocation — see wall_pass_pipelined).
    EXP_DIAG.store(
        std::env::var("HFT_EXP_DIAG").is_ok(),
        std::sync::atomic::Ordering::Relaxed,
    );
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
    // R8: L3-aware affinity — main + RX share an L3 domain (different
    // physical cores) so the per-batch mailbox handoff stays off the
    // cross-CCD path (measured 2.5x consumer-side difference between
    // runner types whose placements differed only in L3 locality).
    let (main_cpu, rx_cpu) = nf_testkit::affinity::pipeline_placement();
    if let Some(cpu) = main_cpu {
        let _ = nf_testkit::affinity::pin_current_to(cpu);
    }
    // Single transport for all passes (see wall_pass): identical bytes, warm pages.
    let mut transport = ReplayTransport::new(&gt, sched.clone(), sess);
    // R8: RX coalescing for the throughput arms (NAPI-style receipt batching;
    // see set_poll_coalesce). HFT_COALESCE=1 disables it (exact pre-R8 pacing).
    let co = std::env::var("HFT_COALESCE")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(128);
    transport.set_poll_coalesce(co);
    // R8: the SPAN arm runs on the RX-pipelined transport (coalesced,
    // RX pinned to topology slot 1); the classic per-message arm keeps
    // the single-threaded transport (its JSON contract measures the
    // engine-only path).
    let mut piped = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce_cpu(
        &gt, sched, sess, co, rx_cpu,
    );
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

    // ── R3 span arm (closed-form contiguous emission ceiling; R8: pipelined) ──
    for w in 0..warmup {
        let r = wall_pass_pipelined(&mut piped, sess, golden_count, || SpanCountSink { count: 0 }, |s| {
            s.count
        });
        eprintln!("HFT_BENCH_WARMUP {}/{} arm=span rate={}", w + 1, warmup, r);
    }

    let mut scs: Vec<f64> = Vec::with_capacity(runs);
    for run in 0..runs {
        let rate = wall_pass_pipelined(&mut piped, sess, golden_count, || SpanCountSink { count: 0 }, |s| {
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
    // F-1 (HFT_RXWARM): the RX pipeline's per-run telemetry — the Front A
    // attribution lines (prod_ms is the entry-build share the warm start
    // attacks) + the warm-start verdict line (fixes/last_pass_fixes; the
    // CI arm greps RXWARM_DIAGNOSTIC).
    piped.diag_summary("span");
    // F-3 (I-6): the span arm's entry-walk telemetry — which ladder shape
    // the consumer ran (HFT_VEC_LADDER=1 arms the vectorized 8-entry group
    // path + the RX's elig baking; unset is the scalar steady_step). The
    // 11sl arm greps this line to prove the lever live (the flip-validation
    // lesson: never price an arm on faith — the diagnostic says armed).
    eprintln!(
        "LADDER_DIAGNOSTIC span: vectorized={} (F-3 entry walk; HFT_VEC_LADDER=1 arms the 8-entry group path — the Front A consumer IS the post-F-2 wall)",
        nf_testkit::soa::ladder8_best().is_some()
    );

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
    // R16: the 5B target — REPORTED per draw (non-asserting until the
    // median healthy draw crosses it; the lever is rxbuild, docs/29 §5).
    let r16_verdict = nf_protocol::gates::evaluate_pr1_r16_pure_ingest(span_rate).as_str();
    eprintln!(
        "PR1_R16_PURE_INGEST_VERDICT rate={} target={} -> {} (R16: 5B msg/s pure ingest — the Double Helix program, docs/29)",
        span_rate,
        nf_protocol::gates::PR1_R16_PURE_INGEST_MIN_MSG_PER_SEC,
        r16_verdict
    );
    // R18: the 6B target — REPORTED per draw, non-asserting until the
    // median healthy draw crosses it (the R12 protocol; the vector-ingest
    // program is docs/30). The gate separates the record band (5.29-5.46B,
    // draws 37338823281 / 37348948183) from the claim band.
    let r18_verdict = nf_protocol::gates::evaluate_pr1_r18_pure_ingest(span_rate).as_str();
    eprintln!(
        "PR1_R18_PURE_INGEST_VERDICT rate={} target={} -> {} (R18: 6B msg/s pure ingest — the vector-ingest program, docs/30)",
        span_rate,
        nf_protocol::gates::PR1_R18_PURE_INGEST_MIN_MSG_PER_SEC,
        r18_verdict
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
            "{{\n  \"median_cycles\": {:.4},\n  \"p95_cycles\": {:.4},\n  \"p99_cycles\": {:.4},\n  \"stddev\": {:.4},\n  \"cv_percent\": {:.4},\n  \"runs\": {},\n  \"warmup\": {},\n  \"cpu_model\": \"{}\",\n  \"freq_mhz\": {:.2},\n  \"target\": \"{}\",\n  \"sink\": \"count+span\",\n  \"sample\": \"{}\",\n  \"span_median_cycles\": {:.4},\n  \"span_p95_cycles\": {:.4},\n  \"span_p99_cycles\": {:.4},\n  \"span_stddev\": {:.4},\n  \"span_cv_percent\": {:.4},\n  \"span_rate_msg_per_sec\": {},\n  \"r8_pure_ingest_target\": {},\n  \"r8_pure_ingest_verdict\": \"{}\",\n  \"r16_pure_ingest_target\": {},\n  \"r16_pure_ingest_verdict\": \"{}\",\n  \"r18_pure_ingest_target\": {},\n  \"r18_pure_ingest_verdict\": \"{}\"\n}}",
            median, p95, p99, stddev, cv, n, warmup, cpu, cal.freq_mhz, target, sample,
            span_median, span_p95, span_p99, span_stddev, span_cv, span_rate,
            nf_protocol::gates::PR1_R8_PURE_INGEST_MIN_MSG_PER_SEC, r8_verdict,
            nf_protocol::gates::PR1_R16_PURE_INGEST_MIN_MSG_PER_SEC, r16_verdict,
            nf_protocol::gates::PR1_R18_PURE_INGEST_MIN_MSG_PER_SEC, r18_verdict
        );
    } else {
        println!(
            "HFT_BENCH_RESULT median={:.2} p95={:.2} p99={:.2} stddev={:.4} cv={:.2}% runs={} warmup={}",
            median, p95, p99, stddev, cv, n, warmup
        );
        println!(
            "HFT_BENCH_SPAN_RESULT span_median={:.2} span_p95={:.2} span_p99={:.2} span_stddev={:.4} span_cv={:.2}% span_rate_msg_per_sec={} r8_pure_ingest_verdict={} r16_pure_ingest_verdict={} r18_pure_ingest_verdict={}",
            span_median, span_p95, span_p99, span_stddev, span_cv, span_rate, r8_verdict, r16_verdict, r18_verdict
        );
    }
}
