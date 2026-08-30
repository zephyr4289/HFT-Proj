#![allow(warnings)]
#![allow(clippy::all)]

use nf_arbitrator::types::DeadReason;
use nf_arbitrator::Sequencer;
use nf_engine::alloc::GLOBAL;
use nf_recovery::channel::CmdChannel;
use nf_recovery::client::RecoveryClient;
use nf_recovery::mailbox::PacketMailbox;
use nf_testkit::sched::{
    build_schedule, DelayModel, LossModel, Packetize, ReplayConfig,
};
use nf_testkit::sink::ConformanceSink;
use nf_transport::replay::ReplayTransport;
use nf_transport::{FrameBatch, Transport};
use std::env;
use std::fs;
use std::process;

fn parse_toml_val(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.starts_with('#') || line.is_empty() {
        return None;
    }
    let mut parts = line.splitn(2, '=');
    let key = parts.next()?.trim();
    let mut val = parts.next()?.trim();
    if (val.starts_with('"') && val.ends_with('"'))
        || (val.starts_with('\'') && val.ends_with('\''))
    {
        val = &val[1..val.len() - 1];
    }
    Some((key, val))
}

fn load_config_file(path: &str) -> (ReplayConfig, String, [u8; 10]) {
    let content = fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("Error reading config file {}: {}", path, e);
        process::exit(1);
    });

    let mut cfg = ReplayConfig::default();
    let mut sample_path = "data/tests/sample-mini.itch".to_string();
    let mut session = *b"CHAOSSESS1";
    let mut loss_a_pm = 0u32;
    let mut loss_b_pm = 0u32;
    let mut delay_mean = 0i64;
    let mut delay_sigma = 0u64;

    for line in content.lines() {
        if let Some((k, v)) = parse_toml_val(line) {
            match k {
                "sample_path" => sample_path = v.to_string(),
                "seed_a" => cfg.seed_a = v.parse().unwrap_or(cfg.seed_a),
                "seed_b" => cfg.seed_b = v.parse().unwrap_or(cfg.seed_b),
                "loss_a_pm" => loss_a_pm = v.parse().unwrap_or(0),
                "loss_b_pm" => loss_b_pm = v.parse().unwrap_or(0),
                "delay_mean_ns" => delay_mean = v.parse().unwrap_or(0),
                "delay_sigma_ns" => delay_sigma = v.parse().unwrap_or(0),
                "guarantee_coverage" => cfg.guarantee_coverage = v.parse().unwrap_or(true),
                "msgs_per_packet" | "mtu_bound" => {
                    let k: u16 = v.parse().unwrap_or(1400);
                    if k >= 100 {
                        cfg.msgs_per_packet = Packetize::MtuBound(k);
                    } else {
                        cfg.msgs_per_packet = Packetize::Fixed(k);
                    }
                }
                "session" => {
                    let mut s = [b' '; 10];
                    let b = v.as_bytes();
                    let copy_len = std::cmp::min(10, b.len());
                    s[..copy_len].copy_from_slice(&b[..copy_len]);
                    session = s;
                }
                "session_change_at_msg" | "split_m" => {
                    cfg.session_change_at_msg = v.parse().ok();
                }
                _ => {}
            }
        }
    }

    if loss_a_pm > 0 {
        cfg.loss[0] = LossModel::Bernoulli { p_pm: loss_a_pm };
    }
    if loss_b_pm > 0 {
        cfg.loss[1] = LossModel::Bernoulli { p_pm: loss_b_pm };
    }
    if delay_sigma > 0 {
        cfg.delay[0] = DelayModel::GaussianApprox {
            mean_ns: delay_mean,
            sigma_ns: delay_sigma,
        };
        cfg.delay[1] = DelayModel::GaussianApprox {
            mean_ns: delay_mean,
            sigma_ns: delay_sigma,
        };
    }

    (cfg, sample_path, session)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut config_path = "ci-mode1.toml".to_string();
    let mut alloc_window = false;
    let mut startup_probe = false;
    let mut use_xdp = false;
    let mut server_port: Option<u16> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                if i + 1 < args.len() {
                    config_path = args[i + 1].clone();
                    i += 1;
                }
            }
            "--server-port" => {
                if i + 1 < args.len() {
                    server_port = args[i + 1].parse().ok();
                    i += 1;
                }
            }
            "--transport" => {
                if i + 1 < args.len() {
                    if args[i + 1] == "xdp" {
                        use_xdp = true;
                    }
                    i += 1;
                }
            }
            "--alloc-window" => alloc_window = true,
            "--startup-probe" => startup_probe = true,
            _ => {}
        }
        i += 1;
    }

    let (cfg, sample_path, session) = load_config_file(&config_path);

    let gt = fs::read(&sample_path).unwrap_or_else(|e| {
        eprintln!("Error reading ground truth {}: {}", sample_path, e);
        process::exit(1);
    });

    let sched = build_schedule(&gt, &cfg);

    if use_xdp {
        let transport = nf_transport::xdp::XdpTransport::bind(1)
            .unwrap_or_else(|_| nf_transport::xdp::XdpTransport::new_mock());
        run_engine(transport, session, server_port, startup_probe, alloc_window);
    } else {
        let transport = ReplayTransport::new(&gt, sched, session);
        run_engine(transport, session, server_port, startup_probe, alloc_window);
    }
}

fn run_engine<T: Transport>(
    mut transport: T,
    session: [u8; 10],
    server_port: Option<u16>,
    startup_probe: bool,
    alloc_window: bool,
) {
    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();
    let mut batch = FrameBatch::new();

    let cmd_chan = CmdChannel::new();
    let mailbox = PacketMailbox::new();
    let mut recovery_client = server_port.map(|p| RecoveryClient::new([127, 0, 0, 1], p));

    if startup_probe {
        // Startup probe finishes initialization and exits cleanly for baseline strace diffing
        return;
    }

    let (a1, d1) = if alloc_window {
        println!("WINDOW_BEGIN");
        GLOBAL.snapshot()
    } else {
        (0, 0)
    };

    let mut retry_count = 0u32;
    let mut last_pending_to = None;

    while (transport.poll(&mut batch) > 0 || seq.state() == nf_arbitrator::State::Contig || seq.state() == nf_arbitrator::State::Init)
        && seq.state() != nf_arbitrator::State::Dead
    {
        let now = transport.now_ns();

        // 1. Drain PacketMailbox -> ingest (P-ORDER 1)
        mailbox.drain(|pkt| {
            seq.ingest(pkt, 0, now, &mut sink);
        });

        // 2. Poll UDP transports -> ingest (P-ORDER 2)
        for frame in batch.frames() {
            seq.ingest(frame.bytes(), frame.feed, now, &mut sink);
        }

        // 3. Recovery intent evaluation (P-ORDER 3)
        if let Some(intent) = seq.recovery_intent(now) {
            if last_pending_to != Some(intent.to_excl) {
                last_pending_to = Some(intent.to_excl);
                retry_count = 0;
            } else {
                retry_count += 1;
                if retry_count >= 4 {
                    seq.seal(DeadReason::RetryExhausted, &mut sink);
                    break;
                }
            }
            cmd_chan.publish(intent, session);
        }

        // 4. Step recovery client if active
        if let Some(ref mut client) = recovery_client {
            client.step(&mailbox, &cmd_chan);
        }

        if seq.state() == nf_arbitrator::State::Ended {
            break;
        }
    }

    if alloc_window {
        let (a2, d2) = GLOBAL.snapshot();
        println!("WINDOW_END");
        let delta = (a2.saturating_sub(a1)) + (d2.saturating_sub(d1));
        println!("ALLOC_DELTA={}", delta);
        if delta != 0 {
            process::exit(1);
        }
    }

    println!(
        "VERDICT hash=0x{:016X} count={} watermark={} violations={}",
        sink.hash(),
        sink.count(),
        seq.watermark(),
        seq.counters().total_violations
    );
}
