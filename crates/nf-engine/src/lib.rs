//! Engine loop, thread pinning, and end-to-end wiring.

#![allow(clippy::disallowed_types, clippy::disallowed_methods, clippy::field_reassign_with_default)]

pub mod alloc;
pub mod clock;
pub mod histogram;

use nf_protocol::itch5;
use std::io::Read;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditReport {
    pub bytes_read: u64,
    pub blocks: u64,
    pub violations: u64,
    pub truncated_tail: bool,
    pub hist: [u64; 256],
}

impl AuditReport {
    pub fn new() -> Self {
        Self {
            bytes_read: 0,
            blocks: 0,
            violations: 0,
            truncated_tail: false,
            hist: [0u64; 256],
        }
    }

    pub fn print_report(&self, path_display: &str) {
        println!(
            "AUDIT file={} bytes={} blocks={} violations={} truncated_tail={}",
            path_display, self.bytes_read, self.blocks, self.violations, self.truncated_tail
        );
        for b in 0..=255u8 {
            let count = self.hist[b as usize];
            if count > 0 {
                let char_repr = if b.is_ascii_graphic() {
                    (b as char).to_string()
                } else {
                    ".".to_string()
                };
                let declared_len = itch5::LENGTH[b as usize];
                let pct = if self.blocks > 0 {
                    (count as f64 / self.blocks as f64) * 100.0
                } else {
                    0.0
                };
                println!(
                    "HIST 0x{:02X} {} len={} count={} pct={:.2}",
                    b, char_repr, declared_len, count, pct
                );
            }
        }
    }
}

impl Default for AuditReport {
    fn default() -> Self {
        Self::new()
    }
}

/// Walks a MoldUDP64 message block stream `[u16 BE len][msg]...` and validates
/// each message against the ITCH 5.0 LENGTH table.
pub fn audit_stream(reader: &mut impl Read) -> std::io::Result<AuditReport> {
    let mut report = AuditReport::new();
    let mut len_buf = [0u8; 2];
    let mut msg_buf = vec![0u8; 65536];

    loop {
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {
                report.bytes_read += 2;
                let msg_len = u16::from_be_bytes(len_buf) as usize;
                if msg_len == 0 {
                    report.violations += 1;
                    continue;
                }
                match reader.read_exact(&mut msg_buf[..msg_len]) {
                    Ok(()) => {
                        report.bytes_read += msg_len as u64;
                        let msg = &msg_buf[..msg_len];
                        if itch5::validate(msg).is_ok() {
                            report.hist[msg[0] as usize] += 1;
                        } else {
                            report.violations += 1;
                        }
                        report.blocks += 1;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        // Truncated tail (sample cut mid-message)
                        report.truncated_tail = true;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Clean EOF
                break;
            }
            Err(e) => return Err(e),
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_in_memory_stream() {
        let mut stream = Vec::new();
        // Msg 1: S (12B)
        let s_msg = [0x53, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x4F];
        stream.extend_from_slice(&(12u16).to_be_bytes());
        stream.extend_from_slice(&s_msg);

        // Msg 2: D (19B)
        let d_msg = [0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        stream.extend_from_slice(&(19u16).to_be_bytes());
        stream.extend_from_slice(&d_msg);

        let mut cursor = std::io::Cursor::new(stream);
        let report = audit_stream(&mut cursor).expect("audit stream");
        assert_eq!(report.blocks, 2);
        assert_eq!(report.violations, 0);
        assert!(!report.truncated_tail);
        assert_eq!(report.hist[b'S' as usize], 1);
        assert_eq!(report.hist[b'D' as usize], 1);
    }

    #[test]
    fn test_audit_truncated_tail() {
        let mut stream = Vec::new();
        // Add full S message
        let s_msg = [0x53, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x4F];
        stream.extend_from_slice(&(12u16).to_be_bytes());
        stream.extend_from_slice(&s_msg);

        // Claim 36 bytes for A, but provide only 10 bytes before EOF
        stream.extend_from_slice(&(36u16).to_be_bytes());
        stream.extend_from_slice(&[b'A', 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        let mut cursor = std::io::Cursor::new(stream);
        let report = audit_stream(&mut cursor).expect("audit stream");
        assert_eq!(report.blocks, 1);
        assert_eq!(report.violations, 0);
        assert!(report.truncated_tail);
        assert_eq!(report.hist[b'S' as usize], 1);
    }
}
