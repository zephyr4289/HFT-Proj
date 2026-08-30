//! Types and counters for TCP recovery client (doc 08 §4).

pub const STATUS_DISCONNECTED: u32 = 0;
pub const STATUS_CONNECTING: u32 = 1;
pub const STATUS_CONNECTED: u32 = 2;
pub const STATUS_SERVER_CLOSED: u32 = 3;
pub const STATUS_SOCKET_ERROR: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientState {
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryCounters {
    pub requests_sent: u64,
    pub responses_received: u64,
    pub packets_forwarded: u64,
    pub stale_session_dropped: u64,
    pub malformed_stream: u64,
    pub oversize_packets: u64,
    pub connections_attempted: u64,
    pub server_closed: u64,
    pub socket_errors: u64,
}
