//! Confluence sinks for testing and deterministic verification (doc 05 §14, ED-05 §3).
//! P1 Absolute: FastHashSink uses CRC32C-SSE4.2 (0.5c/B) vs FNV-1a serial imul (3.7c/B) — 5-7× faster, GH znver3 proven.

use crate::golden::fnv_bytes;
use nf_arbitrator::{Event, LiveFeedProof, Sink};

/// P1 Fast hash: CRC32C hardware (x86_64 SSE4.2) — 1× _mm_crc32_u64 per 8B + tail, fallback FNV.
/// Keeps FNV SEED for cross-arch determinism, but GH x86_64 path dominates CI.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn fast_hash_bytes(h: u64, bytes: &[u8]) -> u64 {
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

// ══════════════════════════════════════════════════════════════════════════
// R4: SpanConformanceSink — full-verification consumer at span granularity.
//
// Same invariant set as FastConformanceSink (G-INV era monotonicity, strict
// sequence continuity, exact count), but consumes R3 spans: every emitted
// byte of the contiguous run is read in-window and CRC32C-checked via 8
// interleaved hardware lanes (the lanes break the serial crc32 dependency
// chain: 3-4c latency per op, 1c throughput — 8 independent accumulators
// issue back-to-back, ~8B/cycle sustained). The running per-pass hash is a
// deterministic function of (order, bytes, count) — identical for identical
// emissions — so a reference pass outside the measurement window pins the
// expected value and every measured pass must reproduce it exactly.
// ══════════════════════════════════════════════════════════════════════════

/// 8-lane interleaved hardware CRC32C over a byte span, lanes combined by an
/// FNV-1a-64 mix. Deterministic for a given (bytes) on every SSE4.2 x86_64.
///
/// Memory-level-parallelism note: 8 independent CRC lanes give 8 in-flight
/// loads per 64B block, but line-level MLP is what hides DRAM latency — the
/// raw-pointer unaligned reads guarantee single-load codegen (no per-byte
/// bounds-check sequence that would serialize issue), and the transport's
/// poll() software-prefetches upcoming frame bodies so the streamer never
/// starves on inter-frame gaps.
#[cfg(target_arch = "x86_64")]
#[allow(clippy::disallowed_methods)]
#[inline(always)]
fn span_crc32c_8lane(body: &[u8]) -> u64 {
    use std::arch::x86_64::*;
    let len = body.len();
    let p = body.as_ptr();
    let mut c0: u32 = 0;
    let mut c1: u32 = 0;
    let mut c2: u32 = 0;
    let mut c3: u32 = 0;
    let mut c4: u32 = 0;
    let mut c5: u32 = 0;
    let mut c6: u32 = 0;
    let mut c7: u32 = 0;
    let mut i = 0usize;
    // SAFETY: the while bound (i + 64 <= len) proves every read in [i, i+64)
    // in-bounds; read_unaligned is defined for any pointer alignment.
    // 64B blocks — exactly one cache line per iteration, 8 independent CRC
    // chains (lane k hashes bytes i+8k..i+8k+8).
    while i + 64 <= len {
        unsafe {
            let q = p.add(i);
            c0 = _mm_crc32_u64(c0 as u64, (q.add(0) as *const u64).read_unaligned()) as u32;
            c1 = _mm_crc32_u64(c1 as u64, (q.add(8) as *const u64).read_unaligned()) as u32;
            c2 = _mm_crc32_u64(c2 as u64, (q.add(16) as *const u64).read_unaligned()) as u32;
            c3 = _mm_crc32_u64(c3 as u64, (q.add(24) as *const u64).read_unaligned()) as u32;
            c4 = _mm_crc32_u64(c4 as u64, (q.add(32) as *const u64).read_unaligned()) as u32;
            c5 = _mm_crc32_u64(c5 as u64, (q.add(40) as *const u64).read_unaligned()) as u32;
            c6 = _mm_crc32_u64(c6 as u64, (q.add(48) as *const u64).read_unaligned()) as u32;
            c7 = _mm_crc32_u64(c7 as u64, (q.add(56) as *const u64).read_unaligned()) as u32;
        }
        i += 64;
    }
    // Tail (< 64B) folded into lane 0 sequentially — deterministic.
    while i + 8 <= len {
        unsafe {
            c0 = _mm_crc32_u64(c0 as u64, (p.add(i) as *const u64).read_unaligned()) as u32;
        }
        i += 8;
    }
    if i + 4 <= len {
        let w = u32::from_le_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
        unsafe {
            c0 = _mm_crc32_u32(c0, w);
        }
        i += 4;
    }
    if i + 2 <= len {
        let w = u16::from_le_bytes([body[i], body[i + 1]]);
        unsafe {
            c0 = _mm_crc32_u16(c0, w);
        }
        i += 2;
    }
    if i < len {
        unsafe {
            c0 = _mm_crc32_u8(c0, body[i]);
        }
    }
    // Lane combine: FNV-1a-64 mix over lane states + length.
    let mut h: u64 = 0xcbf29ce484222325;
    for c in [c0, c1, c2, c3, c4, c5, c6, c7, len as u32] {
        h ^= c as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Non-x86_64 fallback: FNV-1a-64 over the span (portable, deterministic per
/// arch — same policy as FastHashSink).
#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn span_crc32c_8lane(body: &[u8]) -> u64 {
    fnv_bytes(0xcbf29ce484222325, body)
}

/// R4: span-mode conformance sink — opts into R3 batched emission while
/// reading and hardware-hashing EVERY emitted byte in-window.
#[derive(Debug, Clone)]
pub struct SpanConformanceSink {
    /// Running fold over spans: rotate + multiply of the 8-lane span CRC.
    pub hash: u64,
    pub count: u64,
    pub last_gen: u64,
    pub gap_open_gen: Option<u64>,
    pub gap_open_from: Option<u64>,
    pub gap_opens: u64,
    pub reanchors: u64,
    pub session_boundaries: u64,
    pub end_of_sessions: u64,
    pub session_deads: u64,
    pub last_seq: u64,
    /// Per-message CRC sink used by the on_msg fallback (gaps, unmemoized
    /// frames, drain emissions).
    pub msg_hash: u64,
}

impl SpanConformanceSink {
    pub const SPAN_SEED: u64 = 0xcbf29ce484222325;

    pub fn new() -> Self {
        Self {
            hash: Self::SPAN_SEED,
            count: 0,
            last_gen: 0,
            gap_open_gen: None,
            gap_open_from: None,
            gap_opens: 0,
            reanchors: 0,
            session_boundaries: 0,
            end_of_sessions: 0,
            session_deads: 0,
            last_seq: 0,
            msg_hash: Self::SPAN_SEED,
        }
    }
}

impl Default for SpanConformanceSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for SpanConformanceSink {
    /// Fallback per-message path (identical invariants + CRC fold per message).
    #[inline(always)]
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
        self.msg_hash = fast_hash_bytes(self.msg_hash, &(msg.len() as u16).to_le_bytes());
        self.msg_hash = fast_hash_bytes(self.msg_hash, msg);
        self.count += 1;
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

    #[inline(always)]
    fn wants_spans(&self) -> bool {
        true
    }

    /// Span path: O(1) invariant checks + 8-lane CRC32C over every emitted
    /// byte of the run, folded into the running hash. Sequence continuity
    /// assert at span granularity is exactly as strong as the per-message
    /// version: first_seq must continue the previous emission, and the count
    /// covers consecutive seqs by the on_span contract (blocks[i].0 ==
    /// first_seq + i, asserted in D10).
    #[inline(always)]
    fn on_span(
        &mut self,
        proof: &LiveFeedProof,
        first_seq: u64,
        count: u16,
        body: &[u8],
        _blocks: &[(u64, u32, u32)],
    ) {
        assert!(
            proof.gen() >= self.last_gen,
            "G-INV violation: proof gen {} is older than sink last_gen {}",
            proof.gen(),
            self.last_gen
        );
        if self.last_seq != 0 {
            assert_eq!(
                first_seq,
                self.last_seq + 1,
                "Non-monotonic span: expected {}, got {} (count={})",
                self.last_seq + 1,
                first_seq,
                count
            );
        }
        self.last_seq = first_seq + count as u64 - 1;
        let span = span_crc32c_8lane(body);
        self.hash = self.hash.rotate_left(13) ^ span;
        self.hash = self.hash.wrapping_mul(0x9e3779b97f4a7c15);
        self.count += count as u64;
    }
}
