//! Venue-Sender Simulator (doc 09 §6).
//! Renders MoldUDP64 packets from schedule and sends via UDP sockets to Feed A (10000) & Feed B (10001).

use nf_testkit::sched::{build_schedule, ReplayConfig};
use nf_transport::render::render_event;
use std::env;
use std::fs;
use std::net::UdpSocket;
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut config_path = "ci-xdp.toml";
    let mut sample_path = "data/tests/sample-mini.itch";

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                if i + 1 < args.len() {
                    config_path = &args[i + 1];
                    i += 1;
                }
            }
            "--sample" => {
                if i + 1 < args.len() {
                    sample_path = &args[i + 1];
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    let gt = fs::read(sample_path).unwrap_or_else(|_| {
        fs::read("../../data/tests/sample-mini.itch")
            .expect("Failed to load sample ITCH file")
    });

    let config = ReplayConfig::default();
    let sched = build_schedule(&gt, &config);

    let sock = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind UDP client socket");

    let dest_a = "127.0.0.1:10000";
    let dest_b = "127.0.0.1:10001";

    let session = *b"CHAOSSESS1";
    let mut arena = [0u8; 1500];

    println!("VENUE_SENDER starting: sending {} events", sched.events.len());

    let mut sent = 0usize;
    for ev in &sched.events {
        let len = render_event(&gt, ev, &sched, session, &mut arena);
        if len > 0 {
            let dest = if ev.feed == 0 { dest_a } else { dest_b };
            let _ = sock.send_to(&arena[..len], dest);
            sent += 1;
        }
    }

    println!("VENUE_SENDER finished: {} packets transmitted", sent);
}
