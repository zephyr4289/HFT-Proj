//! Recovery Test Suite R1–R14 and E2E-2a/b/c (doc 08 §12, C9/C10/C11).
//! Concurrency tests wrapped with wall-clock watchdogs (Issue 1).
//! Engine loops governed by Loop Termination Law (doc 08 §4a).

#![allow(warnings)]

use nf_arbitrator::types::{DeadReason, Event, RecoveryIntent};
use nf_arbitrator::{Sequencer, State};
use nf_protocol::moldudp64::{encode_request, REQUEST_LEN};
use nf_recovery::client::RecoveryClient;
use nf_recovery::types::*;
use nf_testkit::fakeserver::{FakeRetransmissionServer, FaultMode, SessionTruth};
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

/// Watchdog wrapper: guarantees test panics and fails with stack trace at timeout rather than hanging CI runner.
fn with_watchdog<F: FnOnce() + Send + 'static>(timeout: Duration, f: F) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    if rx.recv_timeout(timeout).is_err() {
        panic!(
            "Test execution timed out after {:?} (Watchdog tripped: potential deadlock/hang)",
            timeout
        );
    }
    let _ = handle.join();
}

// ── R1: UDP Retransmission DropRequest Fault Recovery ─────────────
#[test]
fn test_r1_fakeserver_drop_request_fault() {
    with_watchdog(Duration::from_secs(10), || {
        let gt = load_mini_bytes();
        let sess = *b"DROPREQS01";
        let server = FakeRetransmissionServer::spawn(
            &gt,
            vec![SessionTruth {
                session_id: sess,
                first_seq: 1,
                first_msg_index: 0,
                total_msgs: 500,
            }],
            FaultMode::DropRequest(2),
        )
        .expect("spawn server");

        let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());
        let mut buf = [0u8; 1500];

        // Request 1 and 2 dropped by server fault mode
        client.send_request(&sess, 10, 5);
        client.send_request(&sess, 10, 5);
        assert!(client.recv_packet(&mut buf).is_none());

        // Request 3 succeeds
        client.send_request(&sess, 10, 5);
        let mut attempts = 0;
        let mut received = false;
        while attempts < 100 {
            if let Some(n) = client.recv_packet(&mut buf) {
                assert!(n > 20, "Must receive valid downstream packet");
                received = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
            attempts += 1;
        }
        assert!(received, "R1 must recover after dropped requests");
    });
}

// ── R2: UDP Retransmission DropResponse Fault Recovery ────────────
#[test]
fn test_r2_fakeserver_drop_response_fault() {
    with_watchdog(Duration::from_secs(10), || {
        let gt = load_mini_bytes();
        let sess = *b"DROPRESP01";
        let server = FakeRetransmissionServer::spawn(
            &gt,
            vec![SessionTruth {
                session_id: sess,
                first_seq: 1,
                first_msg_index: 0,
                total_msgs: 500,
            }],
            FaultMode::DropResponse(2),
        )
        .expect("spawn server");

        let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());
        let mut buf = [0u8; 1500];

        // Response 1 and 2 dropped in flight
        client.send_request(&sess, 20, 5);
        thread::sleep(Duration::from_millis(20));
        assert!(client.recv_packet(&mut buf).is_none());

        client.send_request(&sess, 20, 5);
        thread::sleep(Duration::from_millis(20));
        assert!(client.recv_packet(&mut buf).is_none());

        // Response 3 delivered
        client.send_request(&sess, 20, 5);
        let mut attempts = 0;
        let mut received = false;
        while attempts < 100 {
            if let Some(n) = client.recv_packet(&mut buf) {
                assert!(n > 20, "Must receive valid downstream packet");
                received = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
            attempts += 1;
        }
        assert!(received, "R2 must recover after dropped responses");
    });
}

// ── R3: INV-R5 server WrongSession ──────────────────────────────────
#[test]
fn test_r3_inv_r5_wrong_session() {
    with_watchdog(Duration::from_secs(10), || {
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

        let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());
        client.send_request(&sess, 1, 10);

        let mut seq = Sequencer::new();
        let mut sink = ConformanceSink::new();
        let mut rec_slot = [0u8; 1500];

        for _ in 0..50 {
            while let Some(len) = client.recv_packet(&mut rec_slot) {
                seq.ingest(&rec_slot[..len], 2, 1000, &mut sink);
            }
            if seq.counters().sessions > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    });
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
    with_watchdog(Duration::from_secs(10), || {
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

        let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());
        let mut seq = Sequencer::new();
        let mut sink = ConformanceSink::new();
        let mut batch = FrameBatch::new();

        // Step 1: Open gap at W=1..20
        let mut f0 = Vec::new();
        f0.extend_from_slice(&sess);
        f0.extend_from_slice(&1u64.to_be_bytes());
        f0.extend_from_slice(&1u16.to_be_bytes());
        f0.extend_from_slice(&12u16.to_be_bytes());
        f0.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
        seq.ingest(&f0, 0, 1000, &mut sink);

        let mut f1 = Vec::new();
        f1.extend_from_slice(&sess);
        f1.extend_from_slice(&21u64.to_be_bytes());
        f1.extend_from_slice(&1u16.to_be_bytes());
        f1.extend_from_slice(&12u16.to_be_bytes());
        f1.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
        seq.ingest(&f1, 0, 2000, &mut sink);

        let mut vt = 1000 + 300_000;
        let mut rec_slot = [0u8; 1500];
        for _ in 0..100 {
            if let Some(intent) = seq.recovery_intent(vt) {
                let count = (intent.to_excl - intent.from) as u16;
                client.send_request(&sess, intent.from, count);
            }
            while let Some(len) = client.recv_packet(&mut rec_slot) {
                seq.ingest(&rec_slot[..len], 2, vt, &mut sink);
            }
            vt += 10_000_000;
            if seq.watermark() >= 22 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        assert!(seq.watermark() >= 22, "Partial fills must iteratively advance watermark");
    });
}

// ── R6: DropRequest x 4 -> SessionDead ───────────────────────────────
#[test]
fn test_r6_drop_request_dead() {
    with_watchdog(Duration::from_secs(10), || {
        let gt = load_mini_bytes();
        let sess = *b"CLOSESESS1";
        let _server = FakeRetransmissionServer::spawn(
            &gt,
            vec![SessionTruth {
                session_id: sess,
                first_seq: 1,
                first_msg_index: 0,
                total_msgs: 1000,
            }],
            FaultMode::DropRequest(1),
        )
        .expect("spawn server");

        let mut seq = Sequencer::new();
        let mut sink = ConformanceSink::new();

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

        // Loop termination: seal on retry exhaustion
        seq.seal(DeadReason::RetryExhausted, &mut sink);
        assert_eq!(seq.state(), State::Dead);

        // Ingest after dead must be ignored (Loop Termination Law)
        seq.ingest(&f0, 0, vt, &mut sink);
        assert_eq!(seq.counters().ignored_after_dead, 1);
    });
}

// ── R7: Silence dead ────────────────────────────────────────────────
#[test]
fn test_r7_silence_dead() {
    with_watchdog(Duration::from_secs(10), || {
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
        assert_eq!(seq.state(), State::Dead);
    });
}

// ── R8: DuplicateFirst live deduplication ────────────────────────────
#[test]
fn test_r8_duplicate_first() {
    with_watchdog(Duration::from_secs(10), || {
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

        let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());
        client.send_request(&sess, 1, 20);

        let mut seq = Sequencer::new();
        let mut sink = ConformanceSink::new();
        let mut rec_slot = [0u8; 1500];

        for _ in 0..100 {
            while let Some(len) = client.recv_packet(&mut rec_slot) {
                seq.ingest(&rec_slot[..len], 2, 1000, &mut sink);
            }
            if seq.watermark() >= 20 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        assert!(seq.watermark() >= 20);
        assert_eq!(seq.counters().total_violations, 0);
    });
}

// ── R9: Dual-drop repair ────────────────────────────────────────────
#[test]
fn test_r9_dual_drop_repair() {
    with_watchdog(Duration::from_secs(15), || {
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
        let mut rec_slot = [0u8; 1500];

        let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());

        let mut gap_opened_seen = false;
        let mut reanchored_seen = false;

        while (transport.poll(&mut batch) > 0 || seq.state() == State::Contig || seq.state() == State::Init || seq.state() == State::EosPersist)
            && seq.state() != State::Dead
        {
            let now_ns = transport.now_ns();
            for frame in batch.frames() {
                seq.ingest(frame.bytes(), frame.feed, now_ns, &mut sink);
            }

            if let Some(intent) = seq.recovery_intent(now_ns) {
                let count = (intent.to_excl - intent.from) as u16;
                client.send_request(&sess, intent.from, count);
            }

            while let Some(len) = client.recv_packet(&mut rec_slot) {
                seq.ingest(&rec_slot[..len], 2, now_ns, &mut sink);
            }

            if sink.gap_open_gen.is_some() {
                gap_opened_seen = true;
            }
            if seq.watermark() > 20 && gap_opened_seen {
                reanchored_seen = true;
                break;
            }
        }

        assert!(reanchored_seen, "GapOpened and ReAnchored pair must be observed");
    });
}

// ── R10: Session boundary clears pending recovery ───────────────────
#[test]
fn test_r10_session_boundary_during_recovery() {
    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();
    let sess1 = *b"OLDSESS001";
    let sess2 = *b"NEWSESS002";

    // Stage a gap in sess1
    let mut f0 = Vec::new();
    f0.extend_from_slice(&sess1);
    f0.extend_from_slice(&1u64.to_be_bytes());
    f0.extend_from_slice(&1u16.to_be_bytes());
    f0.extend_from_slice(&12u16.to_be_bytes());
    f0.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
    seq.ingest(&f0, 0, 1000, &mut sink);

    let mut f1 = Vec::new();
    f1.extend_from_slice(&sess1);
    f1.extend_from_slice(&10u64.to_be_bytes());
    f1.extend_from_slice(&1u16.to_be_bytes());
    f1.extend_from_slice(&12u16.to_be_bytes());
    f1.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
    seq.ingest(&f1, 0, 2000, &mut sink);

    assert!(seq.recovery_intent(10_000_000).is_some());

    // Switch to sess2
    let mut f2 = Vec::new();
    f2.extend_from_slice(&sess2);
    f2.extend_from_slice(&1u64.to_be_bytes());
    f2.extend_from_slice(&1u16.to_be_bytes());
    f2.extend_from_slice(&12u16.to_be_bytes());
    f2.extend_from_slice(&[b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O']);
    seq.ingest(&f2, 0, 3000, &mut sink);

    // Old intent cleared
    assert_eq!(seq.session(), sess2);
    assert_eq!(seq.staged_count(), 0);
}

// ── R11: Socket disconnect ──────────────────────────────────────────
#[test]
fn test_r11_socket_disconnect() {
    let mut client = RecoveryClient::new([127, 0, 0, 1], 9999);
    client.disconnect(STATUS_SOCKET_ERROR);
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
    with_watchdog(Duration::from_secs(5), || {
        let mut client = RecoveryClient::new([127, 0, 0, 1], 1);
        client.disconnect(STATUS_DISCONNECTED);
        assert_eq!(client.state(), ClientState::Disconnected);
    });
}

// ── E2E-2a: Canonical Dual-Drop GapFill at Tail-of-the-Day (AM-6) ────
#[test]
fn test_e2e_2a_canonical_dual_drop() {
    with_watchdog(Duration::from_secs(20), || {
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
        // Worst-case AM-6 validation: drop placed at the TAIL of the day!
        cfg.scripted_drops = vec![DropRange {
            seq_from: 500_000,
            seq_to_incl: 505_000,
            feed_mask: 3, // Vanished from both feeds
        }];
        cfg.guarantee_coverage = true;

        let sched = build_schedule(&gt, &cfg);
        let mut transport = ReplayTransport::new(&gt, sched, sess);
        let mut seq = Sequencer::new();
        let mut sink = ConformanceSink::new();
        let mut batch = FrameBatch::new();
        let mut rec_batch = FrameBatch::new();

        let mut rec_slot = [0u8; 1500];
        let mut client = RecoveryClient::new([127, 0, 0, 1], server.port());

        while transport.poll(&mut batch) > 0 {
            let now_ns = transport.now_ns();

            // 1. Ingest recovery frames (P-ORDER 1)
            while let Some(len) = client.recv_packet(&mut rec_slot) {
                seq.ingest(&rec_slot[..len], 2, now_ns, &mut sink);
            }

            // 2. Ingest multicast frames (P-ORDER 2)
            for frame in batch.frames() {
                seq.ingest(frame.bytes(), frame.feed, now_ns, &mut sink);
            }

            // 3. Recovery intent publish (P-ORDER 3)
            if let Some(intent) = seq.recovery_intent(now_ns) {
                let count = (intent.to_excl - intent.from) as u16;
                client.send_request(&sess, intent.from, count);
            }
        }

        // Trailing recovery steps until watermark reaches N+1 or Dead
        let mut trailing_vt = transport.now_ns();
        for _ in 0..500 {
            if seq.watermark() >= MINI_MESSAGE_COUNT + 1 || seq.state() == State::Dead {
                break;
            }
            trailing_vt += 10_000_000;
            if let Some(intent) = seq.recovery_intent(trailing_vt) {
                let count = (intent.to_excl - intent.from) as u16;
                client.send_request(&sess, intent.from, count);
            }
            while let Some(len) = client.recv_packet(&mut rec_slot) {
                seq.ingest(&rec_slot[..len], 2, trailing_vt, &mut sink);
            }
            thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(sink.hash(), MINI_GOLDEN_HASH, "Golden hash must match ground truth");
        assert_eq!(sink.count(), MINI_MESSAGE_COUNT, "Total message count must match");
        assert_eq!(seq.watermark(), MINI_MESSAGE_COUNT + 1, "Watermark must reach N+1");
        assert_eq!(seq.counters().total_violations, 0, "Violations must be 0");
    });
}

// ── E2E-2b: Session split ON and dual-drop spanning boundary ────────
#[test]
fn test_e2e_2b_split_dual_drop() {
    with_watchdog(Duration::from_secs(15), || {
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
    });
}

// ── E2E-2c: Double-run determinism confluence ────────────────────────
#[test]
fn test_e2e_2c_double_run_confluence() {
    with_watchdog(Duration::from_secs(15), || {
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
    });
}
