//! Thread R Recovery Client (doc 08 §4).
//! Non-blocking raw libc TCP socket, IP literal only, zero allocation.

use crate::channel::{CmdChannel, CmdPayload};
use crate::mailbox::PacketMailbox;
use crate::types::*;
use nf_protocol::moldudp64::encode_request;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub struct RecoveryClient {
    ip: [u8; 4],
    port: u16,
    fd: Option<i32>,
    state: ClientState,
    rx: Box<[u8; 65536]>,
    have: usize,
    scan: usize,
    last_epoch: u64,
    active_cmd: Option<CmdPayload>,
    counters: RecoveryCounters,
}

impl RecoveryClient {
    pub fn new(ip: [u8; 4], port: u16) -> Self {
        Self {
            ip,
            port,
            fd: None,
            state: ClientState::Disconnected,
            rx: Box::new([0u8; 65536]),
            have: 0,
            scan: 0,
            last_epoch: 0,
            active_cmd: None,
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

    pub fn disconnect(&mut self, cmd_chan: &CmdChannel, status: u32) {
        if let Some(fd) = self.fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
        self.state = ClientState::Disconnected;
        cmd_chan.set_status(status);
        self.have = 0;
        self.scan = 0;
    }

    fn try_connect(&mut self, cmd_chan: &CmdChannel) {
        self.counters.connections_attempted += 1;
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            if fd < 0 {
                self.counters.socket_errors += 1;
                cmd_chan.set_status(STATUS_SOCKET_ERROR);
                return;
            }

            // Set non-blocking
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                libc::close(fd);
                self.counters.socket_errors += 1;
                cmd_chan.set_status(STATUS_SOCKET_ERROR);
                return;
            }

            let mut addr: libc::sockaddr_in = std::mem::zeroed();
            addr.sin_family = libc::AF_INET as libc::sa_family_t;
            addr.sin_port = self.port.to_be();
            addr.sin_addr.s_addr = u32::from_ne_bytes(self.ip);

            let res = libc::connect(
                fd,
                &addr as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            );

            self.fd = Some(fd);

            if res == 0 {
                self.state = ClientState::Connected;
                cmd_chan.set_status(STATUS_CONNECTED);
            } else {
                let err = *libc::__errno_location();
                if err == libc::EINPROGRESS || err == libc::EWOULDBLOCK || err == libc::EAGAIN {
                    self.state = ClientState::Connecting;
                    cmd_chan.set_status(STATUS_CONNECTING);
                } else {
                    libc::close(fd);
                    self.fd = None;
                    self.counters.socket_errors += 1;
                    self.state = ClientState::Disconnected;
                    cmd_chan.set_status(STATUS_SOCKET_ERROR);
                }
            }
        }
    }

    fn poll_writable(&mut self, cmd_chan: &CmdChannel) {
        let fd = match self.fd {
            Some(fd) => fd,
            None => {
                self.state = ClientState::Disconnected;
                return;
            }
        };

        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };

        let res = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, 0) };
        if res > 0 {
            if (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0 {
                self.disconnect(cmd_chan, STATUS_SOCKET_ERROR);
                self.counters.socket_errors += 1;
                return;
            }

            if (pfd.revents & libc::POLLOUT) != 0 {
                let mut err: libc::c_int = 0;
                let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                let opt_res = unsafe {
                    libc::getsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        &mut err as *mut libc::c_int as *mut libc::c_void,
                        &mut len as *mut libc::socklen_t,
                    )
                };

                if opt_res == 0 && err == 0 {
                    self.state = ClientState::Connected;
                    cmd_chan.set_status(STATUS_CONNECTED);
                    if let Some(cmd) = self.active_cmd {
                        self.apply(&cmd);
                    }
                } else {
                    self.disconnect(cmd_chan, STATUS_SOCKET_ERROR);
                    self.counters.socket_errors += 1;
                }
            }
        }
    }

    fn drain_socket(&mut self, mailbox: &PacketMailbox, cmd_chan: &CmdChannel) {
        let fd = match self.fd {
            Some(fd) => fd,
            None => {
                self.state = ClientState::Disconnected;
                return;
            }
        };

        if self.have >= self.rx.len() {
            // Buffer full without valid framing
            self.counters.malformed_stream += 1;
            self.disconnect(cmd_chan, STATUS_SOCKET_ERROR);
            return;
        }

        let n = unsafe {
            libc::recv(
                fd,
                self.rx[self.have..].as_mut_ptr() as *mut libc::c_void,
                self.rx.len() - self.have,
                libc::MSG_DONTWAIT,
            )
        };

        if n > 0 {
            self.have += n as usize;
            self.counters.responses_received += 1;
            self.parse_framed(mailbox, cmd_chan);
        } else if n == 0 {
            self.counters.server_closed += 1;
            self.disconnect(cmd_chan, STATUS_SERVER_CLOSED);
        } else {
            let err = unsafe { *libc::__errno_location() };
            if err != libc::EAGAIN && err != libc::EWOULDBLOCK {
                self.counters.socket_errors += 1;
                self.disconnect(cmd_chan, STATUS_SOCKET_ERROR);
            }
        }
    }

    fn parse_framed(&mut self, mailbox: &PacketMailbox, cmd_chan: &CmdChannel) {
        while self.have.saturating_sub(self.scan) >= 2 {
            let len = u16::from_be_bytes([self.rx[self.scan], self.rx[self.scan + 1]]) as usize;
            if len > 1500 {
                // Oversize/hostile: mailbox cannot bound -> disconnect
                self.counters.malformed_stream += 1;
                self.counters.oversize_packets += 1;
                self.disconnect(cmd_chan, STATUS_SOCKET_ERROR);
                return;
            }

            if self.have.saturating_sub(self.scan) < 2 + len {
                break; // Need more bytes
            }

            let pkt = &self.rx[self.scan + 2..self.scan + 2 + len];

            // INV-R5: Stale-session guard
            let current_cmd = cmd_chan.read_current();
            if pkt.len() >= 10 && pkt[0..10] == current_cmd.session {
                if !mailbox.try_push(pkt) {
                    break;
                }
                self.counters.packets_forwarded += 1;
            } else {
                self.counters.stale_session_dropped += 1;
            }

            self.scan += 2 + len;
        }

        // Compact buffer
        if self.scan > 0 {
            let remaining = self.have - self.scan;
            if remaining > 0 {
                unsafe {
                    std::ptr::copy(
                        self.rx.as_ptr().add(self.scan),
                        self.rx.as_mut_ptr(),
                        remaining,
                    );
                }
            }
            self.have = remaining;
            self.scan = 0;
        }
    }

    fn apply(&mut self, cmd: &CmdPayload) {
        if self.state != ClientState::Connected || !cmd.valid {
            return;
        }

        let fd = match self.fd {
            Some(fd) => fd,
            None => return,
        };

        if cmd.intent.to_excl <= cmd.intent.from {
            return;
        }

        let count = std::cmp::min(cmd.intent.to_excl - cmd.intent.from, 65535) as u16;
        let mut tx20 = [0u8; 20];
        encode_request(&cmd.session, cmd.intent.from, count, &mut tx20);

        let sent = unsafe {
            libc::send(
                fd,
                tx20.as_ptr() as *const libc::c_void,
                20,
                libc::MSG_DONTWAIT,
            )
        };

        if sent == 20 {
            self.counters.requests_sent += 1;
        }
    }

    /// Single non-blocking tick of Thread R loop.
    pub fn step(&mut self, mailbox: &PacketMailbox, cmd_chan: &CmdChannel) {
        if let Some((cmd, epoch)) = cmd_chan.take_latest(self.last_epoch) {
            self.last_epoch = epoch;
            self.active_cmd = Some(cmd);
            if self.state == ClientState::Connected {
                self.apply(&cmd);
            }
        }

        match self.state {
            ClientState::Disconnected => {
                if let Some(cmd) = self.active_cmd {
                    if cmd.valid {
                        self.try_connect(cmd_chan);
                    }
                }
            }
            ClientState::Connecting => {
                self.poll_writable(cmd_chan);
            }
            ClientState::Connected => {
                self.drain_socket(mailbox, cmd_chan);
            }
        }
    }

    /// Spawns Thread R to run continuously until `stop_signal` is set.
    pub fn spawn(
        mut self,
        mailbox: std::sync::Arc<PacketMailbox>,
        cmd_chan: std::sync::Arc<CmdChannel>,
        stop_signal: std::sync::Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<Self> {
        std::thread::spawn(move || {
            while !stop_signal.load(Ordering::Relaxed) {
                self.step(&mailbox, &cmd_chan);
                std::thread::sleep(Duration::from_micros(100));
            }
            if let Some(fd) = self.fd.take() {
                unsafe {
                    libc::close(fd);
                }
            }
            self
        })
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
