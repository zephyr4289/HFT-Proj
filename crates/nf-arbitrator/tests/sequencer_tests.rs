#![allow(clippy::all)]

use nf_arbitrator::*;

#[derive(Default)]
struct TestSink {
    msgs: Vec<(u64, Vec<u8>, u64)>, // (seq, data, gen)
    events: Vec<Event>,
}

impl Sink for TestSink {
    fn on_msg(&mut self, proof: &LiveFeedProof, seq: u64, msg: &[u8]) {
        self.msgs.push((seq, msg.to_vec(), proof.gen()));
    }

    fn on_event(&mut self, ev: &Event) {
        self.events.push(ev.clone());
    }
}

fn make_msg_s(seq: u64) -> Vec<u8> {
    // 12-byte System Event message
    let mut data = vec![b'S', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'O'];
    // Embed seq in payload to verify exact byte identity
    let seq_b = seq.to_be_bytes();
    data[1..9].copy_from_slice(&seq_b);
    data
}

fn make_frame(session: &[u8; 10], first_seq: u64, count: u16) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(session);
    frame.extend_from_slice(&first_seq.to_be_bytes());
    frame.extend_from_slice(&count.to_be_bytes());

    for s in first_seq..first_seq + (count as u64) {
        let msg = make_msg_s(s);
        frame.extend_from_slice(&(msg.len() as u16).to_be_bytes());
        frame.extend_from_slice(&msg);
    }
    frame
}

fn make_hb_frame(session: &[u8; 10], next_seq: u64) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(session);
    frame.extend_from_slice(&next_seq.to_be_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes());
    frame
}

fn make_eos_frame(session: &[u8; 10], next_seq: u64) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(session);
    frame.extend_from_slice(&next_seq.to_be_bytes());
    frame.extend_from_slice(&0xFFFFu16.to_be_bytes());
    frame
}

#[test]
fn test_u1_dup_fast_path() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    let f1 = make_frame(sess, 1, 10);
    seq.ingest(&f1, 0, 1000, &mut sink);
    assert_eq!(seq.watermark(), 11);
    assert_eq!(sink.msgs.len(), 10);

    // Feed B delivers duplicate range
    let f2 = make_frame(sess, 1, 10);
    seq.ingest(&f2, 1, 2000, &mut sink);

    assert_eq!(seq.watermark(), 11);
    assert_eq!(sink.msgs.len(), 10); // Zero additional emissions
    assert_eq!(seq.counters().feed_b.dups, 1);
    assert_eq!(seq.counters().dup_msgs, 10);
}

#[test]
fn test_u2_partial_overlap() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    // Feed A delivers [1..=5]
    let f1 = make_frame(sess, 1, 5);
    seq.ingest(&f1, 0, 1000, &mut sink);
    assert_eq!(seq.watermark(), 6);
    assert_eq!(sink.msgs.len(), 5);

    // Feed B delivers [3..=8] (straddles W=6)
    let f2 = make_frame(sess, 3, 6);
    seq.ingest(&f2, 1, 2000, &mut sink);

    assert_eq!(seq.watermark(), 9);
    assert_eq!(sink.msgs.len(), 8); // msgs 1..8 emitted once each
    for (i, (m_seq, _, _)) in sink.msgs.iter().enumerate() {
        assert_eq!(*m_seq, (i + 1) as u64);
    }
}

#[test]
fn test_u3_reorder_disorder() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    // P0: anchor with [1..=5]
    let f0 = make_frame(sess, 1, 5);
    seq.ingest(&f0, 0, 1000, &mut sink);

    // P2: [11..=15] arrives ahead of P1
    let f2 = make_frame(sess, 11, 5);
    seq.ingest(&f2, 0, 2000, &mut sink);
    assert_eq!(seq.watermark(), 6); // W unchanged
    assert_eq!(seq.staged_count(), 5);
    assert_eq!(seq.state(), State::Gap);

    // P1: [6..=10] arrives, fills gap
    let f1 = make_frame(sess, 6, 5);
    seq.ingest(&f1, 1, 3000, &mut sink);

    assert_eq!(seq.watermark(), 16);
    assert_eq!(seq.staged_count(), 0);
    assert_eq!(seq.state(), State::Contig);
    assert_eq!(sink.msgs.len(), 15);
    for (i, (m_seq, _, _)) in sink.msgs.iter().enumerate() {
        assert_eq!(*m_seq, (i + 1) as u64);
    }
}

#[test]
fn test_u4_window_clamp() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    // Anchor at W=1
    let f0 = make_frame(sess, 1, 1);
    seq.ingest(&f0, 0, 1000, &mut sink);
    assert_eq!(seq.watermark(), 2);

    // Packet arrives with span [1000..=1050] (last=1050 > W+1024=1026)
    let f_big = make_frame(sess, 1000, 51);
    seq.ingest(&f_big, 0, 2000, &mut sink);

    assert!(seq.counters().beyond_window_dropped > 0);
    assert!(seq.staged_count() > 0);
}

#[test]
fn test_u_zombie_hazard() {
    // Exact trace from doc 05 §4.1 extended to W=1224
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    // 1. Anchor at W=100
    let f0 = make_frame(sess, 100, 1);
    seq.ingest(&f0, 0, 1000, &mut sink);
    assert_eq!(seq.watermark(), 101);

    // 2. P1 delivers [200..=205] -> staged
    let f1 = make_frame(sess, 200, 6);
    seq.ingest(&f1, 0, 2000, &mut sink);
    assert_eq!(seq.staged_count(), 6);

    // 3. P2 delivers [101..=205] -> in-order branch jumps W to 206
    let f2 = make_frame(sess, 101, 105);
    seq.ingest(&f2, 1, 3000, &mut sink);
    assert_eq!(seq.watermark(), 206);
    // Slots 200..=205 MUST be cleared by Clear-on-Advance Law
    assert_eq!(seq.staged_count(), 0);

    // 4. Fill hole 206
    let f_hole = make_frame(sess, 206, 1);
    seq.ingest(&f_hole, 0, 4000, &mut sink);
    assert_eq!(seq.watermark(), 207);
    assert_eq!(seq.staged_count(), 0);

    // 5. Advance traffic all the way to W=1224
    let f_bulk = make_frame(sess, 207, 1017);
    seq.ingest(&f_bulk, 0, 5000, &mut sink);
    assert_eq!(seq.watermark(), 1224);

    // Verify slot 200 (1224 & 1023 == 200) is clean, no zombie emitted!
    assert_eq!(seq.lens()[(1224 & 1023) as usize], 0);
    assert_eq!(seq.staged_count(), 0);
}

#[test]
fn test_u6_session_change() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess1 = b"SESSIONS_1";
    let sess2 = b"SESSIONS_2";

    // Stage in sess1
    let f1 = make_frame(sess1, 1, 5);
    seq.ingest(&f1, 0, 1000, &mut sink);
    let f_stage = make_frame(sess1, 10, 5);
    seq.ingest(&f_stage, 0, 2000, &mut sink);
    assert_eq!(seq.staged_count(), 5);

    // Session 2 packet arrives
    let f2 = make_frame(sess2, 100, 5);
    seq.ingest(&f2, 0, 3000, &mut sink);

    assert_eq!(seq.session(), *sess2);
    assert_eq!(seq.counters().sessions, 2);
    assert_eq!(seq.counters().window_flushed, 1);
    assert_eq!(seq.staged_count(), 0);
    assert_eq!(seq.watermark(), 105);

    // SessionBoundary event emitted
    assert!(sink.events.iter().any(|e| matches!(e, Event::SessionBoundary { prev, next, .. } if prev == sess1 && next == sess2)));
}

#[test]
fn test_u7_heartbeat_gap() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    let f0 = make_frame(sess, 1, 5);
    seq.ingest(&f0, 0, 1000, &mut sink);
    assert_eq!(seq.watermark(), 6);

    // Heartbeat announcing next_seq=10 > W=6
    let hb = make_hb_frame(sess, 10);
    seq.ingest(&hb, 0, 2000, &mut sink);

    assert!(seq.is_gap_active());
    assert_eq!(seq.state(), State::Gap);
    assert_eq!(seq.counters().gap_opens, 1);

    // Fill gap [6..=9]
    let f_fill = make_frame(sess, 6, 4);
    seq.ingest(&f_fill, 0, 3000, &mut sink);

    assert!(!seq.is_gap_active());
    assert_eq!(seq.state(), State::Contig);
    assert_eq!(seq.counters().reanchors, 1);
    assert_eq!(seq.watermark(), 10);
}

#[test]
fn test_u8_eos_handling() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    let f0 = make_frame(sess, 1, 5);
    seq.ingest(&f0, 0, 1000, &mut sink);
    assert_eq!(seq.watermark(), 6);

    // Clean EOS
    let eos = make_eos_frame(sess, 6);
    seq.ingest(&eos, 0, 2000, &mut sink);
    assert_eq!(seq.state(), State::Ended);
    assert_eq!(seq.counters().eos_seen, 1);

    // Double EOS
    seq.ingest(&eos, 0, 2100, &mut sink);
    assert_eq!(seq.counters().eos_dup, 1);
}

#[test]
fn test_u9_eos_then_data() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    let f0 = make_frame(sess, 1, 5);
    seq.ingest(&f0, 0, 1000, &mut sink);

    let eos = make_eos_frame(sess, 6);
    seq.ingest(&eos, 0, 2000, &mut sink);
    assert_eq!(seq.state(), State::Ended);

    // Data packet after EOS
    let f_after = make_frame(sess, 6, 2);
    seq.ingest(&f_after, 0, 3000, &mut sink);

    assert_eq!(seq.counters().data_after_eos, 1);
    assert_eq!(seq.counters().total_violations, 1);
}

#[test]
fn test_u10_gen_law() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess1 = b"TESTSESS01";
    let sess2 = b"TESTSESS02";

    let f0 = make_frame(sess1, 1, 5);
    seq.ingest(&f0, 0, 1000, &mut sink);
    let gen0 = seq.gen();

    // Gap open increments gen
    let f_gap = make_frame(sess1, 10, 2);
    seq.ingest(&f_gap, 0, 2000, &mut sink);
    let gen1 = seq.gen();
    assert!(gen1 > gen0);

    // Close gap (ReAnchored does NOT increment gen)
    let f_fill = make_frame(sess1, 6, 4);
    seq.ingest(&f_fill, 0, 3000, &mut sink);
    let gen2 = seq.gen();
    assert_eq!(gen2, gen1);

    // Session boundary increments gen
    let f_sess2 = make_frame(sess2, 1, 2);
    seq.ingest(&f_sess2, 0, 4000, &mut sink);
    let gen3 = seq.gen();
    assert!(gen3 > gen2);
}

#[test]
fn test_u11_seal() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    let f0 = make_frame(sess, 1, 5);
    seq.ingest(&f0, 0, 1000, &mut sink);

    seq.seal(DeadReason::RetryExhausted, &mut sink);
    assert_eq!(seq.state(), State::Dead);

    // Subsequent packets ignored
    let f1 = make_frame(sess, 6, 5);
    seq.ingest(&f1, 0, 2000, &mut sink);
    assert_eq!(seq.counters().ignored_after_dead, 1);
}

#[test]
fn test_u12_recovery_intent() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    // Anchor at W=1
    let f0 = make_frame(sess, 1, 1);
    seq.ingest(&f0, 0, 1_000_000, &mut sink);

    // 1. T-HWM: Stage > 512 messages ahead
    let f_hwm = make_frame(sess, 600, 10);
    seq.ingest(&f_hwm, 0, 1_000_000, &mut sink);

    let intent = seq.recovery_intent(1_000_000);
    assert!(intent.is_some());
    let intent = intent.unwrap();
    assert_eq!(intent.from, 2);
    assert_eq!(intent.to_excl, 609);

    // Close gap
    let f_fill = make_frame(sess, 2, 598);
    seq.ingest(&f_fill, 0, 1_000_000, &mut sink);
    assert_eq!(seq.watermark(), 610);

    // 2. T-TIME: Stage small gap and wait 250 µs
    let f_gap = make_frame(sess, 620, 5);
    seq.ingest(&f_gap, 0, 1_000_000, &mut sink);
    assert_eq!(seq.recovery_intent(1_100_000), None); // Only 100 µs elapsed
    let intent2 = seq.recovery_intent(1_300_000); // 300 µs elapsed >= 250 µs
    assert!(intent2.is_some());
    assert_eq!(intent2.unwrap().from, 610);
    assert_eq!(intent2.unwrap().to_excl, 624);
}

#[test]
fn test_u13_totality() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    // Init -> HB
    let hb = make_hb_frame(sess, 10);
    seq.ingest(&hb, 0, 1000, &mut sink);
    assert_eq!(seq.state(), State::Init);

    // Init -> EOS
    let eos = make_eos_frame(sess, 10);
    seq.ingest(&eos, 0, 2000, &mut sink);
    assert_eq!(seq.state(), State::Ended);
}

#[test]
fn test_u14_byte_identity() {
    let mut seq = Sequencer::new();
    let mut sink = TestSink::default();
    let sess = b"TESTSESS01";

    // Anchor
    let f0 = make_frame(sess, 1, 1);
    seq.ingest(&f0, 0, 1000, &mut sink);

    // Disordered frame
    let f_dis = make_frame(sess, 5, 2);
    seq.ingest(&f_dis, 0, 2000, &mut sink);

    // In-order filler
    let f_fill = make_frame(sess, 2, 3);
    seq.ingest(&f_fill, 0, 3000, &mut sink);

    assert_eq!(sink.msgs.len(), 6);
    for i in 1..=6 {
        let expected_msg = make_msg_s(i);
        assert_eq!(sink.msgs[(i - 1) as usize].1, expected_msg);
    }
}
