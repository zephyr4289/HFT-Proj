//! Reference Arbitrator (doc 16 / G12-T3).
//! Deliberately simple, structurally independent oracle.
//! Zero imports from nf-arbitrator or nf-protocol (R-1).
//! Collects all delivered (seq, bytes), sorts by seq, first-received wins (R-2).

use std::collections::BTreeMap;

pub const HEARTBEAT_COUNT: u16 = 0x0000;
pub const EOS_COUNT: u16 = 0xFFFF;
pub const HEADER_LEN: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefSession {
    pub session_id: [u8; 10],
    pub anchor: Option<u64>,
    pub messages: BTreeMap<u64, Vec<u8>>,
    pub eos_seen: bool,
}

impl RefSession {
    pub fn new(session_id: [u8; 10]) -> Self {
        Self {
            session_id,
            anchor: None,
            messages: BTreeMap::new(),
            eos_seen: false,
        }
    }
}

/// Independent reference arbitrator.
#[derive(Debug, Clone, Default)]
pub struct ReferenceArbitrator {
    pub sessions: Vec<RefSession>,
    pub current_session_idx: Option<usize>,
}

impl ReferenceArbitrator {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            current_session_idx: None,
        }
    }

    /// Hand-parses a MoldUDP64 packet and records messages (R-1, R-2).
    pub fn ingest_packet(&mut self, frame: &[u8]) {
        if frame.len() < HEADER_LEN {
            return;
        }

        let mut session_id = [0u8; 10];
        session_id.copy_from_slice(&frame[0..10]);

        let seq = u64::from_be_bytes(frame[10..18].try_into().unwrap());
        let count = u16::from_be_bytes(frame[18..20].try_into().unwrap());

        // Session management
        let s_idx = match self.current_session_idx {
            Some(idx) if self.sessions[idx].session_id == session_id => idx,
            _ => {
                let idx = self.sessions.len();
                self.sessions.push(RefSession::new(session_id));
                self.current_session_idx = Some(idx);
                idx
            }
        };

        let session = &mut self.sessions[s_idx];

        if count == EOS_COUNT {
            session.eos_seen = true;
            return;
        }

        if count == HEARTBEAT_COUNT {
            return;
        }

        // Data packet: walk [u16 len][payload] blocks
        let mut pos = HEADER_LEN;
        let mut cur_seq = seq;

        for _ in 0..count {
            if pos + 2 > frame.len() {
                break;
            }
            let len = u16::from_be_bytes(frame[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;
            if pos + len > frame.len() {
                break;
            }
            let payload = &frame[pos..pos + len];
            pos += len;

            if session.anchor.is_none() {
                session.anchor = Some(cur_seq);
            }

            // FR-3 / R-2: First-received wins on duplicate sequence number
            if !session.messages.contains_key(&cur_seq) {
                session.messages.insert(cur_seq, payload.to_vec());
            }

            cur_seq = cur_seq.saturating_add(1);
        }
    }

    /// Emits contiguous ordered stream across all sessions and returns (final_anchor, final_watermark, total_hash, emitted).
    pub fn evaluate_all_sessions(&self) -> (u64, u64, u64, Vec<(u64, Vec<u8>)>) {
        let mut running_hash = 0xcbf29ce484222325u64; // FNV-1a basis
        let mut all_emitted = Vec::new();
        let mut final_anchor = 1u64;
        let mut final_wm = 1u64;

        for session in &self.sessions {
            let anchor = session.anchor.unwrap_or(1);
            final_anchor = anchor;
            let mut cur = anchor;

            while let Some(data) = session.messages.get(&cur) {
                all_emitted.push((cur, data.clone()));

                // Canonical golden hash fold: [u16 len le][bytes]
                for &b in &(data.len() as u16).to_le_bytes() {
                    running_hash ^= b as u64;
                    running_hash = running_hash.wrapping_mul(0x100000001b3);
                }
                for &b in data {
                    running_hash ^= b as u64;
                    running_hash = running_hash.wrapping_mul(0x100000001b3);
                }

                cur += 1;
            }

            final_wm = cur;
        }

        (final_anchor, final_wm, running_hash, all_emitted)
    }

    /// Emits contiguous ordered stream for the latest session.
    pub fn evaluate_latest_session(&self) -> (u64, u64, u64, Vec<(u64, Vec<u8>)>) {
        if let Some(session) = self.sessions.last() {
            let anchor = session.anchor.unwrap_or(1);
            let mut cur = anchor;
            let mut emitted = Vec::new();
            let mut running_hash = 0xcbf29ce484222325u64;

            while let Some(data) = session.messages.get(&cur) {
                emitted.push((cur, data.clone()));

                for &b in &(data.len() as u16).to_le_bytes() {
                    running_hash ^= b as u64;
                    running_hash = running_hash.wrapping_mul(0x100000001b3);
                }
                for &b in data {
                    running_hash ^= b as u64;
                    running_hash = running_hash.wrapping_mul(0x100000001b3);
                }

                cur += 1;
            }

            let watermark = cur;
            (anchor, watermark, running_hash, emitted)
        } else {
            (1, 1, 0xcbf29ce484222325u64, Vec::new())
        }
    }
}
