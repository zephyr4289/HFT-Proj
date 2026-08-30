//! Fake Retransmission Server (doc 08 §7).
//! Harness component (std::net allowed); serves ground truth slices over loopback TCP with deterministic fault injection.

use nf_protocol::moldudp64::{HEADER_LEN, REQUEST_LEN};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultMode {
    Ok,
    DelayMs(u64),
    CloseOnRequest(usize),
    IgnoreRequest(usize),
    TruncateAfter(u16),
    WrongSession,
    DuplicateFirst,
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
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let stop_signal = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(ServerCounters::default());

        let gt_owned = gt.to_vec();
        let stop_clone = Arc::clone(&stop_signal);
        let counters_clone = Arc::clone(&counters);

        let handle = thread::spawn(move || {
            let mut req_counter = 0usize;

            while !stop_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        counters_clone.connections.fetch_add(1, Ordering::Relaxed);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));

                        let mut buf = [0u8; REQUEST_LEN];
                        while !stop_clone.load(Ordering::Relaxed) {
                            match stream.read_exact(&mut buf) {
                                Ok(()) => {
                                    req_counter += 1;
                                    counters_clone.requests_seen.fetch_add(1, Ordering::Relaxed);

                                    let req_sess = &buf[0..10];
                                    let req_seq = u64::from_be_bytes(buf[10..18].try_into().unwrap());
                                    let req_count = u16::from_be_bytes(buf[18..20].try_into().unwrap());

                                    match fault_mode {
                                        FaultMode::DelayMs(ms) => {
                                            thread::sleep(Duration::from_millis(ms));
                                        }
                                        FaultMode::CloseOnRequest(n) => {
                                            if req_counter == n {
                                                counters_clone.faults_injected.fetch_add(1, Ordering::Relaxed);
                                                break;
                                            }
                                        }
                                        FaultMode::IgnoreRequest(n) => {
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
                                            break;
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
                                    for pkt in packets {
                                        if fault_mode == FaultMode::DuplicateFirst && is_first {
                                            counters_clone.faults_injected.fetch_add(1, Ordering::Relaxed);
                                            let _ = send_framed_packet(&mut stream, &pkt);
                                            counters_clone.packets_served.fetch_add(1, Ordering::Relaxed);
                                        }
                                        is_first = false;

                                        if send_framed_packet(&mut stream, &pkt).is_err() {
                                            break;
                                        }
                                        counters_clone.packets_served.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Err(e) => {
                                    if e.kind() != std::io::ErrorKind::WouldBlock
                                        && e.kind() != std::io::ErrorKind::TimedOut
                                    {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
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

    #[inline]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    #[inline]
    pub fn counters(&self) -> &ServerCounters {
        &self.counters
    }

    pub fn stop(mut self) {
        self.stop_signal.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for FakeRetransmissionServer {
    fn drop(&mut self) {
        self.stop_signal.store(true, Ordering::Relaxed);
    }
}

fn send_framed_packet(stream: &mut TcpStream, packet: &[u8]) -> std::io::Result<()> {
    let len = packet.len() as u16;
    let len_be = len.to_be_bytes();
    stream.write_all(&len_be)?;
    stream.write_all(packet)?;
    stream.flush()?;
    Ok(())
}

fn pack_response_packets(
    gt: &[u8],
    session: &[u8; 10],
    truth: &SessionTruth,
    from_seq: u64,
    count: u16,
) -> Vec<Vec<u8>> {
    if count == 0 || from_seq < truth.first_seq {
        return Vec::new();
    }

    let rel_seq = from_seq - truth.first_seq;
    let start_msg_idx = truth.first_msg_index + rel_seq;

    // Locate start byte offset in GT
    let mut cur_msg = 0u64;
    let mut pos = 0usize;
    while cur_msg < start_msg_idx && pos + 2 <= gt.len() {
        let len = u16::from_be_bytes([gt[pos], gt[pos + 1]]) as usize;
        pos += 2 + len;
        cur_msg += 1;
    }

    let mut packets = Vec::new();
    let mut remaining = count as u64;
    let mut current_seq = from_seq;

    while remaining > 0 && pos + 2 <= gt.len() {
        let mut pkt = Vec::with_capacity(1400);
        pkt.extend_from_slice(session);
        pkt.extend_from_slice(&current_seq.to_be_bytes());
        pkt.extend_from_slice(&[0u8, 0u8]); // count placeholder

        let mut pkt_msg_count = 0u16;
        let mut pkt_pos = pos;

        while remaining > 0 && pkt_pos + 2 <= gt.len() {
            let msg_len = u16::from_be_bytes([gt[pkt_pos], gt[pkt_pos + 1]]) as usize;
            let block_size = 2 + msg_len;

            if HEADER_LEN + (pkt.len() - HEADER_LEN) + block_size > 1400 && pkt_msg_count > 0 {
                break;
            }

            if pkt_pos + block_size > gt.len() {
                break;
            }

            pkt.extend_from_slice(&gt[pkt_pos..pkt_pos + block_size]);
            pkt_pos += block_size;
            pkt_msg_count += 1;
            remaining -= 1;
        }

        if pkt_msg_count == 0 {
            break;
        }

        // Patch count
        let count_be = pkt_msg_count.to_be_bytes();
        pkt[18] = count_be[0];
        pkt[19] = count_be[1];

        packets.push(pkt);
        pos = pkt_pos;
        current_seq += pkt_msg_count as u64;
    }

    packets
}
