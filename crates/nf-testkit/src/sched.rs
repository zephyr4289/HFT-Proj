//! Replay schedule builder, chaos models, and harness generation.
//! Implements Irwin-Hall-12 Gaussian approximation for IEEE cross-platform
//! bit-determinism (doc 04 §6).

#![allow(clippy::all, dead_code)]

use nf_transport::replay::{ReplaySchedule, SchedEvent, SchedKind};
use nf_transport::FeedId;

#[derive(Debug, Clone)]
pub struct SplitMix64 {
    pub state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / ((1u64 << 53) as f64))
    }

    /// Irwin-Hall-12 Gaussian approximation (doc 04 §6.4).
    /// Sum of 12 uniforms minus 6. Exact IEEE operations, zero libm drift.
    #[inline]
    pub fn next_gaussian_approx(&mut self) -> f64 {
        let mut sum = 0.0;
        for _ in 0..12 {
            sum += self.next_f64();
        }
        sum - 6.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packetize {
    Fixed(u16),
    MtuBound(u16),
    SeededRange { min: u16, max: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossModel {
    None,
    Bernoulli {
        p_pm: u32, // parts per thousand (e.g. 100 = 10%)
    },
    GilbertElliott {
        p_g2b_pm: u32,
        p_b2g_pm: u32,
        p_drop_good_pm: u32,
        p_drop_bad_pm: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayModel {
    None,
    GaussianApprox { mean_ns: i64, sigma_ns: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropRange {
    pub seq_from: u64,
    pub seq_to_incl: u64,
    pub feed_mask: u8, // bit 0 = Feed A (1), bit 1 = Feed B (2)
}

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    pub seed_a: u64,
    pub seed_b: u64,
    pub msgs_per_packet: Packetize,
    pub loss: [LossModel; 2],
    pub delay: [DelayModel; 2],
    pub base_rate_msg_per_sec: u64, // default 1_000_000
    pub heartbeat_interval_ns: u64, // default 1_000_000_000
    pub guarantee_coverage: bool,
    pub scripted_drops: Vec<DropRange>,
    pub session_change_at_msg: Option<u64>,
    pub feeds_enabled: u8, // 1 = A, 2 = B, 3 = Both
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            seed_a: 0x1234_5678_9ABC_DEF0,
            seed_b: 0x0FED_CBA9_8765_4321,
            msgs_per_packet: Packetize::Fixed(10),
            loss: [LossModel::None, LossModel::None],
            delay: [DelayModel::None, DelayModel::None],
            base_rate_msg_per_sec: 1_000_000,
            heartbeat_interval_ns: 1_000_000_000,
            guarantee_coverage: true,
            scripted_drops: Vec::new(),
            session_change_at_msg: None,
            feeds_enabled: 3, // Both feeds
        }
    }
}

struct RawMsgInfo {
    index: u64,
    len: u16,
}

pub fn build_schedule(gt: &[u8], config: &ReplayConfig) -> ReplaySchedule {
    let mut msg_infos = Vec::new();
    let mut pos = 0;
    let mut msg_idx = 0u64;

    while pos + 2 <= gt.len() {
        let len = u16::from_be_bytes([gt[pos], gt[pos + 1]]);
        pos += 2;
        if pos + (len as usize) > gt.len() {
            break;
        }
        pos += len as usize;
        msg_infos.push(RawMsgInfo {
            index: msg_idx,
            len,
        });
        msg_idx += 1;
    }

    let total_msgs = msg_infos.len() as u64;
    if total_msgs == 0 {
        return ReplaySchedule::default();
    }

    let mut prng_a = SplitMix64::new(config.seed_a);
    let mut prng_b = SplitMix64::new(config.seed_b);

    // 1. Packetization
    struct RawPacket {
        first_msg: u64,
        count: u16,
    }

    let mut packets = Vec::new();
    let mut cur_msg = 0usize;

    while cur_msg < msg_infos.len() {
        let count = match config.msgs_per_packet {
            Packetize::Fixed(k) => {
                let rem = (msg_infos.len() - cur_msg) as u16;
                std::cmp::min(k, rem)
            }
            Packetize::MtuBound(mtu) => {
                let max_payload = if mtu >= 20 { mtu - 20 } else { 50 };
                let mut accumulated = 0u16;
                let mut c = 0u16;
                while cur_msg + (c as usize) < msg_infos.len() {
                    let block_len = 2 + msg_infos[cur_msg + (c as usize)].len;
                    if c > 0 && accumulated + block_len > max_payload {
                        break;
                    }
                    accumulated += block_len;
                    c += 1;
                }
                c
            }
            Packetize::SeededRange { min, max } => {
                let range = (max - min + 1) as u64;
                let draw = min + (prng_a.next_u64() % range) as u16;
                let rem = (msg_infos.len() - cur_msg) as u16;
                std::cmp::min(draw, rem)
            }
        };

        let count = std::cmp::max(1, count);
        packets.push(RawPacket {
            first_msg: cur_msg as u64,
            count,
        });
        cur_msg += count as usize;
    }

    // 2. Build schedule events
    let mut events = Vec::new();
    let mut ge_state = [false; 2]; // false = good, true = bad state

    let ns_per_msg = if config.base_rate_msg_per_sec > 0 {
        1_000_000_000 / config.base_rate_msg_per_sec
    } else {
        1000
    };

    let mut cumulative_msgs = 0u64;

    for pkt in &packets {
        let first_seq = match config.session_change_at_msg {
            Some(split_m) if pkt.first_msg >= split_m => (pkt.first_msg - split_m) + 1,
            _ => pkt.first_msg + 1,
        };
        let base_vt = cumulative_msgs * ns_per_msg;
        cumulative_msgs += pkt.count as u64;

        let mut dropped = [false; 2];

        // Loss evaluation per feed
        for feed in 0..2 {
            if (config.feeds_enabled & (1 << feed)) == 0 {
                dropped[feed] = true;
                continue;
            }

            // Scripted drops check
            let pkt_end_seq = first_seq + (pkt.count as u64) - 1;
            let scripted = config.scripted_drops.iter().any(|d| {
                (d.feed_mask & (1 << feed)) != 0
                    && !(pkt_end_seq < d.seq_from || first_seq > d.seq_to_incl)
            });

            if scripted {
                dropped[feed] = true;
                continue;
            }

            // Stochastic loss
            let prng = if feed == 0 { &mut prng_a } else { &mut prng_b };
            match config.loss[feed] {
                LossModel::None => {}
                LossModel::Bernoulli { p_pm } => {
                    let draw = (prng.next_u64() % 1000) as u32;
                    if draw < p_pm {
                        dropped[feed] = true;
                    }
                }
                LossModel::GilbertElliott {
                    p_g2b_pm,
                    p_b2g_pm,
                    p_drop_good_pm,
                    p_drop_bad_pm,
                } => {
                    let is_bad = ge_state[feed];
                    let drop_p = if is_bad {
                        p_drop_bad_pm
                    } else {
                        p_drop_good_pm
                    };
                    let draw = (prng.next_u64() % 1000) as u32;
                    if draw < drop_p {
                        dropped[feed] = true;
                    }
                    // Transition
                    let trans_draw = (prng.next_u64() % 1000) as u32;
                    if is_bad {
                        if trans_draw < p_b2g_pm {
                            ge_state[feed] = false;
                        }
                    } else if trans_draw < p_g2b_pm {
                        ge_state[feed] = true;
                    }
                }
            }
        }

        // Guarantee coverage rejection sampling
        if config.guarantee_coverage
            && dropped[0]
            && dropped[1]
            && (config.feeds_enabled & 1 != 0)
            && (config.feeds_enabled & 2 != 0)
        {
            let chosen_feed = (prng_a.next_u64() & 1) as usize;
            dropped[chosen_feed] = false;
        }

        // Add events for delivered packets
        for feed in 0..2 {
            if !dropped[feed] {
                let prng = if feed == 0 { &mut prng_a } else { &mut prng_b };
                let delay_ns = match config.delay[feed] {
                    DelayModel::None => 0i64,
                    DelayModel::GaussianApprox { mean_ns, sigma_ns } => {
                        let z = prng.next_gaussian_approx();
                        (mean_ns as f64 + z * (sigma_ns as f64)).round() as i64
                    }
                };

                let release_vt = if delay_ns >= 0 {
                    base_vt.saturating_add(delay_ns as u64)
                } else {
                    base_vt.saturating_sub((-delay_ns) as u64)
                };

                events.push(SchedEvent {
                    release_vt,
                    feed: feed as FeedId,
                    kind: SchedKind::Packet {
                        first_seq,
                        first_msg: pkt.first_msg,
                        count: pkt.count,
                    },
                });
            }
        }
    }

    // 3. Harness Termination (TH-1): Heartbeats + EOS per feed
    let final_vt = events
        .iter()
        .map(|e| e.release_vt)
        .max()
        .unwrap_or(0);

    let final_next_seq = match config.session_change_at_msg {
        Some(split_m) if total_msgs >= split_m => (total_msgs - split_m) + 1,
        _ => total_msgs + 1,
    };

    for feed in 0..2 {
        if (config.feeds_enabled & (1 << feed)) != 0 {
            // Rolling heartbeats
            if config.heartbeat_interval_ns > 0 {
                let mut hb_vt = config.heartbeat_interval_ns;
                while hb_vt < final_vt {
                    events.push(SchedEvent {
                        release_vt: hb_vt,
                        feed: feed as FeedId,
                        kind: SchedKind::Heartbeat {
                            next_seq: final_next_seq,
                        },
                    });
                    hb_vt += config.heartbeat_interval_ns;
                }
            }

            // Terminal Heartbeat (N+1 or N-m+1)
            let thb_vt = final_vt + 1000;
            events.push(SchedEvent {
                release_vt: thb_vt,
                feed: feed as FeedId,
                kind: SchedKind::Heartbeat {
                    next_seq: final_next_seq,
                },
            });

            // AM-6: Terminal EOS scheduled at thb_vt + 50ms (>= 250us trigger + 4x10ms grace + slack)
            let eos_lead_ns = 50_000_000u64;
            events.push(SchedEvent {
                release_vt: thb_vt + eos_lead_ns,
                feed: feed as FeedId,
                kind: SchedKind::EndOfSession {
                    next_seq: final_next_seq,
                },
            });
        }
    }

    // 4. Sort by total order law
    events.sort();

    let session_split = config.session_change_at_msg.map(|m| {
        (m, *b"SPLITSESS2")
    });

    ReplaySchedule {
        events,
        session_split,
    }
}
