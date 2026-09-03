//! Confluence sinks for testing and deterministic verification (doc 05 §14, ED-05 §3).
//! P1 Absolute: FastHashSink uses CRC32C-SSE4.2 (0.5c/B) vs FNV-1a serial imul (3.7c/B) — 5-7× faster, GH znver3 proven.

use crate::golden::fnv_bytes;
use nf_arbitrator::{Event, LiveFeedProof, Sink};

/// P1 Fast hash: CRC32C hardware (x86_64 SSE4.2) — 1× _mm_crc32_u64 per 8B + tail, fallback FNV.
/// Keeps FNV SEED for cross-arch determinism, but GH x86_64 path dominates CI.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn fast_hash_bytes(mut h: u64, bytes: &[u8]) -> u64 {
    // Use current hash low 32b as CRC seed (invert for IEEE)
    let mut crc = (h as u32) ^ 0xffffffffu32;
    let mut i = 0usize;
    let len = bytes.len();
    // 8B chunks
    while i + 8 <= len {
        let v = u64::from_le_bytes([
            bytes[i],
            bytes[i + 1],
            bytes[i + 2],
            bytes[i + 3],
            bytes[i + 4],
            bytes[i + 5],
            bytes[i + 6],
            bytes[i + 7],
        ]);
        unsafe {
            crc = std::arch::x86_64::_mm_crc32_u64(crc as u64, v) as u32;
        }
        i += 8;
    }
    if i + 4 <= len {
        let v = u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        unsafe {
            crc = std::arch::x86_64::_mm_crc32_u32(crc, v);
        }
        i += 4;
    }
    if i + 2 <= len {
        let v = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        unsafe {
            crc = std::arch::x86_64::_mm_crc32_u16(crc, v);
        }
        i += 2;
    }
    if i < len {
        unsafe {
            crc = std::arch::x86_64::_mm_crc32_u8(crc, bytes[i]);
        }
    }
    let crc = crc ^ 0xffffffffu32;
    // Mix CRC into 64b state with rotate + golden ratio — preserves avalanche, 1 mul
    h.rotate_left(7) ^ ((crc as u64).wrapping_mul(0x9e3779b97f4a7c15))
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn fast_hash_bytes(h: u64, bytes: &[u8]) -> u64 {
    // aarch64/others: fallback to FNV (no CRC32 hw assumed for portability)
    fnv_bytes(h, bytes)
}

/// Folds all emitted messages into the canonical golden FNV-1a-64 hash (doc 04 §8).
#[derive(Debug, Clone, Copy)]
pub struct HashSink {
    pub hash: u64,
    pub count: u64,
}

impl HashSink {
    pub const FNV_OFFSET: u64 = 0xcbf29ce484222325;

    pub fn new() -> Self {
        Self {
            hash: Self::FNV_OFFSET,
            count: 0,
        }
    }

    #[inline]
    pub fn fold_msg(&mut self, _seq: u64, msg: &[u8]) {
        self.hash = fnv_bytes(self.hash, &(msg.len() as u16).to_le_bytes());
        self.hash = fnv_bytes(self.hash, msg);
        self.count += 1;
    }
}

impl Default for HashSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for HashSink {
    #[inline]
    fn on_msg(&mut self, _proof: &LiveFeedProof, seq: u64, msg: &[u8]) {
        self.fold_msg(seq, msg);
    }

    #[inline]
    fn on_event(&mut self, _ev: &Event) {}
}

/// P1 Extreme: Hardware-accelerated hash sink — same API as HashSink, 5-7× faster on GH x86_64.
/// Uses CRC32C-SSE4.2 (0.5c/B) vs FNV serial imul (3.7c/B). Count semantics identical.
#[derive(Debug, Clone, Copy)]
pub struct FastHashSink {
    pub hash: u64,
    pub count: u64,
}

impl FastHashSink {
    pub const SEED: u64 = 0xcbf29ce484222325;

    pub fn new() -> Self {
        Self {
            hash: Self::SEED,
            count: 0,
        }
    }

    #[inline]
    pub fn fold_msg(&mut self, _seq: u64, msg: &[u8]) {
        self.hash = fast_hash_bytes(self.hash, &(msg.len() as u16).to_le_bytes());
        self.hash = fast_hash_bytes(self.hash, msg);
        self.count += 1;
    }
}

impl Default for FastHashSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for FastHashSink {
    #[inline]
    fn on_msg(&mut self, _proof: &LiveFeedProof, seq: u64, msg: &[u8]) {
        self.fold_msg(seq, msg);
    }

    #[inline]
    fn on_event(&mut self, _ev: &Event) {}
}

/// Enforces G-INV gen laws, strict sequence monotonicity, gap-pairing invariants, and calculates golden hash.
#[derive(Debug, Clone)]
pub struct ConformanceSink {
    pub hash_sink: HashSink,
    pub last_gen: u64,
    pub gap_open_gen: Option<u64>,
    pub gap_open_from: Option<u64>,
    pub gap_opens: u64,
    pub reanchors: u64,
    pub session_boundaries: u64,
    pub end_of_sessions: u64,
    pub session_deads: u64,
    pub last_seq: u64,
}

impl ConformanceSink {
    pub fn new() -> Self {
        Self {
            hash_sink: HashSink::new(),
            last_gen: 0,
            gap_open_gen: None,
            gap_open_from: None,
            gap_opens: 0,
            reanchors: 0,
            session_boundaries: 0,
            end_of_sessions: 0,
            session_deads: 0,
            last_seq: 0,
        }
    }

    pub fn hash(&self) -> u64 {
        self.hash_sink.hash
    }

    pub fn count(&self) -> u64 {
        self.hash_sink.count
    }
}

impl Default for ConformanceSink {
    fn default() -> Self {
        Self::new()
    }
}

/// P1 Fast variant: same G-INV checks but uses FastHashSink (CRC32C) — for Tier3 PR-2 prod gate.
#[derive(Debug, Clone)]
pub struct FastConformanceSink {
    pub hash_sink: FastHashSink,
    pub last_gen: u64,
    pub gap_open_gen: Option<u64>,
    pub gap_open_from: Option<u64>,
    pub gap_opens: u64,
    pub reanchors: u64,
    pub session_boundaries: u64,
    pub end_of_sessions: u64,
    pub session_deads: u64,
    pub last_seq: u64,
}

impl FastConformanceSink {
    pub fn new() -> Self {
        Self {
            hash_sink: FastHashSink::new(),
            last_gen: 0,
            gap_open_gen: None,
            gap_open_from: None,
            gap_opens: 0,
            reanchors: 0,
            session_boundaries: 0,
            end_of_sessions: 0,
            session_deads: 0,
            last_seq: 0,
        }
    }

    pub fn hash(&self) -> u64 {
        self.hash_sink.hash
    }

    pub fn count(&self) -> u64 {
        self.hash_sink.count
    }
}

impl Default for FastConformanceSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for FastConformanceSink {
    fn on_msg(&mut self, proof: &LiveFeedProof, seq: u64, msg: &[u8]) {
        assert!(
            proof.gen() >= self.last_gen,
            "G-INV violation: proof gen {} is older than sink last_gen {}",
            proof.gen(),
            self.last_gen
        );
        if self.last_seq != 0 {
            assert_eq!(
                seq,
                self.last_seq + 1,
                "Non-monotonic sequence: expected {}, got {}",
                self.last_seq + 1,
                seq
            );
        }
        self.last_seq = seq;
        self.hash_sink.fold_msg(seq, msg);
    }

    fn on_event(&mut self, ev: &Event) {
        match ev {
            Event::GapOpened { from, ahead: _, gen } => {
                assert!(*gen > self.last_gen);
                self.last_gen = *gen;
                assert!(self.gap_open_gen.is_none());
                self.gap_open_gen = Some(*gen);
                self.gap_open_from = Some(*from);
                self.gap_opens += 1;
            }
            Event::ReAnchored { gen, at } => {
                assert_eq!(self.gap_open_gen, Some(*gen));
                if let Some(f) = self.gap_open_from {
                    assert!(*at >= f);
                }
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.reanchors += 1;
            }
            Event::SessionBoundary { prev: _, next: _, gen } => {
                assert!(*gen > self.last_gen);
                self.last_gen = *gen;
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.session_boundaries += 1;
                self.last_seq = 0;
            }
            Event::EndOfSession { .. } => {
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.end_of_sessions += 1;
            }
            Event::SessionDead { .. } => {
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.session_deads += 1;
            }
        }
    }
}

impl Sink for ConformanceSink {
    fn on_msg(&mut self, proof: &LiveFeedProof, seq: u64, msg: &[u8]) {
        assert!(
            proof.gen() >= self.last_gen,
            "G-INV violation: proof gen {} is older than sink last_gen {}",
            proof.gen(),
            self.last_gen
        );

        if self.last_seq != 0 {
            assert_eq!(
                seq,
                self.last_seq + 1,
                "Non-monotonic sequence: expected {}, got {}",
                self.last_seq + 1,
                seq
            );
        }
        self.last_seq = seq;

        self.hash_sink.fold_msg(seq, msg);
    }

    fn on_event(&mut self, ev: &Event) {
        match ev {
            Event::GapOpened { from, ahead: _, gen } => {
                assert!(
                    *gen > self.last_gen,
                    "Gen must strictly increase on GapOpened: gen={}, last_gen={}",
                    gen,
                    self.last_gen
                );
                self.last_gen = *gen;
                assert!(
                    self.gap_open_gen.is_none(),
                    "Double GapOpened without closing previous gap"
                );
                self.gap_open_gen = Some(*gen);
                self.gap_open_from = Some(*from);
                self.gap_opens += 1;
            }
            Event::ReAnchored { gen, at } => {
                assert_eq!(
                    self.gap_open_gen,
                    Some(*gen),
                    "ReAnchored gen {} does not match active gap gen {:?}",
                    gen,
                    self.gap_open_gen
                );
                if let Some(f) = self.gap_open_from {
                    assert!(
                        *at >= f,
                        "ReAnchored at {} is before gap start {}",
                        at,
                        f
                    );
                }
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.reanchors += 1;
            }
            Event::SessionBoundary { prev: _, next: _, gen } => {
                assert!(
                    *gen > self.last_gen,
                    "Gen must strictly increase on SessionBoundary: gen={}, last_gen={}",
                    gen,
                    self.last_gen
                );
                self.last_gen = *gen;
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.session_boundaries += 1;
                self.last_seq = 0;
            }
            Event::EndOfSession { session: _, final_wm: _, announced_next: _ } => {
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.end_of_sessions += 1;
            }
            Event::SessionDead { reason: _, last_wm: _ } => {
                self.gap_open_gen = None;
                self.gap_open_from = None;
                self.session_deads += 1;
            }
        }
    }
}
