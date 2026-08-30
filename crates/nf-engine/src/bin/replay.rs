#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use nf_arbitrator::Sequencer;
use nf_engine::alloc::GLOBAL;
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
                "msgs_per_packet" => {
                    let k: u16 = v.parse().unwrap_or(10);
                    cfg.msgs_per_packet = Packetize::Fixed(k);
                }
                "session" => {
                    let mut s = [b' '; 10];
                    let b = v.as_bytes();
                    let copy_len = std::cmp::min(10, b.len());
                    s[..copy_len].copy_from_slice(&b[..copy_len]);
                    session = s;
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

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                if i + 1 < args.len() {
                    config_path = args[i + 1].clone();
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
    let mut transport = ReplayTransport::new(&gt, sched, session);
    let mut seq = Sequencer::new();
    let mut sink = ConformanceSink::new();
    let mut batch = FrameBatch::new();

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

    loop {
        let now = transport.now_ns();
        let n = transport.poll(&mut batch);
        if n == 0 {
            break;
        }
        for frame in batch.frames() {
            seq.ingest(frame.bytes(), frame.feed, now, &mut sink);
        }
        let _ = seq.recovery_intent(now);
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
