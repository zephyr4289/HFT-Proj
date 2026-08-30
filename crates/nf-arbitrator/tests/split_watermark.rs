//! A-1 Watermark discriminator test (doc 08 §12, E2E-2b / A-1).
//! Proves that with session split at message m, final watermark is strictly N - m + 1.

use nf_arbitrator::Sequencer;
use nf_testkit::golden::golden;
use nf_testkit::sched::{build_schedule, Packetize, ReplayConfig};
use nf_testkit::sink::ConformanceSink;
use nf_transport::replay::ReplayTransport;
use nf_transport::{FrameBatch, Transport};

const MINI_PATH: &str = "../../data/tests/sample-mini.itch";

fn load_mini_bytes() -> Vec<u8> {
    std::fs::read(MINI_PATH).unwrap_or_else(|_| {
        std::fs::read("data/tests/sample-mini.itch")
            .expect("Failed to load sample-mini.itch")
    })
}

#[test]
fn test_a1_split_watermark_discriminator() {
    let gt = load_mini_bytes();
    let (expected_hash, total_n) = golden(&gt);
    assert_eq!(total_n, 505_849);

    let split_m = 200_000u64;
    let mut cfg = ReplayConfig::default();
    cfg.msgs_per_packet = Packetize::Fixed(20);
    cfg.session_change_at_msg = Some(split_m);
    cfg.guarantee_coverage = true;

    let sched = build_schedule(&gt, &cfg);
    let mut transport = ReplayTransport::new(&gt, sched, *b"SESSION001");
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

    // A-1 discriminator assertions:
    assert_eq!(sink.count(), total_n, "All N messages must be emitted across the split");
    assert_eq!(sink.hash(), expected_hash, "Output hash must be split-invariant (C-8)");
    assert_eq!(seq.counters().sessions, 2, "Exactly 2 sessions must be observed");

    let expected_final_wm = total_n - split_m + 1;
    assert_eq!(
        seq.watermark(),
        expected_final_wm,
        "Lawful final watermark in Session 2 must be N - m + 1 ({})",
        expected_final_wm
    );
}
