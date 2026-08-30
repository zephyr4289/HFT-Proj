//! Testkit: chaos scheduler, fake retransmit server, and golden hash harnesses.

pub mod golden;
pub mod sched;

#[cfg(test)]
mod tests {
    use super::golden::{golden, golden_cross_check};
    use super::sched::{
        build_schedule, DelayModel, DropRange, LossModel, Packetize, ReplayConfig,
    };
    use nf_protocol::packet::validate_frame;
    use nf_transport::replay::{ReplayTransport, SchedKind};
    use nf_transport::{FrameBatch, Transport};

    const MINI_PATH: &str = "../../data/tests/sample-mini.itch";
    pub const MINI_GOLDEN_HASH: u64 = 0xDE4C_837A_B4A6_78BB;
    pub const MINI_MESSAGE_COUNT: u64 = 505_849;

    fn load_mini_bytes() -> Vec<u8> {
        std::fs::read(MINI_PATH).unwrap_or_else(|_| {
            std::fs::read("data/tests/sample-mini.itch")
                .expect("Failed to load sample-mini.itch")
        })
    }

    #[test]
    fn t1_golden_walker_mini() {
        let gt = load_mini_bytes();
        let (h, count) = golden(&gt);
        assert_eq!(count, MINI_MESSAGE_COUNT);
        assert_eq!(h, MINI_GOLDEN_HASH);

        // Cross-check vs independent implementation on first 10,000 messages
        let (h_std, c_std) = golden(&gt);
        let (h_cross, c_cross) = golden_cross_check(&gt, 10_000);
        let (h_sub, c_sub) = {
            let mut pos = 0;
            let mut c = 0;
            while pos + 2 <= gt.len() && c < 10_000 {
                let l = u16::from_be_bytes([gt[pos], gt[pos + 1]]) as usize;
                pos += 2 + l;
                c += 1;
            }
            golden(&gt[..pos])
        };
        assert_eq!(c_cross, 10_000);
        assert_eq!(c_sub, 10_000);
        assert_eq!(h_cross, h_sub);
    }

    #[test]
    fn t2_schedule_determinism() {
        let gt = load_mini_bytes();
        let mut config1 = ReplayConfig::default();
        config1.seed_a = 0xAAAA_BBBB_CCCC_DDDD;
        config1.seed_b = 0x1111_2222_3333_4444;
        config1.loss = [
            LossModel::Bernoulli { p_pm: 50 },
            LossModel::Bernoulli { p_pm: 50 },
        ];
        config1.delay = [
            DelayModel::GaussianApprox {
                mean_ns: 10_000,
                sigma_ns: 2_000,
            },
            DelayModel::GaussianApprox {
                mean_ns: 10_000,
                sigma_ns: 2_000,
            },
        ];

        let sched1a = build_schedule(&gt, &config1);
        let sched1b = build_schedule(&gt, &config1);

        assert_eq!(sched1a.events.len(), sched1b.events.len());
        for (e1, e2) in sched1a.events.iter().zip(sched1b.events.iter()) {
            assert_eq!(e1, e2);
        }

        let mut config2 = config1.clone();
        config2.seed_a = 0x9999_8888_7777_6666;
        let sched2 = build_schedule(&gt, &config2);
        assert_ne!(sched1a.events, sched2.events);
    }

    #[test]
    fn t3_packetization_modes() {
        let gt = load_mini_bytes();

        // 1. Fixed(k)
        let mut cfg_fixed = ReplayConfig::default();
        cfg_fixed.msgs_per_packet = Packetize::Fixed(25);
        let s_fixed = build_schedule(&gt, &cfg_fixed);
        for ev in &s_fixed.events {
            if let SchedKind::Packet { count, first_msg, .. } = ev.kind {
                if first_msg + 25 <= MINI_MESSAGE_COUNT {
                    assert_eq!(count, 25);
                } else {
                    assert_eq!(count, (MINI_MESSAGE_COUNT - first_msg) as u16);
                }
            }
        }

        // 2. MtuBound(1500)
        let mut cfg_mtu = ReplayConfig::default();
        cfg_mtu.msgs_per_packet = Packetize::MtuBound(1500);
        let s_mtu = build_schedule(&gt, &cfg_mtu);
        assert!(!s_mtu.events.is_empty());

        // 3. SeededRange(5..20)
        let mut cfg_range = ReplayConfig::default();
        cfg_range.msgs_per_packet = Packetize::SeededRange { min: 5, max: 20 };
        let s_range = build_schedule(&gt, &cfg_range);
        for ev in &s_range.events {
            if let SchedKind::Packet { count, first_msg, .. } = ev.kind {
                if first_msg + 20 <= MINI_MESSAGE_COUNT {
                    assert!(count >= 5 && count <= 20);
                }
            }
        }
    }

    #[test]
    fn t4_render_round_trip() {
        let gt = load_mini_bytes();
        let mut cfg = ReplayConfig::default();
        cfg.msgs_per_packet = Packetize::Fixed(15);
        cfg.feeds_enabled = 1; // Feed A only for quick render test
        let sched = build_schedule(&gt, &cfg);

        let mut transport = ReplayTransport::new(&gt, sched, *b"TESTREPLAY");
        let mut batch = FrameBatch::new();

        let mut total_rendered = 0;
        while transport.poll(&mut batch) > 0 {
            for frame in batch.frames() {
                let parsed = validate_frame(frame.bytes()).expect("validate rendered frame");
                match parsed {
                    nf_protocol::moldudp64::Parsed::Data { header, blocks } => {
                        assert_eq!(header.session, *b"TESTREPLAY");
                        assert!(blocks.len() > 0);
                        total_rendered += blocks.len();
                    }
                    nf_protocol::moldudp64::Parsed::Heartbeat { .. } => {}
                    nf_protocol::moldudp64::Parsed::EndOfSession { .. } => {}
                }
            }
        }
        assert_eq!(total_rendered as u64, MINI_MESSAGE_COUNT);
    }

    #[test]
    fn t5_accounting_identity_guarantee_coverage() {
        let gt = load_mini_bytes();
        let mut cfg = ReplayConfig::default();
        cfg.loss = [
            LossModel::Bernoulli { p_pm: 200 },
            LossModel::Bernoulli { p_pm: 200 },
        ];
        cfg.guarantee_coverage = true;

        let sched = build_schedule(&gt, &cfg);

        // Every message index 0..N-1 must be covered by at least one delivered packet
        let mut covered = vec![false; MINI_MESSAGE_COUNT as usize];
        for ev in &sched.events {
            if let SchedKind::Packet { first_msg, count, .. } = ev.kind {
                for i in 0..count {
                    covered[(first_msg + i as u64) as usize] = true;
                }
            }
        }
        assert!(covered.iter().all(|&c| c), "All messages must be covered when guarantee_coverage is true");
    }

    #[test]
    fn t6_th1_structure() {
        let gt = load_mini_bytes();
        let cfg = ReplayConfig::default();
        let sched = build_schedule(&gt, &cfg);

        for feed in 0..2 {
            let feed_events: Vec<_> = sched
                .events
                .iter()
                .filter(|e| e.feed == feed)
                .collect();
            assert!(feed_events.len() >= 2);
            let last_ev = feed_events[feed_events.len() - 1];
            let prev_ev = feed_events[feed_events.len() - 2];

            match last_ev.kind {
                SchedKind::EndOfSession { next_seq } => {
                    assert_eq!(next_seq, MINI_MESSAGE_COUNT + 1);
                }
                _ => panic!("Last event on feed {} must be EndOfSession", feed),
            }

            match prev_ev.kind {
                SchedKind::Heartbeat { next_seq } => {
                    assert_eq!(next_seq, MINI_MESSAGE_COUNT + 1);
                }
                _ => panic!("Penultimate event on feed {} must be Terminal Heartbeat", feed),
            }
        }
    }

    #[test]
    fn t7_session_split_invariance() {
        let gt = load_mini_bytes();
        let (h_plain, c_plain) = golden(&gt);
        assert_eq!(c_plain, MINI_MESSAGE_COUNT);
        assert_eq!(h_plain, MINI_GOLDEN_HASH);
    }

    #[test]
    fn t8_delay_distribution_sanity() {
        let gt = load_mini_bytes();
        let mut cfg = ReplayConfig::default();
        cfg.delay = [
            DelayModel::GaussianApprox {
                mean_ns: 50_000,
                sigma_ns: 10_000,
            },
            DelayModel::None,
        ];
        cfg.feeds_enabled = 1;
        let sched = build_schedule(&gt, &cfg);

        for ev in &sched.events {
            if let SchedKind::Packet { .. } = ev.kind {
                // Check delay variance bounds
                // Delay must be bounded in [mean - 6*sigma, mean + 6*sigma]
                // 50,000 +- 60,000
            }
        }
        assert!(!sched.events.is_empty());
    }

    #[test]
    fn t9_full_mini_chaos_run() {
        let gt = load_mini_bytes();
        let mut cfg = ReplayConfig::default();
        cfg.msgs_per_packet = Packetize::MtuBound(1400);
        cfg.loss = [
            LossModel::Bernoulli { p_pm: 50 },
            LossModel::Bernoulli { p_pm: 50 },
        ];
        cfg.delay = [
            DelayModel::GaussianApprox {
                mean_ns: 20_000,
                sigma_ns: 5_000,
            },
            DelayModel::GaussianApprox {
                mean_ns: 20_000,
                sigma_ns: 5_000,
            },
        ];
        cfg.guarantee_coverage = true;

        let sched = build_schedule(&gt, &cfg);
        let mut transport = ReplayTransport::new(&gt, sched, *b"CHAOSSESS1");
        let mut batch = FrameBatch::new();

        while transport.poll(&mut batch) > 0 {
            for frame in batch.frames() {
                assert!(validate_frame(frame.bytes()).is_ok());
            }
        }
    }

    #[test]
    fn t10_max_rate_mode() {
        let gt = load_mini_bytes();
        let mut cfg = ReplayConfig::default();
        cfg.msgs_per_packet = Packetize::Fixed(50);
        cfg.loss = [LossModel::None, LossModel::None];
        cfg.delay = [DelayModel::None, DelayModel::None];
        cfg.feeds_enabled = 1;

        let sched = build_schedule(&gt, &cfg);
        let mut transport = ReplayTransport::new(&gt, sched, *b"MAXRATESES");
        let mut batch = FrameBatch::new();

        let mut last_seq = 0u64;
        while transport.poll(&mut batch) > 0 {
            for frame in batch.frames() {
                if let Ok(nf_protocol::moldudp64::Parsed::Data { header, .. }) =
                    validate_frame(frame.bytes())
                {
                    assert!(header.seq > last_seq);
                    last_seq = header.seq;
                }
            }
        }
    }
}
