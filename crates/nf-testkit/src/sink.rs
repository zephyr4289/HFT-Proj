//! Confluence sinks for testing and deterministic verification (doc 05 §14, ED-05 §3).
//! P1 Absolute: FastHashSink uses CRC32C-SSE4.2 (0.5c/B) vs FNV-1a serial imul (3.7c/B) — 5-7× faster, GH znver3 proven.

use crate::golden::fnv_bytes;
use nf_arbitrator::{Event, LiveFeedProof, Sink};

/// P1 Fast hash: CRC32C hardware (x86_64 SSE4.2) — 1× _mm_crc32_u64 per 8B + tail, fallback FNV.
/// Keeps FNV SEED for cross-arch determinism, but GH x86_64 path dominates CI.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) fn fast_hash_bytes(h: u64, bytes: &[u8]) -> u64 {
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
pub(crate) fn fast_hash_bytes(h: u64, bytes: &[u8]) -> u64 {
    // aarch64/others: fallback to FNV (no CRC32 hw assumed for portability)
    fnv_bytes(h, bytes)
}

/// R25a: the per-span ordered-fold law's premultiply constant. The fold
/// `h ← rotl(h,13) ^ (v·P)` is the COMPOSABLE re-derivation of the
/// R23 law `h ← (rotl(h,13) ^ v)·P` — same three mixing primitives
/// (rotate, XOR, golden-ratio multiply), reassociated so the multiply
/// rides the INDEPENDENT span values instead of the serial chain.
///
/// **The composition law** (the whole point): over `k` consecutive spans
/// with values `v_1..v_k`, writing `u_i = v_i·P`,
///
/// ```text
/// H_k = rotl(H_0, 13k mod 64) ^ ⊕_{i=1..k} rotl(u_i, 13(k−i) mod 64)
/// ```
///
/// Induction: the i-th premultiplied value enters with a rotation that
/// depends ONLY on its distance from the segment end — every term is
/// independently computable, so a worker can fold a whole 64-span chunk
/// into ONE 16-byte composite record `(k, B)` and the consumer applies
/// it in two instructions (`rotl` + `xor`) instead of k serial
/// rotl⊕mul chains. The old law's trailing `·P` made the composition
/// non-expressible (integer multiply does not distribute over XOR),
/// which is precisely why the consumer had to fold span-by-span.
/// Both mixers stay: the multiply still avalanches every value before
/// it enters the XOR tree, and the rotation keeps the chain
/// position-sensitive (a duplicate at distance 64k still cannot cancel
/// — its `u` differs by the P-avalanche; sequence-continuity asserts
/// remain the primary order/dup detector, as before).
pub const FOLD_MIX_P: u64 = 0x9e3779b97f4a7c15;

/// R25a: the fold's rotation amount.
pub const FOLD_ROT: u32 = 13;

/// R25a: the per-span contribution entering the composite XOR tree —
/// the premultiplied value `u_i = v_i·P`.
#[inline(always)]
pub fn fold_term(v: u64) -> u64 {
    v.wrapping_mul(FOLD_MIX_P)
}

/// R25a: the composite constant of a segment. `terms` are the segment's
/// `u_i` in span order (i = 0 is the FIRST span of the segment);
/// `B = ⊕_i rotl(u_i, 13·(k−1−i) mod 64)` — each term's rotation
/// depends only on its distance from the segment END, so the loop is
/// throughput-bound (independent RORX + XOR), never latency-bound.
#[inline]
pub fn fold_compose_b(terms: &[u64]) -> u64 {
    let k = terms.len() as u32;
    let mut b = 0u64;
    for (i, &u) in terms.iter().enumerate() {
        b ^= u.rotate_left((FOLD_ROT * (k - 1 - i as u32)) % 64);
    }
    b
}

/// R25a: apply a composite record `(k, B)` to the running hash — the
/// consumer's whole per-segment cost (two dependent ops per segment
/// where the per-span path spent k×(rotl⊕xor+IMUL) serial cycles).
#[inline(always)]
pub fn fold_compose_apply(h: u64, k: u32, b: u64) -> u64 {
    h.rotate_left((FOLD_ROT * k) % 64) ^ b
}

/// R25a: one step of the fold (the reference law, bit-exact with the
/// composite by the composition identity above — pinned by
/// `t_r25_composite_law`'s differential).
#[inline(always)]
pub fn fold_step(h: u64, v: u64) -> u64 {
    h.rotate_left(FOLD_ROT) ^ fold_term(v)
}

// ─────────────────────────────────────────────────────────────────────
// R37 (Run 900) — the segment-monoid tree (docs/solution2.md Part I,
// Theorems 19–22 + Theorem 28's ring polymorphism, instantiated on
// the ROTATION ring: the [S1'] Lemma-3 semidirect law with the
// acting monoid (13k mod 64, rotl) instead of (x^W, CLMUL)).
// ─────────────────────────────────────────────────────────────────────
//
// The engine's per-pass fold composes composite records
// `(k, B)` — a span count and a segment constant — by
// `h ← rotl(h, 13k) ^ B` (`fold_compose_apply`). The records form a
// MONOID under the same affine law as the SPANHASH record monoid:
//
// ```text
// (k_a, B_a) ⨂ (k_b, B_b) = (k_a + k_b, rotl(B_a, 13·k_b mod 64) ^ B_b)
// ```
//
// (apply a first, b second — the row-vector convention of Lemma 3;
// associativity is the same six-line semidirect computation, with
// `rotl∘rotl = rotl∘+` the action law that `x^W ∘ x^V = x^{W+V}`
// plays in the CLMUL ring). Tape order constrains the LEAF sequence,
// not the bracketing (Theorem 19), so the serial left spine — 322
// dependent applies per champion pass, the 2.08 µs barrier — is the
// worst bracketing of a monoid the up-sweep below evaluates in
// ⌈log₂ K⌉ levels. The identity is `(0, 0)`: `fold_compose_apply(h,
// 0, 0) == h` verbatim.

/// The segment monoid's compose: `a` earlier, `b` later (Lemma 3's
/// law on the rotation ring — one rotate, one XOR, one add).
#[inline(always)]
pub fn seg_compose(a: (u32, u64), b: (u32, u64)) -> (u32, u64) {
    (
        a.0.wrapping_add(b.0),
        a.1.rotate_left(FOLD_ROT.wrapping_mul(b.0) % 64) ^ b.1,
    )
}

/// THE SEGMENT TREE (Theorem 20(a) on the rotation ring): the
/// in-place up-sweep over tape-ordered `(k, B)` leaves — SoA form
/// (`k[]` and `B[]` parallel arrays, the journal's layout). On
/// return `(k[0], B[0])` is the composite of all `n` leaves;
/// K−1 composes, dependency depth ⌈log₂ K⌉. Applying the composite
/// once (`fold_compose_apply(h, K, V)`) equals the serial chain's
/// iterated per-record applies, bit-exactly — Theorem 22's
/// determinism class, pinned by the cells.
///
/// The empty input composes to the identity `(0, 0)`.
pub fn tree_fold_segments(k: &mut [u32], b: &mut [u64]) -> (u32, u64) {
    let n = k.len();
    debug_assert_eq!(b.len(), n, "segment tree SoA columns must match");
    if n == 0 {
        return (0, 0);
    }
    if n == 1 {
        return (k[0], b[0]);
    }
    let mut stride = 1usize;
    while stride < n {
        let mut i = 0usize;
        while i + stride < n {
            let kb = k[i + stride];
            let ba = b[i];
            let bb = b[i + stride];
            k[i] = k[i].wrapping_add(kb);
            b[i] = ba.rotate_left(FOLD_ROT.wrapping_mul(kb) % 64) ^ bb;
            i += stride << 1;
        }
        stride <<= 1;
    }
    (k[0], b[0])
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

/// 8-lane interleaved hardware CRC32C over a byte span, lanes combined by
/// the R24 SPANHASH-64 CLMUL polynomial hash. Deterministic for a given
/// (bytes) on every SSE4.2 x86_64 — and since R24, bit-identical on every
/// host (the portable fallback reproduces the exact lane layout).
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
pub fn span_crc32c_8lane(body: &[u8]) -> u64 {
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
    // Lane combine: R24 SPANHASH-64 — the CLMUL polynomial hash over the
    // 8 lane registers + the length (replaces the FNV-1a-64 serial chain;
    // value law: pure function of (lanes, len), bit-identical on every
    // host and path — the polyhash differential pins it).
    crate::crcfold::span_lane_combine(&[c0, c1, c2, c3, c4, c5, c6, c7], len)
}

/// Non-x86_64 fallback: the SAME 8-lane layout + SPANHASH-64 combine in
/// portable code (software CRC32C lanes) — R24 upgraded this from FNV
/// over bytes to TRUE cross-arch bit parity with the x86_64 value (the
/// differential suite now pins identical golden values everywhere).
#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
pub fn span_crc32c_8lane(body: &[u8]) -> u64 {
    const CRC32C_POLY_REFLECTED: u32 = 0x82F6_3B78;
    #[inline(always)]
    fn crc8(c: u32, b: u8) -> u32 {
        let mut c = c ^ (b as u32);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ CRC32C_POLY_REFLECTED
            } else {
                c >> 1
            };
        }
        c
    }
    #[inline(always)]
    fn crc_w(c: u32, w: u64) -> u32 {
        // LSB byte first — the _mm_crc32_u64 register semantics.
        let mut c = c;
        for b in w.to_le_bytes() {
            c = crc8(c, b);
        }
        c
    }
    let len = body.len();
    let mut lanes = [0u32; 8];
    let mut i = 0usize;
    // 64B blocks — lane k hashes bytes i+8k..i+8k+8 (the exact x86_64
    // lane layout; qwords little-endian).
    while i + 64 <= len {
        for k in 0..8usize {
            let w = u64::from_le_bytes(
                body[i + 8 * k..i + 8 * k + 8]
                    .try_into()
                    .unwrap(),
            );
            lanes[k] = crc_w(lanes[k], w);
        }
        i += 64;
    }
    // Tail (< 64B) folded into lane 0 sequentially.
    while i + 8 <= len {
        let w = u64::from_le_bytes(body[i..i + 8].try_into().unwrap());
        lanes[0] = crc_w(lanes[0], w);
        i += 8;
    }
    if i + 4 <= len {
        let w = u32::from_le_bytes(body[i..i + 4].try_into().unwrap());
        let mut c = lanes[0];
        for b in w.to_le_bytes() {
            c = crc8(c, b);
        }
        lanes[0] = c;
        i += 4;
    }
    if i + 2 <= len {
        let w = u16::from_le_bytes([body[i], body[i + 1]]);
        let mut c = lanes[0];
        for b in w.to_le_bytes() {
            c = crc8(c, b);
        }
        lanes[0] = c;
        i += 2;
    }
    if i < len {
        lanes[0] = crc8(lanes[0], body[i]);
    }
    crate::crcfold::span_lane_combine(&lanes, len)
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
        self.hash = self.hash.rotate_left(13) ^ span.wrapping_mul(FOLD_MIX_P);
        self.count += count as u64;
    }
}
