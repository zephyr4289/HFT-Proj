//! UDP Fake Retransmission Server (doc 08 §7, C9).
//! Serves ground truth slices over loopback UDP with deterministic fault injection.

use nf_protocol::moldudp64::REQUEST_LEN;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultMode {
    Ok,
    DelayMs(u64),
    DropRequest(usize),
    DropResponse(usize),
    TruncateAfter(u16),
    WrongSession,
    DuplicateFirst,
    Unbound,
}

#[derive(Debug, Clone)]
pub struct SessionTruth {
    pub session_id: [u8; 10],
    pub first_seq: u64,
    pub first_msg_index: u64,
    pub total_msgs: u64,
}

#[derive(Debug, Default)]
pub struct ServerCounters {
    pub requests_seen: AtomicU64,
    pub packets_served: AtomicU64,
    pub connections: AtomicU64,
    pub faults_injected: AtomicU64,
}

pub struct FakeRetransmissionServer {
    addr: SocketAddr,
    stop_signal: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    counters: Arc<ServerCounters>,
}

impl FakeRetransmissionServer {
    pub fn spawn(
        gt: &[u8],
        sessions: Vec<SessionTruth>,
        fault_mode: FaultMode,
    ) -> std::io::Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        let addr = socket.local_addr()?;
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;

        let stop_signal = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(ServerCounters::default());

        let gt_owned = gt.to_vec();
        let stop_clone = Arc::clone(&stop_signal);
        let counters_clone = Arc::clone(&counters);

        let handle = thread::spawn(move || {
            let mut req_counter = 0usize;
            let mut buf = [0u8; 1500];

            while !stop_clone.load(Ordering::Relaxed) {
                if let Ok((amt, src)) = socket.recv_from(&mut buf) {
                    if amt < REQUEST_LEN {
                        continue;
                    }
                    req_counter += 1;
                    counters_clone.requests_seen.fetch_add(1, Ordering::Relaxed);

                    let req_sess = &buf[0..10];
                    let req_seq = u64::from_be_bytes(buf[10..18].try_into().unwrap());
                    let req_count = u16::from_be_bytes(buf[18..20].try_into().unwrap());

                    match fault_mode {
                        FaultMode::DelayMs(ms) => {
                            thread::sleep(Duration::from_millis(ms));
                        }
                        FaultMode::DropRequest(n) => {
                            if req_counter == n {
                                counters_clone.faults_injected.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        }
                        _ => {}
                    }

                    // Find matching session truth
                    let truth = sessions.iter().find(|s| &s.session_id == req_sess);
                    let session_truth = match truth {
                        Some(t) => t,
                        None => {
                            counters_clone.faults_injected.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    };

                    let count_to_serve = match fault_mode {
                        FaultMode::TruncateAfter(k) => {
                            counters_clone.faults_injected.fetch_add(1, Ordering::Relaxed);
                            std::cmp::min(req_count, k)
                        }
                        _ => req_count,
                    };

                    let effective_sess = match fault_mode {
                        FaultMode::WrongSession => {
                            counters_clone.faults_injected.fetch_add(1, Ordering::Relaxed);
                            *b"WRONGSESS1"
                        }
                        _ => session_truth.session_id,
                    };

                    // Serve requested messages
                    let packets = pack_response_packets(
                        &gt_owned,
                        &effective_sess,
                        session_truth,
                        req_seq,
                        count_to_serve,
                    );

                    let mut is_first = true;
                    let mut pkt_idx = 0usize;
                    for pkt in packets {
                        pkt_idx += 1;
                        if let FaultMode::DropResponse(n) = fault_mode {
                            if pkt_idx == n {
                                counters_clone.faults_injected.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        }
                        if fault_mode == FaultMode::DuplicateFirst && is_first {
                            counters_clone.faults_injected.fetch_add(1, Ordering::Relaxed);
                            let _ = socket.send_to(&pkt, src);
                            counters_clone.packets_served.fetch_add(1, Ordering::Relaxed);
                        }
                        is_first = false;
                        let _ = socket.send_to(&pkt, src);
                        counters_clone.packets_served.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });

        Ok(Self {
            addr,
            stop_signal,
            handle: Some(handle),
            counters,
        })
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn counters(&self) -> &ServerCounters {
        &self.counters
    }
}

impl Drop for FakeRetransmissionServer {
    fn drop(&mut self) {
        self.stop_signal.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn pack_response_packets(
    gt: &[u8],
    session: &[u8; 10],
    truth: &SessionTruth,
    from_seq: u64,
    count: u16,
) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    if count == 0 {
        return packets;
    }

    let mut cur_seq = from_seq;
    let end_seq = from_seq + (count as u64);

    let mut cur_msg_index = 0u64;
    let mut cur_offset = 0usize;

    // Scan to find first_msg_index
    let target_msg_index = truth.first_msg_index + (from_seq.saturating_sub(truth.first_seq));

    while cur_msg_index < target_msg_index && cur_offset + 2 <= gt.len() {
        let len = u16::from_be_bytes([gt[cur_offset], gt[cur_offset + 1]]) as usize;
        cur_offset += 2 + len;
        cur_msg_index += 1;
    }

    while cur_seq < end_seq && cur_offset < gt.len() {
        let mut pkt = Vec::with_capacity(1400);
        pkt.extend_from_slice(session);
        pkt.extend_from_slice(&cur_seq.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes()); // placeholder count

        let mut pkt_count = 0u16;
        let mut pkt_offset = cur_offset;

        while cur_seq + (pkt_count as u64) < end_seq && pkt_offset + 2 <= gt.len() {
            let len = u16::from_be_bytes([gt[pkt_offset], gt[pkt_offset + 1]]) as usize;
            if pkt_offset + 2 + len > gt.len() {
                break;
            }
            let block_total = 2 + len;
            if pkt.len() + block_total > 1400 && pkt_count > 0 {
                break;
            }
            pkt.extend_from_slice(&gt[pkt_offset..pkt_offset + block_total]);
            pkt_offset += block_total;
            pkt_count += 1;
        }

        if pkt_count == 0 {
            break;
        }

        // Patch count
        pkt[18..20].copy_from_slice(&pkt_count.to_be_bytes());
        packets.push(pkt);

        cur_seq += pkt_count as u64;
        cur_offset = pkt_offset;
    }

    packets
}
