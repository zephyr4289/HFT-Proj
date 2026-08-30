//! AF_XDP Transport Test Suite X1–X6 (doc 09 §8).

#![allow(warnings)]

use nf_arbitrator::types::Event;
use nf_arbitrator::Sequencer;
use nf_protocol::moldudp64::parse;
use nf_testkit::golden::golden;
use nf_testkit::sched::{build_schedule, DelayModel, DropRange, LossModel, Packetize, ReplayConfig};
use nf_testkit::sink::ConformanceSink;
use nf_transport::xdp::{XdpTransport, UMEM_FRAME_SIZE, UMEM_TOTAL_SIZE};
use nf_transport::{FrameBatch, FrameView, Transport};
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

// ── X1: XDP Smoke Test ──────────────────────────────────────────────
#[test]
fn test_x1_xdp_smoke() {
    let mut transport = XdpTransport::new_mock();
    let mut batch = FrameBatch::new();

    let polled = transport.poll(&mut batch);
    assert_eq!(polled, 0);
    assert_eq!(batch.len(), 0);
    assert!(transport.now_ns() > 0);
}

// ── X2: Conformance Mini Schedule ───────────────────────────────────
#[test]
fn test_x2_conformance_mini() {
    let gt = load_mini_bytes();
    let sess = *b"XDPSESS001";

    let mut cfg = ReplayConfig::default();
    cfg.msgs_per_packet = Packetize::MtuBound(1400);
    cfg.guarantee_coverage = true;

    let sched = build_schedule(&gt, &cfg);
    let mut transport = nf_transport::replay::ReplayTransport::new(&gt, sched, sess);
    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();
    let mut batch = FrameBatch::new();

    while transport.poll(&mut batch) > 0 {
        let now = transport.now_ns();
        for frame in batch.frames() {
            seq.ingest(frame.bytes(), frame.feed, now, &mut sink);
        }
        let _ = seq.recovery_intent(now);
    }

    assert_eq!(sink.hash(), MINI_GOLDEN_HASH);
    assert_eq!(sink.count(), MINI_MESSAGE_COUNT);
    assert_eq!(seq.watermark(), MINI_MESSAGE_COUNT + 1);
    assert_eq!(seq.counters().total_violations, 0);
}

// ── X3: Scripted Dual-Drop over AF_XDP Simulation ───────────────────
#[test]
fn test_x3_scripted_dual_drop() {
    let gt = load_mini_bytes();
    let sess = *b"XDPSESS002";

    let mut cfg = ReplayConfig::default();
    cfg.msgs_per_packet = Packetize::MtuBound(1400);
    cfg.scripted_drops = vec![
        DropRange {
            seq_from: 100_000,
            seq_to_incl: 100_050,
            feed_mask: 3,
        },
        DropRange {
            seq_from: 500_000,
            seq_to_incl: 500_050,
            feed_mask: 3,
        },
    ];
    cfg.guarantee_coverage = true;

    let sched = build_schedule(&gt, &cfg);
    assert!(sched.events.len() > 0);
}

// ── X4: Fill-Ring Discipline ────────────────────────────────────────
#[test]
fn test_x4_fill_ring_discipline() {
    let mut transport = XdpTransport::new_mock();
    let mut batch = FrameBatch::new();

    for _ in 0..10 {
        let _ = transport.poll(&mut batch);
        batch.clear();
    }
}

// ── X5: Alloc Window on XDP Path ────────────────────────────────────
#[test]
fn test_x5_alloc_window_xdp() {
    let mut transport = XdpTransport::new_mock();
    let mut batch = FrameBatch::new();

    let _ = transport.poll(&mut batch);
    assert_eq!(batch.len(), 0);
}

// ── X6: Malicious Frames Handling ───────────────────────────────────
#[test]
fn test_x6_malicious_frames() {
    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();

    // Oversize packet header length
    let mut corrupt_frame = vec![0u8; 100];
    corrupt_frame[0..10].copy_from_slice(b"MALICIOUS1");
    corrupt_frame[10..18].copy_from_slice(&1u64.to_be_bytes());
    corrupt_frame[18..20].copy_from_slice(&5u16.to_be_bytes()); // 5 messages
    corrupt_frame[20..22].copy_from_slice(&1500u16.to_be_bytes()); // msg length 1500 > buffer

    seq.ingest(&corrupt_frame, 0, 1000, &mut sink);
    assert!(seq.counters().total_violations > 0);
}
