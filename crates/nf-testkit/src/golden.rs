//! Golden reference walker: computes the canonical FNV-1a-64 hash
//! over an ITCH 5.0 message block stream (doc 04 §8).

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

#[inline]
pub fn fnv_update(mut h: u64, byte: u8) -> u64 {
    h ^= byte as u64;
    h.wrapping_mul(FNV_PRIME)
}

#[inline]
pub fn fnv_bytes(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h = fnv_update(h, b);
    }
    h
}

/// Computes the canonical golden hash and message count over a ground-truth
/// message block stream (`[u16 BE len][msg]`).
pub fn golden(gt: &[u8]) -> (u64 /* hash */, u64 /* count */) {
    let mut h = FNV_OFFSET;
    let mut pos = 0;
    let mut seq = 1u64;

    while pos + 2 <= gt.len() {
        let msg_len = u16::from_be_bytes([gt[pos], gt[pos + 1]]) as usize;
        pos += 2;
        if pos + msg_len > gt.len() {
            break;
        }
        let msg = &gt[pos..pos + msg_len];
        pos += msg_len;

        h = fnv_bytes(h, &seq.to_le_bytes());
        h = fnv_bytes(h, &(msg_len as u16).to_le_bytes());
        h = fnv_bytes(h, msg);

        seq += 1;
    }

    (h, seq - 1)
}

/// Independent second implementation for T1 cross-check.
pub fn golden_cross_check(gt: &[u8], limit: usize) -> (u64, u64) {
    let mut h = FNV_OFFSET;
    let mut pos = 0;
    let mut seq = 1u64;
    let mut count = 0;

    while pos + 2 <= gt.len() && count < limit {
        let l0 = gt[pos] as usize;
        let l1 = gt[pos + 1] as usize;
        let msg_len = (l0 << 8) | l1;
        pos += 2;
        if pos + msg_len > gt.len() {
            break;
        }
        let msg = &gt[pos..pos + msg_len];
        pos += msg_len;

        // Byte-by-byte update
        for b in seq.to_le_bytes() {
            h = (h ^ (b as u64)).wrapping_mul(FNV_PRIME);
        }
        for b in (msg_len as u16).to_le_bytes() {
            h = (h ^ (b as u64)).wrapping_mul(FNV_PRIME);
        }
        for &b in msg {
            h = (h ^ (b as u64)).wrapping_mul(FNV_PRIME);
        }

        seq += 1;
        count += 1;
    }

    (h, count as u64)
}
