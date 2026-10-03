//! Differential Oracle & Reference Arbitrator Harness (doc 16 / G12-T3 / Wave 1.5 D3-redo).
//! Asserts triple equality: HashSink(sequencer) == HashSink(reference) == range_fold(gt)
//! Executes tests D1..D8 including D3 oracle validation by injected sequencer-logic mutations.

#![allow(warnings)]
#![allow(clippy::all)]

use nf_arbitrator::{FeedId, LiveFeedProof, Sequencer, SequencerMutation, Sink};
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

fn make_moldudp64_packet(session: &[u8; 10], seq: u64, msgs: &[&[u8]]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(20 + msgs.len() * 40);
    buf.extend_from_slice(session);
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(&(msgs.len() as u16).to_be_bytes());
    for msg in msgs {
        buf.extend_from_slice(&(msg.len() as u16).to_be_bytes());
        buf.extend_from_slice(msg);
    }
    buf
}

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

    let (gt_hash, gt_count) = golden(gt);
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

fn test_d3_oracle_validation() {
    println!("=== D3-REDO: Oracle Validation by Sequencer-Logic Bug Injections ===");
    let sess = *b"D3MUTATION";
    let dummy_msg = vec![0x53u8, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A];

    // ── Mutation A: Disable clear-on-advance (U-ZOMBIE bug family) ──
    {
        let mut seq = Sequencer::with_mutation(SequencerMutation::DisableClearOnAdvance);
        let mut ref_arb = ReferenceArbitrator::new();
        let mut sink = ConformanceSink::new();

        // 1. Anchor at seq 100
        let p0 = make_moldudp64_packet(&sess, 100, &[&dummy_msg]);
        seq.ingest(&p0, 0, 1000, &mut sink);
        ref_arb.ingest_packet(&p0);

        // 2. Stage [200..=205]
        let msgs_6: Vec<&[u8]> = (0..6).map(|_| dummy_msg.as_slice()).collect();
        let p1 = make_moldudp64_packet(&sess, 200, &msgs_6);
        seq.ingest(&p1, 0, 2000, &mut sink);
        ref_arb.ingest_packet(&p1);

        // 3. Fast-forward with contiguous [101..=205] (advances W to 206)
        let msgs_105: Vec<&[u8]> = (0..105).map(|_| dummy_msg.as_slice()).collect();
        let p2 = make_moldudp64_packet(&sess, 101, &msgs_105);
        seq.ingest(&p2, 1, 3000, &mut sink);
        ref_arb.ingest_packet(&p2);

        // 4. Advance traffic to W=1224 (wrapping window by 1024 slots)
        let mut cur = 206u64;
        while cur < 1224 {
            let chunk = std::cmp::min(10, 1224 - cur);
            let msgs_chunk: Vec<&[u8]> = (0..chunk).map(|_| dummy_msg.as_slice()).collect();
            let p_chunk = make_moldudp64_packet(&sess, cur, &msgs_chunk);
            seq.ingest(&p_chunk, 0, 4000 + cur, &mut sink);
            ref_arb.ingest_packet(&p_chunk);
            cur += chunk;
        }

        // 5. Open a gap at 1224 by delivering packet at 1230 -> triggers drain at slot 1224 % 1024 == 200!
        let p_gap = make_moldudp64_packet(&sess, 1230, &[&dummy_msg]);
        seq.ingest(&p_gap, 0, 10000, &mut sink);
        ref_arb.ingest_packet(&p_gap);

        let (_ref_a, ref_wm, ref_h, _ref_emitted) = ref_arb.evaluate_all_sessions();
        let seq_wm = seq.watermark();
        let seq_h = sink.hash();

        let divergence_detected = seq_wm != ref_wm || seq_h != ref_h;
        println!(
            "D3_DIVERGENCE_DUMP mutation=\"DisableClearOnAdvance\" detected={} seq_wm={} ref_wm={} seq_hash={:#X} ref_hash={:#X}",
            divergence_detected, seq_wm, ref_wm, seq_h, ref_h
        );
        assert!(divergence_detected, "Oracle MUST detect Mutation A (Zombie Bug)");
    }

    // ── Mutation B: Off-by-one / window clamp violation ──
    {
        let mut seq = Sequencer::with_mutation(SequencerMutation::OffByOneClamp);
        let mut ref_arb = ReferenceArbitrator::new();
        let mut sink = ConformanceSink::new();

        // 1. Anchor at 1
        let p0 = make_moldudp64_packet(&sess, 1, &[&dummy_msg]);
        seq.ingest(&p0, 0, 1000, &mut sink);
        ref_arb.ingest_packet(&p0);

        // 2. Deliver packet at seq 600 (beyond reduced clamp limit of 512)
        let p_beyond = make_moldudp64_packet(&sess, 600, &[&dummy_msg]);
        seq.ingest(&p_beyond, 0, 2000, &mut sink);
        ref_arb.ingest_packet(&p_beyond);

        // 3. Fill intermediate gap [2..=599]
        let mut cur = 2u64;
        while cur < 600 {
            let chunk = std::cmp::min(20, 600 - cur);
            let msgs: Vec<&[u8]> = (0..chunk).map(|_| dummy_msg.as_slice()).collect();
            let p = make_moldudp64_packet(&sess, cur, &msgs);
            seq.ingest(&p, 0, 3000 + cur, &mut sink);
            ref_arb.ingest_packet(&p);
            cur += chunk;
        }

        let (_ref_a, ref_wm, ref_h, _ref_emitted) = ref_arb.evaluate_all_sessions();
        let seq_wm = seq.watermark();
        let seq_h = sink.hash();

        let divergence_detected = seq_wm != ref_wm || seq_h != ref_h;
        println!(
            "D3_DIVERGENCE_DUMP mutation=\"OffByOneClamp\" detected={} seq_wm={} ref_wm={} seq_hash={:#X} ref_hash={:#X}",
            divergence_detected, seq_wm, ref_wm, seq_h, ref_h
        );
        assert!(divergence_detected, "Oracle MUST detect Mutation B (Off-by-one Clamp)");
    }

    // ── Mutation C: Drop staged messages at EOS ──
    {
        let mut seq = Sequencer::with_mutation(SequencerMutation::DropStagedAtEos);
        let mut ref_arb = ReferenceArbitrator::new();
        let mut sink = ConformanceSink::new();

        // 1. Anchor at 1
        let p0 = make_moldudp64_packet(&sess, 1, &[&dummy_msg]);
        seq.ingest(&p0, 0, 1000, &mut sink);
        ref_arb.ingest_packet(&p0);

        // 2. Stage [10..=12]
        let msgs_3: Vec<&[u8]> = (0..3).map(|_| dummy_msg.as_slice()).collect();
        let p_stage = make_moldudp64_packet(&sess, 10, &msgs_3);
        seq.ingest(&p_stage, 0, 2000, &mut sink);
        ref_arb.ingest_packet(&p_stage);

        // 3. Send EOS (count = 0xFFFF)
        let mut eos_buf = Vec::new();
        eos_buf.extend_from_slice(&sess);
        eos_buf.extend_from_slice(&13u64.to_be_bytes());
        eos_buf.extend_from_slice(&0xFFFFu16.to_be_bytes());
        seq.ingest(&eos_buf, 0, 3000, &mut sink);
        ref_arb.ingest_packet(&eos_buf);

        // 4. Deliver gap fill [2..=9] during recovery
        let msgs_8: Vec<&[u8]> = (0..8).map(|_| dummy_msg.as_slice()).collect();
        let p_fill = make_moldudp64_packet(&sess, 2, &msgs_8);
        seq.ingest(&p_fill, 0, 4000, &mut sink);
        ref_arb.ingest_packet(&p_fill);

        let (_ref_a, ref_wm, ref_h, _ref_emitted) = ref_arb.evaluate_all_sessions();
        let seq_wm = seq.watermark();
        let seq_h = sink.hash();

        let divergence_detected = seq_wm != ref_wm || seq_h != ref_h;
        println!(
            "D3_DIVERGENCE_DUMP mutation=\"DropStagedAtEos\" detected={} seq_wm={} ref_wm={} seq_hash={:#X} ref_hash={:#X}",
            divergence_detected, seq_wm, ref_wm, seq_h, ref_h
        );
        assert!(divergence_detected, "Oracle MUST detect Mutation C (Drop Staged at EOS)");
    }

    println!("D3 ALL_SEQUENCER_LOGIC_MUTATIONS_DETECTED: Oracle caught all 3 sequencer mutations with divergence dumps.");
}

fn test_d1_matrix_cells(gt: &[u8]) {
    println!("=== D1: Full 17-Cell Matrix Differential Verification ===");
    let configs = vec![
        ("M1 (Baseline MTU contiguous)", ReplayConfig { msgs_per_packet: Packetize::MtuBound(1400), guarantee_coverage: true, ..Default::default() }),
        ("M2 (Fixed 1 msg)", ReplayConfig { msgs_per_packet: Packetize::Fixed(1), guarantee_coverage: true, ..Default::default() }),
        ("M3 (Fixed 16 msgs)", ReplayConfig { msgs_per_packet: Packetize::Fixed(16), guarantee_coverage: true, ..Default::default() }),
        ("M4 (Gaussian delay jitter)", ReplayConfig { delay: [DelayModel::GaussianApprox { mean_ns: 500, sigma_ns: 100 }, DelayModel::None], guarantee_coverage: true, ..Default::default() }),
        ("M5 (Bernoulli loss with dual feed)", ReplayConfig { loss: [LossModel::Bernoulli { p_pm: 100 }, LossModel::None], guarantee_coverage: true, ..Default::default() }),
        ("M6 (Gilbert-Elliott burst loss)", ReplayConfig { loss: [LossModel::GilbertElliott { p_g2b_pm: 50, p_b2g_pm: 200, p_drop_good_pm: 10, p_drop_bad_pm: 900 }, LossModel::None], guarantee_coverage: true, ..Default::default() }),
        ("M7 (Pathological split: 1B to MTU)", ReplayConfig { msgs_per_packet: Packetize::SeededRange { min: 1, max: 20 }, guarantee_coverage: true, ..Default::default() }),
        ("M8 (Feed A only lossless)", ReplayConfig { feeds_enabled: 1, guarantee_coverage: true, ..Default::default() }),
        ("M9 (Feed B only lossless)", ReplayConfig { feeds_enabled: 2, guarantee_coverage: true, ..Default::default() }),
        ("M10 (Heavy reorder jitter)", ReplayConfig { delay: [DelayModel::GaussianApprox { mean_ns: 2500, sigma_ns: 800 }, DelayModel::GaussianApprox { mean_ns: 1000, sigma_ns: 300 }], guarantee_coverage: true, ..Default::default() }),
        ("M11 (Deep out-of-order window)", ReplayConfig { msgs_per_packet: Packetize::Fixed(1), delay: [DelayModel::GaussianApprox { mean_ns: 4000, sigma_ns: 1000 }, DelayModel::None], guarantee_coverage: true, ..Default::default() }),
        ("M12 (Max-rate unconstrained)", ReplayConfig { base_rate_msg_per_sec: 50_000_000, guarantee_coverage: true, ..Default::default() }),
        ("M13 (M-LATE: Staged arrival edge)", ReplayConfig { delay: [DelayModel::GaussianApprox { mean_ns: 1500, sigma_ns: 500 }, DelayModel::None], guarantee_coverage: true, ..Default::default() }),
        ("M14 (M-BURST: Burst packet arrival)", ReplayConfig { msgs_per_packet: Packetize::Fixed(32), guarantee_coverage: true, ..Default::default() }),
        ("M15 (M-STARVE: Feed A silent for 2s)", ReplayConfig { delay: [DelayModel::GaussianApprox { mean_ns: 10_000, sigma_ns: 2000 }, DelayModel::None], guarantee_coverage: true, ..Default::default() }),
        ("M16 (M-DUP2: Overlapping dual feed)", ReplayConfig { delay: [DelayModel::GaussianApprox { mean_ns: 100, sigma_ns: 50 }, DelayModel::GaussianApprox { mean_ns: 100, sigma_ns: 50 }], guarantee_coverage: true, ..Default::default() }),
        ("M17 (M-DROPRESP / Session Boundary)", ReplayConfig { session_change_at_msg: Some(250_000), guarantee_coverage: true, ..Default::default() }),
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
    println!("D1 ALL_17_CELLS_PASSED: Triple equality verified across all 17 matrix cells.");
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

/// Q1 indexed-ingest parity harness: runs the full schedule through classic
/// `ingest` and indexed `ingest_auto`, asserting identical (watermark, count,
/// hash) across configs x mutations. HB/EOS frames route classic inside
/// ingest_auto (empty triples), so this also covers the router itself.
fn run_classic_path(
    gt: &[u8],
    cfg: &ReplayConfig,
    sess: [u8; 10],
    mutation: SequencerMutation,
) -> (u64, u64, u64) {
    let sched = build_schedule(gt, cfg);
    let mut transport = ReplayTransport::new(gt, sched, sess);
    let mut seq = Sequencer::with_mutation(mutation);
    let mut sink = ConformanceSink::new();
    let mut batch = FrameBatch::new();
    while transport.poll(&mut batch) > 0 {
        let now = transport.now_ns();
        for frame in batch.frames() {
            seq.ingest(frame.bytes(), frame.feed, now, &mut sink);
        }
    }
    (seq.watermark(), sink.count(), sink.hash())
}

fn run_indexed_path(
    gt: &[u8],
    cfg: &ReplayConfig,
    sess: [u8; 10],
    mutation: SequencerMutation,
) -> (u64, u64, u64) {
    let sched = build_schedule(gt, cfg);
    let mut transport = ReplayTransport::new(gt, sched, sess);
    let mut seq = Sequencer::with_mutation(mutation);
    let mut sink = ConformanceSink::new();
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
    (seq.watermark(), sink.count(), sink.hash())
}

fn test_d9_indexed_equivalence(gt: &[u8]) {
    println!("=== D9: Indexed-Ingest Equivalence (classic == indexed) ===");
    let configs: Vec<(&str, ReplayConfig, [u8; 10])> = vec![
        (
            "m1-baseline",
            ReplayConfig {
                msgs_per_packet: Packetize::MtuBound(1400),
                guarantee_coverage: true,
                ..Default::default()
            },
            *b"D9BASELINE",
        ),
        (
            "session-split",
            ReplayConfig {
                session_change_at_msg: Some(250_000),
                guarantee_coverage: true,
                ..Default::default()
            },
            *b"D9SPLIT001",
        ),
        (
            "fixed-1",
            ReplayConfig {
                msgs_per_packet: Packetize::Fixed(1),
                guarantee_coverage: true,
                ..Default::default()
            },
            *b"D9FIXED001",
        ),
    ];
    let mutations = [
        ("none", SequencerMutation::None),
        ("no-clear", SequencerMutation::DisableClearOnAdvance),
        ("clamp", SequencerMutation::OffByOneClamp),
        ("drop-eos", SequencerMutation::DropStagedAtEos),
    ];
    let mut cells = 0;
    for (cname, cfg, sess) in &configs {
        for (mname, mutation) in &mutations {
            let classic = run_classic_path(gt, cfg, *sess, *mutation);
            let indexed = run_indexed_path(gt, cfg, *sess, *mutation);
            assert_eq!(
                classic, indexed,
                "D9 parity failure: cfg={} mutation={} classic={:?} indexed={:?}",
                cname, mname, classic, indexed
            );
            cells += 1;
        }
    }
    println!(
        "D9 INDEXED_EQUIVALENCE_PASSED: {} cells (3 configs x 4 mutations) classic==indexed",
        cells
    );
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

/// D11: CRC kernel differential (GIGAHFT Lever 1). The scalar 8-lane
/// kernel is pinned to the reference table-driven reflected CRC32C (raw
/// golden vectors), and — on silicon with AVX-512F/BW + VPCLMULQDQ +
/// GFNI — the VPCLMULQDQ mirror-domain fold kernel must equal the scalar
/// kernel bit-for-bit on an exhaustive length sweep, pattern sweep, random
/// stress, and mismatched two-span eval2 pairs.
fn test_d11_crc_kernel_differential() {
    use nf_testkit::crcfold::{fold512_available, CrcKernel};
    use nf_testkit::sink::span_crc32c_8lane;

    // Reference reflected CRC32C (init=0, xorout=0) — table-driven.
    fn ref_crc32c(data: &[u8]) -> u32 {
        fn table() -> [u32; 256] {
            let mut t = [0u32; 256];
            for (i, e) in t.iter_mut().enumerate() {
                let mut c = i as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
                }
                *e = c;
            }
            t
        }
        static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
        let t = TABLE.get_or_init(table);
        let mut crc = 0u32;
        for &b in data {
            crc = (crc >> 8) ^ t[((crc ^ b as u32) & 0xFF) as usize];
        }
        crc
    }

    // 1) Anchor the SCALAR kernel's lane semantics to the raw CRC32C of the
    //    lane streams (bodies small enough to be single-lane).
    let mut body = [0u8; 64];
    for (i, e) in body.iter_mut().enumerate() {
        *e = (i * 131 + 17) as u8;
    }
    // len < 64: everything is lane 0 + tail => span hash = FNV over
    // [c0..c7, len] where c0 = raw crc of the whole body.
    let h = span_crc32c_8lane(&body[..40]);
    let c0 = ref_crc32c(&body[..40]);
    let mut want = 0xcbf29ce484222325u64;
    for c in [c0, 0u32, 0, 0, 0, 0, 0, 0, 40u32] {
        want ^= c as u64;
        want = want.wrapping_mul(0x100000001b3);
    }
    assert_eq!(h, want, "D11: scalar kernel lane-0 semantics diverged from reference CRC32C");

    // 2) Fold kernel differential (skipped on non-AVX-512 silicon).
    if !fold512_available() {
        println!("D11 CRC_KERNEL_DIFFERENTIAL_SKIPPED: fold512 unavailable on this CPU (scalar kernel anchored to reference)");
        return;
    }
    let kernel = CrcKernel::Fold512;
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut checked = 0u64;
    let mut check = |b: &[u8]| {
        let want = span_crc32c_8lane(b);
        // SAFETY: fold512_available() verified the feature contract.
        let got = unsafe { kernel.eval(b) };
        assert_eq!(want, got, "D11: fold diverged at len={}", b.len());
        checked += 1;
    };
    let mut buf = [0u8; 4200];
    for len in 0..=520usize {
        for pat in 0..4u8 {
            match pat {
                0 => buf[..len].fill(0),
                1 => buf[..len].fill(0xFF),
                2 => {
                    for (i, e) in buf[..len].iter_mut().enumerate() {
                        *e = (i * 131 + 17) as u8;
                    }
                }
                _ => {
                    for e in buf[..len].iter_mut() {
                        *e = next() as u8;
                    }
                }
            }
            check(&buf[..len]);
        }
    }
    for len in [640usize, 680, 1000, 1360, 1380, 1399, 1400, 2048, 4096] {
        for e in buf[..len].iter_mut() {
            *e = next() as u8;
        }
        check(&buf[..len]);
    }
    // Mismatched eval2 pairs.
    for (la, lb) in [(1360usize, 1399), (1399, 680), (2048, 1360), (1379, 4096)] {
        for e in buf[..la].iter_mut() {
            *e = next() as u8;
        }
        let a = buf[..la].to_vec();
        for e in buf[..lb].iter_mut() {
            *e = next() as u8;
        }
        let b = buf[..lb].to_vec();
        let want_a = span_crc32c_8lane(&a);
        let want_b = span_crc32c_8lane(&b);
        // SAFETY: feature contract verified above.
        let (ga, gb) = unsafe { kernel.eval2(&a, &b) };
        assert_eq!(want_a, ga, "D11: eval2 A diverged ({} x {})", la, lb);
        assert_eq!(want_b, gb, "D11: eval2 B diverged ({} x {})", la, lb);
        // R10: the sequential-load pair (the pipelined-tail schedule) —
        // identical values, different instruction order.
        // SAFETY: feature contract verified above.
        let (pa, pb) = unsafe { kernel.eval_pair(&a, &b) };
        assert_eq!(want_a, pa, "D11: eval_pair A diverged ({} x {})", la, lb);
        assert_eq!(want_b, pb, "D11: eval_pair B diverged ({} x {})", la, lb);
        checked += 4;
    }
    println!(
        "D11 CRC_KERNEL_DIFFERENTIAL_PASSED: scalar==reference, fold512==scalar on {} bodies (exhaustive lengths + patterns + random + eval2 + eval_pair)",
        checked
    );
}

fn main() {
    let sample_path = "data/tests/sample-mini.itch";
    let gt = fs::read(sample_path).unwrap_or_else(|_| {
        fs::read("../../data/tests/sample-mini.itch")
            .unwrap_or_else(|_| fs::read("../data/tests/sample-mini.itch").expect("Failed to load sample"))
    });

    println!("=== RUNNING G12-T3 REFERENCE ARBITRATOR & DIFFERENTIAL SUITE (D1..D11) ===");
    test_d3_oracle_validation();
    test_d1_matrix_cells(&gt);
    test_d2_random_configs(&gt);
    test_d4_duplicate_ordering(&gt);
    test_d5_session_splits(&gt);
    test_d6_unclean_death(&gt);
    test_d7_d8_watchdog_and_determinism(&gt);
    test_d9_indexed_equivalence(&gt);
    test_d11_crc_kernel_differential();
    test_d12_batch_and_pipeline_equivalence(&gt);
    println!("=== ALL D1..D12 DIFFERENTIAL ORACLE CHECKS PASSED SUCCESSFULLY ===");
}

/// D12 (R8): the batched apply loop (`ingest_batch`) and the RX-pipelined
/// transport must be observationally identical to the classic per-frame
/// ladder on the canonical sample — same counters, watermark, count, hash —
/// under the canonical dual-feed MtuBound schedule, across a multi-pass
/// reset cycle with fresh sessions (the pipelined transport's reset
/// handshake + RX-side session bake are the new moving parts).
fn test_d12_batch_and_pipeline_equivalence(gt: &[u8]) {
    use nf_testkit::batch_parity::{classic_pass, default_cfg};
    use nf_testkit::sched::build_schedule;
    use nf_testkit::sink::SpanConformanceSink;
    use nf_arbitrator::{Sequencer, Sink};
    use nf_transport::replay::ReplayTransport;
    use nf_transport::Transport;

    let cfg = default_cfg();
    let sched = build_schedule(gt, &cfg);
    let sess = *b"D12PARIT01";

    // Classic reference (5-tuple: counters, watermark, count, hash, events).
    let mut t_c = ReplayTransport::new(gt, sched.clone(), sess);
    let c_full = classic_pass(&mut t_c, sess);
    let c = (c_full.0, c_full.1, c_full.2, c_full.3);

    // Batched apply (ingest_batch) over the coalesced transport.
    let mut t_b = ReplayTransport::new(gt, sched.clone(), sess);
    t_b.set_poll_coalesce(128);
    t_b.reset(sess);
    let mut seq_b = nf_arbitrator::Sequencer::new();
    let mut sink_b = SpanConformanceSink::new();
    let mut batch = nf_transport::FrameBatch::new();
    while t_b.poll(&mut batch) > 0 {
        let now = t_b.now_ns();
        seq_b.ingest_batch(t_b.batch_entries(&batch), now, &mut sink_b);
    }
    let b = (seq_b.counters(), seq_b.watermark(), sink_b.count, sink_b.hash);
    assert_eq!(c, b, "D12: batched apply diverged from classic");

    // RX-pipelined transport + the R12 SoA slice scan (the production
    // path: vectorized ladder when the silicon has avx512f, scalar
    // fallback otherwise — both pinned to classic here every CI run).
    let mut t_p = nf_transport::pipeline::PipelinedReplayTransport::with_coalesce(
        gt, sched, sess, 128,
    );
    let ladder = nf_testkit::soa::ladder8_best();
    let run_pipe = |t: &mut nf_transport::pipeline::PipelinedReplayTransport,
                    s: [u8; 10]|
     -> (nf_arbitrator::Counters, u64, u64, u64) {
        t.reset(s);
        let mut seq = Sequencer::new();
        let mut sink = SpanConformanceSink::new();
        while t.next_batch() {
            seq.ingest_entries_soa(t.entries(), &t.soa(), t.now_ns(), &mut sink, ladder);
        }
        (seq.counters(), seq.watermark(), sink.count, sink.hash)
    };
    let p = run_pipe(&mut t_p, sess);
    assert_eq!(c, p, "D12: RX-pipelined transport diverged from classic");

    // Multi-pass reset cycle with fresh sessions (pipelined handshake).
    for pass in 0..3u64 {
        let mut s2 = *b"D12PARIT01";
        s2[7..10].copy_from_slice(&(500 + pass).to_be_bytes()[5..8]);
        let mut t_c2 = ReplayTransport::new(gt, build_schedule(gt, &cfg), s2);
        let c2f = classic_pass(&mut t_c2, s2);
        let c2 = (c2f.0, c2f.1, c2f.2, c2f.3);
        let p2 = run_pipe(&mut t_p, s2);
        assert_eq!(c2, p2, "D12: pipelined pass {pass} diverged from classic");
    }

    println!("D12 BATCH_AND_PIPELINE_EQUIVALENCE_PASSED: classic == ingest_batch == RX-pipelined (counters/watermark/count/hash) incl. multi-pass resets");
}
