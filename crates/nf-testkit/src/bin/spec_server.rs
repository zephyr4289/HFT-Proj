//! Spec-Only MoldUDP64 Retransmission Server (doc 14 §3.2 / F-31).
//! Independent clean-room implementation derived strictly from NASDAQ MoldUDP64 V1.00 spec text:
//! - Section 3.1: 20-byte Request Packet grammar (Session [10B], Sequence [8B BE], Message Count [2B BE])
//! - Section 3.2: Standard Downstream Packet unicast response (Session [10B], Sequence [8B BE], Count [2B BE], Message Blocks [2B BE len + data])
//! Zero imports from nf-arbitrator or nf-recovery.

#![allow(warnings)]
#![allow(clippy::all)]

use std::collections::BTreeMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const SPEC_HEADER_LEN: usize = 20;
pub const SPEC_REQUEST_LEN: usize = 20;

pub struct SpecRetransmissionServer {
    port: u16,
    stop_signal: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SpecRetransmissionServer {
    /// Spawns the clean-room specification server serving a message database.
    pub fn spawn(session: [u8; 10], messages: BTreeMap<u64, Vec<u8>>) -> std::io::Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        let port = socket.local_addr()?.port();
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;

        let stop_signal = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop_signal);

        let handle = thread::spawn(move || {
            let mut req_buf = [0u8; 1500];

            while !stop_clone.load(Ordering::Relaxed) {
                if let Ok((amt, src)) = socket.recv_from(&mut req_buf) {
                    if amt < SPEC_REQUEST_LEN {
                        continue;
                    }
                    let req_sess = &req_buf[0..10];
                    if req_sess != session {
                        continue; // Session mismatch: silent drop per §3.1
                    }

                    let req_seq = u64::from_be_bytes(req_buf[10..18].try_into().unwrap());
                    let req_count = u16::from_be_bytes(req_buf[18..20].try_into().unwrap());

                    // Build standard MoldUDP64 Downstream Packets
                    let mut cur_seq = req_seq;
                    let mut remaining = req_count;

                    while remaining > 0 {
                        let mut pkt = Vec::with_capacity(1500);
                        // 20-byte Header: Session (10B), Sequence (8B BE), Count (2B BE placeholder)
                        pkt.extend_from_slice(&session);
                        pkt.extend_from_slice(&cur_seq.to_be_bytes());
                        pkt.extend_from_slice(&0u16.to_be_bytes()); // placeholder

                        let mut pkt_count = 0u16;
                        let mut pkt_seq = cur_seq;

                        while remaining > 0 {
                            if let Some(msg_data) = messages.get(&pkt_seq) {
                                if pkt.len() + 2 + msg_data.len() > 1400 {
                                    break; // MTU bound reached
                                }
                                pkt.extend_from_slice(&(msg_data.len() as u16).to_be_bytes());
                                pkt.extend_from_slice(msg_data);
                                pkt_count += 1;
                                pkt_seq += 1;
                                remaining -= 1;
                            } else {
                                break; // End of available messages
                            }
                        }

                        if pkt_count == 0 {
                            break;
                        }

                        // Patch actual count into header offset 18..20
                        pkt[18..20].copy_from_slice(&pkt_count.to_be_bytes());
                        let _ = socket.send_to(&pkt, src);
                        cur_seq = pkt_seq;
                    }
                }
            }
        });

        Ok(Self {
            port,
            stop_signal,
            handle: Some(handle),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for SpecRetransmissionServer {
    fn drop(&mut self) {
        self.stop_signal.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn main() {
    println!("=== SPEC-ONLY SERVER CLEAN-ROOM VALIDATION (doc 14 §3.2 / F-31) ===");
    let sess = *b"SPECSESS01";
    let mut db = BTreeMap::new();

    // Populate database with 100 sample messages
    for seq in 1..=100u64 {
        let msg = format!("ITCH_MSG_FOR_SEQ_{:06}", seq).into_bytes();
        db.insert(seq, msg);
    }

    let server = SpecRetransmissionServer::spawn(sess, db).expect("spawn spec server");
    println!("Spec server spawned on UDP port {}", server.port());

    // Client validation: send 20-byte Request Packet
    let client_sock = UdpSocket::bind("127.0.0.1:0").expect("bind client");
    client_sock.set_read_timeout(Some(Duration::from_millis(500))).expect("timeout");

    let mut req = Vec::with_capacity(20);
    req.extend_from_slice(&sess);
    req.extend_from_slice(&10u64.to_be_bytes()); // from seq 10
    req.extend_from_slice(&25u16.to_be_bytes()); // count 25

    client_sock.send_to(&req, format!("127.0.0.1:{}", server.port())).expect("send request");

    let mut resp_buf = [0u8; 1500];
    let (n, _) = client_sock.recv_from(&mut resp_buf).expect("recv response");

    assert!(n >= 20, "Response must contain at least 20-byte header");
    let resp_sess = &resp_buf[0..10];
    let resp_seq = u64::from_be_bytes(resp_buf[10..18].try_into().unwrap());
    let resp_cnt = u16::from_be_bytes(resp_buf[18..20].try_into().unwrap());

    println!(
        "SPEC_SERVER_RESPONSE: session={:?} start_seq={} count={} bytes_received={}",
        std::str::from_utf8(resp_sess).unwrap_or("???"),
        resp_seq,
        resp_cnt,
        n
    );

    assert_eq!(resp_sess, &sess, "Response session mismatch");
    assert_eq!(resp_seq, 10, "Response start seq mismatch");
    assert_eq!(resp_cnt, 25, "Response message count mismatch");

    println!("=== SPEC-ONLY SERVER CLEAN-ROOM VALIDATION PASSED (F-31 CLOSED) ===");
}
