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

// ══════════════════════════════════════════════════════════════════════════
// R3 span-protocol equivalence: classic per-message emission == indexed span
// emission, judged by the strongest observable available — the golden FNV-1a
// fold over every emitted message — plus watermark, count, and counters.
// ══════════════════════════════════════════════════════════════════════════

fn fnv1a(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

const FNV_OFFSET: u64 = 0xcbf29ce484222325;

/// Per-message golden-fold sink (classic reference behavior).
struct GoldenHashSink {
    hash: u64,
    count: u64,
}

impl GoldenHashSink {
    fn new() -> Self {
        Self {
            hash: FNV_OFFSET,
            count: 0,
        }
    }
}

impl Sink for GoldenHashSink {
    fn on_msg(&mut self, _proof: &LiveFeedProof, _seq: u64, msg: &[u8]) {
        self.hash = fnv1a(self.hash, &(msg.len() as u16).to_le_bytes());
        self.hash = fnv1a(self.hash, msg);
        self.count += 1;
    }
    fn on_event(&mut self, _ev: &Event) {}
}

/// R3 span-mode golden-fold sink: consumes spans per the on_span contract
/// (message i = body[blocks[i].1 - base .. blocks[i].2 - base], seq
/// continuity blocks[i].0 == first_seq + i) and folds EACH message with the
/// identical per-message function. If the span contract delivered anything
/// other than the exact same bytes in the exact same order, the final hash
/// diverges from the classic run.
struct GoldenSpanSink {
    hash: u64,
    count: u64,
}

impl GoldenSpanSink {
    fn new() -> Self {
        Self {
            hash: FNV_OFFSET,
            count: 0,
        }
    }
}

impl Sink for GoldenSpanSink {
    fn on_msg(&mut self, _proof: &LiveFeedProof, _seq: u64, msg: &[u8]) {
        // Non-span fallback (gaps, unmemoized frames, drain) — identical fold.
        self.hash = fnv1a(self.hash, &(msg.len() as u16).to_le_bytes());
        self.hash = fnv1a(self.hash, msg);
        self.count += 1;
    }
    fn on_event(&mut self, _ev: &Event) {}
    fn wants_spans(&self) -> bool {
        true
    }
    fn on_span(
        &mut self,
        _proof: &LiveFeedProof,
        first_seq: u64,
        count: u16,
        body: &[u8],
        blocks: &[(u64, u32, u32)],
    ) {
        let base = blocks[0].1 as usize;
        for i in 0..count as usize {
            assert_eq!(
                blocks[i].0,
                first_seq + i as u64,
                "span contract: seq continuity"
            );
            let s = blocks[i].1 as usize - base;
            let e = blocks[i].2 as usize - base;
            assert!(e <= body.len(), "span contract: body bounds");
            let msg = &body[s..e];
            self.hash = fnv1a(self.hash, &(msg.len() as u16).to_le_bytes());
            self.hash = fnv1a(self.hash, msg);
            self.count += 1;
        }
    }
}

/// Walk a rendered frame's [len|msg] chain into (seq, start, end) triples —
/// the same walk the transport performs at construction.
fn frame_triples(frame: &[u8], first_seq: u64) -> Vec<(u64, u32, u32)> {
    let mut out = Vec::new();
    let mut pos = 20usize;
    let mut seq = first_seq;
    while pos + 2 <= frame.len() {
        let len = u16::from_be_bytes([frame[pos], frame[pos + 1]]) as usize;
        out.push((seq, (pos + 2) as u32, (pos + 2 + len) as u32));
        pos += 2 + len;
        seq += 1;
    }
    out
}

/// Compute the R2 verdict memo for a frame (exact leading-valid prefix) —
/// the same computation the transport performs at construction.
fn frame_memo(frame: &[u8], triples: &[(u64, u32, u32)]) -> FrameMemo {
    let mut valid: u16 = 0;
    for (i, t) in triples.iter().enumerate() {
        let msg = &frame[t.1 as usize..t.2 as usize];
        let ok = !msg.is_empty()
            && nf_protocol::itch5::LENGTH[msg[0] as usize] != 0
            && nf_protocol::itch5::LENGTH[msg[0] as usize] as usize == msg.len();
        if ok {
            valid += 1;
        } else {
            let _ = i;
            break;
        }
    }
    FrameMemo { valid_count: valid }
}

fn ingest_frame_indexed<S: Sink>(
    seq: &mut Sequencer,
    frame: &[u8],
    feed: FeedId,
    now: u64,
    sink: &mut S,
) {
    let first_seq = u64::from_be_bytes([
        frame[10], frame[11], frame[12], frame[13], frame[14], frame[15], frame[16], frame[17],
    ]);
    let triples = frame_triples(frame, first_seq);
    let memo = frame_memo(frame, &triples);
    seq.ingest_auto(frame, feed, now, sink, &triples, Some(memo));
}

/// D10a: classic vs span on a contiguous multi-packet flow with a partial
/// overlap (dup prefix) — hashes, counts, watermarks, emitted counters all
/// identical.
#[test]
fn test_r3_span_equivalence_contiguous_flow() {
    let sess = b"TESTSESS01";
    // [1..=10] w=11; [11..=20] w=21; [15..=24] straddles W (skip=6) emits
    // [21..=24] w=25; [25..=29] emits 5 more, w=30. Total = 10+10+4+5 = 29.
    let frames = [
        make_frame(sess, 1, 10),
        make_frame(sess, 11, 10),
        make_frame(sess, 15, 10),
        make_frame(sess, 25, 5),
    ];

    let mut seq_c = Sequencer::new();
    let mut sink_c = GoldenHashSink::new();
    for (i, f) in frames.iter().enumerate() {
        seq_c.ingest(f, 0, 1000 + i as u64, &mut sink_c);
    }

    let mut seq_s = Sequencer::new();
    let mut sink_s = GoldenSpanSink::new();
    for (i, f) in frames.iter().enumerate() {
        ingest_frame_indexed(&mut seq_s, f, 0, 1000 + i as u64, &mut sink_s);
    }

    assert_eq!(sink_c.count, 29);
    assert_eq!(seq_c.counters().dup_msgs, 6);
    assert_eq!(sink_c.count, sink_s.count, "span count divergence");
    assert_eq!(sink_c.hash, sink_s.hash, "span golden-fold hash divergence");
    assert_eq!(seq_c.watermark(), seq_s.watermark());
    assert_eq!(
        seq_c.counters().msgs_emitted,
        seq_s.counters().msgs_emitted
    );
    assert_eq!(seq_c.counters().dup_msgs, seq_s.counters().dup_msgs);
}

/// D10b: invalid message in frame — span path must NOT be taken (memo verdict
/// < n), classic error semantics preserved: same emitted prefix, same
/// violation counters, same watermark.
#[test]
fn test_r3_span_fallback_on_invalid() {
    let sess = b"TESTSESS01";
    let good = make_frame(sess, 1, 5);
    // second frame: 4 valid + 1 unknown-type message
    let mut bad = make_frame(sess, 6, 5);
    // message 5 of the frame starts after 4 * (2 + 12) body bytes + 20 header
    let bad_type_pos = 20 + 4 * 14 + 2;
    bad[bad_type_pos] = 0xFE;

    let frames = [good.clone(), bad, make_frame(sess, 11, 3)];

    let mut seq_c = Sequencer::new();
    let mut sink_c = GoldenHashSink::new();
    for (i, f) in frames.iter().enumerate() {
        seq_c.ingest(f, 0, 1000 + i as u64, &mut sink_c);
    }

    let mut seq_s = Sequencer::new();
    let mut sink_s = GoldenSpanSink::new();
    for (i, f) in frames.iter().enumerate() {
        ingest_frame_indexed(&mut seq_s, f, 0, 1000 + i as u64, &mut sink_s);
    }

    assert_eq!(sink_c.count, sink_s.count, "invalid-frame count divergence");
    assert_eq!(sink_c.hash, sink_s.hash, "invalid-frame hash divergence");
    assert_eq!(seq_c.watermark(), seq_s.watermark());
    assert_eq!(
        seq_c.counters().msgs_emitted,
        seq_s.counters().msgs_emitted
    );
    assert_eq!(
        seq_c.counters().total_violations,
        seq_s.counters().total_violations
    );
    assert!(seq_s.counters().total_violations >= 1);
}

/// D10c: gap packet arrives ahead of W — messages stage, drain emits per
/// message through the sink's on_msg fallback; identical hash to classic.
#[test]
fn test_r3_span_gap_staging_drain() {
    let sess = b"TESTSESS01";
    let frames = [
        make_frame(sess, 1, 5),
        make_frame(sess, 11, 5),  // gap: [11..=15] staged
        make_frame(sess, 6, 5),   // fills [6..=10], drains staged [11..=15]
    ];

    let mut seq_c = Sequencer::new();
    let mut sink_c = GoldenHashSink::new();
    for (i, f) in frames.iter().enumerate() {
        seq_c.ingest(f, 0, 1000 + i as u64, &mut sink_c);
    }

    let mut seq_s = Sequencer::new();
    let mut sink_s = GoldenSpanSink::new();
    for (i, f) in frames.iter().enumerate() {
        ingest_frame_indexed(&mut seq_s, f, 0, 1000 + i as u64, &mut sink_s);
    }

    assert_eq!(sink_c.count, 15);
    assert_eq!(sink_c.count, sink_s.count, "gap-path count divergence");
    assert_eq!(sink_c.hash, sink_s.hash, "gap-path hash divergence");
    assert_eq!(seq_c.watermark(), seq_s.watermark());
    assert_eq!(seq_c.staged_count(), seq_s.staged_count());
}

/// D10d: sink that does NOT opt in keeps exact per-message semantics on the
/// indexed+memo path (memo only skips validate calls that would pass).
#[test]
fn test_r3_no_optin_unchanged() {
    let sess = b"TESTSESS01";
    let frames = [
        make_frame(sess, 1, 8),
        make_frame(sess, 5, 8), // partial overlap
        make_frame(sess, 20, 4),
    ];
    let mut seq_c = Sequencer::new();
    let mut sink_c = GoldenHashSink::new();
    for (i, f) in frames.iter().enumerate() {
        seq_c.ingest(f, 1, 1000 + i as u64, &mut sink_c);
    }
    let mut seq_i = Sequencer::new();
    let mut sink_i = GoldenHashSink::new();
    for (i, f) in frames.iter().enumerate() {
        ingest_frame_indexed(&mut seq_i, f, 1, 1000 + i as u64, &mut sink_i);
    }
    assert_eq!(sink_c.hash, sink_i.hash);
    assert_eq!(sink_c.count, sink_i.count);
    assert_eq!(seq_c.watermark(), seq_i.watermark());
    assert_eq!(
        seq_c.counters().msgs_emitted,
        seq_i.counters().msgs_emitted
    );
}

/// D10e: HB/EOS frames route classic (empty triples) on both arms; span sink
/// observes identical events.
#[test]
fn test_r3_span_hb_eos() {
    let sess = b"TESTSESS01";
    let frames = [
        make_frame(sess, 1, 5),
        make_hb_frame(sess, 6),
        make_frame(sess, 6, 4),
        make_eos_frame(sess, 10),
    ];
    let mut seq_c = Sequencer::new();
    let mut sink_c = GoldenHashSink::new();
    for (i, f) in frames.iter().enumerate() {
        seq_c.ingest(f, 0, 1000 + i as u64, &mut sink_c);
    }
    let mut seq_s = Sequencer::new();
    let mut sink_s = GoldenSpanSink::new();
    for (i, f) in frames.iter().enumerate() {
        ingest_frame_indexed(&mut seq_s, f, 0, 1000 + i as u64, &mut sink_s);
    }
    assert_eq!(sink_c.hash, sink_s.hash);
    assert_eq!(sink_c.count, sink_s.count);
    assert_eq!(seq_c.watermark(), seq_s.watermark());
    assert_eq!(seq_c.state(), seq_s.state());
}
