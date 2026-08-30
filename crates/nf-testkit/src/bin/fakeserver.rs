#![allow(warnings)]
#![allow(clippy::all)]

use nf_testkit::fakeserver::{FakeRetransmissionServer, FaultMode, SessionTruth};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::process;
use std::sync::atomic::Ordering;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut gt_path = "data/tests/sample-mini.itch".to_string();
    let mut fault_str = "ok".to_string();
    let mut session_str = "CHAOSSESS1".to_string();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--gt" => {
                if i + 1 < args.len() {
                    gt_path = args[i + 1].clone();
                    i += 1;
                }
            }
            "--fault" => {
                if i + 1 < args.len() {
                    fault_str = args[i + 1].clone();
                    i += 1;
                }
            }
            "--session" => {
                if i + 1 < args.len() {
                    session_str = args[i + 1].clone();
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    let gt = fs::read(&gt_path).unwrap_or_else(|e| {
        eprintln!("Error reading ground truth {}: {}", gt_path, e);
        process::exit(1);
    });

    let fault_mode = match fault_str.as_str() {
        "ok" => FaultMode::Ok,
        "wrong_session" => FaultMode::WrongSession,
        "duplicate_first" => FaultMode::DuplicateFirst,
        s if s.starts_with("delay:") => {
            let ms: u64 = s["delay:".len()..].parse().unwrap_or(50);
            FaultMode::DelayMs(ms)
        }
        s if s.starts_with("close:") => {
            let n: usize = s["close:".len()..].parse().unwrap_or(1);
            FaultMode::CloseOnRequest(n)
        }
        s if s.starts_with("ignore:") => {
            let n: usize = s["ignore:".len()..].parse().unwrap_or(1);
            FaultMode::IgnoreRequest(n)
        }
        s if s.starts_with("truncate:") => {
            let k: u16 = s["truncate:".len()..].parse().unwrap_or(10);
            FaultMode::TruncateAfter(k)
        }
        _ => FaultMode::Ok,
    };

    let mut sess_bytes = [b' '; 10];
    let b = session_str.as_bytes();
    let copy_len = std::cmp::min(10, b.len());
    sess_bytes[..copy_len].copy_from_slice(&b[..copy_len]);

    let sessions = vec![SessionTruth {
        session_id: sess_bytes,
        first_seq: 1,
        first_msg_index: 0,
        total_msgs: 10_000_000,
    }];

    let server = FakeRetransmissionServer::spawn(&gt, sessions, fault_mode).unwrap_or_else(|e| {
        eprintln!("Error binding fake server: {}", e);
        process::exit(1);
    });

    println!("PORT={}", server.port());

    // Wait on stdin EOF
    let mut stdin_buf = [0u8; 64];
    let _ = io::stdin().read(&mut stdin_buf);

    let cnt = server.counters();
    println!(
        "SERVER_COUNTERS requests={} packets={} connections={} faults={}",
        cnt.requests_seen.load(Ordering::Relaxed),
        cnt.packets_served.load(Ordering::Relaxed),
        cnt.connections.load(Ordering::Relaxed),
        cnt.faults_injected.load(Ordering::Relaxed)
    );
}
