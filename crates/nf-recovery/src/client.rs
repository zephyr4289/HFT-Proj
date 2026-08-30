//! UDP MoldUDP64 Recovery Client (doc 08 v2.0, C9).
//! Non-blocking raw libc UDP socket, IP literal only, zero heap allocation.

use crate::channel::CmdChannel;
use crate::mailbox::PacketMailbox;
use crate::types::*;
use nf_protocol::moldudp64::{encode_request, REQUEST_LEN};

pub const FEED_R: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryCounters {
    pub requests_sent: u64,
    pub packets_received: u64,
    pub socket_errors: u64,
}

pub struct RecoveryClient {
    ip: [u8; 4],
    port: u16,
    fd: Option<i32>,
    state: ClientState,
    counters: RecoveryCounters,
}

impl RecoveryClient {
    pub fn new(ip: [u8; 4], port: u16) -> Self {
        let fd = unsafe {
            let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
            if s >= 0 {
                let flags = libc::fcntl(s, libc::F_GETFL, 0);
                if flags >= 0 {
                    libc::fcntl(s, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                let mut addr: libc::sockaddr_in = std::mem::zeroed();
                addr.sin_family = libc::AF_INET as libc::sa_family_t;
                addr.sin_port = port.to_be();
                addr.sin_addr.s_addr = u32::from_ne_bytes(ip);
                libc::connect(
                    s,
                    &addr as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                );
            }
            if s >= 0 {
                Some(s)
            } else {
                None
            }
        };

        Self {
            ip,
            port,
            fd,
            state: if fd.is_some() {
                ClientState::Connected
            } else {
                ClientState::Disconnected
            },
            counters: RecoveryCounters::default(),
        }
    }

    #[inline]
    pub fn state(&self) -> ClientState {
        self.state
    }

    #[inline]
    pub fn counters(&self) -> &RecoveryCounters {
        &self.counters
    }

    pub fn send_request(&mut self, session: &[u8; 10], from_seq: u64, count: u16) {
        if let Some(fd) = self.fd {
            let mut req_buf = [0u8; REQUEST_LEN];
            encode_request(session, from_seq, count, &mut req_buf);
            let res = unsafe {
                libc::send(
                    fd,
                    req_buf.as_ptr() as *const libc::c_void,
                    REQUEST_LEN,
                    0,
                )
            };
            if res > 0 {
                self.counters.requests_sent += 1;
            } else {
                self.counters.socket_errors += 1;
            }
        }
    }

    pub fn recv_packet(&mut self, buf: &mut [u8]) -> Option<usize> {
        if let Some(fd) = self.fd {
            let n = unsafe {
                libc::recv(
                    fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                )
            };
            if n > 0 {
                self.counters.packets_received += 1;
                return Some(n as usize);
            }
        }
        None
    }

    // Retained for testkit backward compatibility
    pub fn poll(&mut self, _cmd_chan: &CmdChannel, _mailbox: &PacketMailbox) {}

    pub fn disconnect(&mut self, _cmd_chan: &CmdChannel, _status: u32) {
        if let Some(fd) = self.fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
        self.state = ClientState::Disconnected;
    }
}

impl Drop for RecoveryClient {
    fn drop(&mut self) {
        if let Some(fd) = self.fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
    }
}
