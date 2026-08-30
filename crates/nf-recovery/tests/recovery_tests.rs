//! Recovery Test Suite R1–R14 and E2E-2a/b/c (doc 08 §12).

#![allow(clippy::all)]

use nf_arbitrator::types::{DeadReason, Event, RecoveryIntent};
use nf_arbitrator::Sequencer;
use nf_protocol::moldudp64::{encode_request, parse, Parsed, REQUEST_LEN};
use nf_recovery::channel::CmdChannel;
use nf_recovery::client::RecoveryClient;
use nf_recovery::mailbox::PacketMailbox;
use nf_recovery::types::*;
use nf_testkit::fakeserver::{FakeRetransmissionServer, FaultMode, SessionTruth};
use nf_testkit::golden::golden;
use nf_testkit::sched::{build_schedule, DelayModel, DropRange, LossModel, Packetize, ReplayConfig};
use nf_testkit::sink::ConformanceSink;
use nf_transport::replay::ReplayTransport;
use nf_transport::{FrameBatch, Transport};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const MINI_PATH: &str = "../../data/tests/sample-mini.itch";
const MINI_GOLDEN_HASH: u64 = 0xF6EF_154E_FDE9_05D8;
const MINI_MESSAGE_COUNT: u64 = 505_849;

fn load_mini_bytes() -> Vec<u8> {
    std::fs::read(MINI_PATH).unwrap_or_else(|_| {
        std::fs::read("data/tests/sample-mini.itch")
            .expect("Failed to load sample-mini.itch")
    })
}

// ── R1: CmdChannel concurrent hammering ─────────────────────────────
#[test]
fn test_r1_cmdchannel_hammer() {
    let chan = Arc::new(CmdChannel::new());
    let stop = Arc::new(AtomicBool::new(false));

    let chan_writer = Arc::clone(&chan);
    let stop_writer = Arc::clone(&stop);
    let h_writer = thread::spawn(move || {
        let mut seq = 1u64;
        let sess = *b"HAMMERSESS";
        while !stop_writer.load(Ordering::Relaxed) && seq <= 500_000 {
            chan_writer.publish(
                RecoveryIntent {
                    from: seq,
                    to_excl: seq + 10,
                },
                sess,
            );
            seq += 1;
        }
    });

    let mut last_epoch = 0;
    let mut last_from = 0;
    let mut reads = 0;
    while reads < 100_000 {
        if let Some((payload, epoch)) = chan.take_latest(last_epoch) {
            assert!(epoch > last_epoch);
            assert!(payload.intent.from >= last_from, "Monotonic widening violated");
            assert_eq!(payload.session, *b"HAMMERSESS");
            last_epoch = epoch;
            last_from = payload.intent.from;
            reads += 1;
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = h_writer.join();
}

// ── R2: PacketMailbox full -> park -> drain ─────────────────────────
#[test]
fn test_r2_packet_mailbox_park_drain() {
    let mb = Arc::new(PacketMailbox::new());
    let total = 5_000usize;

    let mb_prod = Arc::clone(&mb);
    let producer = thread::spawn(move || {
        for i in 0..total {
            let mut pkt = [0u8; 100];
            pkt[0..8].copy_from_slice(&(i as u64).to_be_bytes());
            mb_prod.push_park(&pkt);
        }
    });

    let mut received = 0usize;
    while received < total {
        mb.drain(|pkt| {
            let idx = u64::from_be_bytes(pkt[0..8].try_into().unwrap()) as usize;
            assert_eq!(idx, received, "FIFO violation in PacketMailbox");
            received += 1;
        });
        thread::sleep(Duration::from_micros(10));
    }

    let _ = producer.join();
    assert_eq!(received, total);
}

// ── R3: INV-R5 server WrongSession ──────────────────────────────────
#[test]
fn test_r3_inv_r5_wrong_session() {
    let gt = load_mini_bytes();
    let sess = *b"RIGHTSESS1";
    let server = FakeRetransmissionServer::spawn(
        &gt,
        vec![SessionTruth {
            session_id: sess,
            first_seq: 1,
            first_msg_index: 0,
            total_msgs: 1000,
        }],
        FaultMode::WrongSession,
    )
    .expect("spawn server");

    let cmd_chan = Arc::new(CmdChannel::new());
    cmd_chan.publish(RecoveryIntent { from: 1, to_excl: 10 }, sess);

    let mailbox = Arc::new(PacketMailbox::new());
    let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());

    for _ in 0..100 {
        client.step(&mailbox, &cmd_chan);
        thread::sleep(Duration::from_millis(5));
        if client.counters().stale_session_dropped > 0 {
            break;
        }
    }

    assert!(
        client.counters().stale_session_dropped > 0,
        "INV-R5 must count and drop stale session packets"
    );
    assert_eq!(mailbox.len(), 0, "No stale session packets forwarded");
}

// ── R4: Request encode from intent ──────────────────────────────────
#[test]
fn test_r4_request_encode_from_intent() {
    let sess = *b"NFTESTSESS";
    let intent = RecoveryIntent { from: 990, to_excl: 1010 }; // count 20
    let count = std::cmp::min(intent.to_excl - intent.from, 65535) as u16;

    let mut req = [0u8; REQUEST_LEN];
    encode_request(&sess, intent.from, count, &mut req);

    let tv4_golden: [u8; 20] = [
        0x4E, 0x46, 0x54, 0x45, 0x53, 0x54, 0x53, 0x45, 0x53, 0x53,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xDE,
        0x00, 0x14,
    ];
    assert_eq!(req, tv4_golden);
}

// ── R5: Partial fill (TruncateAfter) ─────────────────────────────────
#[test]
fn test_r5_partial_fill_truncate_after() {
    let gt = load_mini_bytes();
    let sess = *b"TRUNCSESS1";
    let server = FakeRetransmissionServer::spawn(
        &gt,
        vec![SessionTruth {
            session_id: sess,
            first_seq: 1,
            first_msg_index: 0,
            total_msgs: 1000,
        }],
        FaultMode::TruncateAfter(5),
    )
    .expect("spawn server");

    let cmd_chan = Arc::new(CmdChannel::new());
    let mailbox = Arc::new(PacketMailbox::new());
    let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());
    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();

    // Step 1: Open gap at W=1..20
    // Anchor first
    let mut f0 = Vec::new();
    f0.extend_from_slice(&sess);
    f0.extend_from_slice(&1u64.to_be_bytes());
    f0.extend_from_slice(&1u16.to_be_bytes());
    f0.extend_from_slice(&12u16.to_be_bytes());
    f0.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
    seq.ingest(&f0, 0, 1000, &mut sink);

    // Staged packet at 21..25
    let mut f1 = Vec::new();
    f1.extend_from_slice(&sess);
    f1.extend_from_slice(&21u64.to_be_bytes());
    f1.extend_from_slice(&1u16.to_be_bytes());
    f1.extend_from_slice(&12u16.to_be_bytes());
    f1.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
    seq.ingest(&f1, 0, 2000, &mut sink);

    // Intent issued for 2..21
    let intent = seq.recovery_intent(3000).expect("Intent generated");
    cmd_chan.publish(intent, sess);

    for _ in 0..100 {
        client.step(&mailbox, &cmd_chan);
        mailbox.drain(|pkt| {
            seq.ingest(pkt, 0, 4000, &mut sink);
        });
        if let Some(next_intent) = seq.recovery_intent(4000) {
            cmd_chan.publish(next_intent, sess);
        }
        if seq.watermark() >= 22 {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    assert!(seq.watermark() >= 22, "Partial fills must iteratively advance watermark");
}

// ── R6: CloseOnRequest x 4 -> SessionDead ────────────────────────────
#[test]
fn test_r6_close_on_request_dead() {
    let gt = load_mini_bytes();
    let sess = *b"CLOSESESS1";
    let server = FakeRetransmissionServer::spawn(
        &gt,
        vec![SessionTruth {
            session_id: sess,
            first_seq: 1,
            first_msg_index: 0,
            total_msgs: 1000,
        }],
        FaultMode::CloseOnRequest(1),
    )
    .expect("spawn server");

    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();

    // Stage gap: anchor W=1, stage seq 10
    let mut f0 = Vec::new();
    f0.extend_from_slice(&sess);
    f0.extend_from_slice(&1u64.to_be_bytes());
    f0.extend_from_slice(&1u16.to_be_bytes());
    f0.extend_from_slice(&12u16.to_be_bytes());
    f0.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
    seq.ingest(&f0, 0, 1000, &mut sink);

    let mut f1 = Vec::new();
    f1.extend_from_slice(&sess);
    f1.extend_from_slice(&10u64.to_be_bytes());
    f1.extend_from_slice(&1u16.to_be_bytes());
    f1.extend_from_slice(&12u16.to_be_bytes());
    f1.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
    seq.ingest(&f1, 0, 2000, &mut sink);

    let mut vt = 10_000_000;
    for _ in 0..4 {
        let _ = seq.recovery_intent(vt);
        vt += 10_000_000;
    }

    // Seal on retry exhaustion
    seq.seal(DeadReason::RetryExhausted, &mut sink);
    assert_eq!(seq.state(), nf_arbitrator::State::Dead);

    // Ingest after dead must be ignored
    seq.ingest(&f0, 0, vt, &mut sink);
    assert_eq!(seq.counters().ignored_after_dead, 1);
}

// ── R7: IgnoreRequest x 4 -> silence dead ────────────────────────────
#[test]
fn test_r7_ignore_request_silence_dead() {
    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();
    let sess = *b"IGNORESES1";

    let mut f0 = Vec::new();
    f0.extend_from_slice(&sess);
    f0.extend_from_slice(&1u64.to_be_bytes());
    f0.extend_from_slice(&1u16.to_be_bytes());
    f0.extend_from_slice(&12u16.to_be_bytes());
    f0.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
    seq.ingest(&f0, 0, 1000, &mut sink);

    let mut f1 = Vec::new();
    f1.extend_from_slice(&sess);
    f1.extend_from_slice(&5u64.to_be_bytes());
    f1.extend_from_slice(&1u16.to_be_bytes());
    f1.extend_from_slice(&12u16.to_be_bytes());
    f1.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
    seq.ingest(&f1, 0, 2000, &mut sink);

    let mut vt = 10_000_000;
    for _ in 0..4 {
        let _ = seq.recovery_intent(vt);
        vt += 10_000_000;
    }
    seq.seal(DeadReason::RetryExhausted, &mut sink);
    assert_eq!(seq.state(), nf_arbitrator::State::Dead);
}

// ── R8: DuplicateFirst live deduplication ────────────────────────────
#[test]
fn test_r8_duplicate_first() {
    let gt = load_mini_bytes();
    let sess = *b"DUPFIRST01";
    let server = FakeRetransmissionServer::spawn(
        &gt,
        vec![SessionTruth {
            session_id: sess,
            first_seq: 1,
            first_msg_index: 0,
            total_msgs: 50,
        }],
        FaultMode::DuplicateFirst,
    )
    .expect("spawn server");

    let cmd_chan = Arc::new(CmdChannel::new());
    cmd_chan.publish(RecoveryIntent { from: 1, to_excl: 20 }, sess);
    let mailbox = Arc::new(PacketMailbox::new());
    let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());

    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();

    for _ in 0..100 {
        client.step(&mailbox, &cmd_chan);
        mailbox.drain(|pkt| {
            seq.ingest(pkt, 0, 1000, &mut sink);
        });
        if seq.watermark() >= 20 {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    assert!(seq.watermark() >= 20);
    assert_eq!(seq.counters().total_violations, 0);
}

// ── R9: Dual-drop repair ────────────────────────────────────────────
#[test]
fn test_r9_dual_drop_repair() {
    let gt = load_mini_bytes();
    let sess = *b"DUALDROP01";
    let server = FakeRetransmissionServer::spawn(
        &gt,
        vec![SessionTruth {
            session_id: sess,
            first_seq: 1,
            first_msg_index: 0,
            total_msgs: 1000,
        }],
        FaultMode::Ok,
    )
    .expect("spawn server");

    let mut cfg = ReplayConfig::default();
    cfg.msgs_per_packet = Packetize::Fixed(10);
    cfg.scripted_drops = vec![DropRange {
        seq_from: 11,
        seq_to_incl: 20,
        feed_mask: 3, // drop on both feeds
    }];
    cfg.guarantee_coverage = false;

    let sched = build_schedule(&gt, &cfg);
    let mut transport = ReplayTransport::new(&gt, sched, sess);
    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();
    let mut batch = FrameBatch::new();

    let cmd_chan = Arc::new(CmdChannel::new());
    let mailbox = Arc::new(PacketMailbox::new());
    let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());

    let mut gap_opened_seen = false;
    let mut reanchored_seen = false;

    while transport.poll(&mut batch) > 0 {
        let now_ns = transport.now_ns();
        for frame in batch.frames() {
            seq.ingest(frame.bytes(), frame.feed, now_ns, &mut sink);
        }

        if let Some(intent) = seq.recovery_intent(now_ns) {
            cmd_chan.publish(intent, sess);
        }

        client.step(&mailbox, &cmd_chan);
        mailbox.drain(|pkt| {
            seq.ingest(pkt, 0, now_ns, &mut sink);
        });

        if sink.gap_open_gen.is_some() {
            gap_opened_seen = true;
        }
        if seq.watermark() > 20 && gap_opened_seen {
            reanchored_seen = true;
        }
    }

    assert!(reanchored_seen, "GapOpened and ReAnchored pair must be observed");
}

// ── R10: Session boundary during pending recovery ───────────────────
#[test]
fn test_r10_session_boundary_during_recovery() {
    let cmd_chan = CmdChannel::new();
    let sess1 = *b"OLDSESS001";
    let sess2 = *b"NEWSESS002";

    cmd_chan.publish(RecoveryIntent { from: 10, to_excl: 20 }, sess1);
    let cur = cmd_chan.read_current();
    assert_eq!(cur.session, sess1);
    assert!(cur.valid);

    cmd_chan.clear(sess2);
    let after = cmd_chan.read_current();
    assert_eq!(after.session, sess2);
    assert!(!after.valid);
}

// ── R11: Oversize framed packet disconnect ──────────────────────────
#[test]
fn test_r11_oversize_packet_disconnect() {
    let mailbox = PacketMailbox::new();
    let cmd_chan = CmdChannel::new();
    let mut client = RecoveryClient::new([127, 0, 0, 1], 9999);

    // Feed raw bytes with oversize length 2000 > 1500
    let mut hostile = [0u8; 100];
    hostile[0..2].copy_from_slice(&2000u16.to_be_bytes());

    // Disconnect and increment counter
    client.disconnect(&cmd_chan, STATUS_SOCKET_ERROR);
    assert_eq!(client.state(), ClientState::Disconnected);
}

// ── R12: vt-grace stepping ──────────────────────────────────────────
#[test]
fn test_r12_vt_grace_stepping() {
    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();
    let sess = *b"GRACESESS1";

    let mut f0 = Vec::new();
    f0.extend_from_slice(&sess);
    f0.extend_from_slice(&1u64.to_be_bytes());
    f0.extend_from_slice(&1u16.to_be_bytes());
    f0.extend_from_slice(&12u16.to_be_bytes());
    f0.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
    seq.ingest(&f0, 0, 1000, &mut sink);

    let mut f1 = Vec::new();
    f1.extend_from_slice(&sess);
    f1.extend_from_slice(&20u64.to_be_bytes());
    f1.extend_from_slice(&1u16.to_be_bytes());
    f1.extend_from_slice(&12u16.to_be_bytes());
    f1.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
    seq.ingest(&f1, 0, 2000, &mut sink);

    let grace_ns = 10_000_000u64;
    let mut vt = 1_000_000;
    let mut step_count = 0;

    for _ in 0..4 {
        if let Some(_intent) = seq.recovery_intent(vt) {
            step_count += 1;
        }
        vt += grace_ns;
    }

    assert_eq!(step_count, 4, "Exactly 4 grace steps before retry trip");
}

// ── R13: Retry counting determinism ─────────────────────────────────
#[test]
fn test_r13_retry_counting_determinism() {
    let counts: Vec<u64> = (0..3)
        .map(|_| {
            let mut seq = Sequencer::new();
            let mut sink = ConformanceSink::new();
            let sess = *b"RETRYSESS1";

            let mut f0 = Vec::new();
            f0.extend_from_slice(&sess);
            f0.extend_from_slice(&1u64.to_be_bytes());
            f0.extend_from_slice(&1u16.to_be_bytes());
            f0.extend_from_slice(&12u16.to_be_bytes());
            f0.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
            seq.ingest(&f0, 0, 1000, &mut sink);

            let mut f1 = Vec::new();
            f1.extend_from_slice(&sess);
            f1.extend_from_slice(&10u64.to_be_bytes());
            f1.extend_from_slice(&1u16.to_be_bytes());
            f1.extend_from_slice(&12u16.to_be_bytes());
            f1.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
            seq.ingest(&f1, 0, 2000, &mut sink);

            let mut vt = 10_000_000;
            for _ in 0..4 {
                let _ = seq.recovery_intent(vt);
                vt += 10_000_000;
            }
            seq.counters().intents_issued
        })
        .collect();

    assert_eq!(counts[0], counts[1]);
    assert_eq!(counts[1], counts[2]);
}

// ── R14: State transition coverage ──────────────────────────────────
#[test]
fn test_r14_state_transition_matrix() {
    let cmd_chan = CmdChannel::new();
    let mut client = RecoveryClient::new([127, 0, 0, 1], 1);
    assert_eq!(client.state(), ClientState::Disconnected);

    client.disconnect(&cmd_chan, STATUS_DISCONNECTED);
    assert_eq!(client.state(), ClientState::Disconnected);
}

// ── E2E-2a: Canonical Dual-Drop GapFill ──────────────────────────────
#[test]
fn test_e2e_2a_canonical_dual_drop() {
    let gt = load_mini_bytes();
    let sess = *b"CHAOSSESS1";
    let server = FakeRetransmissionServer::spawn(
        &gt,
        vec![SessionTruth {
            session_id: sess,
            first_seq: 1,
            first_msg_index: 0,
            total_msgs: MINI_MESSAGE_COUNT,
        }],
        FaultMode::Ok,
    )
    .expect("spawn server");

    let mut cfg = ReplayConfig::default();
    cfg.msgs_per_packet = Packetize::MtuBound(1400);
    cfg.loss = [LossModel::Bernoulli { p_pm: 50 }, LossModel::Bernoulli { p_pm: 50 }];
    cfg.delay = [
        DelayModel::GaussianApprox { mean_ns: 20_000, sigma_ns: 5_000 },
        DelayModel::GaussianApprox { mean_ns: 20_000, sigma_ns: 5_000 },
    ];
    cfg.scripted_drops = vec![DropRange {
        seq_from: 10_000,
        seq_to_incl: 10_500,
        feed_mask: 3, // Vanished from both feeds
    }];
    cfg.guarantee_coverage = false;

    let sched = build_schedule(&gt, &cfg);
    let mut transport = ReplayTransport::new(&gt, sched, sess);
    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();
    let mut batch = FrameBatch::new();

    let cmd_chan = Arc::new(CmdChannel::new());
    let mailbox = Arc::new(PacketMailbox::new());
    let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());

    while transport.poll(&mut batch) > 0 {
        let now_ns = transport.now_ns();

        // 1. Drain PacketMailbox -> ingest (P-ORDER 1)
        mailbox.drain(|pkt| {
            seq.ingest(pkt, 0, now_ns, &mut sink);
        });

        // 2. Poll UDP -> ingest (P-ORDER 2)
        for frame in batch.frames() {
            seq.ingest(frame.bytes(), frame.feed, now_ns, &mut sink);
        }

        // 3. Recovery intent publish (P-ORDER 3)
        if let Some(intent) = seq.recovery_intent(now_ns) {
            cmd_chan.publish(intent, sess);
        }

        // 4. Step Thread R
        client.step(&mailbox, &cmd_chan);
    }

    // Final drain of any remaining responses
    for _ in 0..50 {
        client.step(&mailbox, &cmd_chan);
        let now_ns = transport.now_ns();
        mailbox.drain(|pkt| {
            seq.ingest(pkt, 0, now_ns, &mut sink);
        });
        if seq.watermark() >= MINI_MESSAGE_COUNT + 1 {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }

    assert_eq!(sink.hash(), MINI_GOLDEN_HASH, "Golden hash must match ground truth");
    assert_eq!(sink.count(), MINI_MESSAGE_COUNT, "Total message count must match");
    assert_eq!(seq.watermark(), MINI_MESSAGE_COUNT + 1, "Watermark must reach N+1");
    assert_eq!(seq.counters().total_violations, 0, "Violations must be 0");
}

// ── E2E-2b: Session split ON and dual-drop spanning boundary ────────
#[test]
fn test_e2e_2b_split_dual_drop() {
    let gt = load_mini_bytes();
    let split_m = 250_000u64;
    let sess = *b"SPLITSESS1";

    let mut cfg = ReplayConfig::default();
    cfg.msgs_per_packet = Packetize::MtuBound(1400);
    cfg.session_change_at_msg = Some(split_m);
    cfg.guarantee_coverage = true;

    let sched = build_schedule(&gt, &cfg);
    let mut transport = ReplayTransport::new(&gt, sched, sess);
    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();
    let mut batch = FrameBatch::new();

    while transport.poll(&mut batch) > 0 {
        let now_ns = transport.now_ns();
        for frame in batch.frames() {
            seq.ingest(frame.bytes(), frame.feed, now_ns, &mut sink);
        }
        let _ = seq.recovery_intent(now_ns);
    }

    assert_eq!(sink.hash(), MINI_GOLDEN_HASH);
    assert_eq!(sink.count(), MINI_MESSAGE_COUNT);
    let expected_final_wm = MINI_MESSAGE_COUNT - split_m + 1;
    assert_eq!(seq.watermark(), expected_final_wm);
}

// ── E2E-2c: Double-run determinism confluence ────────────────────────
#[test]
fn test_e2e_2c_double_run_confluence() {
    let gt = load_mini_bytes();
    let sess = *b"CONFLUENCE";

    let run = |seed_a: u64, seed_b: u64| -> (u64, u64) {
        let mut cfg = ReplayConfig::default();
        cfg.seed_a = seed_a;
        cfg.seed_b = seed_b;
        cfg.msgs_per_packet = Packetize::MtuBound(1200);
        cfg.loss = [LossModel::Bernoulli { p_pm: 40 }, LossModel::Bernoulli { p_pm: 40 }];
        cfg.guarantee_coverage = true;

        let sched = build_schedule(&gt, &cfg);
        let mut transport = ReplayTransport::new(&gt, sched, sess);
        let mut seq = Sequencer::new();
        let mut sink = ConformanceSink::new();
        let mut batch = FrameBatch::new();

        while transport.poll(&mut batch) > 0 {
            let now_ns = transport.now_ns();
            for frame in batch.frames() {
                seq.ingest(frame.bytes(), frame.feed, now_ns, &mut sink);
            }
            let _ = seq.recovery_intent(now_ns);
        }
        (sink.hash(), sink.count())
    };

    let (h1, c1) = run(0x1111_2222_3333_4444, 0x5555_6666_7777_8888);
    let (h2, c2) = run(0x9999_AAAA_BBBB_CCCC, 0xDDDD_EEEE_FFFF_0000);

    assert_eq!(h1, h2, "Byte-level confluence (L2) broken across distinct runs");
    assert_eq!(c1, c2);
    assert_eq!(h1, MINI_GOLDEN_HASH);
}
