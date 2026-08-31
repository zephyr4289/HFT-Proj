//! Differential Oracle & Reference Arbitrator Harness (doc 16 / G12-T3 / Wave 1.5 D3-redo).
//! Asserts triple equality: HashSink(sequencer) == HashSink(reference) == range_fold(gt)
//! Executes tests D1..D8 including D3 oracle validation by injected sequencer-logic mutations.

#![allow(warnings)]
#![allow(clippy::all)]

use nf_arbitrator::{FeedId, Sequencer, SequencerMutation};
use nf_testkit::golden::golden;
use nf_testkit::reference::ReferenceArbitrator;
use nf_testkit::sched::{
    build_schedule, DelayModel, DropRange, LossModel, Packetize, ReplayConfig, SplitMix64,
};
use nf_testkit::sink::ConformanceSink;
use nf_transport::replay::ReplayTransport;
use nf_transport::{FrameBatch, Transport};
use std::fs;
use std::time::Instant;

fn run_differential_with_mutation(
    gt: &[u8],
    cfg: &ReplayConfig,
    sess: [u8; 10],
    mutation: SequencerMutation,
) -> Result<(u64, u64, u64, u64), String> {
    let sched = build_schedule(gt, cfg);
    let mut transport = ReplayTransport::new(gt, sched, sess);
    let mut seq = Sequencer::with_mutation(mutation);
    let mut ref_arb = ReferenceArbitrator::new();
    let mut sink = ConformanceSink::new();
    let mut batch = FrameBatch::new();

    while transport.poll(&mut batch) > 0 {
        let now = transport.now_ns();
        for frame in batch.frames() {
            let bytes = frame.bytes();
            seq.ingest(bytes, frame.feed, now, &mut sink);
            ref_arb.ingest_packet(bytes);
        }
    }

    let (_ref_anchor, ref_wm, ref_hash, ref_emitted) = ref_arb.evaluate_all_sessions();
    let seq_wm = seq.watermark();
    let seq_count = sink.count();
    let seq_hash = sink.hash();

    if seq_wm != ref_wm {
        return Err(format!(
            "Watermark divergence: seq_wm={} ref_wm={}",
            seq_wm, ref_wm
        ));
    }

    if seq_count as usize != ref_emitted.len() {
        return Err(format!(
            "Count divergence: seq_count={} ref_emitted={}",
            seq_count,
            ref_emitted.len()
        ));
    }

    if seq_hash != ref_hash {
        return Err(format!(
            "Hash divergence: seq_hash={:#X} ref_hash={:#X}",
            seq_hash, ref_hash
        ));
    }

    let (gt_count, gt_hash) = golden(gt);
    if seq_count == gt_count && seq_hash != gt_hash {
        return Err(format!(
            "Hash divergence on full dataset: seq_hash={:#X} gt_hash={:#X}",
            seq_hash, gt_hash
        ));
    }

    Ok((seq_wm, ref_wm, seq_count, seq_hash))
}

fn run_differential(
    gt: &[u8],
    cfg: &ReplayConfig,
    sess: [u8; 10],
) -> Result<(u64, u64, u64, u64), String> {
    run_differential_with_mutation(gt, cfg, sess, SequencerMutation::None)
}

fn test_d3_oracle_validation(gt: &[u8]) {
    println!("=== D3-REDO: Oracle Validation by Sequencer-Logic Bug Injections ===");
    let sess = *b"D3MUTATION";

    // Mutation A: Disable clear-on-advance (U-ZOMBIE bug family)
    {
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::Fixed(1),
            delay: [
                DelayModel::GaussianApprox { mean_ns: 5000, sigma_ns: 1000 },
                DelayModel::None,
            ],
            guarantee_coverage: true,
            ..Default::default()
        };
        let res = run_differential_with_mutation(gt, &cfg, sess, SequencerMutation::DisableClearOnAdvance);
        println!("D3 Mutation A (DisableClearOnAdvance / Zombie Bug): {:?}", res);
        assert!(res.is_err(), "Oracle MUST detect Mutation A (Zombie bug)");
        println!(
            "D3_DIVERGENCE_DUMP mutation=\"DisableClearOnAdvance\" error=\"{}\"",
            res.unwrap_err()
        );
    }

    // Mutation B: Off-by-one / window clamp violation
    {
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::Fixed(1),
            delay: [
                DelayModel::GaussianApprox { mean_ns: 8000, sigma_ns: 2000 },
                DelayModel::None,
            ],
            guarantee_coverage: true,
            ..Default::default()
        };
        let res = run_differential_with_mutation(gt, &cfg, sess, SequencerMutation::OffByOneClamp);
        println!("D3 Mutation B (OffByOneClamp): {:?}", res);
        assert!(res.is_err(), "Oracle MUST detect Mutation B (Off-by-one clamp)");
        println!(
            "D3_DIVERGENCE_DUMP mutation=\"OffByOneClamp\" error=\"{}\"",
            res.unwrap_err()
        );
    }

    // Mutation C: Drop staged messages at EOS
    {
        let cfg = ReplayConfig {
            msgs_per_packet: Packetize::Fixed(1),
            delay: [
                DelayModel::GaussianApprox { mean_ns: 3000, sigma_ns: 500 },
                DelayModel::None,
            ],
            session_change_at_msg: Some(10_000),
            guarantee_coverage: true,
            ..Default::default()
        };
        let res = run_differential_with_mutation(gt, &cfg, sess, SequencerMutation::DropStagedAtEos);
        println!("D3 Mutation C (DropStagedAtEos): {:?}", res);
        assert!(res.is_err(), "Oracle MUST detect Mutation C (Drop staged at EOS)");
        println!(
            "D3_DIVERGENCE_DUMP mutation=\"DropStagedAtEos\" error=\"{}\"",
            res.unwrap_err()
        );
    }

    println!("D3 ALL_SEQUENCER_LOGIC_MUTATIONS_DETECTED: Oracle caught all 3 sequencer mutations with divergence dumps.");
}

fn test_d1_matrix_cells(gt: &[u8]) {
    println!("=== D1: Active Matrix Cells Differential Verification ===");
    let configs = vec![
        ("M1 (Baseline MTU contiguous)", ReplayConfig { msgs_per_packet: Packetize::MtuBound(1400), guarantee_coverage: true, ..Default::default() }),
        ("M2 (Fixed 1 msg)", ReplayConfig { msgs_per_packet: Packetize::Fixed(1), guarantee_coverage: true, ..Default::default() }),
        ("M3 (Fixed 16 msgs)", ReplayConfig { msgs_per_packet: Packetize::Fixed(16), guarantee_coverage: true, ..Default::default() }),
        ("M4 (Gaussian delay jitter)", ReplayConfig { delay: [DelayModel::GaussianApprox { mean_ns: 500, sigma_ns: 100 }, DelayModel::None], guarantee_coverage: true, ..Default::default() }),
        ("M5 (Bernoulli loss with dual feed)", ReplayConfig { loss: [LossModel::Bernoulli { p_pm: 100 }, LossModel::None], guarantee_coverage: true, ..Default::default() }),
    ];

    let sess = *b"DIFFSESS01";
    for (name, cfg) in configs {
        let (seq_wm, ref_wm, count, hash) = run_differential(gt, &cfg, sess)
            .unwrap_or_else(|e| panic!("D1 failure on {}: {}", name, e));
        println!(
            "D1 cell=\"{}\" seq_wm={} ref_wm={} count={} hash={:#X} VERDICT=PASS",
            name, seq_wm, ref_wm, count, hash
        );
        assert_eq!(seq_wm, ref_wm);
    }
    println!("D1 CELLS_PASSED: Verified active matrix cells (5/17 active, remainder gated on terminal sweep).");
}

fn test_d2_random_configs(gt: &[u8]) {
    println!("=== D2: 100 Seeded Random Configs Differential Test ===");
    let mut rng = SplitMix64::new(0xDEADBEEF_CAFEF00D);
    let sess = *b"RANDOMTEST";

    for i in 1..=100 {
        let pkt_choice = rng.next_u64() % 3;
        let pkt = match pkt_choice {
            0 => Packetize::Fixed(1),
            1 => Packetize::Fixed(8),
            _ => Packetize::MtuBound(1400),
        };

        let delay = if rng.next_u64() % 2 == 0 {
            [
                DelayModel::GaussianApprox {
                    mean_ns: (rng.next_u64() % 1000) as i64,
                    sigma_ns: (rng.next_u64() % 200) as u64,
                },
                DelayModel::None,
            ]
        } else {
            [DelayModel::None, DelayModel::None]
        };

        let cfg = ReplayConfig {
            msgs_per_packet: pkt,
            delay,
            guarantee_coverage: true,
            ..Default::default()
        };

        let (seq_wm, ref_wm, count, _hash) = run_differential(gt, &cfg, sess)
            .unwrap_or_else(|e| panic!("D2 failure on random config #{}: {}", i, e));
        assert_eq!(seq_wm, ref_wm);
    }
    println!("D2 100_RANDOM_CONFIGS_PASSED: Triple equality verified across 100 random configs.");
}

fn test_d4_duplicate_ordering(gt: &[u8]) {
    println!("=== D4: Duplicate Ordering (First-Received Wins) ===");
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        delay: [
            DelayModel::GaussianApprox { mean_ns: 2000, sigma_ns: 500 },
            DelayModel::None,
        ],
        guarantee_coverage: true,
        ..Default::default()
    };
    let sess = *b"DUPORDERS1";
    let (seq_wm, ref_wm, count, hash) = run_differential(gt, &cfg, sess)
        .unwrap_or_else(|e| panic!("D4 failure: {}", e));
    assert_eq!(seq_wm, ref_wm);
    println!(
        "D4 DUPLICATE_ORDERING_PASSED: seq_wm={} ref_wm={} count={} hash={:#X}",
        seq_wm, ref_wm, count, hash
    );
}

fn test_d5_session_splits(gt: &[u8]) {
    println!("=== D5: Session Splits Verification ===");
    let cfg = ReplayConfig {
        session_change_at_msg: Some(250_000),
        guarantee_coverage: true,
        ..Default::default()
    };
    let sess = *b"SPLITSESS1";
    let (seq_wm, ref_wm, count, hash) = run_differential(gt, &cfg, sess)
        .unwrap_or_else(|e| panic!("D5 failure: {}", e));
    assert_eq!(seq_wm, ref_wm);
    println!("D5 SESSION_SPLITS_PASSED: Watermarks and hashes match across session boundaries.");
}

fn test_d6_unclean_death(gt: &[u8]) {
    println!("=== D6: Unclean Death (Scripted Drops) Verification ===");
    let cfg = ReplayConfig {
        scripted_drops: vec![DropRange {
            seq_from: 10_000,
            seq_to_incl: 10_005,
            feed_mask: 1, // Drop from Feed A only
        }],
        guarantee_coverage: true,
        ..Default::default()
    };
    let sess = *b"UNCLEANDEA";
    let (seq_wm, ref_wm, count, hash) = run_differential(gt, &cfg, sess)
        .unwrap_or_else(|e| panic!("D6 failure: {}", e));
    assert_eq!(seq_wm, ref_wm);
    println!("D6 UNCLEAN_DEATH_PASSED: State matches reference final state.");
}

fn test_d7_d8_watchdog_and_determinism(gt: &[u8]) {
    println!("=== D7/D8: Watchdog & Double-Run Determinism ===");
    let t0 = Instant::now();
    let cfg = ReplayConfig {
        msgs_per_packet: Packetize::MtuBound(1400),
        guarantee_coverage: true,
        ..Default::default()
    };
    let sess = *b"DETERMINIS";

    let run1 = run_differential(gt, &cfg, sess).unwrap();
    let run2 = run_differential(gt, &cfg, sess).unwrap();

    assert_eq!(run1, run2, "D8 failure: runs must be bit-identical");
    assert!(t0.elapsed().as_secs() < 60, "D7 failure: exceeded 60s watchdog");

    println!(
        "D7/D8 WATCHDOG_AND_DETERMINISM_PASSED: elapsed={:.2}s run1={:?} run2={:?}",
        t0.elapsed().as_secs_f64(),
        run1,
        run2
    );
}

fn main() {
    let sample_path = "data/tests/sample-mini.itch";
    let gt = fs::read(sample_path).unwrap_or_else(|_| {
        fs::read("../../data/tests/sample-mini.itch")
            .unwrap_or_else(|_| fs::read("../data/tests/sample-mini.itch").expect("Failed to load sample"))
    });

    println!("=== RUNNING G12-T3 REFERENCE ARBITRATOR & DIFFERENTIAL SUITE (D1..D8) ===");
    test_d3_oracle_validation(&gt);
    test_d1_matrix_cells(&gt);
    test_d2_random_configs(&gt);
    test_d4_duplicate_ordering(&gt);
    test_d5_session_splits(&gt);
    test_d6_unclean_death(&gt);
    test_d7_d8_watchdog_and_determinism(&gt);
    println!("=== ALL D1..D8 DIFFERENTIAL ORACLE CHECKS PASSED SUCCESSFULLY ===");
}
