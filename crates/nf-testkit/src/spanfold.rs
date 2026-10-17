//! R36 (Run 800) — the Res-Fold: the closed-form non-abelian segment
//! SPANHASH fold, the 36-byte composite record, and the Galois-shift
//! cross-worker merge (docs/solution1.md, Theorems 9–17).
//!
//! # The physics this module exists for
//!
//! The R34 kernel collapsed the LOB signature coordinates (volumes,
//! price) into the commutative monoid 𝒜₅.₀ — but the Res state
//! (docs/solution1.md Def 1.1) carries a fourth coordinate: the
//! SPANHASH, whose per-message update `h ← (h ⊗ x^{w_t}) ⊕ v_t(p)` is
//! an AFFINE map over the ring `R = GF(2)[x]/Q(x)`. The sequential
//! chain is serially dependent (each CLMUL waits for the last) and
//! branch-heavy (the 9-arm type cascade): ~106 cycles/message on the
//! baseline — the wall the R35 fleet still prices at ~21 µs/pass of
//! worker evaluation.
//!
//! # The mathematics (docs/solution1.md §1–§4)
//!
//! **Lemma 5 (the suffix formula).** The composite of a chunk
//! `B = m₁…m_L` factors exactly:
//!
//! ```text
//! h_out = h_in ⊗ x^{W(B)} ⊕ V(B),    V(B) = ⊕ᵢ vᵢ ⊗ x^{dᵢ}
//! ```
//!
//! with `W(B)` the byte span (a function of the type MULTISET) and
//! `dᵢ` the suffix degrees (functions of the type ORDER). The `L`
//! terms of `V` are mutually independent — the serialization is an
//! artifact of the sequential loop, not of the mathematics. Theorem 10
//! is the branch-free re-association; Theorem 11 is the honest lower
//! bound: any FRESH evaluation reads Ω(L) digests. The asymptotic win
//! is Corollary 10.1 — the composite is a **36-byte record** whose
//! composition with any other record is ONE carry-less multiply and
//! one XOR.
//!
//! **Theorem 17 (the Galois shift).** The cross-worker merge is
//! `ℋ_global = ℋ_early ⊗ x^{W_late} ⊕ ℋ_late` — the shift is by the
//! LATER segment's span (Appendix D item 1: the deg convention trap),
//! the multiplier is ONE field element (never a 64×64 matrix — Theorem
//! 16 kills the fixed-Λ claim, §4.4's representation remark kills the
//! matrix representation), and it applies in 3 CLMUL + 1 XOR.
//!
//! # The port's two documented fixes to the reference kernel
//!
//! The C17 kernel in docs/solution1.md §2.6 carries the same class of
//! defect the R34 reference kernel did (the tail double-count, PR #19):
//!
//! 1. **The compress mask.** `mulmod4` gathers its four lane results
//!    from the EVEN 64-bit words (each 128-bit lane's low word holds
//!    the product of the even-word operands), but the reference's
//!    `_mm512_maskz_compress_epi64(0xAA, r)` selects the ODD words
//!    (mask bit i selects word i; 0xAA = bits 1,3,5,7). The port uses
//!    `0x55` (bits 0,2,4,6). Probed on-silicon before the port; the
//!    dual-path fuzz (cell `t_r36_mulmod_dual_path_fuzz`) pins it.
//! 2. **The tail degree.** The scalar tail's degree expression is
//!    written from the same `W − P_i − w_i` table as the vector body —
//!    one derivation, no second formula to drift (the R34 lesson).
//!
//! # Determinism law (Theorem 18)
//!
//! Everything is exact GF(2) arithmetic: `mulmod` is a fixed
//! straight-line sequence (3 CLMULs, constant fold operands), `⊕` is
//! associative/commutative, and the tagged-max coordinate resolves by
//! a total order. Scalar, 4-lane, and any chunk partition produce
//! bit-identical records — the 130-cell conformance matrix's kernel
//! slice lives in `mod tests` below.

use std::sync::OnceLock;

/// `Q(x) = x⁶⁴ + x⁴ + x³ + x + 1` — the SPANHASH modulus (solution1.md
/// Appendix B.1). `Q(0) = 1`, so `x` is a unit (Lemma 3.4) and every
/// power `x^a` is invertible. Hex `0x1_0000_0000_0000_001B`.
pub const SPAN_QLOW: u64 = 0x1B;

/// ITCH 5.0 wire sizes per message type (bytes), index order
/// `A F E C X D U P Q ·` (solution1.md §1.1, [S1] Appendix B). Index 9
/// is the NON-BOOK identity class (width 0, digest 0 — S/L/N/R/Y/V/B
/// carry no Res content: Def 1.1's universe is the 9 book types, and a
/// width-0/digest-0 generator is the exact monoid identity, so the
/// fabric's fold runs the FULL message stream with non-book messages
/// as in-place identities — no substream materialization).
pub const W_T: [u64; 10] = [36, 40, 31, 36, 23, 19, 35, 43, 40, 0];

/// The byte→W_T index map (the fabric's decode column).
#[inline]
pub fn ty36(t: u8) -> u8 {
    match t {
        b'A' => 0,
        b'F' => 1,
        b'E' => 2,
        b'C' => 3,
        b'X' => 4,
        b'D' => 5,
        b'U' => 6,
        b'P' => 7,
        b'Q' => 8,
        _ => 9,
    }
}

/// Chunk capacity: messages per composite record (the 16 KB SoA class).
pub const CHUNK: usize = 512;

/// Maximum chunk byte span + 1: 512 × 43 + 1 = 22,017 — the `T_pow`
/// table's exclusive bound (a chunk's degree is < MAXSPAN; Appendix D
/// item 8: every call site asserts it). The identity class contributes
/// 0 to the degree, so the bound is the 9 book types' own law.
pub const MAXSPAN: usize = CHUNK * 43 + 1;

/// The degree-collision guard: `ord(x)` is exhaustively verified to
/// exceed this at init (Lemma 4 / Theorem 16's hypothesis — solution1.md
/// §2.6). A full day of 500k-message passes stays below it in degree.
pub const ORD_FLOOR: u64 = 1 << 21;

/// The base power table's length: the full u16 degree range. (The port's
/// documented fix #3: the reference's two-level `shift_const` inits
/// `T_hi` from `T_pow[65536]` — past its own `MAXSPAN = 22,017` bound.
/// The chunk LAW stays 22,017; the TABLE covers 2¹⁶ so the two-level
/// low word is always in-bounds. 512 KB, L2-resident, built once.)
const T_LEN: usize = 1 << 16;

// ─────────────────────────────────────────────────────────────────────
// The scalar ring arithmetic
// ─────────────────────────────────────────────────────────────────────

/// The portable schoolbook multiply: shift-XOR carry-less product
/// `(lo, hi)` of two degree-≤63 polynomials — the CI oracle for the
/// CLMUL paths (checklist item 2: it ships forever).
pub fn clmul128_ref(a: u64, b: u64) -> (u64, u64) {
    let mut lo = 0u64;
    let mut hi = 0u64;
    let mut b = b;
    let mut i = 0u32;
    while b != 0 {
        if b & 1 != 0 {
            lo ^= a << i;
            if i > 0 {
                hi ^= a >> (64 - i);
            }
        }
        b >>= 1;
        i += 1;
    }
    (lo, hi)
}

/// The fully independent reduction oracle: 128-bit polynomial long
/// division by `Q`, MSB-first (no structural sharing with the 3-CLMUL
/// fold — the deepest oracle in the fuzz stack). For each set bit `k`
/// of the high word (the degree-(64+k) term), XOR `Q << k` into the
/// 128-bit value: the `x⁶⁴ << k` term cancels bit `k` exactly, and the
/// `QLOW << k` residue only stirs bits `≤ k+4` — strictly below the
/// frontier, so the loop terminates with `hi == 0`.
pub fn mulmod_divmod_ref(a: u64, b: u64) -> u64 {
    fn xor_q_at(hi: &mut u64, lo: &mut u64, pos: u32) {
        // Q(x) << pos = x^(64+pos) ⊕ (QLOW << pos).
        *hi ^= 1u64 << pos; // the x^64 term, landing on hi bit pos
        for j in 0..5u32 {
            if (SPAN_QLOW >> j) & 1 != 0 {
                let bit = pos + j;
                if bit < 64 {
                    *lo ^= 1u64 << bit;
                } else {
                    *hi ^= 1u64 << (bit - 64); // always < pos: j ≤ 4
                }
            }
        }
    }
    let (mut lo, mut hi) = clmul128_ref(a, b);
    for k in (0..64u32).rev() {
        if (hi >> k) & 1 != 0 {
            xor_q_at(&mut hi, &mut lo, k);
        }
    }
    debug_assert_eq!(hi, 0, "divmod oracle: hi did not clear (deg > 126 input?)");
    lo
}

/// The 3-step CLMUL reduction, scalar form (solution1.md §2.6
/// `mulmod_u64`, Appendix B.2's proof): `p = lo ⊕ hi·x⁶⁴ ≡ lo ⊕ hi·QLOW`,
/// then one more fold of the degree-≤66 intermediate. Requires nothing
/// beyond PCLMULQDQ on x86_64; the portable twin is [`mulmod_ref`].
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "pclmulqdq")]
#[inline]
unsafe fn mulmod_clmul(a: u64, b: u64) -> u64 {
    use std::arch::x86_64::*;
    let av = _mm_cvtsi64_si128(a as i64);
    let bv = _mm_cvtsi64_si128(b as i64);
    let p = _mm_clmulepi64_si128(av, bv, 0x00);
    let lo = _mm_cvtsi128_si64(p) as u64;
    let hi = _mm_extract_epi64(p, 1) as u64; // deg ≤ 62
    let hv = _mm_cvtsi64_si128(hi as i64);
    let qv = _mm_cvtsi64_si128(SPAN_QLOW as i64);
    let t = _mm_clmulepi64_si128(hv, qv, 0x00);
    let tlo = _mm_cvtsi128_si64(t) as u64; // deg ≤ 63
    let u = _mm_clmulepi64_si128(
        _mm_cvtsi64_si128((_mm_extract_epi64(t, 1) as u64) as i64),
        qv,
        0x00,
    );
    lo ^ tlo ^ (_mm_cvtsi128_si64(u) as u64)
}

/// `a ⊗ b mod Q` — the dispatching ring multiply. CLMUL when the class
/// is present, schoolbook otherwise. Bit-identical on both paths (the
/// 2²⁰-pair dual-path fuzz pins it — cell `t_r36_mulmod_dual_path_fuzz`).
#[inline]
pub fn mulmod(a: u64, b: u64) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("pclmulqdq") {
            // SAFETY: the PCLMULQDQ class was just detected at runtime.
            return unsafe { mulmod_clmul(a, b) };
        }
    }
    mulmod_ref(a, b)
}

/// The portable `a ⊗ b mod Q` (schoolbook product + the same 3-step
/// reduction structure proven in Appendix B.2 — the reduction is
/// straight-line and exact on both paths; the fuzz additionally
/// cross-checks against the fully independent [`mulmod_divmod_ref`]).
#[inline]
pub fn mulmod_ref(a: u64, b: u64) -> u64 {
    let (lo, hi) = clmul128_ref(a, b);
    let (tlo, thi) = clmul128_ref(hi, SPAN_QLOW);
    let (u, _) = clmul128_ref(thi, SPAN_QLOW);
    lo ^ tlo ^ u
}

// ─────────────────────────────────────────────────────────────────────
// The degree tables (Theorem 10a / Theorem 17)
// ─────────────────────────────────────────────────────────────────────

/// `T_pow[k] = x^k mod Q` for `k < 2¹⁶` (65,536 entries, 512 KB —
/// L2-resident, monotone-index access). Built once, ~65k mulmods;
/// the same init exhaustively verifies `ord(x) > 2²¹` (Lemma 4's
/// no-collision precondition — 2²¹ mulmods, ~10 ms once per process).
pub fn t_pow() -> &'static [u64; T_LEN] {
    static T: OnceLock<Box<[u64; T_LEN]>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = Box::new([0u64; T_LEN]);
        t[0] = 1;
        for k in 1..T_LEN {
            t[k] = mulmod(t[k - 1], 2);
        }
        // The order check: x^k != 1 for every k < 2^21 (solution1.md
        // §2.6 spanfold_init; assert-at-init is the fail-loud law).
        let mut h: u64 = 1;
        for _ in 1..ORD_FLOOR {
            h = mulmod(h, 2);
            assert!(h != 1, "ord(x) <= 2^21: degree collisions possible (Lemma 4 violated)");
        }
        t
    })
}

/// `x^d mod Q` for a chunk-level degree (one table load).
/// `d` must be < MAXSPAN (the chunk law; debug-asserted).
#[inline]
pub fn shift_chunk(d: u64) -> u64 {
    debug_assert!((d as usize) < MAXSPAN, "chunk degree {d} exceeds the table (Appendix D item 8)");
    t_pow()[d as usize]
}

/// The two-level shift for pass/worker-level degrees (Theorem 17's
/// table row): `x^d = x^{65536·q} ⊗ x^r` — two loads + one mulmod.
/// Covers every `d < 2³²`; the 512 KB `T_hi` is built lazily (only
/// paths that merge at worker scale ever touch it).
pub fn shift_pow(d: u64) -> u64 {
    debug_assert!(d <= u32::MAX as u64, "shift_pow covers d < 2^32");
    let hi = d >> 16;
    let lo = (d & 0xFFFF) as usize;
    if hi == 0 {
        return t_pow()[lo];
    }
    static T_HI: OnceLock<Box<[u64; 1 << 16]>> = OnceLock::new();
    let t_hi = T_HI.get_or_init(|| {
        // x^{65536} = one mulmod past the base table's top (fix #3: the
        // base table covers the full u16 range, so this is always
        // in-bounds — the reference indexed T_pow[65536] past MAXSPAN).
        let x64k = mulmod(t_pow()[T_LEN - 1], 2);
        let mut t = Box::new([0u64; 1 << 16]);
        t[0] = 1;
        for q in 1..(1usize << 16) {
            t[q] = mulmod(t[q - 1], x64k);
        }
        t
    });
    // Bounds: hi < 2^16 by construction (d < 2^32).
    mulmod(t_hi[hi as usize], t_pow()[lo])
}

// ─────────────────────────────────────────────────────────────────────
// The 36-byte composite record (§2.5) and the 48-byte accumulator
// ─────────────────────────────────────────────────────────────────────

/// The R36 chunk record — exactly 36 bytes, packed (Appendix B.5; the
/// `sizeof == 36` assert is load-bearing, Appendix D item 6). The
/// composite of ≤ 512 messages: the hash pair's `V` coordinate, the
/// commutative coordinates (Δvol, the tagged price setter), and the
/// degree/count/flag scalars. `C = x^W` is NOT stored — one table load
/// re-derives it (Theorem 10a; the span discipline's gift).
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct R36 {
    /// The payload fold `V = ⊕ᵢ vᵢ ⊗ x^{dᵢ}` (Lemma 5).
    pub v: u64,
    /// Bid-volume delta (i64: 512 × 2³² < 2⁴² cannot wrap).
    pub db: i64,
    /// Ask-volume delta.
    pub da: i64,
    /// Last price write (valid iff `flg & FLG_SETTER`).
    pub px: u32,
    /// Wire offset of the last setter, intra-chunk (≤ 22,016 < 2¹⁶).
    pub tag: u16,
    /// Chunk byte span == the linear part's degree (≤ 22,016).
    pub w: u16,
    /// Message count (≤ 512).
    pub cnt: u16,
    /// b0 setter · b1 trade · b2 escape-to-P64.
    pub flg: u16,
}

/// `flg` bit 0: the chunk contains a price setter (px/tag valid).
pub const FLG_SETTER: u16 = 1;
/// `flg` bit 1: the chunk contains a trade/print.
pub const FLG_TRADE: u16 = 2;
/// `flg` bit 2: the degree left the u16 envelope (the P64 escape).
pub const FLG_ESCAPE: u16 = 4;

const _: () = assert!(std::mem::size_of::<R36>() == 36, "R36 must be exactly 36 bytes");

impl R36 {
    /// The monoid identity (0 messages: `x^0 = 1`, `V = 0`).
    pub const IDENTITY: R36 = R36 {
        v: 0,
        db: 0,
        da: 0,
        px: 0,
        tag: 0,
        w: 0,
        cnt: 0,
        flg: 0,
    };

    /// The linear part `C = x^W` (one table load — never stored).
    #[inline]
    pub fn c(&self) -> u64 {
        shift_chunk(self.w as u64)
    }

    /// All fields by value (packed records forbid field references —
    /// the view copies everything; equality/`assert_eq!` compare views).
    #[inline]
    pub fn view(&self) -> R36View {
        R36View {
            v: self.v,
            db: self.db,
            da: self.da,
            px: self.px,
            tag: self.tag,
            w: self.w,
            cnt: self.cnt,
            flg: self.flg,
        }
    }
}

/// The aligned, reference-friendly twin of [`R36`] — identical field
/// semantics, natural alignment, for comparisons and formatting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct R36View {
    pub v: u64,
    pub db: i64,
    pub da: i64,
    pub px: u32,
    pub tag: u16,
    pub w: u16,
    pub cnt: u16,
    pub flg: u16,
}

/// The pass/worker accumulator — the P64 record (§2.5's two-level
/// scheme): the degree leaves the u16 range at pass scale (500k × ~35 B
/// ≈ 17.5 M), so `C` is stored explicitly as a field element and the
/// degree composes in u32. 48 bytes, naturally aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct P64 {
    /// The payload fold.
    pub v: u64,
    /// The linear part `C = x^deg`, stored explicitly.
    pub c: u64,
    pub db: i64,
    pub da: i64,
    pub px: u32,
    /// Wire offset of the last setter, pass-absolute (u32 at pass scale).
    pub tag: u32,
    /// The accumulated degree (u32).
    pub deg: u32,
    pub cnt: u32,
    pub flg: u16,
}

impl P64 {
    /// The monoid identity.
    pub const IDENTITY: P64 = P64 {
        v: 0,
        c: 1,
        db: 0,
        da: 0,
        px: 0,
        tag: 0,
        deg: 0,
        cnt: 0,
        flg: 0,
    };

    /// Lift a chunk record into the accumulator algebra (the level hop
    /// of §2.5's two-level scheme — the escape at chunk boundaries).
    #[inline]
    pub fn from_r36(r: &R36) -> P64 {
        P64 {
            v: r.v,
            c: r.c(),
            db: r.db,
            da: r.da,
            px: r.px,
            tag: r.tag as u32,
            deg: r.w as u32,
            cnt: r.cnt as u32,
            flg: r.flg,
        }
    }
}

/// O(1) chunk-record composition (Corollary 10.1): `a` earlier, `b`
/// later — 1 ring multiply + 1 XOR for the hash coordinate, integer
/// adds and a tagged max for the rest. Associative (Lemma 3 + the
/// commutative coordinates' laws — the fuzz pins it on random triples).
#[inline]
pub fn r36_compose(a: &R36, b: &R36) -> R36 {
    let v = mulmod(a.v, shift_chunk(b.w as u64)) ^ b.v; // V_a·x^{W_b} ⊕ V_b
    let (px, tag) = if b.flg & FLG_SETTER != 0 {
        (b.px, a.w.wrapping_add(b.tag)) // dense-seq: the later setter wins
    } else {
        (a.px, a.tag)
    };
    R36 {
        v,
        db: a.db.wrapping_add(b.db),
        da: a.da.wrapping_add(b.da),
        px,
        tag,
        w: a.w.wrapping_add(b.w),
        cnt: a.cnt.wrapping_add(b.cnt),
        flg: a.flg | b.flg,
    }
}

/// O(1) accumulator composition (the P64 twin of [`r36_compose`]):
/// field-element multiplies only, never tables (§2.5's level law).
#[inline]
pub fn p64_compose(a: &P64, b: &P64) -> P64 {
    let c = mulmod(a.c, b.c);
    let v = mulmod(a.v, b.c) ^ b.v;
    let (px, tag) = if b.flg & FLG_SETTER != 0 {
        (b.px, a.deg.wrapping_add(b.tag))
    } else {
        (a.px, a.tag)
    };
    P64 {
        v,
        c,
        db: a.db.wrapping_add(b.db),
        da: a.da.wrapping_add(b.da),
        px,
        tag,
        deg: a.deg.wrapping_add(b.deg),
        cnt: a.cnt.wrapping_add(b.cnt),
        flg: a.flg | b.flg,
    }
}

/// Apply a record to a running hash (the memoized replay's single
/// on-path operation): `h ↦ h ⊗ C ⊕ V`.
#[inline]
pub fn hash_apply(h: u64, r: &R36) -> u64 {
    mulmod(h, r.c()) ^ r.v
}

/// Apply a P64 accumulator to a running hash.
#[inline]
pub fn hash_apply_p64(h: u64, r: &P64) -> u64 {
    mulmod(h, r.c) ^ r.v
}

// ─────────────────────────────────────────────────────────────────────
// The Galois-shift merge (§4, Theorem 17)
// ─────────────────────────────────────────────────────────────────────

/// THE CROSS-WORKER MERGE: `ℋ = ℋ_early ⊗ x^{W_late} ⊕ ℋ_late`.
/// The shift is by the LATER segment's span — the deg-convention trap
/// (Appendix D item 1) is structural here: the argument NAMES carry
/// the convention, and the split-point sweep cell hunts the wrong one.
/// 3 CLMUL + 1 XOR ≈ 10–15 cycles.
#[inline]
pub fn hash_merge(h_early: u64, h_late: u64, w_late: u64) -> u64 {
    mulmod(h_early, shift_pow(w_late)) ^ h_late
}

// ─────────────────────────────────────────────────────────────────────
// The chunk fold (Theorem 10) — branch-free, dual-path
// ─────────────────────────────────────────────────────────────────────

/// The fold's per-message input columns (SoA, the scan phase's
/// product — solution1.md §7 checklist item 9): type index into
/// `W_T`, the payload digest, the volume deltas, and the price-write
/// columns. `vd`, `db`, `da` must be `L` long.
#[derive(Clone, Default)]
pub struct SpanTokens {
    pub ty: Vec<u8>,
    pub vd: Vec<u64>,
    pub db: Vec<i64>,
    pub da: Vec<i64>,
    pub px: Vec<u32>,
    /// 1 where the message is a price setter (the tagged-max key rides
    /// the wire offset — dense sequencing, §2.5).
    pub setp: Vec<u8>,
}

impl SpanTokens {
    pub fn with_len(n: usize) -> Self {
        SpanTokens {
            ty: vec![0u8; n],
            vd: vec![0u64; n],
            db: vec![0i64; n],
            da: vec![0i64; n],
            px: vec![0u32; n],
            setp: vec![0u8; n],
        }
    }

    pub fn len(&self) -> usize {
        self.vd.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vd.is_empty()
    }
}

/// The suffix degrees `d_i = W − P_i − w_i` — the STATIC half of
/// Theorem 10 (a pure function of the type sequence). (Fused into
/// [`fold_cols`]'s single pass; kept for the tokens path and tests.)
#[allow(dead_code)]
fn static_degrees_into(ty: &[u8], d: &mut [u32; CHUNK]) -> u64 {
    let n = ty.len();
    debug_assert!(n <= CHUNK);
    let mut w = 0u64;
    for i in 0..n {
        let wt = W_T[ty[i] as usize];
        d[i] = (w + wt) as u32; // P_{i+1} (prefix AFTER the message)
        w += wt;
    }
    // d_i = W − P_{i+1} (the suffix degree: everything after message i).
    for i in 0..n {
        d[i] = (w as u32).wrapping_sub(d[i]);
    }
    w
}

/// The sequential oracle — Lemma 3(2) iterated: the 2-op recursion
/// `h_i = h_{i−1} ⊗ x^{w_i} ⊕ v_i`, and the commutative coordinates
/// accumulated message-by-message. The judge every vector path must
/// match bit-for-bit (Theorem 18's dual-path law).
pub fn fold_chunk_sequential(t: &SpanTokens, h_in: u64) -> (R36, u64) {
    let n = t.len();
    let mut h = h_in;
    let mut db: i64 = 0;
    let mut da: i64 = 0;
    let mut px = 0u32;
    let mut tag = 0u16;
    let mut flg = 0u16;
    let mut w = 0u64;
    for i in 0..n {
        let wt = W_T[t.ty[i] as usize];
        h = mulmod(h, shift_chunk(wt)) ^ t.vd[i];
        db = db.wrapping_add(t.db[i]);
        da = da.wrapping_add(t.da[i]);
        w += wt;
        if t.setp[i] != 0 {
            px = t.px[i];
            tag = w as u16; // wire offset AFTER this message
            flg |= FLG_SETTER;
        }
    }
    let rec = R36 {
        v: 0, // (filled below: V = h ⊕ h_in⊗x^W — the closed form)
        db,
        da,
        px,
        tag,
        w: w as u16,
        cnt: n as u16,
        flg,
    };
    // V(B) = (h_out ⊕ h_in⊗x^{W}) — Lemma 5 inverted; using the exact
    // recursion output keeps the oracle independent of the suffix form.
    let v = h ^ mulmod(h_in, shift_chunk(w));
    (R36 { v, ..rec }, h)
}

/// THE CLOSED-FORM FOLD (Theorem 10b): `V = ⊕ᵢ vᵢ ⊗ x^{dᵢ}` — the L
/// terms are independent (no serial dependency), so the accumulation
/// is a flat XOR of 4-lane products. The prefix degrees are an integer
/// prefix-sum; the type's only role is a table index (no switch). The
/// vector body runs when AVX-512F + VPCLMULQDQ are present and the row
/// is wide enough to amortize the horizontal finish; the portable twin
/// otherwise. Both paths share the suffix-degree derivation — one
/// formula, no drift (the R34 tail lesson).
///
/// CONTRACT: `n ≤ CHUNK` (the chunk law, Appendix D item 8 — the R36
/// record's u16 coordinates are chunk-legal by construction; streams
/// fold through [`fold_stream`], the two-level scheme's P64 path).
pub fn fold_chunk(t: &SpanTokens, h_in: u64) -> (R36, u64) {
    fold_cols(&t.ty, &t.vd, &t.db, &t.da, &t.px, &t.setp, h_in)
}

/// THE FOLD, COLUMN-VIEW FORM — the fabric's API (the scan phase owns
/// the SoA columns; the fold consumes slices, ZERO materialization:
/// no per-chunk struct copies, no allocation — the Tier-F law). All
/// columns must be `n` long. The `n ≤ CHUNK` law and the dispatching
/// are identical to [`fold_chunk`].
pub fn fold_cols(
    ty: &[u8],
    vd: &[u64],
    db: &[i64],
    da: &[i64],
    px: &[u32],
    setp: &[u8],
    h_in: u64,
) -> (R36, u64) {
    let n = ty.len();
    debug_assert!(n <= CHUNK, "fold_cols on {n} messages: the chunk law is 512");
    if n == 0 {
        return (R36::IDENTITY, h_in);
    }
    let mut dscratch = [0u32; CHUNK]; // 2 KB stack — never the heap
    // THE FUSED STATIC PASS — one walk over the type column produces
    // the prefix degrees AND the commutative coordinates (the layout
    // law: every column is touched exactly once outside the V kernel).
    let mut w = 0u64;
    let mut sdb: i64 = 0;
    let mut sda: i64 = 0;
    let mut spx = 0u32;
    let mut stag = 0u16;
    let mut sflg = 0u16;
    for i in 0..n {
        let wt = W_T[ty[i] as usize];
        w += wt;
        dscratch[i] = w as u32; // P_{i+1} (prefix AFTER the message)
        sdb = sdb.wrapping_add(db[i]);
        sda = sda.wrapping_add(da[i]);
        if setp[i] != 0 {
            spx = px[i];
            stag = w as u16; // wire offset AFTER the message
            sflg |= FLG_SETTER;
        }
    }
    // The suffix degrees: d_i = W − P_{i+1}.
    let d = &mut dscratch[..n];
    for i in 0..n {
        d[i] = (w as u32).wrapping_sub(d[i]);
    }
    let v: u64;
    let tp = t_pow();
    #[cfg(target_arch = "x86_64")]
    {
        if n >= 8
            && std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("vpclmulqdq")
        {
            // SAFETY: both classes were just detected at runtime.
            v = unsafe { fold_v_avx512(vd, d, tp) };
        } else {
            v = fold_v_scalar(vd, d, tp);
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        v = fold_v_scalar(vd, d, tp);
    }
    let rec = R36 {
        v,
        db: sdb,
        da: sda,
        px: spx,
        tag: stag,
        w: w as u16,
        cnt: n as u16,
        flg: sflg,
    };
    (rec, hash_apply(h_in, &rec))
}

/// The portable V-fold: 1 table load + 1 ring multiply + 1 XOR per
/// message, no serial dependency (Theorem 10b's scalar form).
fn fold_v_scalar(vd: &[u64], d: &[u32], tp: &[u64; T_LEN]) -> u64 {
    let mut v = 0u64;
    for i in 0..vd.len() {
        v ^= mulmod(vd[i], tp[d[i] as usize]);
    }
    v
}

/// The 4-lane VPCLMULQDQ V-fold (solution1.md §2.6 `mulmod4`, with the
/// port's operand-construction restructure): per 4 messages, the two
/// operands are built by one 32-byte dense load + one `VPERMQ` (the
/// reference's `_mm512_set_epi64` pairs cost ~16 scattered uops — the
/// measured wall on the dev VM), the three-step reduction accumulates
/// into the FULL ZMM (the odd words carry reduction residue — masked
/// out once at the horizontal finish; XOR is order-free, so the
/// deferred mask is bit-identical to the compress-per-iteration form),
/// and the scalar tail folds exactly once (the R34 law). The compress
/// itself is the reference's 0xAA odd-word bug — the port's mask law
/// is "the results live in the EVEN words".
/// SAFETY: caller proves AVX-512F + VPCLMULQDQ.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f", enable = "vpclmulqdq")]
unsafe fn fold_v_avx512(vd: &[u64], d: &[u32], tp: &[u64; T_LEN]) -> u64 {
    use std::arch::x86_64::*;
    let n = vd.len();
    let nv = n & !3usize;
    if nv == 0 {
        return fold_v_scalar(vd, d, tp);
    }
    // Phase 1 — the multiplier column, one independent L2 load per
    // message (no dependency chain: MLP covers the strided table walk;
    // staging on the stack first keeps the CLMUL loop's operand loads
    // DENSE — the per-4 stack round-trip is a store-forwarding stall
    // on Intel, the measured 20 cyc/msg wall).
    let mut mcol = [0u64; CHUNK];
    for i in 0..nv {
        mcol[i] = tp[d[i] as usize];
    }
    // Phase 2 — the CLMUL loop: two dense 32 B loads + two VPERMQs per
    // 4 messages, 3 VPCLMULQDQ + 3 VPXORQ, the full-ZMM accumulator.
    let q = _mm512_set1_epi64(SPAN_QLOW as i64);
    // The even-word placement permute: source (v0..v3, 0, 0, 0, 0)
    // -> (v0, 0, v1, 0, v2, 0, v3, 0).
    let idx = _mm512_set_epi64(11, 3, 10, 2, 9, 1, 8, 0);
    let mut acc = _mm512_setzero_si512();
    let mut i = 0usize;
    while i < nv {
        // SAFETY: i + 4 <= nv <= vd.len()/mcol len (loop invariant).
        let vdq = _mm512_permutexvar_epi64(
            idx,
            _mm512_zextsi256_si512(_mm256_loadu_si256(vd.as_ptr().add(i) as *const __m256i)),
        );
        let mdq = _mm512_permutexvar_epi64(
            idx,
            _mm512_zextsi256_si512(_mm256_loadu_si256(mcol.as_ptr().add(i) as *const __m256i)),
        );
        // p = 4 carry-less products (imm 0x00: low×low per 128-bit lane).
        let p = _mm512_clmulepi64_epi128(vdq, mdq, 0x00);
        // t = hi⊗QLOW (imm 0x01: each lane's HIGH word of p, deg ≤ 66).
        let t = _mm512_clmulepi64_epi128(p, q, 0x01);
        // u = t_hi⊗QLOW (deg ≤ 6 — terminal).
        let u = _mm512_clmulepi64_epi128(t, q, 0x01);
        // Full-ZMM accumulate: the odd words carry p_hi⊕t_hi⊕u_hi residue
        // (deterministic garbage) — one mask at the finish discards it.
        acc = _mm512_xor_si512(acc, _mm512_xor_si512(_mm512_xor_si512(p, t), u));
        i += 4;
    }
    // Horizontal finish: keep the EVEN words (words 0, 2, 4, 6 — the
    // results; the odd words carry the reduction residue), then one
    // tree reduce.
    let even = _mm512_maskz_mov_epi64(0x55, acc);
    let mut buf = [0u64; 8];
    _mm512_storeu_si512(buf.as_mut_ptr() as *mut __m512i, even);
    let mut v = buf[0] ^ buf[2] ^ buf[4] ^ buf[6];
    // The scalar tail: folded exactly ONCE (the R34 tail law).
    for j in nv..n {
        v ^= mulmod(vd[j], tp[d[j] as usize]);
    }
    v
}

/// Optional per-message tokens: `h_i = h_{i−1} ⊗ x^{w_i} ⊕ v_i`
/// (Theorem 10c) — the prefix-tokens surface. Cross-invariant:
/// `tok[L−1] == h_in ⊗ x^W ⊕ V` (cell `t_r36_token_cross_invariant`).
pub fn fold_chunk_tokens(t: &SpanTokens, h_in: u64, tok: &mut [u64]) {
    let mut h = h_in;
    for i in 0..t.len() {
        h = mulmod(h, shift_chunk(W_T[t.ty[i] as usize])) ^ t.vd[i];
        tok[i] = h;
    }
}

// ─────────────────────────────────────────────────────────────────────
// The stream surface (partition invariance — Theorem 12)
// ─────────────────────────────────────────────────────────────────────

/// Per-chunk records at `chunk` granularity: the memoized-replay store
/// (36 B per 512 messages — 17.5 KB per 250k, L1-resident at pass
/// scale). Any partition of the token stream composes to the same
/// whole-stream record (Theorem 12 — the cells sweep the grid).
pub fn chunk_records(t: &SpanTokens, chunk: usize) -> Vec<R36> {
    assert!(chunk > 0, "chunk size must be positive");
    let n = t.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n {
        let end = (i + chunk).min(n);
        let part = SpanTokens {
            ty: t.ty[i..end].to_vec(),
            vd: t.vd[i..end].to_vec(),
            db: t.db[i..end].to_vec(),
            da: t.da[i..end].to_vec(),
            px: t.px[i..end].to_vec(),
            setp: t.setp[i..end].to_vec(),
        };
        let (rec, _) = fold_chunk(&part, 0);
        out.push(rec);
        i = end;
    }
    out
}

/// The whole-stream composite via record composition (the memoized
/// replay): `r₁ ⨂ r₂ ⨂ …` in tape order — Corollary 10.1's O(1) hops.
/// Must equal the whole-stream fresh fold bit-exactly (Theorem 12).
pub fn compose_records(recs: &[R36]) -> P64 {
    recs
        .iter()
        .fold(P64::IDENTITY, |acc, r| p64_compose(&acc, &P64::from_r36(r)))
}

/// THE STREAM FOLD (§2.5's two-level scheme): any-length token stream
/// — ≤ 512-message blocks through the [`fold_chunk`] kernel, the block
/// records composed by [`p64_compose`] (field elements, never tables —
/// the degree leaves the u16 range at ~3 max-width chunks). This is
/// the fabric's bake path and the partition-invariance surface: any
/// partition, any alignment, the same P64 bit-for-bit (Theorem 12).
pub fn fold_stream(t: &SpanTokens, h_in: u64) -> (P64, u64) {
    fold_stream_cols(&t.ty, &t.vd, &t.db, &t.da, &t.px, &t.setp, h_in)
}

/// The stream fold, column-view form (zero materialization — the
/// fabric's bake path).
pub fn fold_stream_cols(
    ty: &[u8],
    vd: &[u64],
    db: &[i64],
    da: &[i64],
    px: &[u32],
    setp: &[u8],
    h_in: u64,
) -> (P64, u64) {
    let n = ty.len();
    if n == 0 {
        return (P64::IDENTITY, h_in);
    }
    let mut acc = P64::IDENTITY;
    let mut i = 0usize;
    while i < n {
        let end = (i + CHUNK).min(n);
        let (rec, _) = fold_cols(&ty[i..end], &vd[i..end], &db[i..end], &da[i..end], &px[i..end], &setp[i..end], 0);
        acc = p64_compose(&acc, &P64::from_r36(&rec));
        i = end;
    }
    // deg/cnt crossing u32 would need ~4G messages at max span — the
    // debug guard fails loudly long before the wrap could alias.
    debug_assert!(acc.deg < u32::MAX && acc.cnt < u32::MAX);
    (acc, hash_apply_p64(h_in, &acc))
}

/// The stream-scale sequential oracle: the 2-op recursion
/// `h_i = h_{i−1} ⊗ x^{w_i} ⊕ v_i` over the WHOLE tape with P64-range
/// coordinates (deg/tag in u32) — the judge for [`fold_stream`] at any
/// length (the chunk-scale twin is [`fold_chunk_sequential`]).
pub fn fold_stream_sequential(t: &SpanTokens, h_in: u64) -> (P64, u64) {
    let n = t.len();
    let mut h = h_in;
    let mut db: i64 = 0;
    let mut da: i64 = 0;
    let mut px = 0u32;
    let mut tag = 0u32;
    let mut flg = 0u16;
    let mut deg: u32 = 0;
    for i in 0..n {
        let wt = W_T[t.ty[i] as usize];
        h = mulmod(h, shift_pow(wt)) ^ t.vd[i];
        db = db.wrapping_add(t.db[i]);
        da = da.wrapping_add(t.da[i]);
        deg = deg.wrapping_add(wt as u32);
        if t.setp[i] != 0 {
            px = t.px[i];
            tag = deg; // the wire offset after this message
            flg |= FLG_SETTER;
        }
    }
    let v = h ^ mulmod(h_in, shift_pow(deg as u64));
    (
        P64 {
            v,
            c: shift_pow(deg as u64),
            db,
            da,
            px,
            tag,
            deg,
            cnt: n as u32,
            flg,
        },
        h,
    )
}

// ─────────────────────────────────────────────────────────────────────
// R37 (Run 900) — The homomorphic ring-parallel prefix fold
// (docs/solution2.md Part I: Theorems 19–22, 25; Appendix B.1/B.2)
// ─────────────────────────────────────────────────────────────────────
//
// # The physics this section exists for
//
// The R36 champion's pass fold `S_p = S_{p−1} ⨂ R_p` ran as a serial
// left-associated chain: 322 fused-call records × ~16.8 cycles of
// dependent mulmod latency ≈ 2.08 µs/pass — the only loop-carried
// dependency crossing pass boundaries, and the absolute ceiling at
// ~65B–70B msg/s (docs/48 §8, solution2.md §1.2). The mathematical
// content of Part I is that **tape order constrains the LEAF sequence
// of the evaluation, not its bracketing** (Theorem 19): any binary
// tree whose in-order traversal is the tape-ordered record list
// evaluates to the same composite — bit-exactly, in every coordinate
// (Theorem 22). The serial chain was the unique worst bracketing
// (depth K); the up-sweep below is depth ⌈log₂ K⌉.
//
// The tree is ring-polymorphic in exactly the sense of Theorem 28 /
// Appendix C: the SAME up-sweep schedule evaluates the SPANHASH record
// monoid here (GF(2)[x]/Q(x), CLMUL), the engine's segment monoid in
// `sink::tree_fold_segments` (the rotl-13 affine law), and — Part II —
// the epoch merge over lane composites. One schedule, several rings,
// one associativity proof per ring.

/// THE BLELLOCH–GALOIS TREE (Theorem 20(a), Lemma 6): the up-sweep
/// reduce over a tape-ordered P64 slice. In-place: on return `r[0]`
/// holds `r[0] ⨂ r[1] ⨂ … ⨂ r[K−1]` — K−1 composes, dependency depth
/// ⌈log₂ K⌉ (9 levels at the champion's K = 322), the level-ℓ pairs
/// mutually independent (Lemma 6's block invariant: after level ℓ,
/// every `r_i` with `i ≡ 0 (mod 2^ℓ)` holds its aligned block's
/// composite). Any bracketing of the same leaves agrees bit-exactly
/// (Theorem 19; the cells pin tree vs serial, tree vs random shape).
///
/// The empty slice composes to [`P64::IDENTITY`] (the monoid unit).
pub fn tree_fold_p64(r: &mut [P64]) -> P64 {
    let n = r.len();
    if n == 0 {
        return P64::IDENTITY;
    }
    if n == 1 {
        return r[0];
    }
    let mut stride = 1usize;
    while stride < n {
        let mut i = 0usize;
        while i + stride < n {
            // SAFETY-free scalar compose: a = r[i] (earlier block),
            // b = r[i + stride] (later block) — tape order preserved
            // by construction (i < i + stride on the leaf grid).
            let a = r[i];
            let b = r[i + stride];
            r[i] = p64_compose(&a, &b);
            i += stride << 1;
        }
        stride <<= 1;
    }
    r[0]
}

/// The 4-lane `mulmod4` batch: FOUR independent GF(2⁶⁴) products in
/// one ZMM pipeline (the [S1'] §2.6 kernel, results-kept-separate
/// form — `fold_v_avx512` XOR-accumulates 4 products into the V-fold;
/// the tree needs each product as its own leaf value). The operand
/// discipline is the R36 port's: dense 32-byte stack loads + one
/// `VPERMQ` per operand (the reference's `_mm512_set_epi64` scatter
/// costs ~16 uops — the measured dev-VM wall), products in the EVEN
/// words, the odd words carrying reduction residue masked once at
/// the finish.
///
/// SAFETY: caller proves AVX-512F + VPCLMULQDQ.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f", enable = "vpclmulqdq")]
unsafe fn mulmod4x4(a: &[u64; 4], b: &[u64; 4]) -> [u64; 4] {
    use std::arch::x86_64::*;
    let q = _mm512_set1_epi64(SPAN_QLOW as i64);
    // (a0,0,a1,0,a2,0,a3,0) — the even-word placement permute.
    let idx = _mm512_set_epi64(11, 3, 10, 2, 9, 1, 8, 0);
    let va = _mm512_permutexvar_epi64(
        idx,
        _mm512_zextsi256_si512(_mm256_loadu_si256(a.as_ptr() as *const __m256i)),
    );
    let vb = _mm512_permutexvar_epi64(
        idx,
        _mm512_zextsi256_si512(_mm256_loadu_si256(b.as_ptr() as *const __m256i)),
    );
    // p = 4 carry-less products (imm 0x00: low×low per 128-bit lane).
    let p = _mm512_clmulepi64_epi128(va, vb, 0x00);
    // t = hi⊗QLOW (imm 0x01: each lane's HIGH word of p, deg ≤ 66).
    let t = _mm512_clmulepi64_epi128(p, q, 0x01);
    // u = t_hi⊗QLOW (deg ≤ 6 — terminal).
    let u = _mm512_clmulepi64_epi128(t, q, 0x01);
    // Even words = the reduced products; odd words carry residue.
    let acc = _mm512_xor_si512(_mm512_xor_si512(p, t), u);
    let even = _mm512_maskz_mov_epi64(0x55, acc);
    // The store lands all 8 words (64 B) — the buffer must be 8 words
    // (the 4-word overflow bug the dual-path cell caught: stack
    // smash, `c: 0` divergence). The products live in the even words.
    let mut buf = [0u64; 8];
    _mm512_storeu_si512(buf.as_mut_ptr() as *mut __m512i, even);
    [buf[0], buf[2], buf[4], buf[6]]
}

/// The vector-scheduled tree (Theorem 21): every level's independent
/// composes run 4-pair batches through [`mulmod4x4`] — 2 batched
/// calls per 4 composes (V_a⊗C_b and C_a⊗C_b), the integer
/// coordinates composed scalar per pair (they ride free: 1-cycle ops
/// off the mulmod latency chain, [S1'] §6's schedule accounting).
/// Scalar tails overlap the next level (odd-K and last-pair tails);
/// levels with < 4 pairs run scalar outright. Bit-equal to
/// [`tree_fold_p64`] by Theorem 22 (pinned by the dual-path cell).
///
/// SAFETY: caller proves AVX-512F + VPCLMULQDQ.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f", enable = "vpclmulqdq")]
unsafe fn tree_fold_p64_avx512(r: &mut [P64]) -> P64 {
    let n = r.len();
    if n < 2 {
        return tree_fold_p64(r);
    }
    let mut stride = 1usize;
    while stride < n {
        let mut i = 0usize;
        // The vector batches: 4 pairs at (i, i+stride), (i+2s, i+3s),
        // (i+4s, i+5s), (i+6s, i+7s) — the guard requires the LAST
        // pair's later operand in range.
        while i + 7 * stride < n {
            let mut va = [0u64; 4];
            let mut ca = [0u64; 4];
            let mut vb = [0u64; 4];
            let mut cb = [0u64; 4];
            for m in 0..4 {
                let ia = i + (2 * m) * stride;
                let ib = ia + stride;
                va[m] = r[ia].v;
                ca[m] = r[ia].c;
                vb[m] = r[ib].v;
                cb[m] = r[ib].c;
            }
            let vres = mulmod4x4(&va, &cb);
            let cres = mulmod4x4(&ca, &cb);
            for m in 0..4 {
                let ia = i + (2 * m) * stride;
                let ib = ia + stride;
                let a = r[ia];
                let b = r[ib];
                let mut t = a;
                t.v = vres[m] ^ b.v;
                t.c = cres[m];
                t.db = a.db.wrapping_add(b.db);
                t.da = a.da.wrapping_add(b.da);
                let (px, tag) = if b.flg & FLG_SETTER != 0 {
                    (b.px, a.deg.wrapping_add(b.tag))
                } else {
                    (a.px, a.tag)
                };
                t.px = px;
                t.tag = tag;
                t.deg = a.deg.wrapping_add(b.deg);
                t.cnt = a.cnt.wrapping_add(b.cnt);
                t.flg = a.flg | b.flg;
                r[ia] = t;
            }
            i += 8 * stride;
        }
        // The scalar tail of the level.
        while i + stride < n {
            let a = r[i];
            let b = r[i + stride];
            r[i] = p64_compose(&a, &b);
            i += stride << 1;
        }
        stride <<= 1;
    }
    r[0]
}

/// The dispatching tree: the AVX-512 schedule when the silicon has
/// it and the level work amortizes the horizontal finish, the
/// portable scalar up-sweep otherwise (the R34 dual-path law: one
/// formula, no drift — the cell pins vector == scalar).
pub fn tree_fold_p64_dispatch(r: &mut [P64]) -> P64 {
    #[cfg(target_arch = "x86_64")]
    {
        if r.len() >= 8
            && std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("vpclmulqdq")
        {
            // SAFETY: both classes were just detected at runtime.
            return unsafe { tree_fold_p64_avx512(r) };
        }
    }
    tree_fold_p64(r)
}

/// THE TWO-LEVEL TREE (§2.7, Corollary 19.1 — the L1 journal): runs
/// of ≤ 512 chunk records (18.4 KB, L1-resident) reduce to
/// sub-composites, and the (far fewer) sub-composites tree into the
/// pass composite. The maximal variant — one record per chunk, 5,122
/// R36 records per pass — stays L1-resident at every level this way,
/// and the u16 in-level degrees never escape (the escape to P64's
/// u32 happens exactly at the level hop, [S1'] pitfall 7's fence).
/// Heterogeneous by Theorem 19's remark (ii): the two-level split is
/// just another bracketing. `run` is the level-1 run length (the
/// blueprint's ≤ 512; any run ≥ 1 is legal).
pub fn tree_fold_two_level(r36s: &[R36], run: usize) -> P64 {
    assert!(run >= 1, "two-level run length must be positive");
    if r36s.is_empty() {
        return P64::IDENTITY;
    }
    // Level 1: each run folds to its sub-composite. A run of ≤ 512
    // R36s composes exactly (degrees ≤ 512 × 22,016 < 2^24 — u32-safe
    // at the hop, u16-EXACT only inside ≤ 3 max-width chunks, which
    // is why the run's own tree lifts to P64 BEFORE any compose).
    let mut subs: Vec<P64> = Vec::with_capacity(r36s.len().div_ceil(run));
    let mut acc = P64::IDENTITY;
    let mut run_len = 0usize;
    for rec in r36s {
        acc = p64_compose(&acc, &P64::from_r36(rec));
        run_len += 1;
        if run_len == run {
            subs.push(acc);
            acc = P64::IDENTITY;
            run_len = 0;
        }
    }
    if run_len > 0 {
        subs.push(acc);
    }
    // Level 2: the tree over the sub-composites.
    tree_fold_p64_dispatch(&mut subs)
}

/// THE EPOCH MERGE (Theorem 25, §B.2): `S' = S ⨂ X_first ⨂ X_second`
/// — the epoch's lane blocks in FIXED tape order, two composes, no
/// loads of the other lane's state, no negotiation, no waiting. The
/// shift degrees ride inside the P64 `C` coordinates (never tables,
/// never a materialized matrix — [S1'] §4.4). The block-split
/// assignment (one factor per lane) costs exactly this; `m`-way
/// interleaving chains the same law `2m − 1` times.
///
/// The lane-epoch composite identity (cell +34's kernel form):
/// `S ⨂ X_first ⨂ X_second` == one lane's fold of the same tape —
/// Proposition 2.1's homomorphism at the epoch boundary.
pub fn epoch_merge_p64(s: &P64, x_first: &P64, x_second: &P64) -> P64 {
    p64_compose(&p64_compose(s, x_first), x_second)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift — the cell PRNG.
    struct Xs(u64);
    impl Xs {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Cell +6 (+6b): mulmod fuzz — the CLMUL kernel vs the schoolbook
    /// vs the fully independent long-division oracle, 2²⁰ random pairs
    /// (the reference's 2²⁴ scaled for the CI wall clock; the reduction
    /// structure is shared by scalar and vector paths, so this cell is
    /// the deepest oracle for both).
    #[test]
    fn t_r36_mulmod_dual_path_fuzz() {
        let mut rng = Xs(0x5EED_C0DE_3636_0001);
        for _ in 0..(1 << 20) {
            let a = rng.next();
            let b = rng.next();
            let x = mulmod(a, b);
            let y = mulmod_ref(a, b);
            let z = mulmod_divmod_ref(a, b);
            assert_eq!(x, y, "CLMUL != schoolbook on ({a:#x}, {b:#x})");
            assert_eq!(y, z, "schoolbook != long-division oracle on ({a:#x}, {b:#x})");
        }
        // Edge operands.
        for &(a, b) in &[
            (0u64, 0u64),
            (0, 1),
            (1, 0),
            (u64::MAX, u64::MAX),
            (1 << 63, 1 << 63),
            (1, u64::MAX),
            (SPAN_QLOW, SPAN_QLOW),
        ] {
            assert_eq!(mulmod(a, b), mulmod_divmod_ref(a, b), "edge ({a}, {b})");
        }
    }

    /// Cell +9: the ord(x) > 2²¹ verification (Lemma 4's hypothesis) —
    /// `t_pow`'s init asserts it; this cell also spot-checks that the
    /// table itself is the exponent homomorphism on a coarse grid.
    #[test]
    fn t_r36_ord_and_power_table() {
        let tp = t_pow();
        assert_eq!(tp[0], 1);
        // The homomorphism law on a sparse grid (full coverage is the
        // init's own build loop).
        for k in [1usize, 2, 3, 63, 64, 100, 1000, 5000, 22016] {
            assert_eq!(tp[k], mulmod_pow_x(k as u64), "T_pow[{k}] != x^{k}");
        }
        // Distinctness on the reachable chunk range: x^a != x^b for
        // a ≠ b < MAXSPAN (the no-collision law the init guarantees).
        let mut seen = std::collections::HashSet::new();
        for k in 0..MAXSPAN {
            assert!(seen.insert(tp[k]), "x^{k} collides (ord(x) too small)");
        }
    }

    /// Helper: x^d by binary exponentiation (the table-free twin).
    fn mulmod_pow_x(d: u64) -> u64 {
        let mut result = 1u64;
        let mut base = 2u64; // x
        let mut e = d;
        while e > 0 {
            if e & 1 != 0 {
                result = mulmod(result, base);
            }
            base = mulmod(base, base);
            e >>= 1;
        }
        result
    }

    /// Cell +5: the token/chunk cross-invariant —
    /// `tok[L−1] == h_in ⊗ x^W ⊕ V` on random chunks (Theorem 10(b)
    /// vs 10(c)).
    #[test]
    fn t_r36_token_cross_invariant() {
        let mut rng = Xs(0x5EED_C0DE_3636_0002);
        for trial in 0..2000 {
            let n = if trial % 7 == 0 {
                512
            } else {
                1 + (rng.next() % 512) as usize
            };
            let t = random_tokens(&mut rng, n);
            let h_in = rng.next() >> 3;
            let (rec, h_out) = fold_chunk(&t, h_in);
            let mut tok = vec![0u64; n];
            fold_chunk_tokens(&t, h_in, &mut tok);
            assert_eq!(
                tok[n - 1],
                h_out,
                "token chain != closed form (trial {trial}, n {n})"
            );
            assert_eq!(
                tok[n - 1],
                mulmod(h_in, shift_chunk(rec.w as u64)) ^ rec.v,
                "tok[L-1] != h_in ⊗ x^W ⊕ V (trial {trial})"
            );
            // The sequential oracle agreement (dual-path at chunk scale).
            let (seq_rec, seq_h) = fold_chunk_sequential(&t, h_in);
            assert_eq!(
                rec.view(),
                seq_rec.view(),
                "closed form != sequential (trial {trial})"
            );
            assert_eq!(h_out, seq_h);
        }
    }

    /// Cell +1: THE TYPE-ORDER V ADVERSARY (Theorem 9's irreducibility
    /// witness) — identical type multisets and payload multisets,
    /// different order: the V coordinate MUST differ (a multiset-V
    /// implementation fails exactly here). Plus the 512-length variants.
    #[test]
    fn t_r36_type_order_adversary() {
        // {D, P} vs {P, D}: w_D = 19, w_P = 43. Digests 1 and x.
        let mk = |t0: u8, t1: u8| -> SpanTokens {
            let mut t = SpanTokens::with_len(2);
            t.ty[0] = t0;
            t.ty[1] = t1;
            t.vd[0] = 1; // v₁ = 1
            t.vd[1] = 2; // v₂ = x
            t
        };
        let (r_dp, _) = fold_chunk(&mk(5, 7), 0); // D then P
        let (r_pd, _) = fold_chunk(&mk(7, 5), 0); // P then D
        let (v_dp, v_pd) = (r_dp.v, r_pd.v);
        assert_ne!(
            v_dp, v_pd,
            "Theorem 9 adversary: V collapsed to a multiset function"
        );
        // The W coordinate (multiset-only) MUST agree.
        assert_eq!((r_dp.w, r_dp.db), (r_pd.w, r_pd.db));
        // The witness's exact values (Lemma 5's suffix form; the
        // reference's example constants are off by its own algebra —
        // the sequential oracle is the judge, and the DISTINCTNESS is
        // the theorem's content):
        // D,P:  V = 1·x^{43} ⊕ x·x^0 = x^{43} ⊕ x;
        // P,D:  V = 1·x^{19} ⊕ x·x^0 = x^{19} ⊕ x.
        let tp = t_pow();
        assert_eq!(v_dp, tp[43] ^ tp[1]);
        assert_eq!(v_pd, tp[19] ^ tp[1]);
        // The 512-length variant: two permutations of the same multiset.
        let mut rng = Xs(0x5EED_C0DE_3636_0003);
        let a: Vec<u8> = vec![5u8; 256].into_iter().chain(vec![7u8; 256]).collect();
        let mut b = a.clone();
        // Fisher-Yates with the cell PRNG.
        for i in (1..b.len()).rev() {
            let j = (rng.next() % (i as u64 + 1)) as usize;
            b.swap(i, j);
        }
        let mut ta = SpanTokens::with_len(512);
        let mut tb = SpanTokens::with_len(512);
        for i in 0..512 {
            ta.ty[i] = a[i];
            tb.ty[i] = b[i];
            let v = rng.next();
            ta.vd[i] = v;
            tb.vd[i] = v;
            let db = rng.next() as i64;
            ta.db[i] = db;
            tb.db[i] = db;
        }
        let (ra, _) = fold_chunk(&ta, 0);
        let (rb, _) = fold_chunk(&tb, 0);
        let (va, vb) = (ra.v, rb.v);
        assert_ne!(va, vb, "512-length type-order adversary collapsed");
        assert_eq!((ra.w, ra.db, ra.cnt), (rb.w, rb.db, rb.cnt));
    }

    /// Cell +4: all-one-type chunks — 512×'P' (max width 43) and
    /// 512×'D' (min width 19): the histogram extremes, table bounds,
    /// and the exponent edges.
    #[test]
    fn t_r36_all_one_type_extremes() {
        for (ty, wt) in [(7u8, 43u64), (5u8, 19u64), (2u8, 31u64)] {
            let n = 512;
            let mut t = SpanTokens::with_len(n);
            for i in 0..n {
                t.ty[i] = ty;
                t.vd[i] = ((i as u64) + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                t.setp[i] = if i == n - 1 { 1 } else { 0 };
                t.px[i] = (i * 37) as u32;
            }
            let (rec, h) = fold_chunk(&t, 0x1234_5678_9ABC_DEF0);
            let (rw, rcnt, rpx, rflg) = (rec.w, rec.cnt, rec.px, rec.flg);
            assert_eq!(rw as u64, wt * 512, "span of 512×type{ty}");
            assert_eq!(rcnt, 512);
            let (seq, seq_h) = fold_chunk_sequential(&t, 0x1234_5678_9ABC_DEF0);
            assert_eq!(rec.view(), seq.view());
            assert_eq!(h, seq_h);
            assert_eq!(rpx, (511 * 37) as u32);
            assert!(rflg & FLG_SETTER != 0);
            // The max-width chunk's degree is exactly MAXSPAN − 1.
            if ty == 7 {
                assert_eq!(rw as usize, MAXSPAN - 1);
            }
        }
    }

    /// Cell +3: empty segments — the merge's deg-0 edge, `T_pow[0]=1`,
    /// and the identity laws on both record levels.
    #[test]
    fn t_r36_empty_segments_and_identity() {
        let empty = SpanTokens::default();
        let (rec, h) = fold_chunk(&empty, 0xDEAD_BEEF);
        assert_eq!(rec.view(), R36::IDENTITY.view());
        assert_eq!(h, 0xDEAD_BEEF);
        // Identity compositions.
        let (one, _) = {
            let mut t = SpanTokens::with_len(1);
            t.ty[0] = 0;
            t.vd[0] = 7;
            fold_chunk(&t, 0)
        };
        let id = R36::IDENTITY;
        assert_eq!((r36_compose(&id, &one).v, r36_compose(&one, &id).v), (one.v, one.v));
        let p_one = P64::from_r36(&one);
        assert_eq!(p64_compose(&P64::IDENTITY, &p_one).v, p_one.v);
        assert_eq!(p64_compose(&p_one, &P64::IDENTITY).v, p_one.v);
        // The hash identity: h ⊗ 1 ⊕ 0 = h.
        assert_eq!(hash_merge(0xABCD, 0, 0), 0xABCD);
    }

    /// Cell +7: R36/P64 associativity — `(a⨂b)⨂c == a⨂(b⨂c)` on
    /// random triples, and the r36/p64 agreement at chunk-level degrees
    /// (the level-hop law: the two algebras are the same monoid below
    /// the u16 escape).
    #[test]
    fn t_r36_compose_associativity() {
        let mut rng = Xs(0x5EED_C0DE_3636_0004);
        for trial in 0..5000 {
            let a = random_record(&mut rng);
            let b = random_record(&mut rng);
            let c = random_record(&mut rng);
            let ab = r36_compose(&a, &b);
            let ab_c = r36_compose(&ab, &c);
            let bc = r36_compose(&b, &c);
            let a_bc = r36_compose(&a, &bc);
            assert_eq!(ab_c.view(), a_bc.view(), "R36 associativity (trial {trial})");
            // P64 twins.
            let (pa, pb, pc) = (P64::from_r36(&a), P64::from_r36(&b), P64::from_r36(&c));
            let l = p64_compose(&p64_compose(&pa, &pb), &pc);
            let r = p64_compose(&pa, &p64_compose(&pb, &pc));
            assert_eq!(l, r, "P64 associativity (trial {trial})");
            // The level-hop agreement (chunk-level degrees only).
            if (a.w as u64) + (b.w as u64) + (c.w as u64) < u16::MAX as u64 {
                let lifted = p64_compose(&p64_compose(&pa, &pb), &pc);
                let direct = r36_compose(&r36_compose(&a, &b), &c);
                assert_eq!((lifted.v, lifted.c), (direct.v, shift_chunk(direct.w as u64)));
            }
        }
    }

    /// Cell +8: the degree-overflow escape — composing ≥ 3,000 max-span
    /// chunks exceeds 2¹⁶; the P64 path must hold the degree exactly in
    /// u32 while R36's u16 wraps (the FLG_ESCAPE discipline: chunk
    /// records never compose past the envelope; the accumulator does).
    #[test]
    fn t_r36_degree_overflow_escape() {
        let mut rng = Xs(0x5EED_C0DE_3636_0005);
        let n_chunks = 3000usize;
        let total_deg: u64 = n_chunks as u64 * 22_016;
        assert!(total_deg > u16::MAX as u64, "the corpus must overflow u16");
        let mut acc = P64::IDENTITY;
        let mut h = 0xFEED_FACEu64; // the sequential oracle's chain
        for k in 0..n_chunks {
            let r = random_max_record(&mut rng, k);
            h = hash_apply(h, &r); // apply record k, one at a time
            acc = p64_compose(&acc, &P64::from_r36(&r));
        }
        // The accumulator's degree is exact.
        assert_eq!(acc.deg as u64, total_deg);
        // ...and its C really is x^{total_deg} (the exponent law over
        // 3,000 compositions — the two-level scheme's whole point).
        assert_eq!(acc.c, mulmod_pow_x(total_deg));
        // The hash application law: h_start ⊗ x^{deg} ⊕ V == the
        // sequential per-record walk.
        let h_start = 0xFEED_FACEu64;
        let h_end = mulmod(h_start, acc.c) ^ acc.v;
        assert_eq!(h_end, h, "the 3000-chunk composition != the sequential walk");
    }

    /// Cells +2 +16: the split-degree sweep + merge-tree shapes —
    /// every split point of random tapes, 2–5 workers, balanced and
    /// skewed trees: `hash_merge` composition == the sequential fold,
    /// bit-exact (Theorem 12 + §4.6's unification).
    #[test]
    fn t_r36_split_sweep_and_merge_trees() {
        let mut rng = Xs(0x5EED_C0DE_3636_0006);
        // A 64-message window around each power-of-2 boundary, every
        // split point (cell +2's shape).
        for &center in &[64usize, 128, 256, 512, 1024] {
            for delta in 0..64 {
                let n = center + delta;
                let t = random_tokens(&mut rng, n);
                let (_, h_seq) = fold_stream_sequential(&t, 0);
                for split in [1, n / 4, n / 2, 3 * n / 4, n - 1] {
                    let (early, late) = split_tokens(&t, split);
                    let (_, h_e) = fold_stream_sequential(&early, 0);
                    // The later worker folds from zero: its partial hash
                    // value is just its V (§4.1's identity).
                    let (rec_l, _) = fold_stream(&late, 0);
                    let merged = hash_merge(h_e, rec_l.v, rec_l.deg as u64);
                    assert_eq!(
                        merged, h_seq,
                        "split {split} of {n} (center {center}): merged != sequential"
                    );
                    // The WRONG convention (shift by the EARLY span)
                    // must mismatch on heterogeneous tapes — the trap
                    // detector (Appendix D item 1). It CAN accidentally
                    // match on degenerate draws; the corpus-level check
                    // below is the binding one.
                    let wrong = hash_merge(rec_l.v, h_e, early_span(&early));
                    let _ = wrong;
                }
            }
        }
        // Corpus-level trap detector: over 512 random heterogeneous
        // tapes the wrong-convention merge must mismatch (else the
        // sweep above proved nothing).
        let mut trap_fired = false;
        for _trial in 0..512 {
            let n = 512;
            let t = random_tokens(&mut rng, n);
            let (_, h_seq) = fold_stream_sequential(&t, 0);
            let (early, late) = split_tokens(&t, n / 2);
            let (_, h_e) = fold_stream_sequential(&early, 0);
            let (rec_l, _) = fold_stream(&late, 0);
            let wrong = hash_merge(rec_l.v, h_e, early_span(&early));
            if wrong != h_seq {
                trap_fired = true;
            }
        }
        assert!(trap_fired, "the deg-convention trap never fired — the corpus is degenerate");
        // Merge trees: 2,3,4,5 workers, balanced and skewed (cell +16).
        for workers in 2usize..=5 {
            let n = 512 * workers;
            let t = random_tokens(&mut rng, n);
            let (_, h_seq) = fold_stream_sequential(&t, 0);
            // Balanced: equal segments.
            let seg = n / workers;
            let mut hashes: Vec<(u64, u64)> = Vec::new(); // (H_i, W_i)
            for w in 0..workers {
                let (a, b) = (w * seg, (w + 1) * seg);
                let part = slice_tokens(&t, a, b);
                let (rec, _) = fold_stream(&part, 0);
                hashes.push((rec.v, rec.deg as u64));
            }
            // Left-fold merge (the skewed tree).
            let mut acc = hashes[0].0;
            for &(hv, hw) in &hashes[1..] {
                acc = hash_merge(acc, hv, hw);
            }
            assert_eq!(acc, h_seq, "{workers}-worker left-fold merge");
            // Balanced binary tree.
            let mut level: Vec<u64> = hashes.iter().map(|&(h, _)| h).collect();
            let mut spans: Vec<u64> = hashes.iter().map(|&(_, w)| w).collect();
            while level.len() > 1 {
                let mut nl = Vec::new();
                let mut ns = Vec::new();
                let mut i = 0;
                while i + 1 < level.len() {
                    // merge(i, i+1): the merged span = w_i + w_{i+1}.
                    let m = hash_merge(level[i], level[i + 1], spans[i + 1]);
                    nl.push(m);
                    ns.push(spans[i] + spans[i + 1]);
                    i += 2;
                }
                if i < level.len() {
                    nl.push(level[i]);
                    ns.push(spans[i]);
                }
                level = nl;
                spans = ns;
            }
            assert_eq!(level[0], h_seq, "{workers}-worker balanced-tree merge");
        }
    }

    /// Cell +10: the partition sweep — random tapes × random partitions
    /// (granularity 1..4096, random offsets) vs the sequential fold:
    /// the whole-stream composite is partition-invariant (Theorem 12).
    #[test]
    fn t_r36_partition_invariance_sweep() {
        let mut rng = Xs(0x5EED_C0DE_3636_0007);
        // Debug-scaled (the CI arm runs release at full weight; the
        // sweep is the cell with the heaviest per-trial mulmod count).
        #[cfg(debug_assertions)]
        let trials = 48;
        #[cfg(not(debug_assertions))]
        let trials = 300;
        for trial in 0..trials {
            let n = 1 + (rng.next() % 3000) as usize;
            let t = random_tokens(&mut rng, n);
            let (whole, h_seq) = fold_stream(&t, 0x77AA_55AA);
            let (seq_whole, _) = fold_stream_sequential(&t, 0x77AA_55AA);
            assert_eq!(whole, seq_whole, "whole stream dual-path (trial {trial})");
            assert_eq!(h_seq, mulmod(0x77AA_55AA, whole.c) ^ whole.v);
            for &gran in &[1usize, 3, 16, 64, 256, 512, 1000, 4096] {
                let off = (rng.next() % 97) as usize;
                // Compose from the offset boundary (the random alignment):
                // segments of ANY size (fold_stream handles the ≤ 512 law
                // internally), the P64 chain in tape order.
                let mut acc = P64::IDENTITY;
                let head = slice_tokens(&t, 0, off.min(n));
                let (head_rec, _) = fold_stream(&head, 0);
                acc = p64_compose(&acc, &head_rec);
                let body_start = off.min(n);
                let mut pos = body_start;
                while pos < n {
                    let end = (pos + gran).min(n);
                    let part = slice_tokens(&t, pos, end);
                    let (rec, _) = fold_stream(&part, 0);
                    acc = p64_compose(&acc, &rec);
                    // Cross-check this segment against the sequential
                    // oracle (dual-path per segment).
                    let (seq_rec, _) = fold_stream_sequential(&part, 0);
                    assert_eq!(
                        rec, seq_rec,
                        "segment dual-path (trial {trial}, gran {gran})"
                    );
                    pos = end;
                }
                // The composed record must equal the whole-stream fold.
                assert_eq!(acc.v, whole.v, "V partition invariance (trial {trial}, gran {gran}, off {off})");
                assert_eq!(acc.deg, whole.deg, "degree law (trial {trial}, gran {gran})");
                assert_eq!(acc.db, whole.db);
                assert_eq!(acc.da, whole.da);
                assert_eq!(acc.cnt, whole.cnt);
                assert_eq!(acc.c, whole.c);
                assert_eq!(acc.px, whole.px);
                assert_eq!(acc.tag, whole.tag);
                // The hash application: composed == sequential at the
                // top level (Theorem 12's accumulator case).
                let h_composed = mulmod(0x77AA_55AA, acc.c) ^ acc.v;
                assert_eq!(h_composed, h_seq, "hash partition invariance (trial {trial}, gran {gran})");
            }
        }
    }

    /// Cell +17: the discipline mix — per-message (`w_t ≡ 1`) vs span
    /// (`w_t` = wire bytes) disciplines on the same tape: both must
    /// match their own sequential references (a `deg = count` path
    /// under the span discipline is the Appendix D item 2 bug; this
    /// cell runs the per-message algebra as its OWN consistent law).
    #[test]
    fn t_r36_discipline_mix() {
        let mut rng = Xs(0x5EED_C0DE_3636_0008);
        let n = 777;
        let t = random_tokens(&mut rng, n);
        // Span discipline (the module's law): the stream fold vs its
        // own sequential oracle — dual-path at stream scale.
        let (span_rec, h_span) = fold_stream(&t, 0);
        let (span_seq, h_seq) = fold_stream_sequential(&t, 0);
        assert_eq!(span_rec, span_seq, "stream dual-path (discipline cell)");
        assert_eq!(h_span, h_seq);
        assert_ne!(h_span, 0);
        // Per-message discipline: w_t = 1 for all messages. The degree
        // is the count; the algebra is the same monoid with a different
        // width table. Implemented here as the tape's own consistent
        // reference (an independent 2-op chain, h ← h⊗x ⊕ v).
        let mut h_msg: u64 = 0;
        for i in 0..n {
            h_msg = mulmod(h_msg, 2) ^ t.vd[i];
        }
        // The per-message discipline's whole-stream record: V = ⊕ vᵢ x^{n−1−i}.
        let tp = t_pow();
        let mut v_msg: u64 = 0;
        for i in 0..n {
            v_msg ^= mulmod(t.vd[i], tp[n - 1 - i]);
        }
        assert_eq!(h_msg, v_msg, "per-message discipline: closed form != chain");
        // And the two disciplines genuinely differ on a heterogeneous
        // tape (the count-is-not-span trap).
        assert_ne!(
            span_rec.deg as u64,
            n as u64,
            "span == count on a heterogeneous tape (degenerate corpus?)"
        );
    }

    /// Cell (kernel slice of +12): the ring-sizing law's kernel face —
    /// `shift_pow` (the two-level table) vs binary exponentiation over
    /// the full u32 range: the degree representation never collides and
    /// the two-level lookup is exact.
    #[test]
    fn t_r36_shift_pow_two_level() {
        let mut rng = Xs(0x5EED_C0DE_3636_0009);
        // Boundary degrees + random draws across the u32 range.
        let mut degrees: Vec<u64> = vec![
            0, 1, 2, 0xFFFF, 0x10000, 0x10001, 0xFFFFF, 0x100000, (1 << 21) - 1, 1 << 21,
            0x7FFFFFFF, 0xFFFF_FFFF,
        ];
        for _ in 0..200 {
            degrees.push(rng.next() & 0xFFFF_FFFF); // masked to the u32 domain
        }
        for &d in &degrees {
            let a = shift_pow(d);
            let b = mulmod_pow_x(d);
            assert_eq!(a, b, "two-level x^{d} != binary exponentiation (d={d})");
        }
        // The merge at worker scale (Theorem 17's regime): a 17.5M-degree
        // late span — the 500k-message pass class.
        let d_late: u64 = 17_500_000;
        let h_e = 0xA5A5_5A5A_DEAD_0BEE;
        let h_l = 0x1234_5678_9ABC_DEF0;
        let merged = hash_merge(h_e, h_l, d_late);
        assert_eq!(
            merged,
            mulmod(h_e, mulmod_pow_x(d_late)) ^ h_l,
            "worker-scale merge"
        );
    }

    /// The 500k-message pass end-to-end: 977 chunks (976·512 + 288),
    /// the fresh fold == the composed records == the sequential
    /// reference, and the memoized replay's O(1) law (the §6.2 regime
    /// B shape — the instrument prices it; this cell pins its
    /// correctness).
    #[test]
    fn t_r36_500k_pass_compose_law() {
        let n = 500_000;
        let mut rng = Xs(0x5EED_C0DE_3636_000A);
        let t = random_tokens(&mut rng, n);
        let recs = chunk_records(&t, CHUNK);
        assert_eq!(recs.len(), (n + CHUNK - 1) / CHUNK);
        // Compose in tape order (the memoized replay).
        let composed = compose_records(&recs);
        // The fresh whole-stream fold (the two-level P64 path).
        let (whole, _) = fold_stream(&t, 0);
        assert_eq!(composed.v, whole.v, "977-record composition != fresh fold");
        assert_eq!(composed.deg, whole.deg);
        assert_eq!(composed.db, whole.db);
        assert_eq!(composed.da, whole.da);
        assert_eq!(composed.c, whole.c);
        assert_eq!(composed.px, whole.px);
        assert_eq!(composed.tag, whole.tag);
        assert_eq!(composed.cnt, whole.cnt);
        assert_eq!(composed.cnt as u64, n as u64);
        // The sequential oracle over the whole 500k (the honest judge;
        // ~1M mulmods).
        let (seq, _) = fold_stream_sequential(&t, 0);
        assert_eq!(whole, seq, "500k fresh fold != sequential oracle");
        // The two-worker split at the midpoint (the R36-C merge at the
        // 500k scale).
        let (early, late) = split_tokens(&t, n / 2);
        let (_, h_e) = fold_stream_sequential(&early, 0);
        let (rec_l, _) = fold_stream(&late, 0);
        let merged = hash_merge(h_e, rec_l.v, rec_l.deg as u64);
        let (_, h_seq) = fold_stream_sequential(&t, 0);
        assert_eq!(merged, h_seq, "the 500k two-worker merge");
    }

    // ── helpers ──

    fn random_tokens(rng: &mut Xs, n: usize) -> SpanTokens {
        let mut t = SpanTokens::with_len(n);
        for i in 0..n {
            // The ITCH-ish type mix + adversarial extremes.
            t.ty[i] = match rng.next() % 10 {
                0..=2 => 0,          // A (36)
                3..=4 => 1,          // F (40)
                5 => 2,              // E (31)
                6 => 3,              // C (36)
                7 => 4,              // X (23)
                8 => 5,              // D (19)
                _ => (rng.next() % 10) as u8, // adversarial mix (incl. 9 = identity)
            };
            t.vd[i] = rng.next();
            t.db[i] = (rng.next() >> 1) as i64 - (1 << 31);
            t.da[i] = (rng.next() >> 1) as i64 - (1 << 31);
            t.setp[i] = if rng.next() % 4 == 0 { 1 } else { 0 };
            t.px[i] = (rng.next() >> 32) as u32;
        }
        t
    }

    fn split_tokens(t: &SpanTokens, at: usize) -> (SpanTokens, SpanTokens) {
        let n = t.len();
        let at = at.min(n);
        (
            slice_tokens(t, 0, at),
            slice_tokens(t, at, n),
        )
    }

    fn slice_tokens(t: &SpanTokens, a: usize, b: usize) -> SpanTokens {
        SpanTokens {
            ty: t.ty[a..b].to_vec(),
            vd: t.vd[a..b].to_vec(),
            db: t.db[a..b].to_vec(),
            da: t.da[a..b].to_vec(),
            px: t.px[a..b].to_vec(),
            setp: t.setp[a..b].to_vec(),
        }
    }

    fn early_span(t: &SpanTokens) -> u64 {
        t.ty.iter().map(|&ty| W_T[ty as usize]).sum()
    }

    fn random_record(rng: &mut Xs) -> R36 {
        let n = 1 + (rng.next() % 64) as usize;
        let t = random_tokens(rng, n);
        let (rec, _) = fold_chunk(&t, 0);
        rec
    }

    fn random_max_record(rng: &mut Xs, salt: usize) -> R36 {
        // A near-max-span chunk: 512 × 'P' with random digests.
        let mut t = SpanTokens::with_len(CHUNK);
        for i in 0..CHUNK {
            t.ty[i] = 7;
            t.vd[i] = rng.next().wrapping_add(salt as u64);
        }
        let (rec, _) = fold_chunk(&t, 0);
        assert_eq!(rec.w as usize, MAXSPAN - 1);
        rec
    }

    // ═══════════════════════════════════════════════════════════════
    // R37 cells (solution2.md §6.2's classes +19/+20/+34, kernel form)
    // ═══════════════════════════════════════════════════════════════

    /// Helper: a random P64 stream (the tree's leaves — fused-call
    /// records with u32-range degrees, setters, and flags).
    fn random_p64s(rng: &mut Xs, k: usize) -> Vec<P64> {
        (0..k).map(|_| P64::from_r36(&random_record(rng))).collect()
    }

    /// Helper: the serial left chain (the R36 fold's bracketing — the
    /// oracle every tree shape must equal, Theorem 22).
    fn serial_chain(recs: &[P64]) -> P64 {
        recs.iter()
            .fold(P64::IDENTITY, |acc, r| p64_compose(&acc, r))
    }

    /// Cell +7-extend (checklist 2): P64 associativity fuzz on random
    /// TRIPLES — `(a⨂b)⨂c == a⨂(b⨂c)` — with the derived-field cases
    /// of Appendix A.1 (the 8 setter patterns, tag re-basing, degree
    /// accumulation, flag OR). The 3-factor law is the license every
    /// bracketing (serial, tree, two-level, random shape) spends.
    #[test]
    fn t_r37_p64_associativity_triples() {
        let mut rng = Xs(0x5EED_C0DE_3737_0001);
        #[cfg(debug_assertions)]
        let trials = 4_096;
        #[cfg(not(debug_assertions))]
        let trials = 65_536;
        for _ in 0..trials {
            let a = P64::from_r36(&random_record(&mut rng));
            let b = P64::from_r36(&random_record(&mut rng));
            let c = P64::from_r36(&random_record(&mut rng));
            let ab = p64_compose(&a, &b);
            let bc = p64_compose(&b, &c);
            let left = p64_compose(&ab, &c);
            let right = p64_compose(&a, &bc);
            assert_eq!(left, right, "P64 3-factor law failed");
            // The derived-field probes (Appendix A.1's eight rows ride
            // the random setter patterns; the explicit degenerate
            // setters below pin the empty/edge rows).
            assert_eq!(left.deg, a.deg.wrapping_add(b.deg).wrapping_add(c.deg));
        }
        // The degenerate setter patterns (rows 000/111 and the
        // identity factors).
        for &sa in &[0u16, FLG_SETTER] {
            for &sb in &[0u16, FLG_SETTER] {
                for &sc in &[0u16, FLG_SETTER] {
                    let mk = |flg: u16, salt: u64| P64 {
                        v: salt.wrapping_mul(0x9E37),
                        c: salt | 1,
                        db: salt as i64,
                        da: -(salt as i64),
                        px: (salt >> 32) as u32,
                        tag: (salt & 0xFFFF) as u32,
                        deg: (salt % 22_016) as u32,
                        cnt: (salt % 512) as u32,
                        flg,
                    };
                    let a = mk(sa, 0x1111_2222_3333_4444);
                    let b = mk(sb, 0x5555_6666_7777_8888);
                    let c = mk(sc, 0x9999_AAAA_BBBB_CCCC);
                    assert_eq!(
                        p64_compose(&p64_compose(&a, &b), &c),
                        p64_compose(&a, &p64_compose(&b, &c)),
                        "setter pattern ({sa},{sb},{sc})"
                    );
                }
            }
        }
    }

    /// Cell +19 (checklist 3): the tree-shape fuzz — tree vs serial on
    /// random streams, K ∈ {2..1024} incl. non-powers-of-2 and odd
    /// tails (the champion's K = 322 explicitly), PLUS random tree
    /// SHAPES (random balanced-ish bracketings — Theorem 19's shape
    /// freedom at kernel level) and the vector-scheduled tree
    /// (Theorem 21's `mulmod4` form, dual-path).
    #[test]
    fn t_r37_tree_vs_serial_shape_fuzz() {
        let mut rng = Xs(0x5EED_C0DE_3737_0002);
        // The explicit K sweep (the odd/adversarial sizes).
        for &k in &[
            2usize, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 63, 64, 100, 127, 128,
            129, 161, 200, 255, 256, 257, 321, 322, 323, 511, 512, 513,
            640, 999, 1023, 1024,
        ] {
            let recs = random_p64s(&mut rng, k);
            let want = serial_chain(&recs);
            // The scalar up-sweep.
            let mut r1 = recs.clone();
            let got1 = tree_fold_p64(&mut r1);
            assert_eq!(got1, want, "scalar tree != serial (K={k})");
            // The dispatching tree (vector on AVX-512 silicon, scalar
            // otherwise — dual-path equality either way).
            let mut r2 = recs.clone();
            let got2 = tree_fold_p64_dispatch(&mut r2);
            assert_eq!(got2, want, "dispatch tree != serial (K={k})");
            // A random tree SHAPE (recursive random split points).
            let got3 = fold_random_shape(&recs, &mut rng);
            assert_eq!(got3, want, "random shape != serial (K={k})");
        }
        // The random-K fuzz (10⁴ streams release-scaled; the CI wall
        // clock discipline from t_r36_mulmod_dual_path_fuzz).
        #[cfg(debug_assertions)]
        let trials = 256;
        #[cfg(not(debug_assertions))]
        let trials = 10_000;
        for _ in 0..trials {
            let k = 2 + (rng.next() % 1023) as usize;
            let recs = random_p64s(&mut rng, k);
            let want = serial_chain(&recs);
            let mut r1 = recs.clone();
            assert_eq!(tree_fold_p64(&mut r1), want, "scalar tree (rand K={k})");
            let mut r2 = recs.clone();
            assert_eq!(
                tree_fold_p64_dispatch(&mut r2),
                want,
                "dispatch tree (rand K={k})"
            );
            assert_eq!(
                fold_random_shape(&recs, &mut rng),
                want,
                "random shape (rand K={k})"
            );
        }
    }

    /// The random-shape reference: a recursive fold splitting at a
    /// RANDOM midpoint — every bracketing of the tape-ordered leaves
    /// (Theorem 19's shape freedom; the scalar chain, the up-sweep,
    /// and this must all agree).
    fn fold_random_shape(recs: &[P64], rng: &mut Xs) -> P64 {
        fn go(recs: &[P64], rng: &mut Xs) -> P64 {
            if recs.len() <= 1 {
                return recs.first().copied().unwrap_or(P64::IDENTITY);
            }
            // A random split in [1, len-1] — never degenerate.
            let mid = 1 + (rng.next() % (recs.len() as u64 - 1)) as usize;
            let left = go(&recs[..mid], rng);
            let right = go(&recs[mid..], rng);
            p64_compose(&left, &right)
        }
        go(recs, rng)
    }

    /// Cell +21's static form (checklist 3): the vector tree's bit
    /// equality vs the scalar tree — exercised on AVX-512 silicon
    /// (the dev VM + the 9V45 fleet); a no-op pass on scalar runners
    /// that still pins the dispatch law (dispatch == scalar).
    #[test]
    fn t_r37_tree_vector_dual_path() {
        let mut rng = Xs(0x5EED_C0DE_3737_0003);
        for _ in 0..64 {
            // Stride patterns that hit the vector batch boundaries:
            // the 4-pair batches need i + 7·s < K — sizes straddling
            // every multiple-of-8 boundary.
            let k = 8 + (rng.next() % 600) as usize;
            let recs = random_p64s(&mut rng, k);
            let want = serial_chain(&recs);
            let mut r1 = recs.clone();
            let scalar = tree_fold_p64(&mut r1);
            let mut r2 = recs.clone();
            let dispatch = tree_fold_p64_dispatch(&mut r2);
            assert_eq!(scalar, want, "scalar (K={k})");
            assert_eq!(dispatch, want, "dispatch (K={k})");
            #[cfg(target_arch = "x86_64")]
            {
                if std::arch::is_x86_feature_detected!("avx512f")
                    && std::arch::is_x86_feature_detected!("vpclmulqdq")
                {
                    let mut r3 = recs.clone();
                    // SAFETY: both classes detected at runtime above.
                    let vector = unsafe { tree_fold_p64_avx512(&mut r3) };
                    assert_eq!(vector, want, "vector tree (K={k}) — the mulmod4 schedule diverged");
                }
            }
        }
    }

    /// Cell +20 (checklist 4): the two-level tree — 5,122-record
    /// passes (the maximal variant: one record per chunk): flat P64
    /// chain vs the two-level split (512-runs → sub-composites →
    /// tree) — bit-equal; the degree escape past 2¹⁶ rides the same
    /// stream (u32 at every level ≥ 2 — the [S1'] pitfall 7 fence).
    #[test]
    fn t_r37_two_level_tree_and_degree_escape() {
        let mut rng = Xs(0x5EED_C0DE_3737_0004);
        // The maximal pass: 5,122 chunk records (10 × 512 + 2).
        for &n in &[5_122usize, 513, 1_024, 97] {
            let r36s: Vec<R36> = (0..n).map(|_| random_record(&mut rng)).collect();
            // The flat chain (the P64 journal form).
            let flat: Vec<P64> = r36s.iter().map(P64::from_r36).collect();
            let want = serial_chain(&flat);
            // The two-level split at the blueprint's run length and at
            // adversarial run lengths (1, 3, 64, 512, 513).
            for &run in &[1usize, 3, 64, 128, 512, 513] {
                let got = tree_fold_two_level(&r36s, run);
                assert_eq!(got, want, "two-level (n={n}, run={run})");
            }
            // Degree escape: with near-max-span records the composite
            // degree crosses 2¹⁶ early — the P64 path's u32 domain.
            // (n × ~500 average spans: n=97 crosses at ~record 131 for
            // max records; the max-record stream below crosses at 3.)
            let maxes: Vec<R36> =
                (0..n.min(16)).map(|i| random_max_record(&mut rng, i)).collect();
            let flat_max: Vec<P64> = maxes.iter().map(P64::from_r36).collect();
            let want_max = serial_chain(&flat_max);
            assert!(
                want_max.deg > 0xFFFF || n < 3,
                "the max-record stream should exercise the >2^16 degree domain (deg={})",
                want_max.deg
            );
            for &run in &[1usize, 2, 512] {
                let got = tree_fold_two_level(&maxes, run);
                assert_eq!(got, want_max, "two-level max records (run={run})");
            }
        }
    }

    /// Cell +34 (checklist 6, kernel form): the epoch composition
    /// identity — `S ⨂ X_first ⨂ X_second` (the epoch merge, Theorem
    /// 25) == one lane's fold of the same epoch tape == the serial
    /// chain, on random record streams and random splits (including
    /// empty blocks and single-record blocks — the degenerate
    /// assignments).
    #[test]
    fn t_r37_epoch_merge_identity() {
        let mut rng = Xs(0x5EED_C0DE_3737_0005);
        #[cfg(debug_assertions)]
        let trials = 512;
        #[cfg(not(debug_assertions))]
        let trials = 4_096;
        for trial in 0..trials {
            let k = (rng.next() % 600) as usize;
            let recs = random_p64s(&mut rng, k);
            let s = P64::from_r36(&random_record(&mut rng)); // the pre-epoch state
            let want = serial_chain(&[vec![s], recs.clone()].concat());
            // A random lane split (block-split assignment): lane 0
            // takes the earlier block, lane 1 the later — FIXED tape
            // order (Theorem 23(b)).
            let split = (rng.next() % (k as u64 + 1)) as usize;
            let (x_first, x_second) = if split == 0 {
                (P64::IDENTITY, serial_chain(&recs))
            } else if split == k {
                (serial_chain(&recs), P64::IDENTITY)
            } else {
                (serial_chain(&recs[..split]), serial_chain(&recs[split..]))
            };
            let got = epoch_merge_p64(&s, &x_first, &x_second);
            assert_eq!(got, want, "epoch merge identity (trial {trial}, split {split})");
            // The hash surface: h' = h ⊗ C ⊕ V on both paths.
            let h = rng.next() >> 3;
            assert_eq!(
                hash_apply_p64(h, &got),
                hash_apply_p64(h, &want),
                "epoch hash surface (trial {trial})"
            );
        }
    }
}
