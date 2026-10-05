//! R8: batch-ingest differential parity (docs/22-r8-teraphase.md).
//!
//! `Sequencer::ingest_batch` MUST be observationally identical to feeding
//! the same frames through `ingest_auto` one by one — same emissions, same
//! counters, same watermark, same sink state — for every schedule. These
//! tests pin that equivalence on a battery of schedules (clean dual-feed,
//! lossy, delayed/reordered, session-split, single-feed) and three sink
//! shapes (per-message count, span-emitting conformance, hydra fabric
//! inline), plus the multi-pass reset cycle the sustained arms use.
//!
//! This is the unit-level core of the D12 differential oracle (which adds
//! the reference arbitrator as a third leg in CI).

use crate::sched::{build_schedule, Packetize, ReplayConfig};
use crate::sink::SpanConformanceSink;
use nf_arbitrator::Sequencer;
use nf_transport::replay::ReplayTransport;
use nf_transport::{FrameBatch, Transport};

#[cfg(test)]
use crate::sched::{DelayModel, LossModel};
#[cfg(test)]
use crate::sink::{ConformanceSink, FastConformanceSink};

/// One classic (per-frame ingest_auto) pass → the observable tuple:
/// (counters, watermark, count, hash, events seen).
pub fn classic_pass(
    transport: &mut ReplayTransport,
    sess: [u8; 10],
) -> (nf_arbitrator::Counters, u64, u64, u64, u64) {
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
    // R12 fix: the FINAL sink counters (the pre-R12 form added the
    // cumulative counters once per poll, inflating the count by the poll
    // granularity — invisible while every leg polled at the same
    // granularity, false-divergent against the pipeline's big batches).
    (
        seq.counters(),
        seq.watermark(),
        sink.count,
        sink.hash,
        sink.session_boundaries + sink.gap_opens + sink.reanchors,
    )
}

/// One batched (ingest_batch) pass over the identical byte stream.
pub fn batch_pass(
    transport: &mut ReplayTransport,
    sess: [u8; 10],
) -> (nf_arbitrator::Counters, u64, u64, u64, u64) {
    transport.reset(sess);
    let mut seq = Sequencer::new();
    let mut sink = SpanConformanceSink::new();
    let mut batch = FrameBatch::new();
    while transport.poll(&mut batch) > 0 {
        let now = transport.now_ns();
        seq.ingest_batch(transport.batch_entries(&batch), now, &mut sink);
    }
    (
        seq.counters(),
        seq.watermark(),
        sink.count,
        sink.hash,
        sink.session_boundaries + sink.gap_opens + sink.reanchors,
    )
}

pub fn assert_parity(
    label: &str,
    cfg: &ReplayConfig,
    gt: &[u8],
) {
    let sched = build_schedule(gt, cfg);
    let sess = *b"PARITY0001";
    let mut t_classic = ReplayTransport::new(gt, sched.clone(), sess);
    let mut t_batch = ReplayTransport::new(gt, sched, sess);
    let c = classic_pass(&mut t_classic, sess);
    let b = batch_pass(&mut t_batch, sess);
    assert_eq!(c.0, b.0, "{label}: counters diverged\nclassic={:#?}\nbatch ={:#?}", c.0, b.0);
    assert_eq!(c.1, b.1, "{label}: watermark diverged");
    assert_eq!(c.2, b.2, "{label}: sink count diverged");
    assert_eq!(c.3, b.3, "{label}: sink span hash diverged");
    assert_eq!(c.4, b.4, "{label}: event counts diverged");
}

pub fn default_cfg() -> ReplayConfig {
    ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        guarantee_coverage: true,
        ..Default::default()
    }
}

/// Clean dual-feed MtuBound schedule — the canonical bench workload.
#[test]
fn t_r8_batch_parity_default_schedule() {
    let gt = mini_gt(4000);
    assert_parity("default", &default_cfg(), &gt);
}

/// Lossy dual-feed (Bernoulli both feeds, coverage guaranteed): gaps open,
/// out-of-order staging, drain emissions — the cold paths must interleave
/// with steady stretches exactly like the classic ladder.
#[test]
fn t_r8_batch_parity_lossy_schedule() {
    let gt = mini_gt(4000);
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        loss: [LossModel::Bernoulli { p_pm: 120 }, LossModel::Bernoulli { p_pm: 180 }],
        guarantee_coverage: true,
        ..Default::default()
    };
    assert_parity("lossy", &cfg, &gt);
}

/// Delayed/reordered dual-feed: Gaussian delays on one feed re-order frames
/// across feeds — partial-dup skips and gap-fill staging must match.
#[test]
fn t_r8_batch_parity_reorder_schedule() {
    let gt = mini_gt(4000);
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        delay: [
            DelayModel::None,
            DelayModel::GaussianApprox { mean_ns: 300_000, sigma_ns: 150_000 },
        ],
        guarantee_coverage: true,
        ..Default::default()
    };
    assert_parity("reorder", &cfg, &gt);
}

/// Session split mid-stream: boundary events + fresh session anchor both
/// take the cold path — emission order across the boundary must match.
#[test]
fn t_r8_batch_parity_session_split() {
    let gt = mini_gt(4000);
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        session_change_at_msg: Some(1500),
        guarantee_coverage: true,
        ..Default::default()
    };
    assert_parity("session-split", &cfg, &gt);
}

/// Single-feed schedule (no duplicates) + SeededRange packetization.
#[test]
fn t_r8_batch_parity_single_feed_seeded() {
    let gt = mini_gt(4000);
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::SeededRange { min: 3, max: 40 },
        feeds_enabled: 1,
        guarantee_coverage: true,
        ..Default::default()
    };
    assert_parity("single-feed-seeded", &cfg, &gt);
}

/// Per-message (non-span) sinks: the batch loop's on_msg path must emit the
/// identical message sequence (count + golden FNV hash + monotonicity).
#[test]
fn t_r8_batch_parity_per_message_sink() {
    let gt = mini_gt(3000);
    let sched = build_schedule(&gt, &default_cfg());
    let sess = *b"PARITY0002";

    let run = |batched: bool| -> (u64, u64, u64, nf_arbitrator::Counters) {
        let mut t = ReplayTransport::new(&gt, sched.clone(), sess);
        t.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = ConformanceSink::new();
        let mut batch = FrameBatch::new();
        while t.poll(&mut batch) > 0 {
            let now = t.now_ns();
            if batched {
                seq.ingest_batch(t.batch_entries(&batch), now, &mut sink);
            } else {
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
        }
        (sink.count(), sink.hash(), sink.last_seq, seq.counters())
    };
    let c = run(false);
    let b = run(true);
    assert_eq!(c, b, "per-message sink diverged between classic and batch");
}

// ─── R12: the SoA vectorized-ladder parity (docs/25) ─────────────────────
//
// The 8-entry vectorized watermark ladder must be observationally
// identical to the scalar steady ladder AND to the classic per-frame
// ladder, on the RX-pipelined transport that publishes the SoA sidecar.
// Three legs per schedule: classic (per-frame, single-threaded),
// pipelined-scalar (ingest_entries — the R8 path), pipelined-vector
// (ingest_entries_ladder + the best ladder for this silicon; on non-AVX-512
// hosts the ladder is None and the leg degenerates to the scalar
// semantics — the scalar fallback IS the same code).

/// One pipelined pass with the caller's scan mode → the parity tuple.
fn soa_pass(
    transport: &mut nf_transport::pipeline::PipelinedReplayTransport,
    sess: [u8; 10],
    ladder: Option<nf_protocol::packet::SoaLadder8>,
) -> (nf_arbitrator::Counters, u64, u64, u64, u64) {
    transport.reset(sess);
    let mut seq = Sequencer::new();
    let mut sink = SpanConformanceSink::new();
    while transport.next_batch() {
        let now = transport.now_ns();
        if ladder.is_some() {
            seq.ingest_entries_ladder(transport.entries(), now, &mut sink, ladder);
        } else {
            seq.ingest_entries(transport.entries(), now, &mut sink);
        }
    }
    (
        seq.counters(),
        seq.watermark(),
        sink.count,
        sink.hash,
        sink.session_boundaries + sink.gap_opens + sink.reanchors,
    )
}

/// The 3-way differential: classic vs pipelined-scalar vs pipelined-vector.
pub fn assert_soa_parity(label: &str, cfg: &ReplayConfig, gt: &[u8]) {
    let sched = build_schedule(gt, cfg);
    let sess = *b"SOAPAR0001";
    let mut t_classic = ReplayTransport::new(gt, sched.clone(), sess);
    let mut t_pl_scalar =
        nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(&gt, sched.clone(), sess, 128);
    let mut t_pl_vec =
        nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(&gt, sched, sess, 128);
    let c = classic_pass(&mut t_classic, sess);
    let s = soa_pass(&mut t_pl_scalar, sess, None);
    let ladder = crate::soa::ladder8_best();
    let v = soa_pass(&mut t_pl_vec, sess, ladder);
    assert_eq!(
        c, s,
        "{label}: pipelined-scalar diverged from classic\nclassic={c:#?}\nscalar ={s:#?}"
    );
    assert_eq!(
        c, v,
        "{label}: pipelined-VECTOR diverged from classic\nclassic={c:#?}\nvector  ={v:#?}"
    );
}

/// Clean dual-feed MtuBound — the canonical bench workload; on AVX-512
/// silicon the vector ladder takes ~every steady group (strict emit/dup
/// alternation).
#[test]
fn t_r12_soa_parity_default_schedule() {
    let gt = mini_gt(4000);
    assert_soa_parity("soa-default", &default_cfg(), &gt);
}

/// Lossy dual-feed: gaps open and close — the vector path must fall back
/// at every break and resume exactly where the scalar ladder would.
#[test]
fn t_r12_soa_parity_lossy_schedule() {
    let gt = mini_gt(4000);
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        loss: [LossModel::Bernoulli { p_pm: 120 }, LossModel::Bernoulli { p_pm: 180 }],
        guarantee_coverage: true,
        ..Default::default()
    };
    assert_soa_parity("soa-lossy", &cfg, &gt);
}

/// Delayed/reordered dual-feed: partial-dup overlaps (first < w <= last)
/// must be rejected by the ladder's pair/dup-le relations and take the
/// classic skip-prefix path.
#[test]
fn t_r12_soa_parity_reorder_schedule() {
    let gt = mini_gt(4000);
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        delay: [
            DelayModel::None,
            DelayModel::GaussianApprox { mean_ns: 300_000, sigma_ns: 150_000 },
        ],
        guarantee_coverage: true,
        ..Default::default()
    };
    assert_soa_parity("soa-reorder", &cfg, &gt);
}

/// Session split mid-stream: the boundary frame's sidecar ok bit (session
/// compare against the baked template) routes it cold — the dispatch
/// ladder must run exactly as the classic path would.
#[test]
fn t_r12_soa_parity_session_split() {
    let gt = mini_gt(4000);
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        session_change_at_msg: Some(1500),
        guarantee_coverage: true,
        ..Default::default()
    };
    assert_soa_parity("soa-session-split", &cfg, &gt);
}

/// Single-feed + SeededRange: NO dups at all — every group fails pair-eq
/// and the whole pass runs the scalar fallback through the SoA entry
/// point (the None-ladder equivalence plus the rejected-group path).
#[test]
fn t_r12_soa_parity_single_feed_seeded() {
    let gt = mini_gt(4000);
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::SeededRange { min: 3, max: 40 },
        feeds_enabled: 1,
        guarantee_coverage: true,
        ..Default::default()
    };
    assert_soa_parity("soa-single-feed", &cfg, &gt);
}

/// Multi-pass with fresh sessions through the SoA path (the sustained
/// arm's shape): each pass re-bakes, the sidecar's ok bits re-key on the
/// new template, and the per-pass tuples must match the classic legs.
#[test]
fn t_r12_soa_parity_multi_pass_resets() {
    let gt = mini_gt(3000);
    let sched = build_schedule(&gt, &default_cfg());
    let sess = *b"SOAPAR0002";
    let mut t_classic = ReplayTransport::new(&gt, sched.clone(), sess);
    let mut t_pl =
        nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(&gt, sched, sess, 128);
    let ladder = crate::soa::ladder8_best();
    for pass in 0..3u64 {
        let mut s2 = *b"SOAPAR0002";
        s2[7..10].copy_from_slice(&(200 + pass).to_be_bytes()[5..8]);
        let c = {
            t_classic.reset(s2);
            let mut seq = Sequencer::new();
            let mut sink = SpanConformanceSink::new();
            let mut batch = FrameBatch::new();
            while t_classic.poll(&mut batch) > 0 {
                let now = t_classic.now_ns();
                for (pos, frame) in batch.frames().iter().enumerate() {
                    seq.ingest_auto(
                        frame.bytes(),
                        frame.feed,
                        now,
                        &mut sink,
                        t_classic.batch_blocks(pos),
                        t_classic.batch_memo(pos),
                    );
                }
            }
            (seq.counters(), seq.watermark(), sink.count, sink.hash)
        };
        let v = {
            let t = &mut t_pl;
            t.reset(s2);
            let mut seq = Sequencer::new();
            let mut sink = SpanConformanceSink::new();
            while t.next_batch() {
                seq.ingest_entries_ladder(t.entries(), t.now_ns(), &mut sink, ladder);
            }
            (seq.counters(), seq.watermark(), sink.count, sink.hash)
        };
        assert_eq!(c, v, "soa multi-pass {pass} diverged");
    }
}

/// Fast (CRC32C) conformance sink variant.
#[test]
fn t_r8_batch_parity_fast_sink() {
    let gt = mini_gt(3000);
    let sched = build_schedule(&gt, &default_cfg());
    let sess = *b"PARITY0003";

    let run = |batched: bool| -> (u64, u64, u64) {
        let mut t = ReplayTransport::new(&gt, sched.clone(), sess);
        t.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = FastConformanceSink::new();
        let mut batch = FrameBatch::new();
        while t.poll(&mut batch) > 0 {
            let now = t.now_ns();
            if batched {
                seq.ingest_batch(t.batch_entries(&batch), now, &mut sink);
            } else {
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
        }
        (sink.count(), sink.hash(), seq.watermark())
    };
    assert_eq!(run(false), run(true), "fast conformance sink diverged");
}

/// Multi-pass reset cycle (the sustained-arm pattern): fresh sessions per
/// pass, batch loop re-hoisting after every reset — parity per pass.
#[test]
fn t_r8_batch_parity_multi_pass_reset() {
    let gt = mini_gt(2500);
    let sched = build_schedule(&gt, &default_cfg());
    let mut t_classic = ReplayTransport::new(&gt, sched.clone(), *b"PARITY0004");
    let mut t_batch = ReplayTransport::new(&gt, sched, *b"PARITY0004");

    for pass in 0..4u64 {
        let mut sess = *b"PARITY0004";
        sess[7..10].copy_from_slice(&(1000 + pass).to_be_bytes()[5..8]);
        let c = classic_pass(&mut t_classic, sess);
        let b = batch_pass(&mut t_batch, sess);
        assert_eq!(c, b, "pass {pass} diverged");
        assert!(c.2 > 0, "pass {pass} emitted nothing");
    }
}

/// Deterministic synthetic ground truth: [len|msg] chain of 12B System
/// Event messages (same shape as the transport tests').
#[allow(clippy::disallowed_types)]
pub fn mini_gt(count: u64) -> Vec<u8> {
    let mut gt = Vec::new();
    for i in 0..count {
        let mut msg = [b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O'];
        msg[1..9].copy_from_slice(&(i + 1).to_be_bytes());
        gt.extend_from_slice(&(msg.len() as u16).to_be_bytes());
        gt.extend_from_slice(&msg);
    }
    gt
}

/// R8: RX-coalesced pacing (set_poll_coalesce(k)) must be observationally
/// identical to the exact default pacing on non-decreasing-vt schedules
/// (the DelayModel::None configuration every throughput arm renders): the
/// same frames, in the same order, with the same emissions and counters.
/// The per-batch `now` values differ (coalesced polls report the latest
/// released group's vt) — the sequencer's emissions and counters do not
/// depend on it in lossless schedules (no gaps, no intents).
#[test]
fn t_r8_coalesced_pacing_parity() {
    let gt = mini_gt(3000);
    let cfg = default_cfg();
    let sched = build_schedule(&gt, &cfg);
    let sess = *b"PARITY0009";
    let mut t_a = ReplayTransport::new(&gt, sched.clone(), sess);
    let mut t_b = ReplayTransport::new(&gt, sched.clone(), sess);
    let mut t_c = ReplayTransport::new(&gt, sched, sess);
    t_b.set_poll_coalesce(4);
    t_c.set_poll_coalesce(8);

    let run = |t: &mut ReplayTransport| -> (nf_arbitrator::Counters, u64, u64, u64) {
        t.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = SpanConformanceSink::new();
        let mut batch = FrameBatch::new();
        while t.poll(&mut batch) > 0 {
            let now = t.now_ns();
            seq.ingest_batch(t.batch_entries(&batch), now, &mut sink);
        }
        (seq.counters(), seq.watermark(), sink.count, sink.hash)
    };
    let a = run(&mut t_a);
    let b = run(&mut t_b);
    let c = run(&mut t_c);
    assert_eq!(a, b, "coalesce=4 diverged from exact pacing");
    assert_eq!(a, c, "coalesce=8 diverged from exact pacing");
    assert!(a.2 > 0);
}

/// R8: the RX-PIPELINED transport must be observationally identical to the
/// single-threaded transport on the canonical schedule — same frames, same
/// order, same `now_ns` values per batch, same counters/watermark/count/
/// hash — including the multi-pass reset cycle with fresh sessions.
#[test]
fn t_r8_pipeline_parity_canonical() {
    let gt = mini_gt(3000);
    let cfg = default_cfg();
    let sched = build_schedule(&gt, &cfg);
    let sess = *b"PIPEPAR001";

    let mut t_st = ReplayTransport::new(&gt, sched.clone(), sess);
    t_st.set_poll_coalesce(128);
    let mut t_pl = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
        &gt, sched, sess, 128,
    );

    let run_st = |t: &mut ReplayTransport| -> (nf_arbitrator::Counters, u64, u64, u64) {
        t.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = SpanConformanceSink::new();
        let mut batch = FrameBatch::new();
        while t.poll(&mut batch) > 0 {
            let now = t.now_ns();
            seq.ingest_batch(t.batch_entries(&batch), now, &mut sink);
        }
        (seq.counters(), seq.watermark(), sink.count, sink.hash)
    };
    let run_pl =
        |t: &mut nf_transport::pipeline::PipelinedReplayTransport| -> (nf_arbitrator::Counters, u64, u64, u64) {
            t.reset(sess);
            let mut seq = Sequencer::new();
            let mut sink = SpanConformanceSink::new();
            while t.next_batch() {
                seq.ingest_entries(t.entries(), t.now_ns(), &mut sink);
            }
            (seq.counters(), seq.watermark(), sink.count, sink.hash)
        };

    let a = run_st(&mut t_st);
    let b = run_pl(&mut t_pl);
    assert_eq!(a, b, "pipelined transport diverged from single-threaded");

    // Multi-pass reset cycle with fresh sessions.
    for pass in 0..3u64 {
        let mut s2 = *b"PIPEPAR001";
        s2[7..10].copy_from_slice(&(100 + pass).to_be_bytes()[5..8]);
        t_st.reset(s2);
        let mut seq1 = Sequencer::new();
        let mut sink1 = SpanConformanceSink::new();
        let mut batch = FrameBatch::new();
        while t_st.poll(&mut batch) > 0 {
            let now = t_st.now_ns();
            seq1.ingest_batch(t_st.batch_entries(&batch), now, &mut sink1);
        }
        t_pl.reset(s2);
        let mut seq2 = Sequencer::new();
        let mut sink2 = SpanConformanceSink::new();
        while t_pl.next_batch() {
            seq2.ingest_entries(t_pl.entries(), t_pl.now_ns(), &mut sink2);
        }
        assert_eq!(
            (seq1.counters(), seq1.watermark(), sink1.count, sink1.hash),
            (seq2.counters(), seq2.watermark(), sink2.count, sink2.hash),
            "pipelined pass {pass} diverged"
        );
    }
}

// ─── I-7: the prepatch-race hardening soak (docs/29 §I-7) ────────────────

/// I-7: the chaos harness — the sustained auto-advance shape (the fleet
/// arm's exact structure) under DETERMINISTIC consumer chaos: tardy pass
/// starts (the freed frontier parked on the previous pass's EOS marker
/// through the RX's full runahead — the draw-19 window, forced), mid-pass
/// consumer stalls, and periodic mid-pass abandons (the unstick path).
/// The schedule is a delayed dual-feed at multi-publication scale — feed
/// 1's duplicates lag feed 0's primaries, so regions straddle publication
/// boundaries exactly like the real corpus (the adjacent-pair test
/// schedules never straddle, which is why this class survived every
/// existing suite).
///
/// Every completed pass must reproduce the per-session reference tuple
/// (count, hash, msg_hash) exactly, and every entry must carry ITS pass's
/// session in the frame bytes — the EOS-marker sentinel over-patch (the
/// draw-19 +35 / R9 +39 count-divergence class) breaks the session
/// invariant first and the tuple second.
#[test]
fn t_i7_prepatch_chaos_sustained_soak() {
    // Multi-publication scale: ~2.1k frames per pass (~3 publications at
    // the RX's 776-frame accumulation granularity).
    let gt = mini_gt(120_000);
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        delay: [
            DelayModel::None,
            DelayModel::GaussianApprox {
                mean_ns: 2_000_000,
                sigma_ns: 800_000,
            },
        ],
        guarantee_coverage: true,
        ..Default::default()
    };
    let sched = build_schedule(&gt, &cfg);

    fn prog(pass: u64) -> [u8; 10] {
        const S: [[u8; 10]; 4] = [
            *b"I7SOAKSE01",
            *b"I7SOAKSE02",
            *b"I7SOAKSE03",
            *b"I7SOAKSE04",
        ];
        if pass == 0 {
            S[0]
        } else {
            S[((pass - 1) % 4) as usize]
        }
    }

    // Per-session sequential references (the classic legs — the parity
    // law's ground truth, one per session in the rotation).
    let mut want = Vec::new();
    for k in 0..4u64 {
        let sess = prog(k + 1);
        let mut t = ReplayTransport::new(&gt, sched.clone(), sess);
        t.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = SpanConformanceSink::new();
        let mut batch = FrameBatch::new();
        while t.poll(&mut batch) > 0 {
            seq.ingest_batch(t.batch_entries(&batch), t.now_ns(), &mut sink);
        }
        want.push((sink.count, sink.hash, sink.msg_hash));
    }

    let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce_cpu_auto(
        &gt,
        sched,
        prog(0),
        128,
        None,
        Some(prog),
    );

    // Deterministic LCG chaos — reproducible failure modes, no RNG dep.
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut draw = move || {
        rng = rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        rng
    };

    const PASSES: u64 = 160;
    const ABANDON_EVERY: u64 = 37;
    let mut checked = 0u64;
    for pass in 1..=PASSES {
        let sess = prog(pass);
        t.reset_pass(pass, sess);
        let mut seq = Sequencer::new();
        let mut sink = SpanConformanceSink::new();
        // Tardy start (~35% of passes): park the consumer past the RX's
        // runahead so the freed frontier sits on the previous pass's EOS
        // marker through several publications.
        if draw() % 100 < 35 {
            std::thread::sleep(std::time::Duration::from_millis(1 + draw() % 18));
        }
        let mut abandoned = false;
        let mut batch_idx = 0u64;
        while t.next_batch() {
            batch_idx += 1;
            for e in t.entries() {
                assert_eq!(
                    &e.bytes[..10],
                    &sess[..],
                    "pass {pass} batch {batch_idx}: foreign session in frame \
                     bytes — the EOS-marker sentinel over-patch (the I-7 class)"
                );
            }
            seq.ingest_entries(t.entries(), t.now_ns(), &mut sink);
            // Mid-pass consumer stall (~15% of batches).
            if draw() % 100 < 15 {
                std::thread::sleep(std::time::Duration::from_millis(1 + draw() % 3));
            }
            // Periodic mid-pass abandon (the unstick path).
            if batch_idx == 2 && pass % ABANDON_EVERY == 0 {
                abandoned = true;
                break;
            }
        }
        if !abandoned {
            assert_eq!(
                (sink.count, sink.hash, sink.msg_hash),
                want[((pass - 1) % 4) as usize],
                "pass {pass}: sustained tuple diverged — the I-7 class's \
                 downstream symptom (the re-anchor re-emission)"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= PASSES - PASSES / ABANDON_EVERY,
        "the soak must check every drained pass (checked {checked})"
    );
}

/// F-1 (CHECKLIST F-1 / ROADMAP2 §6.1): the fleet-faithful WARM-START
/// soak — the I-7 chaos schedule (delayed dual-feed, tardy consumer
/// starts, mid-pass stalls, periodic mid-pass abandons) run with the RX
/// frame-entry warm start ARMED, against the per-session classic
/// reference legs. Asserts the three laws the lever lives inside:
/// (1) every entry of every batch carries THIS pass's session (the
///     bake's template rewrite is never trusted blind — the warm
///     compare proves it per frame);
/// (2) every drained pass's tuple is bit-identical to the classic leg
///     (the warm path's observables ARE the classic observables);
/// (3) steady state (pass >= 2): warm fixes == 0 — the pass-invariance
///     law; pass 1's fill count is expected and recorded.
#[test]
fn t_f1_rxwarm_chaos_sustained_soak() {
    // Same multi-publication scale as the I-7 soak (~2.1k frames/pass).
    let gt = mini_gt(120_000);
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        delay: [
            DelayModel::None,
            DelayModel::GaussianApprox {
                mean_ns: 2_000_000,
                sigma_ns: 800_000,
            },
        ],
        guarantee_coverage: true,
        ..Default::default()
    };
    let sched = build_schedule(&gt, &cfg);

    fn prog(pass: u64) -> [u8; 10] {
        const S: [[u8; 10]; 4] = [
            *b"F1WARMSE01",
            *b"F1WARMSE02",
            *b"F1WARMSE03",
            *b"F1WARMSE04",
        ];
        if pass == 0 {
            S[0]
        } else {
            S[((pass - 1) % 4) as usize]
        }
    }

    // Per-session sequential references (the classic legs — ground truth).
    let mut want = Vec::new();
    for k in 0..4u64 {
        let sess = prog(k + 1);
        let mut t = ReplayTransport::new(&gt, sched.clone(), sess);
        t.reset(sess);
        let mut seq = Sequencer::new();
        let mut sink = SpanConformanceSink::new();
        let mut batch = FrameBatch::new();
        while t.poll(&mut batch) > 0 {
            seq.ingest_batch(t.batch_entries(&batch), t.now_ns(), &mut sink);
        }
        want.push((sink.count, sink.hash, sink.msg_hash));
    }

    let mut t = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce_cpu_auto_forced(
        &gt,
        sched,
        prog(0),
        128,
        None,
        Some(prog),
        true,
    );

    // Deterministic LCG chaos (the I-7 soak's exact program).
    let mut rng: u64 = 0x0123_4567_89AB_CDEF;
    let mut draw = move || {
        rng = rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        rng
    };

    const PASSES: u64 = 40;
    const ABANDON_EVERY: u64 = 11;
    let mut checked = 0u64;
    for pass in 1..=PASSES {
        let sess = prog(pass);
        t.reset_pass(pass, sess);
        let mut seq = Sequencer::new();
        let mut sink = SpanConformanceSink::new();
        if draw() % 100 < 35 {
            std::thread::sleep(std::time::Duration::from_millis(1 + draw() % 18));
        }
        let mut abandoned = false;
        let mut batch_idx = 0u64;
        while t.next_batch() {
            batch_idx += 1;
            for e in t.entries() {
                assert_eq!(
                    &e.bytes[..10],
                    &sess[..],
                    "pass {pass} batch {batch_idx}: foreign session in frame bytes — \
                     the warm path published an unverified entry (the F-1 law broken)"
                );
            }
            seq.ingest_entries(t.entries(), t.now_ns(), &mut sink);
            if draw() % 100 < 15 {
                std::thread::sleep(std::time::Duration::from_millis(1 + draw() % 3));
            }
            if batch_idx == 2 && pass % ABANDON_EVERY == 0 {
                abandoned = true;
                break;
            }
        }
        if !abandoned {
            assert_eq!(
                (sink.count, sink.hash, sink.msg_hash),
                want[((pass - 1) % 4) as usize],
                "pass {pass}: sustained tuple diverged under the warm start"
            );
            checked += 1;
        }
        // The steady-state law from pass 2: the last ENDED pass fixed
        // nothing (pass 1's fill is the documented exception).
        if pass >= 2 {
            let (_, uncovered, last_pass) = t.rx_warm_stats();
            assert_eq!(uncovered, 0, "pass {pass}: uncovered warm frames");
            if !abandoned {
                assert_eq!(
                    last_pass, 0,
                    "pass {pass}: steady-state warm fixes must be 0 (pass-invariance broken)"
                );
            }
        }
    }
    assert!(
        checked >= PASSES - PASSES / ABANDON_EVERY,
        "the warm soak must check every drained pass (checked {checked})"
    );
}
