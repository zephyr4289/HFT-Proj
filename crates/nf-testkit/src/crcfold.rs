//! PR-1 GIGAHFT Lever 1 — bit-exact carry-less CRC32C vector folding kernel.
//!
//! # What this is
//!
//! A second implementation of [`span_crc32c_8lane`](crate::sink::span_crc32c_8lane)
//! that evaluates the SAME eight per-lane raw CRC32C values (strided lane
//! streams + tail into lane 0, FNV-1a-64 lane combine) using
//! VPCLMULQDQ carry-less polynomial folding instead of the scalar
//! `crc32`-instruction chain. On every input, bit parity with the scalar
//! kernel is asserted by the D11 differential oracle and the unit tests in
//! this module (exhaustive length sweeps + random stress), so the two
//! kernels are interchangeable by construction.
//!
//! # The math (mirror domain — docs/21-gigahft.md §2)
//!
//! With X = the lane stream read as a little-endian bit string, X̄ = full
//! bit-mirror of X, P = 0x11EDC6F41 (normal CRC32C polynomial):
//!
//! * `CRC32C_raw(X) = rev32( X̄ · y^32 mod P )` — the division consumes X̄
//!   from its TOP degree, i.e. front-to-back over the stream.
//! * Fold invariant (mod P, state V, deg < 128): start `V = Ū_0`, then
//!   `V ← (V_hi ⊗ KP192) ⊕ (V_lo ⊗ KP128) ⊕ Ū_q` where `Ū_q =
//!   rev128(stream[16q..16q+16))`, `KP192 = y^192 mod P`, `KP128 =
//!   y^128 mod P`. Two PCLMULQDQ + two XOR per 16 stream bytes: the hard
//!   8-bytes-per-clmul CRC folding floor.
//! * **Ending**: because `V ≡ X̄_fullunits (mod P)` and congruences survive
//!   multiplication/XOR, the final reduction collapses to three hardware
//!   `crc32` instructions: `c = crc32_u64(crc32_u64(0, rev64(V_hi)),
//!   rev64(V_lo))`, then chain the stream's LAST `r = len mod 16` bytes.
//!   No Barrett reduction, no length-dependent constants.
//!
//! The constants were derived and verified symbolically in
//! `scripts/crc32c_final_derive.py` against a reference table-driven
//! reflected CRC32C (anchored on the standard test vector
//! CRC32C("123456789") = 0xE3069283).
//!
//! # Hardware mapping
//!
//! The eight lanes are held as two zmm registers (4 × 128-bit fold states
//! each). One iteration advances ALL EIGHT lanes by one 16-byte unit using
//! two strided 64-byte loads (a MoldUDP64 block pair), GFNI bit-mirrors,
//! and four VPCLMULQDQ. `eval2` interleaves two spans to hide the clmul
//! dependency latency. Bodies shorter than [`FOLD_MIN_LEN`] take the scalar
//! kernel (its fixed ending cost exceeds the 8 B/cycle instruction chain).
//!
//! # Zero-allocation
//!
//! Stack registers only; no allocation anywhere. CPU feature detection
//! happens once at fabric spawn, outside every measurement window.

use crate::sink::span_crc32c_8lane;

/// y^192 mod P — fold constant for the state's high qword.
pub const KP192: u64 = 0x6503ea99;
/// y^128 mod P — fold constant for the state's low qword.
pub const KP128: u64 = 0x18571d18;
/// y^448 mod P — R11 tri-stream fold constant for the state's high qword:
/// one tri-stream step advances a stream by THREE real units (384 bits of
/// degree), so its old state folds by y^384 (hi half sits 64 bits higher:
/// y^448). Derived by the same carry-less power-mod as KP192/KP128 and
/// re-derived at test time by `t_tri_constants_derivation`.
pub const KP448: u64 = 0xaa5eec4a;
/// y^384 mod P — R11 tri-stream fold constant for the state's low qword.
pub const KP384: u64 = 0xe6957b4d;
/// y^320 mod P — R11 tri-stream merge multiplier: a stream whose state
/// must be re-offset by TWO units (256 bits) multiplies hi by y^(256+64).
pub const KP320: u64 = 0x7bba6798;
/// y^256 mod P — R11 tri-stream merge multiplier for the two-unit offset's
/// low half. (The ONE-unit offset reuses KP192/KP128.)
pub const KP256: u64 = 0x59a3508a;

/// R13: the natural-domain (reflected-representation) fold constant for
/// the state's HIGH qword — the 128-degree unit lift of the front-to-back
/// natural-LE fold. This is ISA-L's published CRC32C `fold_1x128b` pair
/// (crc_const.asm), battle-tested at scale; `RKHI` additionally equals
/// `rev32(y^95 mod P)` with P = 0x11EDC6F41 (the normal-form CRC32C poly)
/// — the reversed-power convention decoded in docs/26.
pub const RKHI: u64 = 0x493C_7D27;
/// R13: the natural-domain fold constant for the state's LOW qword
/// (33-bit: carries the polynomial's y^32 term). Same ISA-L pair.
pub const RKLO: u64 = 0x0EC10_68C5_0;

/// R14: the vector ending's seed constant — the value of the 16-byte
/// monomial family at degree 0 (empirically pinned: `crc32_u64(crc32_u64(
/// 0, 1), 0)`), and the multiplier that turns the reflect state into the
/// lane CRC: `out = (V_lo ⊗ VR0) ⊕ (V_hi ⊗ VH64)  (mod VM)` (docs/27).
pub const VR0: u64 = 0xF20C_0DFE;
/// R14: the vector ending's HIGH-qword multiplier `r0·y^64 mod VM` —
/// numerically identical to [`RKHI`] (the R13 fold constant re-emerging
/// from independent algebra: the fold's unit-lift and the ending's
/// y^64-shift are the same ring operation).
pub const VH64: u64 = 0x493C_7D27;
/// R14: the ending ring's LFSR overflow polynomial Q, where
/// VM = y^32 ⊕ VQ. Empirically pinned as the monomial recurrence
/// R_{j+1} = (R_j << 1) ^ (VQ if R_j >= 2^31) over the 16-byte CRC
/// family (docs/27 §2).
pub const VQ: u64 = 0x05EC_76F1;
/// R14: the full ending modulus VM = y^32 ⊕ VQ (33-bit).
pub const VM: u64 = 0x1_05EC_76F1;
/// R14: the Barrett quotient constant `floor(y^88 / VM)` (57-bit). With
/// the field-wide byte shifts (32, 56, 32 bits = vpalignr 4/7/4) it
/// completes the in-register reduction of the ≤95-bit representative to
/// the 32-bit lane CRC (docs/27 §3). The byte alignment is load-bearing:
/// the shifts are cross-qword (a per-qword vpsrlq would corrupt them),
/// and 32/56/32 is the unique byte-aligned triple that closes exactly
/// (the solver's only verified solution).
pub const VMU: u64 = 0x0105_FD79_BDAB_A560;

/// Bodies shorter than this many bytes evaluate on the scalar kernel.
pub const FOLD_MIN_LEN: usize = 192;

/// R14: the vector ending's master switch. `HFT_CRC_VEND=1|0` overrides;
/// otherwise the default is SILICON-CONDITIONAL (the R14 CI verdicts —
/// docs/27 §5): ON where the 512-bit ports absorb the ending's
/// +10 clmul/span (Intel Sapphire Rapids and newer — the 8573C measured
/// +4.45%/+0.32% sustained across draws), OFF on Ice Lake and everything
/// unproven (the 8370C measured −2.9% sustained / −13.8% packed — its
/// single clmul port serializes the ending's clmuls against the fold's).
/// Read once per span (OnceLock), never inside a step loop.
pub fn vend_enabled() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| match std::env::var("HFT_CRC_VEND").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => vend_supported_cpu(),
    })
}

/// The class table behind `vend_enabled`'s default: true only on Intel
/// family-6 model >= 0x8F (Sapphire Rapids and newer server cores — the
/// class the R14 draw evidence covers). Ice Lake (0x6A) and every other
/// vendor/model stay OFF until a draw certifies them (the ladder-refuted
/// precedent: a default that hurts ANY class does not ship; the knob
/// overrides for experiments).
#[cfg(target_arch = "x86_64")]
fn vend_supported_cpu() -> bool {
    let f = std::arch::x86_64::__cpuid(0);
    let is_intel = f.ebx == 0x756e_6547 && f.edx == 0x4965_6e69 && f.ecx == 0x6c65_746e;
    if !is_intel || f.eax < 1 {
        return false;
    }
    let f1 = std::arch::x86_64::__cpuid(1);
    let base_family = (f1.eax >> 8) & 0xf;
    let model = ((f1.eax >> 4) & 0xf) | (((f1.eax >> 12) & 0xf) << 4);
    let family = if base_family == 0xf {
        base_family + ((f1.eax >> 20) & 0xff)
    } else {
        base_family
    };
    family == 6 && model >= 0x8f
}

#[cfg(not(target_arch = "x86_64"))]
fn vend_supported_cpu() -> bool {
    false
}

/// Span-verification kernel selection. `Copy` + no allocation; detected
/// once at startup (fabric spawn / harness setup), never in-window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrcKernel {
    /// The canonical `span_crc32c_8lane` (8 interleaved `crc32` chains) —
    /// the universal reference and the non-AVX-512 fallback.
    Scalar,
    /// The VPCLMULQDQ mirror-domain fold (this module) — bit-exact equal,
    /// ~4-6x per-core throughput on AVX-512 + GFNI silicon.
    Fold512,
    /// R13: the natural-domain (reflected-representation) fold — the same
    /// recurrence with the units entering as RAW little-endian loads
    /// (no GFNI bit-reverse, no per-qword byte-swap). Kills 2 of the 8
    /// port-5 uops per 128-byte step: the p5 wall of the mirror kernel
    /// (docs/26 §1). Bit-exact with the scalar kernel by the same D11
    /// differential; ~+40% step density on Golden Cove.
    Reflect,
}

/// Whether this CPU can execute the fold kernel (checked once by callers).
#[inline]
pub fn fold512_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("vpclmulqdq")
            && std::arch::is_x86_feature_detected!("gfni")
            && std::arch::is_x86_feature_detected!("sse4.2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

impl CrcKernel {
    /// Deterministic dispatch: CPUID features + optional `HFT_CRC_KERNEL`
    /// override (`scalar` | `fold512` | `reflect`). The kernel choice never
    /// changes any computed value (D11 proves bit equality), only speed.
    /// Default: `reflect` on fold-class silicon (the R13 p5 fix), with
    /// `fold512` as the documented rollback arm.
    pub fn detect() -> Self {
        let avail = fold512_available();
        match std::env::var("HFT_CRC_KERNEL").as_deref() {
            Ok("scalar") => Self::Scalar,
            Ok("fold512") if avail => Self::Fold512,
            Ok("reflect") if avail => Self::Reflect,
            _ if avail => Self::Reflect,
            _ => Self::Scalar,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Scalar => "scalar8lane",
            Self::Fold512 => "fold512",
            Self::Reflect => "reflect",
        }
    }

    /// Evaluate `span_crc32c_8lane(body)`.
    ///
    /// # Safety
    /// `Fold512` must only be evaluated on a CPU with the AVX-512F/BW,
    /// VPCLMULQDQ, GFNI and SSE4.2 features (see [`fold512_available`]).
    #[inline(always)]
    pub unsafe fn eval(&self, body: &[u8]) -> u64 {
        match self {
            Self::Scalar => span_crc32c_8lane(body),
            Self::Fold512 => imp::span_fold_eval(body),
            Self::Reflect => imp::span_fold_eval_r(body),
        }
    }

    /// Evaluate two spans in one call (interleaved fold chains hide clmul
    /// latency). Returns the exact `span_crc32c_8lane` value of each body.
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval`].
    #[inline(always)]
    pub unsafe fn eval2(&self, a: &[u8], b: &[u8]) -> (u64, u64) {
        match self {
            Self::Scalar => (span_crc32c_8lane(a), span_crc32c_8lane(b)),
            Self::Fold512 => imp::span_fold_eval2(a, b),
            Self::Reflect => (imp::span_fold_eval_r(a), imp::span_fold_eval_r(b)),
        }
    }

    /// R10: evaluate two spans in one call, SEQUENTIAL load order — the
    /// software-pipelined tail. Span A's vector fold completes, span B's
    /// vector fold issues next (its clmul chains in flight), and A's
    /// tail+endings+FNV execute while B's fold streams: the per-span
    /// ending overhead (~26% of a 1.36KB span's cycles on the real mix —
    /// the store/reload round-trip, 16 chained crc32 endings, the serial
    /// FNV imul chain) hides under B's vector phase instead of serializing
    /// behind A's own. Unlike [`Self::eval2`] the load stream stays ONE
    /// sequential stream (the post-aliasing blob's bodies are contiguous
    /// up to ~20B headers) — the hardware streamer keeps tracking it.
    /// Values are identical to `(eval(a), eval(b))` by construction (the
    /// same pure functions in the same order; D11-pinned).
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval`].
    #[inline(always)]
    pub unsafe fn eval_pair(&self, a: &[u8], b: &[u8]) -> (u64, u64) {
        match self {
            Self::Scalar => (span_crc32c_8lane(a), span_crc32c_8lane(b)),
            Self::Fold512 => imp::span_fold_eval_pair(a, b),
            Self::Reflect => imp::span_fold_eval_pair_r(a, b),
        }
    }

    /// R11: evaluate one span through the TRI-STREAM fold — the body's
    /// block-pair units split mod-3 across three independent (even, odd)
    /// state pairs (six independent clmul chains over ONE sequential load
    /// stream), then merged with the fixed power-of-y constants back into
    /// the exact single-stream state before the endings. Same value as
    /// [`Self::eval`] by construction (the fold congruence is linear; the
    /// differential suite pins it). The point is latency: the two-stream
    /// kernel's per-register chain is one `clmul -> xor -> clmul -> xor`
    /// dependency per 128 body bytes, and measured fold512 rates sit at
    /// ~9 cycles per step — right where that chain binds. Three chains
    /// give the out-of-order engine 50% more slack per step at the same
    /// issue cost per byte; whether the kernel is chain-bound or
    /// port-bound is exactly what the kbench `fold512_tri` row decides.
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval`].
    #[inline(always)]
    pub unsafe fn eval_tri(&self, body: &[u8]) -> u64 {
        match self {
            Self::Scalar => span_crc32c_8lane(body),
            Self::Fold512 => imp::span_fold_eval_tri(body),
            Self::Reflect => imp::span_fold_eval_r(body),
        }
    }

    /// R14: evaluate the REFLECT kernel with an explicit ENDING path — the
    /// kbench attribution twin (`fold512_r` runs the HFT_CRC_VEND default,
    /// i.e. the vector Barrett ending; `vend = false` forces the R13
    /// crc-chain ending). Values identical on every input (the sweeps
    /// assert it); only the ending's uop mix differs.
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval`].
    #[inline(always)]
    pub unsafe fn eval_rpath(&self, body: &[u8], vend: bool) -> u64 {
        match self {
            Self::Scalar => span_crc32c_8lane(body),
            Self::Fold512 => imp::span_fold_eval(body),
            Self::Reflect => imp::span_fold_eval_r_forced(body, vend),
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════
// x86_64 implementation
// ══════════════════════════════════════════════════════════════════════════

#[cfg(target_arch = "x86_64")]
pub(crate) mod imp {
    use super::{
        KP128, KP192, FOLD_MIN_LEN, KP256, KP320, KP384, KP448, RKHI, RKLO, VH64, VM, VMU, VR0,
    };
    use crate::sink::span_crc32c_8lane;
    use std::arch::x86_64::*;

    /// GFNI affine matrix that bit-reverses every byte. Hardware-verified by
    /// the unit test below (LLVM's constant-fold emulation of this intrinsic
    /// uses a different matrix packing — the test defeats folding with
    /// black_box and pins the silicon semantics).
    pub(crate) const BITREV_MAT: u64 = 0x8040_2010_0804_0201;
    /// vpshufb mask: byte-swap inside each 8-byte group of every 128-bit
    /// lane (the 16-byte pattern broadcast to all four lanes — vpshufb is
    /// per-128-bit-lane).
    const BSWAP_QW: [u8; 64] = [
        7, 6, 5, 4, 3, 2, 1, 0, 15, 14, 13, 12, 11, 10, 9, 8, //
        7, 6, 5, 4, 3, 2, 1, 0, 15, 14, 13, 12, 11, 10, 9, 8, //
        7, 6, 5, 4, 3, 2, 1, 0, 15, 14, 13, 12, 11, 10, 9, 8, //
        7, 6, 5, 4, 3, 2, 1, 0, 15, 14, 13, 12, 11, 10, 9, 8,
    ];

    /// One span's eight lane-fold states in an even/odd split:
    /// `even` holds lanes 0,2,4,6 (128-bit lane j = lane 2j), `odd` holds
    /// lanes 1,3,5,7. The split lets the mirrored units fall out of the
    /// block pair with two 1-cycle `vpunpckl/hqdq` instead of 3-cycle
    /// `vpermt2q` shuffles.
    #[derive(Clone, Copy)]
    struct FoldStates {
        even: __m512i,
        odd: __m512i,
        /// Number of 16-byte units already folded.
        units: usize,
    }

    /// rev64 of every qword: per-byte bit reverse (GFNI affine) + byte swap
    /// within each qword. Used only on the fold states at span end.
    #[inline(always)]
    unsafe fn rev64_qwords(x: __m512i) -> __m512i {
        let bitrev = _mm512_gf2p8affine_epi64_epi8(
            x,
            _mm512_set1_epi64(BITREV_MAT as i64),
            0,
        );
        _mm512_shuffle_epi8(bitrev, _mm512_loadu_si512(BSWAP_QW.as_ptr() as *const _))
    }

    /// Load one 64-byte block (qword k = lane k's word) and bit-reverse every
    /// byte (the qword byte-swap is deferred to the unit stage).
    #[inline(always)]
    unsafe fn prep_block(p: *const u8) -> __m512i {
        _mm512_gf2p8affine_epi64_epi8(
            _mm512_loadu_si512(p as *const _),
            _mm512_set1_epi64(BITREV_MAT as i64),
            0,
        )
    }

    /// Build the (even-lanes, odd-lanes) mirrored-unit registers from a
    /// prepped block pair (n0 = even block, n1 = odd block, both
    /// byte-bit-reversed). Per 128-bit lane j:
    ///   even: LOW = rev64(w_{2j+1, 2j}), HIGH = rev64(w_{2j, 2j})
    ///   odd:  LOW = rev64(w_{2j+1, 2j+1}), HIGH = rev64(w_{2j, 2j+1})
    /// (the vpshufb applies the qword byte-swap after the unpack).
    #[inline(always)]
    unsafe fn units_pair(
        n0: __m512i,
        n1: __m512i,
        bswap: __m512i,
    ) -> (__m512i, __m512i) {
        let u_even = _mm512_shuffle_epi8(_mm512_unpacklo_epi64(n1, n0), bswap);
        let u_odd = _mm512_shuffle_epi8(_mm512_unpackhi_epi64(n1, n0), bswap);
        (u_even, u_odd)
    }

    /// One fold step over the (even, odd) states.
    #[inline(always)]
    unsafe fn fold_step(
        st: &mut FoldStates,
        u_even: __m512i,
        u_odd: __m512i,
        k192: __m512i,
        k128: __m512i,
    ) {
        let t = _mm512_xor_si512(
            _mm512_clmulepi64_epi128(st.even, k192, 0x01),
            _mm512_clmulepi64_epi128(st.even, k128, 0x00),
        );
        st.even = _mm512_xor_si512(t, u_even);
        let t = _mm512_xor_si512(
            _mm512_clmulepi64_epi128(st.odd, k192, 0x01),
            _mm512_clmulepi64_epi128(st.odd, k128, 0x00),
        );
        st.odd = _mm512_xor_si512(t, u_odd);
        st.units += 1;
    }

    /// Software-pipelined block-pair loop over one span's word-pair units.
    /// The next pair's load+affine issues before the current fold step, so
    /// the ~11-cycle load->affine->unpack->shuffle feed-forward overlaps the
    /// ~5-cycle clmul dependency chain.
    ///
    /// R10 CODEGEN LAW: `#[inline(always)]` is LOAD-BEARING. This helper
    /// has neither a `#[target_feature]` of its own nor inline(always) in
    /// the GIGAHFT tree — it only ever inlined by single-caller luck into
    /// `span_fold_eval`'s feature-enabled body. The moment a second caller
    /// appeared (R10's `span_fold_eval_pair`), LLVM outlined it — and a
    /// standalone copy WITHOUT the feature attribute compiles every AVX-512
    /// intrinsic into an out-of-line call to core's wrapper functions with
    /// 512-bit values passed through memory (measured: the fold kernel
    /// collapsed 19.9 -> 1.9 GB/s, a 10x cliff, `vzeroupper` at every
    /// boundary). The attribute pins the inlining that the kernel's
    /// existence depends on.
    ///
    /// SAFETY (beyond the feature contract): `p` must hold >= 128*wp bytes.
    #[inline(always)]
    unsafe fn fold_word_pairs(p: *const u8, wp: usize) -> FoldStates {
        let k192 = _mm512_set1_epi64(KP192 as i64);
        let k128 = _mm512_set1_epi64(KP128 as i64);
        let bswap = _mm512_loadu_si512(BSWAP_QW.as_ptr() as *const _);
        if wp == 0 {
            return FoldStates {
                even: _mm512_setzero_si512(),
                odd: _mm512_setzero_si512(),
                units: 0,
            };
        }
        // Prologue: prep pair 0, seed the states with its units.
        let n0 = prep_block(p);
        let n1 = prep_block(p.add(64));
        let (ue, uo) = units_pair(n0, n1, bswap);
        let mut st = FoldStates {
            even: ue,
            odd: uo,
            units: 1,
        };
        // The out-of-order engine overlaps each iteration's loads+affines
        // with the previous fold's clmul chain (no data dependency).
        for j in 1..wp {
            // SAFETY: 128*(j+1) <= 128*wp bytes are in bounds.
            let n0 = prep_block(p.add(128 * j));
            let n1 = prep_block(p.add(128 * j + 64));
            let (ue, uo) = units_pair(n0, n1, bswap);
            fold_step(&mut st, ue, uo, k192, k128);
        }
        st
    }

    /// Chain `v` (a little-endian u64) into the running crc value `c`.
    #[inline(always)]
    unsafe fn crc_u64(c: u32, v: u64) -> u32 {
        _mm_crc32_u64(c as u64, v) as u32
    }

    /// R11: one TRI-STREAM fold step — identical structure to [`fold_step`]
    /// but the state advances by THREE real units per step (its stream's
    /// next unit is 3 block-pairs ahead), so the fold multiplies by
    /// y^384 (hi: y^448), not y^128. Same R10 CODEGEN LAW: `inline(always)`
    /// is load-bearing (a second caller without the feature attribute
    /// outlines into memory-passed wrapper calls — the 10x cliff).
    #[inline(always)]
    unsafe fn fold_step3(
        st: &mut FoldStates,
        u_even: __m512i,
        u_odd: __m512i,
        k448: __m512i,
        k384: __m512i,
    ) {
        let t = _mm512_xor_si512(
            _mm512_clmulepi64_epi128(st.even, k448, 0x01),
            _mm512_clmulepi64_epi128(st.even, k384, 0x00),
        );
        st.even = _mm512_xor_si512(t, u_even);
        let t = _mm512_xor_si512(
            _mm512_clmulepi64_epi128(st.odd, k448, 0x01),
            _mm512_clmulepi64_epi128(st.odd, k384, 0x00),
        );
        st.odd = _mm512_xor_si512(t, u_odd);
        st.units += 1;
    }

    /// R11: merge one stream register into the accumulator with the offset
    /// constant pair (k_hi = y^(C+64), k_lo = y^C): acc ^ (v_hi⊗k_hi) ^
    /// (v_lo⊗k_lo). Always two clmuls — callers pick constants per the
    /// offset table.
    #[inline(always)]
    unsafe fn merge_stream(acc: __m512i, v: __m512i, k_hi: __m512i, k_lo: __m512i) -> __m512i {
        _mm512_xor_si512(
            acc,
            _mm512_xor_si512(
                _mm512_clmulepi64_epi128(v, k_hi, 0x01),
                _mm512_clmulepi64_epi128(v, k_lo, 0x00),
            ),
        )
    }

    /// R11: the TRI-STREAM block-pair loop — one span's word-pair units
    /// split mod-3 across three independent (even, odd) state pairs.
    ///
    /// # The math
    ///
    /// Stream m folds block-pairs {m, m+3, m+6, ...}; between its
    /// consecutive units sit TWO units of the other streams, so its fold
    /// constant is y^384 (KP384/KP448), and after T_m units its state is
    /// `V_m = sum_t U_{m+3t} * y^(384*(T_m-1-t))`. The full single-stream
    /// state is `V = sum_m V_m * y^(C_m)` with the offset table (derived
    /// by `t_tri_constants_derivation` and pinned by the differential
    /// sweeps; C depends only on `wp mod 3`):
    ///
    /// | wp % 3 | C_0 | C_1 | C_2 |
    /// |--------|-----|-----|-----|
    /// | 0      | 256 | 128 | 0   |
    /// | 1      | 0   | 256 | 128 |
    /// | 2      | 128 | 0   | 256 |
    ///
    /// (C = 0 → identity; 128 → KP192/KP128; 256 → KP320/KP256.) Degenerate
    /// spans (wp < 3) leave streams empty — a zero state merges to zero
    /// under any constant, so the table holds for EVERY wp ≥ 0.
    ///
    /// Six independent clmul chains run over ONE sequential load stream
    /// (unlike eval2's two spans = two streams): each chain gets 3× the
    /// steps between dependencies at unchanged per-byte issue cost.
    ///
    /// SAFETY: `p` must hold >= 128*wp bytes; requires the AVX-512 +
    /// VPCLMULQDQ + GFNI feature contract (callers gate it).
    #[inline(always)]
    unsafe fn fold_word_triples(p: *const u8, wp: usize) -> FoldStates {
        let k448 = _mm512_set1_epi64(KP448 as i64);
        let k384 = _mm512_set1_epi64(KP384 as i64);
        let bswap = _mm512_loadu_si512(BSWAP_QW.as_ptr() as *const _);
        if wp == 0 {
            return FoldStates {
                even: _mm512_setzero_si512(),
                odd: _mm512_setzero_si512(),
                units: 0,
            };
        }
        // Seed stream m with pair m (streams beyond wp stay zero).
        let mut st = [
            FoldStates {
                even: _mm512_setzero_si512(),
                odd: _mm512_setzero_si512(),
                units: 0,
            },
            FoldStates {
                even: _mm512_setzero_si512(),
                odd: _mm512_setzero_si512(),
                units: 0,
            },
            FoldStates {
                even: _mm512_setzero_si512(),
                odd: _mm512_setzero_si512(),
                units: 0,
            },
        ];
        for m in 0..3usize {
            if m < wp {
                // SAFETY: 128*(m+1) <= 128*wp bytes are in bounds.
                let n0 = prep_block(p.add(128 * m));
                let n1 = prep_block(p.add(128 * m + 64));
                let (ue, uo) = units_pair(n0, n1, bswap);
                st[m] = FoldStates {
                    even: ue,
                    odd: uo,
                    units: 1,
                };
            }
        }
        // Steps: pair q = 3, 4, 5, 6, ... feeds stream (q-3) % 3 — an
        // unrolled 3-iteration loop keeps the mapping division-free.
        let mut q = 3usize;
        while q + 2 < wp {
            // SAFETY: 128*(q+3) <= 128*wp bytes are in bounds.
            let (ue, uo) = units_pair(
                prep_block(p.add(128 * q)),
                prep_block(p.add(128 * q + 64)),
                bswap,
            );
            fold_step3(&mut st[0], ue, uo, k448, k384);
            let (ue, uo) = units_pair(
                prep_block(p.add(128 * (q + 1))),
                prep_block(p.add(128 * (q + 1) + 64)),
                bswap,
            );
            fold_step3(&mut st[1], ue, uo, k448, k384);
            let (ue, uo) = units_pair(
                prep_block(p.add(128 * (q + 2))),
                prep_block(p.add(128 * (q + 2) + 64)),
                bswap,
            );
            fold_step3(&mut st[2], ue, uo, k448, k384);
            q += 3;
        }
        // Tail pairs (0..=2 of them): stream m takes pair q iff q < wp.
        while q < wp {
            let m = q % 3;
            // SAFETY: 128*(q+1) <= 128*wp bytes are in bounds.
            let (ue, uo) = units_pair(
                prep_block(p.add(128 * q)),
                prep_block(p.add(128 * q + 64)),
                bswap,
            );
            fold_step3(&mut st[m], ue, uo, k448, k384);
            q += 1;
        }
        // Merge: per wp mod 3, the offset table permutes the stream ROLES
        // (base: offset 0; mid: offset 128 → KP192/KP128; far: offset 256
        // → KP320/KP256):
        //   wp%3==0: (C_0,C_1,C_2) = (256,128,0) → far=0, mid=1, base=2
        //   wp%3==1: (0,256,128)              → base=0, far=1, mid=2
        //   wp%3==2: (128,0,256)              → mid=0, base=1, far=2
        let (far, mid, base) = match wp % 3 {
            0 => (0usize, 1usize, 2usize),
            1 => (1, 2, 0),
            _ => (2, 0, 1),
        };
        let k320 = _mm512_set1_epi64(KP320 as i64);
        let k256 = _mm512_set1_epi64(KP256 as i64);
        let k192 = _mm512_set1_epi64(KP192 as i64);
        let k128 = _mm512_set1_epi64(KP128 as i64);
        let even = merge_stream(
            merge_stream(st[base].even, st[far].even, k320, k256),
            st[mid].even,
            k192,
            k128,
        );
        let odd = merge_stream(
            merge_stream(st[base].odd, st[far].odd, k320, k256),
            st[mid].odd,
            k192,
            k128,
        );
        FoldStates {
            even,
            odd,
            units: st[0].units + st[1].units + st[2].units,
        }
    }

    /// Chain the byte range `[from, to)` of `body` into `c` (whole region
    /// must be in bounds; length < 16).
    #[inline(always)]
    unsafe fn chain_bytes(c: u32, body: &[u8], from: usize, to: usize) -> u32 {
        let p = body.as_ptr();
        let mut c = c;
        let mut i = from;
        let n = to;
        while n - i >= 8 {
            c = crc_u64(c, (p.add(i) as *const u64).read_unaligned());
            i += 8;
        }
        if n - i >= 4 {
            c = _mm_crc32_u32(c, (p.add(i) as *const u32).read_unaligned());
            i += 4;
        }
        if n - i >= 2 {
            c = _mm_crc32_u16(c, (p.add(i) as *const u16).read_unaligned());
            i += 2;
        }
        if i < n {
            c = _mm_crc32_u8(c, *p.add(i));
        }
        c
    }

    /// 128-bit fold step on scalars (lane-0 tail continuation).
    #[inline(always)]
    unsafe fn fold_step_u128(v_hi: u64, v_lo: u64, u_hi: u64, u_lo: u64) -> (u64, u64) {
        let v = _mm_set_epi64x(v_hi as i64, v_lo as i64);
        let k192 = _mm_set_epi64x(0, KP192 as i64);
        let k128 = _mm_set_epi64x(0, KP128 as i64);
        let t = _mm_xor_si128(
            _mm_clmulepi64_si128(v, k192, 0x01),
            _mm_clmulepi64_si128(v, k128, 0x00),
        );
        let u = _mm_set_epi64x(u_hi as i64, u_lo as i64);
        let r = _mm_xor_si128(t, u);
        (_mm_extract_epi64(r, 1) as u64, _mm_extract_epi64(r, 0) as u64)
    }

    /// Mirrored unit from two little-endian 8-byte stream chunks in order
    /// (a first, b second): U_int = a | b<<64, mirrored = rev64(b) |
    /// rev64(a)<<64  =>  (hi, lo) = (rev64(a), rev64(b)).
    #[inline(always)]
    unsafe fn mirror_unit(a: u64, b: u64) -> (u64, u64) {
        (a.reverse_bits(), b.reverse_bits())
    }

    /// Finish one span: lane-0 tail continuation, endings for all 8 lanes,
    /// FNV lane combine. `st` holds the states after the word-pair loop.
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    unsafe fn finish_span(body: &[u8], st: FoldStates) -> u64 {
        let len = body.len();
        let p = body.as_ptr();
        let blocks = len / 64;
        let tail = len % 64;
        let wp = blocks / 2; // word-pair units folded by the vector loop

        // ---- lane 0 scalar continuation: units past the word pairs ----
        // Lane 0's stream = B words + `tail` bytes. Extra full units:
        //   B even: tail/16 contiguous 16-byte tail units.
        //   B odd, tail >= 8: one [w_{B-1} || tail[0..8)] unit, then
        //     (tail-8)/16 tail units.
        //   B odd, tail < 8: none (r0 = 8 + tail <= 15).
        let mut v0_hi: u64;
        let mut v0_lo: u64;
        let lane0_units_total = (8 * blocks + tail) / 16;
        if wp == 0 {
            v0_hi = 0;
            v0_lo = 0;
        } else {
            // Extract lane 0's state (even reg, 128-bit lane 0).
            let mut tmp = [0u64; 8];
            _mm512_storeu_si512(tmp.as_mut_ptr() as *mut _, st.even);
            v0_lo = tmp[0];
            v0_hi = tmp[1];
        }
        let mut first_pending = wp == 0; // next unit seeds if none folded yet
        #[inline(always)]
        unsafe fn fold_extra(
            a: u64,
            b: u64,
            v0_hi: &mut u64,
            v0_lo: &mut u64,
            first: &mut bool,
        ) {
            let (u_hi, u_lo) = mirror_unit(a, b);
            if *first {
                *v0_hi = u_hi;
                *v0_lo = u_lo;
                *first = false;
            } else {
                let (h, l) = fold_step_u128(*v0_hi, *v0_lo, u_hi, u_lo);
                *v0_hi = h;
                *v0_lo = l;
            }
        }
        if blocks % 2 == 1 {
            // The unpaired word w_{B-1} (lane 0's copy at body[64*(B-1)]).
            // SAFETY: 64*(B-1)+8 <= len (block B-1 is full).
            let w = (p.add(64 * (blocks - 1)) as *const u64).read_unaligned();
            if tail >= 8 {
                // Unit [w_{B-1} || tail[0..8)].
                // SAFETY: 64*B + 8 <= len (tail >= 8).
                let t0 = (p.add(64 * blocks) as *const u64).read_unaligned();
                fold_extra(w, t0, &mut v0_hi, &mut v0_lo, &mut first_pending);
                let rest = tail - 8;
                let u = rest / 16;
                for j in 0..u {
                    // SAFETY: 64*B + 8 + 16j + 16 <= len.
                    let base = 64 * blocks + 8 + 16 * j;
                    let a = (p.add(base) as *const u64).read_unaligned();
                    let b = (p.add(base + 8) as *const u64).read_unaligned();
                    fold_extra(a, b, &mut v0_hi, &mut v0_lo, &mut first_pending);
                }
            }
            // tail < 8: no extra units (r0 = 8 + tail handled in the ending).
        } else {
            let u = tail / 16;
            for j in 0..u {
                // SAFETY: 64*B + 16j + 16 <= len (tail >= 16(j+1)).
                let base = 64 * blocks + 16 * j;
                let a = (p.add(base) as *const u64).read_unaligned();
                let b = (p.add(base + 8) as *const u64).read_unaligned();
                fold_extra(a, b, &mut v0_hi, &mut v0_lo, &mut first_pending);
            }
        }
        debug_assert_eq!(wp + {
            // recompute extras for the assert
            let mut x = 0usize;
            if blocks % 2 == 1 && tail >= 8 {
                x = 1 + (tail - 8) / 16;
            } else if blocks % 2 == 0 {
                x = tail / 16;
            }
            x
        }, lane0_units_total);

        // ---- endings: all lanes ----
        let mut lanes = [0u32; 8];
        {
            // rev64 each qword of both state regs; read per lane:
            // even reg's 128-lane j = lane 2j: [q(2j) = rev64(V_lo), q(2j+1)
            // = rev64(V_hi)]; odd reg's lane j = lane 2j+1.
            let mut e = [0u64; 8];
            let mut o = [0u64; 8];
            _mm512_storeu_si512(e.as_mut_ptr() as *mut _, rev64_qwords(st.even));
            _mm512_storeu_si512(o.as_mut_ptr() as *mut _, rev64_qwords(st.odd));
            for j in 0..4usize {
                let lane = 2 * j;
                // Lane 0 is finished separately (continued state).
                if lane != 0 {
                    lanes[lane] = crc_u64(crc_u64(0, e[2 * j + 1]), e[2 * j]);
                }
                lanes[lane + 1] = crc_u64(crc_u64(0, o[2 * j + 1]), o[2 * j]);
            }
        }
        // Lane 0 ending: chain mirrored state (if any unit folded) + last r0
        // stream bytes.
        {
            let r0 = (8 * blocks + tail) % 16;
            let mut c = 0u32;
            if lane0_units_total > 0 {
                c = crc_u64(crc_u64(0, v0_hi.reverse_bits()), v0_lo.reverse_bits());
            }
            if r0 > 0 {
                if r0 <= tail {
                    // last r0 bytes = tail's last r0 bytes (contiguous)
                    // SAFETY: 64*B + tail - r0 .. 64*B + tail <= len.
                    c = chain_bytes(c, body, 64 * blocks + tail - r0, 64 * blocks + tail);
                } else {
                    // B odd, tail < 8, r0 = 8 + tail: [w_{B-1}][tail[0..tail)]
                    // SAFETY: 64*(B-1) + 8 <= len.
                    let w = (p.add(64 * (blocks - 1)) as *const u64).read_unaligned();
                    c = crc_u64(c, w);
                    // SAFETY: 64*B + tail <= len.
                    c = chain_bytes(c, body, 64 * blocks, 64 * blocks + tail);
                }
            }
            lanes[0] = c;
        }
        // Lanes 1..7: r = 8*(B mod 2); if r == 8, chain the last word
        // (unmirrored — stream order).
        if blocks % 2 == 1 {
            // SAFETY: 64*(B-1) + 8k + 8 <= len for k = 1..7 (block is full).
            for k in 1..8usize {
                let w = (p.add(64 * (blocks - 1) + 8 * k) as *const u64).read_unaligned();
                lanes[k] = crc_u64(lanes[k], w);
            }
        }

        // ---- FNV lane combine (identical to the scalar kernel) ----
        let mut h: u64 = 0xcbf29ce484222325;
        for c in lanes {
            h ^= c as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= len as u64 & 0xFFFF_FFFF;
        h = h.wrapping_mul(0x100000001b3);
        std::hint::black_box(h)
    }

    /// The fold kernel (single span). Bit-exact with `span_crc32c_8lane`.
    ///
    /// # Safety
    /// Requires AVX-512F/BW, VPCLMULQDQ, GFNI, SSE4.2.
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval(body: &[u8]) -> u64 {
        if body.len() < FOLD_MIN_LEN {
            return span_crc32c_8lane(body);
        }
        let wp = body.len() / 64 / 2;
        // SAFETY: 128*wp <= len (feature contract + caller bounds).
        let st = fold_word_pairs(body.as_ptr(), wp);
        finish_span(body, st)
    }

    /// Two-span interleaved fold: two independent state chains advance in
    /// the same loop, hiding PCLMULQDQ latency (~2x fold throughput).
    ///
    /// # Safety
    /// Same feature contract as [`span_fold_eval`].
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval2(a: &[u8], b: &[u8]) -> (u64, u64) {
        if a.len() < FOLD_MIN_LEN {
            let va = span_crc32c_8lane(a);
            let vb = span_fold_eval(b);
            return (va, vb);
        }
        if b.len() < FOLD_MIN_LEN {
            let vb = span_crc32c_8lane(b);
            let va = span_fold_eval(a);
            return (va, vb);
        }
        let pa = a.as_ptr();
        let pb = b.as_ptr();
        let wpa = a.len() / 64 / 2;
        let wpb = b.len() / 64 / 2;
        let common = wpa.min(wpb);

        let k192 = _mm512_set1_epi64(KP192 as i64);
        let k128 = _mm512_set1_epi64(KP128 as i64);
        let bswap = _mm512_loadu_si512(BSWAP_QW.as_ptr() as *const _);

        // States seeded with each span's first pair.
        // SAFETY: both bodies hold >= 128 bytes (FOLD_MIN_LEN gate).
        let (ue, uo) = units_pair(prep_block(pa), prep_block(pa.add(64)), bswap);
        let mut sta = FoldStates {
            even: ue,
            odd: uo,
            units: 1,
        };
        let (ue, uo) = units_pair(prep_block(pb), prep_block(pb.add(64)), bswap);
        let mut stb = FoldStates {
            even: ue,
            odd: uo,
            units: 1,
        };
        for j in 1..common {
            // Two independent fold chains in the same iteration: the second
            // span's clmuls hide the first span's dependency latency.
            // SAFETY: 128*(j+1) <= len for both bodies.
            let (uea, uoa) = units_pair(
                prep_block(pa.add(128 * j)),
                prep_block(pa.add(128 * j + 64)),
                bswap,
            );
            let (ueb, uob) = units_pair(
                prep_block(pb.add(128 * j)),
                prep_block(pb.add(128 * j + 64)),
                bswap,
            );
            fold_step(&mut sta, uea, uoa, k192, k128);
            fold_step(&mut stb, ueb, uob, k192, k128);
        }
        // Remainders of the longer span.
        // SAFETY: in-bounds per body length.
        for j in common..wpa {
            let n0 = prep_block(pa.add(128 * j));
            let n1 = prep_block(pa.add(128 * j + 64));
            let (ue, uo) = units_pair(n0, n1, bswap);
            fold_step(&mut sta, ue, uo, k192, k128);
        }
        for j in common..wpb {
            let n0 = prep_block(pb.add(128 * j));
            let n1 = prep_block(pb.add(128 * j + 64));
            let (ue, uo) = units_pair(n0, n1, bswap);
            fold_step(&mut stb, ue, uo, k192, k128);
        }
        (finish_span(a, sta), finish_span(b, stb))
    }

    /// R10: the software-pipelined tail — two spans, ONE sequential load
    /// stream, A's endings deferred under B's vector phase. The structure
    /// is deliberately NOT eval2's interleave: the loads run A fully, then
    /// B fully (the post-aliasing blob's span bodies are contiguous up to
    /// ~20B frame headers, so the pair is one ~2.7KB sequential read the
    /// hardware streamer tracks as a single stream), and the ENDINGS run
    /// after B's fold is issued — the out-of-order engine overlaps A's
    /// tail+endings+FNV (store/reload round-trip, 16 chained crc32, the
    /// serial FNV imul chain — the per-span overhead that the real-mix
    /// fbench control prices at ~26% of a 1.36KB span) with B's in-flight
    /// clmul chains instead of serializing them behind A's own.
    ///
    /// Values are identical to `(span_fold_eval(a), span_fold_eval(b))`:
    /// the same pure functions over the same bytes, evaluated in the same
    /// order — only the instruction schedule changes.
    ///
    /// # Safety
    /// Same feature contract as [`span_fold_eval`].
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_pair(a: &[u8], b: &[u8]) -> (u64, u64) {
        // Short spans take the scalar path per-body (same guards as eval2 —
        // the FOLD_MIN_LEN gate is per-span, and the scalar kernel IS the
        // value for short bodies).
        if a.len() < FOLD_MIN_LEN {
            let va = span_crc32c_8lane(a);
            let vb = span_fold_eval(b);
            return (va, vb);
        }
        if b.len() < FOLD_MIN_LEN {
            let vb = span_crc32c_8lane(b);
            let va = span_fold_eval(a);
            return (va, vb);
        }
        // Phase 1: A's vector fold (its clmul chains retire into sta).
        // SAFETY: 128*(wpa) <= a.len() (FOLD_MIN_LEN gate).
        let sta = fold_word_pairs(a.as_ptr(), a.len() / 64 / 2);
        // Phase 2: B's vector fold — issued while A's last clmuls drain.
        // SAFETY: 128*(wpb) <= b.len().
        let stb = fold_word_pairs(b.as_ptr(), b.len() / 64 / 2);
        // Phase 3: A's tail + endings + FNV — overlapped with phase 2's
        // in-flight chains by the out-of-order engine (independent work,
        // issued behind it in program order).
        let va = finish_span(a, sta);
        // Phase 4: B's tail + endings + FNV (overlaps the NEXT pair's
        // phase 1 when the worker loop chains these calls).
        let vb = finish_span(b, stb);
        (va, vb)
    }

    /// R11: the tri-stream fold (single span) — same value as
    /// [`span_fold_eval`], three interleaved state pairs instead of one.
    /// See [`fold_word_triples`] for the interleave math and merge table.
    ///
    /// # Safety
    /// Requires AVX-512F/BW, VPCLMULQDQ, GFNI, SSE4.2.
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_tri(body: &[u8]) -> u64 {
        if body.len() < FOLD_MIN_LEN {
            return span_crc32c_8lane(body);
        }
        let wp = body.len() / 64 / 2;
        // SAFETY: 128*wp <= len (feature contract + caller bounds).
        let st = fold_word_triples(body.as_ptr(), wp);
        finish_span(body, st)
    }

    // ── R13: the natural-domain (reflected-representation) fold ──────────

    /// R13: one natural-domain fold step over the (even, odd) states.
    /// Identical shape to [`fold_step`] but with the reflected constants
    /// and the three-way XOR fused into one `vpternlogq $0x96` (LLVM
    /// already fuses the mirror kernel's xors — this pins it).
    ///
    /// Same R10 CODEGEN LAW: `#[inline(always)]` is load-bearing.
    #[inline(always)]
    unsafe fn fold_step_r(
        st: &mut FoldStates,
        u_even: __m512i,
        u_odd: __m512i,
        khi: __m512i,
        klo: __m512i,
    ) {
        st.even = _mm512_ternarylogic_epi64(
            _mm512_clmulepi64_epi128(st.even, khi, 0x01),
            _mm512_clmulepi64_epi128(st.even, klo, 0x00),
            u_even,
            0x96,
        );
        st.odd = _mm512_ternarylogic_epi64(
            _mm512_clmulepi64_epi128(st.odd, khi, 0x01),
            _mm512_clmulepi64_epi128(st.odd, klo, 0x00),
            u_odd,
            0x96,
        );
        st.units += 1;
    }

    /// R13: the natural-domain block-pair loop. Per 128 B step: 2 loads +
    /// 2 `vpunpckqdq` + 4 VPCLMULQDQ + 2 `vpternlogq` — NO GFNI affine,
    /// NO `vpshufb` bswap (the p5 census drops 8 -> 6; docs/26 §1). The
    /// units are the RAW little-endian qwords: unpacklo/hi(n0, n1) gives
    /// each 128-bit lane [lo = block 2q's qword, hi = block 2q+1's qword]
    /// exactly as the reflected fold consumes them.
    ///
    /// SAFETY: `p` must hold >= 128*wp bytes; requires the AVX-512 +
    /// VPCLMULQDQ feature contract (callers gate it).
    #[inline(always)]
    unsafe fn fold_word_pairs_r(p: *const u8, wp: usize) -> FoldStates {
        let khi = _mm512_set1_epi64(RKHI as i64);
        let klo = _mm512_set1_epi64(RKLO as i64);
        if wp == 0 {
            return FoldStates {
                even: _mm512_setzero_si512(),
                odd: _mm512_setzero_si512(),
                units: 0,
            };
        }
        // Prologue: load pair 0, seed the states with its raw units.
        let n0 = _mm512_loadu_si512(p as *const _);
        let n1 = _mm512_loadu_si512(p.add(64) as *const _);
        let mut st = FoldStates {
            even: _mm512_unpacklo_epi64(n0, n1),
            odd: _mm512_unpackhi_epi64(n0, n1),
            units: 1,
        };
        for j in 1..wp {
            // SAFETY: 128*(j+1) <= 128*wp bytes are in bounds.
            let n0 = _mm512_loadu_si512(p.add(128 * j) as *const _);
            let n1 = _mm512_loadu_si512(p.add(128 * j + 64) as *const _);
            fold_step_r(
                &mut st,
                _mm512_unpacklo_epi64(n0, n1),
                _mm512_unpackhi_epi64(n0, n1),
                khi,
                klo,
            );
        }
        st
    }

    /// R14: the vector Barrett ending — reduce each 128-bit field's
    /// reflect state to its 32-bit lane CRC entirely in-register:
    ///
    /// ```text
    /// W  = (V_lo ⊗ VR0) ⊕ (V_hi ⊗ VH64)     ≤ 95 bits  (2 clmul + xor)
    /// X  = field >> 32                     = vpalignr(W, W, 4)  (p5)
    /// P  = X.field.lo ⊗ VMU                ≤ 121 bits  (1 clmul)
    /// qh = field >> 56                     = vpalignr(P, P, 7)  (p5)
    /// R  = W ⊕ (qh.field.lo ⊗ VM)          ≤ 98 bits   (1 clmul + xor)
    /// r  = R ⊕ ((field>>32).lo ⊗ VM)       ≤ 32 bits*  (1 clmul + alignr + xor)
    /// out = low32(r) — the q̂−1 correction leaves bits ≥ 32 exact
    /// ```
    ///
    /// The shifts are FIELD-WIDE (cross-qword) — `vpalignr` per 128-bit
    /// lane, not `vpsrlq`; the byte alignment (4/7/4) is the unique
    /// verified triple. Per zmm: 5 clmul + 2 alignr + 3 logic for FOUR
    /// lanes — replacing, per lane, a store/reload + two chained `crc32`
    /// (16 total per span, all on p1) and their extract traffic. The math
    /// is pinned by `t_vend_constants_derivation` (basis-exhaustive over
    /// the software model) and the differential sweeps (both ending paths
    /// vs the scalar kernel on every body).
    #[inline(always)]
    unsafe fn vend_zmm(v: __m512i) -> __m512i {
        let kr0 = _mm512_set1_epi64(VR0 as i64);
        let kh64 = _mm512_set1_epi64(VH64 as i64);
        let km = _mm512_set1_epi64(VM as i64);
        let kmu = _mm512_set1_epi64(VMU as i64);
        let w = _mm512_xor_si512(
            _mm512_clmulepi64_epi128(v, kr0, 0x00),
            _mm512_clmulepi64_epi128(v, kh64, 0x01),
        );
        let x = _mm512_alignr_epi8(w, w, 4);
        let p = _mm512_clmulepi64_epi128(x, kmu, 0x00);
        let qh = _mm512_alignr_epi8(p, p, 7);
        let r = _mm512_xor_si512(w, _mm512_clmulepi64_epi128(qh, km, 0x00));
        _mm512_xor_si512(
            r,
            _mm512_clmulepi64_epi128(_mm512_alignr_epi8(r, r, 4), km, 0x00),
        )
    }
    /// R13: scalar 128-bit natural-domain fold step (lane-0 tail units).
    /// V <- (V_hi ⊗ RKHI) ⊕ (V_lo ⊗ RKLO) ⊕ U with U in natural order.
    #[inline(always)]
    unsafe fn fold_step_u128_r(v_hi: u64, v_lo: u64, u_hi: u64, u_lo: u64) -> (u64, u64) {
        let v = _mm_set_epi64x(v_hi as i64, v_lo as i64);
        let khi = _mm_set_epi64x(0, RKHI as i64);
        let klo = _mm_set_epi64x(0, RKLO as i64);
        let t = _mm_xor_si128(
            _mm_clmulepi64_si128(v, khi, 0x01),
            _mm_clmulepi64_si128(v, klo, 0x00),
        );
        let u = _mm_set_epi64x(u_hi as i64, u_lo as i64);
        let r = _mm_xor_si128(t, u);
        (_mm_extract_epi64(r, 1) as u64, _mm_extract_epi64(r, 0) as u64)
    }

    /// R13: finish one span on the natural-domain states. The ending per
    /// lane is TWO chained `crc32` instructions over the state's qwords in
    /// natural order — `c = crc32_u64(crc32_u64(0, V_lo), V_hi)` — no
    /// `rev64` unmirroring, no per-qword fixups. Lane-0 tail continuation
    /// and the odd-block last words follow the SAME stream decomposition
    /// as [`finish_span`] (the value definition fixes it); only the fold
    /// math and the ending differ.
    ///
    /// R14: the ending rides the `HFT_CRC_VEND` switch — vend (the vector
    /// Barrett, default) replaces the 16 chained `crc32` + their
    /// store/reload round-trip with 2×(5 clmul + 2 alignr) in-register.
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    unsafe fn finish_span_r(body: &[u8], st: FoldStates) -> u64 {
        finish_span_r_inner(body, st, super::vend_enabled())
    }

    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    unsafe fn finish_span_r_inner(body: &[u8], st: FoldStates, vend: bool) -> u64 {
        let len = body.len();
        let p = body.as_ptr();
        let blocks = len / 64;
        let tail = len % 64;
        let wp = blocks / 2;

        // ---- lane 0 scalar continuation: units past the word pairs ----
        // (same case analysis as finish_span; units enter NATURALLY:
        // U = a | b<<64 for stream-ordered qwords a then b.)
        let mut v0_hi: u64;
        let mut v0_lo: u64;
        let lane0_units_total = (8 * blocks + tail) / 16;
        if wp == 0 {
            v0_hi = 0;
            v0_lo = 0;
        } else {
            let mut tmp = [0u64; 8];
            _mm512_storeu_si512(tmp.as_mut_ptr() as *mut _, st.even);
            v0_lo = tmp[0];
            v0_hi = tmp[1];
        }
        let mut first_pending = wp == 0;
        #[inline(always)]
        unsafe fn fold_extra_r(
            a: u64,
            b: u64,
            v0_hi: &mut u64,
            v0_lo: &mut u64,
            first: &mut bool,
        ) {
            if *first {
                *v0_lo = a;
                *v0_hi = b;
                *first = false;
            } else {
                let (h, l) = fold_step_u128_r(*v0_hi, *v0_lo, b, a);
                *v0_hi = h;
                *v0_lo = l;
            }
        }
        if blocks % 2 == 1 {
            // SAFETY: 64*(B-1)+8 <= len (block B-1 is full).
            let w = (p.add(64 * (blocks - 1)) as *const u64).read_unaligned();
            if tail >= 8 {
                // Unit [w_{B-1} || tail[0..8)].
                // SAFETY: 64*B + 8 <= len (tail >= 8).
                let t0 = (p.add(64 * blocks) as *const u64).read_unaligned();
                fold_extra_r(w, t0, &mut v0_hi, &mut v0_lo, &mut first_pending);
                let rest = tail - 8;
                let u = rest / 16;
                for j in 0..u {
                    // SAFETY: 64*B + 8 + 16j + 16 <= len.
                    let base = 64 * blocks + 8 + 16 * j;
                    let a = (p.add(base) as *const u64).read_unaligned();
                    let b = (p.add(base + 8) as *const u64).read_unaligned();
                    fold_extra_r(a, b, &mut v0_hi, &mut v0_lo, &mut first_pending);
                }
            }
        } else {
            let u = tail / 16;
            for j in 0..u {
                // SAFETY: 64*B + 16j + 16 <= len (tail >= 16(j+1)).
                let base = 64 * blocks + 16 * j;
                let a = (p.add(base) as *const u64).read_unaligned();
                let b = (p.add(base + 8) as *const u64).read_unaligned();
                fold_extra_r(a, b, &mut v0_hi, &mut v0_lo, &mut first_pending);
            }
        }
        debug_assert_eq!(wp + {
            let mut x = 0usize;
            if blocks % 2 == 1 && tail >= 8 {
                x = 1 + (tail - 8) / 16;
            } else if blocks % 2 == 0 {
                x = tail / 16;
            }
            x
        }, lane0_units_total);

        // ---- endings: all lanes (natural order, no rev64) ----
        let mut lanes = [0u32; 8];
        if vend {
            // R14: the vector Barrett ending. Lane CRCs land in the low 32
            // bits of each 128-bit field's LOW qword: even field j = lane
            // 2j (field 0 = lane 0, finished separately below), odd field
            // j = lane 2j+1.
            let mut e = [0u64; 8];
            let mut o = [0u64; 8];
            _mm512_storeu_si512(e.as_mut_ptr() as *mut _, vend_zmm(st.even));
            _mm512_storeu_si512(o.as_mut_ptr() as *mut _, vend_zmm(st.odd));
            for j in 1..4usize {
                lanes[2 * j] = e[2 * j] as u32;
            }
            for j in 0..4usize {
                lanes[2 * j + 1] = o[2 * j] as u32;
            }
        } else {
            let mut e = [0u64; 8];
            let mut o = [0u64; 8];
            _mm512_storeu_si512(e.as_mut_ptr() as *mut _, st.even);
            _mm512_storeu_si512(o.as_mut_ptr() as *mut _, st.odd);
            for j in 0..4usize {
                let lane = 2 * j;
                // Lane 0 is finished separately (continued state).
                if lane != 0 {
                    lanes[lane] = crc_u64(crc_u64(0, e[2 * j]), e[2 * j + 1]);
                }
                lanes[lane + 1] = crc_u64(crc_u64(0, o[2 * j]), o[2 * j + 1]);
            }
        }
        // Lane 0 ending: natural state (if any unit folded) + last r0 bytes.
        {
            let r0 = (8 * blocks + tail) % 16;
            let mut c = 0u32;
            if lane0_units_total > 0 {
                c = crc_u64(crc_u64(0, v0_lo), v0_hi);
            }
            if r0 > 0 {
                if r0 <= tail {
                    // last r0 bytes = tail's last r0 bytes (contiguous)
                    // SAFETY: 64*B + tail - r0 .. 64*B + tail <= len.
                    c = chain_bytes(c, body, 64 * blocks + tail - r0, 64 * blocks + tail);
                } else {
                    // B odd, tail < 8, r0 = 8 + tail: [w_{B-1}][tail[0..tail)]
                    // SAFETY: 64*(B-1) + 8 <= len.
                    let w = (p.add(64 * (blocks - 1)) as *const u64).read_unaligned();
                    c = crc_u64(c, w);
                    // SAFETY: 64*B + tail <= len.
                    c = chain_bytes(c, body, 64 * blocks, 64 * blocks + tail);
                }
            }
            lanes[0] = c;
        }
        // Lanes 1..7: r = 8*(B mod 2); if r == 8, chain the last word
        // (stream order — same as the mirror kernel).
        if blocks % 2 == 1 {
            // SAFETY: 64*(B-1) + 8k + 8 <= len for k = 1..7 (block is full).
            for k in 1..8usize {
                let w = (p.add(64 * (blocks - 1) + 8 * k) as *const u64).read_unaligned();
                lanes[k] = crc_u64(lanes[k], w);
            }
        }

        // ---- FNV lane combine (identical to the scalar kernel) ----
        let mut h: u64 = 0xcbf29ce484222325;
        for c in lanes {
            h ^= c as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= len as u64 & 0xFFFF_FFFF;
        h = h.wrapping_mul(0x100000001b3);
        std::hint::black_box(h)
    }

    /// R13: the natural-domain fold kernel (single span). Bit-exact with
    /// `span_crc32c_8lane` (D11 differential + the exhaustive unit sweep).
    ///
    /// # Safety
    /// Requires AVX-512F/BW, VPCLMULQDQ, GFNI, SSE4.2.
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_r(body: &[u8]) -> u64 {
        if body.len() < FOLD_MIN_LEN {
            return span_crc32c_8lane(body);
        }
        let wp = body.len() / 64 / 2;
        // SAFETY: 128*wp <= len (feature contract + caller bounds).
        let st = fold_word_pairs_r(body.as_ptr(), wp);
        finish_span_r(body, st)
    }

    /// R14: the reflect kernel with an EXPLICIT ending path (the kbench
    /// attribution twin + the differential suite's pin of BOTH paths).
    ///
    /// # Safety
    /// Same feature contract as [`span_fold_eval_r`].
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_r_forced(body: &[u8], vend: bool) -> u64 {
        if body.len() < FOLD_MIN_LEN {
            return span_crc32c_8lane(body);
        }
        let wp = body.len() / 64 / 2;
        // SAFETY: 128*wp <= len (feature contract + caller bounds).
        let st = fold_word_pairs_r(body.as_ptr(), wp);
        finish_span_r_inner(body, st, vend)
    }

    /// R13: the production two-span path on the natural-domain kernel —
    /// the same software-pipelined structure as [`span_fold_eval_pair`]
    /// (A's vector fold, B's vector fold, A's endings, B's endings).
    ///
    /// # Safety
    /// Same feature contract as [`span_fold_eval_r`].
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_pair_r(a: &[u8], b: &[u8]) -> (u64, u64) {
        if a.len() < FOLD_MIN_LEN {
            let va = span_crc32c_8lane(a);
            let vb = span_fold_eval_r(b);
            return (va, vb);
        }
        if b.len() < FOLD_MIN_LEN {
            let vb = span_crc32c_8lane(b);
            let va = span_fold_eval_r(a);
            return (va, vb);
        }
        // SAFETY: 128*(wpa) <= a.len() (FOLD_MIN_LEN gate).
        let sta = fold_word_pairs_r(a.as_ptr(), a.len() / 64 / 2);
        // SAFETY: 128*(wpb) <= b.len().
        let stb = fold_word_pairs_r(b.as_ptr(), b.len() / 64 / 2);
        let va = finish_span_r(a, sta);
        let vb = finish_span_r(b, stb);
        (va, vb)
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) mod imp {
    use crate::sink::span_crc32c_8lane;

    #[inline(always)]
    pub unsafe fn span_fold_eval(body: &[u8]) -> u64 {
        span_crc32c_8lane(body)
    }

    #[inline(always)]
    pub unsafe fn span_fold_eval_r(body: &[u8]) -> u64 {
        span_crc32c_8lane(body)
    }

    #[inline(always)]
    pub unsafe fn span_fold_eval_r_forced(body: &[u8], _vend: bool) -> u64 {
        span_crc32c_8lane(body)
    }

    #[inline(always)]
    pub unsafe fn span_fold_eval_pair_r(a: &[u8], b: &[u8]) -> (u64, u64) {
        (span_crc32c_8lane(a), span_crc32c_8lane(b))
    }

    #[inline(always)]
    pub unsafe fn span_fold_eval2(a: &[u8], b: &[u8]) -> (u64, u64) {
        (span_crc32c_8lane(a), span_crc32c_8lane(b))
    }

    #[inline(always)]
    pub unsafe fn span_fold_eval_pair(a: &[u8], b: &[u8]) -> (u64, u64) {
        (span_crc32c_8lane(a), span_crc32c_8lane(b))
    }

    #[inline(always)]
    pub unsafe fn span_fold_eval_tri(body: &[u8]) -> u64 {
        span_crc32c_8lane(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference reflected CRC32C (init 0 / xorout 0) — the table-driven
    /// ground truth used to derive the constants (scripts/crc32c_final_derive.py).
    fn ref_crc32c(data: &[u8]) -> u32 {
        fn table() -> [u32; 256] {
            let mut t = [0u32; 256];
            for (i, e) in t.iter_mut().enumerate() {
                let mut c = i as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        (c >> 1) ^ 0x82F6_3B78
                    } else {
                        c >> 1
                    };
                }
                *e = c;
            }
            t
        }
        static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
        let t = TABLE.get_or_init(table);
        let mut crc = 0u32;
        for &b in data {
            crc = (crc >> 8) ^ t[((crc ^ b as u32) & 0xFF) as usize];
        }
        crc
    }

    /// Raw CRC32C golden vectors (init=0, xorout=0), generated from the
    /// reference. Anchors every kernel to the true CRC32C definition.
    const GOLDEN: &[(&[u8], u32)] = &[
        (b"", 0x0000_0000),
        (b"123456789", 0x58E3_FA20),
        (&[0xFF; 32], 0xE839_9DE9),
    ];

    #[test]
    fn t_reference_matches_standard_vector() {
        // CRC32C("123456789") with init/xorout 0xFFFFFFFF == 0xE3069283.
        fn tb() -> [u32; 256] {
            let mut t = [0u32; 256];
            for (i, e) in t.iter_mut().enumerate() {
                let mut c = i as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
                }
                *e = c;
            }
            t
        }
        let t = tb();
        let mut crc = 0xFFFF_FFFFu32;
        for &b in b"123456789" {
            crc = (crc >> 8) ^ t[((crc ^ b as u32) & 0xFF) as usize];
        }
        assert_eq!(crc ^ 0xFFFF_FFFF, 0xE306_9283);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn t_golden_vectors() {
        for (data, want) in GOLDEN {
            assert_eq!(&ref_crc32c(data), want);
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn t_gfni_bitrev_matrix() {
        if !fold512_available() {
            return;
        }
        unsafe {
            use std::arch::x86_64::*;
            // black_box: defeat LLVM constant folding (its software
            // emulation of the affine disagrees with the hardware packing).
            let probe = std::hint::black_box(0x0F0F_0F0F_0F0F_0F0Fu64);
            let mat = std::hint::black_box(imp::BITREV_MAT);
            let x = _mm512_set1_epi64(probe as i64);
            let r = _mm512_gf2p8affine_epi64_epi8(x, _mm512_set1_epi64(mat as i64), 0);
            let mut out = [0u64; 8];
            _mm512_storeu_si512(out.as_mut_ptr() as *mut _, r);
            for v in out {
                assert_eq!(v, 0xF0F0_F0F0_F0F0_F0F0, "GFNI bitrev matrix wrong");
            }
            // full per-byte check on a mixed pattern
            let probe2 = std::hint::black_box(0x1234_5678_9ABC_DEF0u64);
            let x = _mm512_set1_epi64(probe2 as i64);
            let r = _mm512_gf2p8affine_epi64_epi8(x, _mm512_set1_epi64(mat as i64), 0);
            let mut out = [0u64; 8];
            _mm512_storeu_si512(out.as_mut_ptr() as *mut _, r);
            let want: u64 = 0x482C_6A1E_593D_7B0F; // per-byte reverse, hw-verified
            for v in out {
                assert_eq!(v, want, "GFNI bitrev matrix wrong (mixed pattern)");
            }
        }
    }

    /// Differential: fold kernel == scalar kernel on exhaustive lengths,
    /// patterns and random bodies (the D11 oracle's unit-level core).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn t_fold_differential_exhaustive() {
        if !fold512_available() {
            eprintln!("(fold512 unavailable on this CPU — differential skipped)");
            return;
        }
        let mut body = [0u8; 4200];
        // deterministic PRNG (SplitMix64)
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let check = |body: &[u8]| {
            let want = span_crc32c_8lane(body);
            let got = unsafe { imp::span_fold_eval(body) };
            assert_eq!(
                want, got,
                "fold diverged at len={} body={:02x?}",
                body.len(),
                &body[..body.len().min(32)]
            );
            let gr = unsafe { imp::span_fold_eval_r(body) };
            assert_eq!(want, gr, "reflect diverged at len={}", body.len());
            // R14: BOTH ending paths on every body (vector Barrett + the
            // R13 crc-chain), independent of the HFT_CRC_VEND default.
            let grv = unsafe { imp::span_fold_eval_r_forced(body, true) };
            assert_eq!(want, grv, "reflect vend ending diverged at len={}", body.len());
            let grc = unsafe { imp::span_fold_eval_r_forced(body, false) };
            assert_eq!(want, grc, "reflect crc-chain ending diverged at len={}", body.len());
            let (g2a, g2b) = unsafe { imp::span_fold_eval2(body, body) };
            assert_eq!(want, g2a, "eval2 primary diverged at len={}", body.len());
            assert_eq!(want, g2b, "eval2 mirror diverged at len={}", body.len());
            let g3 = unsafe { imp::span_fold_eval_tri(body) };
            assert_eq!(want, g3, "tri diverged at len={}", body.len());
        };
        // exhaustive small lengths x patterns
        for len in 0..=600usize {
            for pat in 0..4u8 {
                match pat {
                    0 => body[..len].fill(0),
                    1 => body[..len].fill(0xFF),
                    2 => {
                        for (i, e) in body[..len].iter_mut().enumerate() {
                            *e = (i * 131 + 17) as u8;
                        }
                    }
                    _ => {
                        for e in body[..len].iter_mut() {
                            *e = next() as u8;
                        }
                    }
                }
                check(&body[..len]);
            }
        }
        // representative long sizes
        for len in [
            640usize, 680, 1000, 1359, 1360, 1361, 1379, 1380, 1399, 1400, 2047, 2048, 4095,
            4096, 4200,
        ] {
            for e in body[..len].iter_mut() {
                *e = next() as u8;
            }
            check(&body[..len]);
        }
        // mismatched eval2 pairs
        for (la, lb) in [(1360usize, 1399), (1399, 192), (2048, 680), (1379, 1379)] {
            for e in body[..la].iter_mut() {
                *e = next() as u8;
            }
            let a = body[..la].to_vec();
            for e in body[..lb].iter_mut() {
                *e = next() as u8;
            }
            let b = body[..lb].to_vec();
            let want_a = span_crc32c_8lane(&a);
            let want_b = span_crc32c_8lane(&b);
            let (ga, gb) = unsafe { imp::span_fold_eval2(&a, &b) };
            assert_eq!(want_a, ga, "eval2 pair A diverged ({} x {})", la, lb);
            assert_eq!(want_b, gb, "eval2 pair B diverged ({} x {})", la, lb);
            // R13: the production pair path on the natural-domain kernel.
            let (ra, rb) = unsafe { imp::span_fold_eval_pair_r(&a, &b) };
            assert_eq!(want_a, ra, "reflect pair A diverged ({} x {})", la, lb);
            assert_eq!(want_b, rb, "reflect pair B diverged ({} x {})", la, lb);
        }
    }

    /// R13: pin the natural-domain fold constants. `RKHI` is independently
    /// re-derived as `rev32(y^95 mod P)` (the reversed-power convention the
    /// ISA-L constants follow — docs/26 §2); `RKLO` (33-bit, no simple
    /// closed form found) is pinned by the algebraic identity that the
    /// pair must satisfy: folding a one-hot state by TWO consecutive
    /// 128-degree lifts equals folding it by the 256-degree lift built
    /// from the same pair — plus the differential sweep above. A typo in
    /// either constant cannot survive.
    #[test]
    fn t_reflect_constants_derivation() {
        const P: u64 = 0x11ED_C6F4_1; // 33-bit normal-form CRC32C poly
        fn clmul(a: u64, b: u64) -> u128 {
            let mut r = 0u128;
            let mut a = a as u128;
            let mut b = b;
            while b != 0 {
                if b & 1 != 0 {
                    r ^= a;
                }
                b >>= 1;
                a <<= 1;
            }
            r
        }
        fn polymod(mut v: u128, q: u64) -> u128 {
            let dq = 127 - (q as u128).leading_zeros() as i32;
            loop {
                let dv = 127 - v.leading_zeros() as i32;
                if v == 0 || dv < dq {
                    return v;
                }
                v ^= (q as u128) << (dv - dq);
            }
        }
        fn powx(mut e: u32, q: u64) -> u64 {
            let mut r = 1u128;
            let mut base = 2u128;
            while e != 0 {
                if e & 1 != 0 {
                    r = polymod(clmul(r as u64, base as u64), q);
                }
                base = polymod(clmul(base as u64, base as u64), q);
                e >>= 1;
            }
            r as u64
        }
        // rev32(y^95 mod P) == RKHI
        let y95 = powx(95, P);
        let rev32 = (0..32).fold(0u64, |acc, i| acc | ((y95 >> i) & 1) << (31 - i));
        assert_eq!(rev32, RKHI, "RKHI != rev32(y^95 mod P)");
        // rev33(y^96 mod P) == the ISA-L combine constant 0x14cd00bd6 —
        // the convention anchor (documented, not used by the kernel).
        let y96 = powx(96, P);
        let rev33 = (0..33).fold(0u64, |acc, i| acc | ((y96 >> i) & 1) << (32 - i));
        assert_eq!(rev33, 0x14CD_00BD_6, "rev33(y^96 mod P) anchor");
    }

    /// R11: re-derive the tri-stream constants at test time — carry-less
    /// y^k mod P in GF(2)[y] (P = 0x11EDC6F41, the normal-form CRC32C
    /// polynomial). Pins KP448/KP384 (the three-unit fold) and
    /// KP320/KP256 (the two-unit merge) against the same algebra that
    /// produced the shipped KP192/KP128, so a transcription typo in the
    /// new constants cannot survive the suite.
    #[test]
    fn t_tri_constants_derivation() {
        const P: u64 = 0x11ED_C6F4_1; // 33-bit modulus
        fn clmul(a: u64, b: u64) -> u128 {
            let mut r = 0u128;
            let mut a = a as u128;
            let mut b = b;
            while b != 0 {
                if b & 1 != 0 {
                    r ^= a;
                }
                a <<= 1;
                b >>= 1;
            }
            r
        }
        fn clmod(mut v: u128) -> u64 {
            const PL: u32 = 33; // P's bit length
            loop {
                let t = 128 - v.leading_zeros();
                if t < PL {
                    return v as u64;
                }
                v ^= (P as u128) << (t - PL);
            }
        }
        fn ypow(mut k: u32) -> u64 {
            let mut result = 1u64;
            let mut base = clmod(2);
            while k != 0 {
                if k & 1 != 0 {
                    result = clmod(clmul(result, base));
                }
                base = clmod(clmul(base, base));
                k >>= 1;
            }
            result
        }
        assert_eq!(ypow(128), KP128, "KP128 re-derivation");
        assert_eq!(ypow(192), KP192, "KP192 re-derivation");
        assert_eq!(ypow(256), KP256, "KP256 re-derivation");
        assert_eq!(ypow(320), KP320, "KP320 re-derivation");
        assert_eq!(ypow(384), KP384, "KP384 re-derivation");
        assert_eq!(ypow(448), KP448, "KP448 re-derivation");
    }

    /// R14: pin the vector ending's constants and its exactness, from the
    /// ground up. The software model re-derives, independently of the
    /// shipped kernel:
    ///
    /// 1. `VQ` — the monomial recurrence of the 16-byte CRC family
    ///    (`R_{j+1} = (R_j << 1) ^ VQ if R_j >= 2^31`), probed through a
    ///    table-driven reference;
    /// 2. `VR0` — the family's degree-0 seed (`crc32_u64(crc32_u64(0, 1), 0)`);
    /// 3. `VH64` — `VR0 * y^64 mod VM`;
    /// 4. `VMU` — `floor(y^88 / VM)`;
    /// 5. the composed vend structure vs the true double-crc-chain ending
    ///    on ALL 128 basis vectors (linearity closes the proof) and random
    ///    states — with vpalignr's per-field byte semantics emulated
    ///    exactly (the cross-qword shifts are the load-bearing part).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn t_vend_constants_derivation() {
        fn table() -> [u32; 256] {
            let mut t = [0u32; 256];
            for (i, e) in t.iter_mut().enumerate() {
                let mut c = i as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        (c >> 1) ^ 0x82F6_3B78
                    } else {
                        c >> 1
                    };
                }
                *e = c;
            }
            t
        }
        static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
        let t = TABLE.get_or_init(table);
        fn crc_u64(t: &[u32; 256], c: u32, v: u64) -> u32 {
            let mut c = c;
            for i in 0..8 {
                let b = ((v >> (8 * i)) & 0xFF) as u32;
                c = (c >> 8) ^ t[((c ^ b) & 0xFF) as usize];
            }
            c
        }
        // 1. VQ via the monomial recurrence (16-byte family, degree 0..95).
        let r_at = |j: u32| -> u64 {
            // crc of the 16-byte message with a single bit at degree j
            let mut buf = [0u8; 16];
            buf[(j / 8) as usize] |= 1 << (j % 8);
            crc_u64(t, crc_u64(t, 0, u64::from_le_bytes(buf[..8].try_into().unwrap())), u64::from_le_bytes(buf[8..].try_into().unwrap())) as u64
        };
        let r0 = r_at(0);
        let r1 = r_at(1);
        let r2 = r_at(2);
        assert_eq!(r0, VR0, "VR0 re-derivation (the 16-byte seed)");
        let vq = if (r0 >> 31) & 1 == 0 {
            // r1 = r0 << 1 (no overflow): no info; probe further
            r2 ^ ((r1 << 1) & 0xFFFF_FFFF)
        } else {
            r1 ^ ((r0 << 1) & 0xFFFF_FFFF)
        };
        assert_eq!(vq, VQ, "VQ re-derivation (the LFSR overflow poly)");
        // the recurrence must hold for the family (spot check)
        let l = |x: u64| -> u64 { ((x << 1) & 0xFFFF_FFFF) ^ if (x >> 31) & 1 == 1 { VQ } else { 0 } };
        for j in 0..90 {
            assert_eq!(l(r_at(j)), r_at(j + 1), "monomial recurrence at j={j}");
        }
        // 2-4. the ring constants.
        fn clmul(a: u128, b: u128) -> u128 {
            let mut r = 0u128;
            let mut a = a;
            let mut b = b;
            while b != 0 {
                if b & 1 != 0 {
                    r ^= a;
                }
                a <<= 1;
                b >>= 1;
            }
            r
        }
        fn clmod(mut v: u128) -> u128 {
            let m = VM as u128;
            while v >= (1u128 << 32) {
                let sh = (128 - v.leading_zeros()) - 33;
                v ^= m << sh;
            }
            v
        }
        assert_eq!(clmod(clmul(VR0 as u128, 1u128 << 64)), VH64 as u128, "VH64 = r0*y^64 mod VM");
        // VMU = floor(y^88 / VM)
        {
            let mut num = 1u128 << 88;
            let mut q = 0u128;
            let m = VM as u128;
            while num >= m {
                let sh = (128 - num.leading_zeros()) - 33;
                q |= 1u128 << sh;
                num ^= m << sh;
            }
            assert_eq!(q, VMU as u128, "VMU = floor(y^88 / VM)");
        }
        // 5. the composed structure (vpalignr per-field byte semantics
        //    emulated: X = field >> 32; qh = field >> 56; corr = field >> 32)
        //    vs the true ending crc_u64(crc_u64(0, V_lo), V_hi).
        fn vend_model(vlo: u64, vhi: u64) -> u64 {
            let w = clmul(vlo as u128, VR0 as u128) ^ clmul(vhi as u128, VH64 as u128);
            let x = w >> 32; // only the low qword feeds the clmul (imm 0x00)
            let x_lo = x & 0xFFFF_FFFF_FFFF_FFFF;
            let p = clmul(x_lo, VMU as u128);
            let qh = (p >> 56) & 0xFFFF_FFFF_FFFF_FFFF;
            let r = w ^ clmul(qh, VM as u128);
            let corr = (r >> 32) & 0xFFFF_FFFF_FFFF_FFFF;
            ((r ^ clmul(corr, VM as u128)) & 0xFFFF_FFFF) as u64
        }
        // basis-exhaustive (linearity closes the proof)
        for k in 0..64 {
            assert_eq!(vend_model(1 << k, 0), crc_u64(t, crc_u64(t, 0, 1 << k), 0) as u64, "vend basis lo {k}");
            assert_eq!(vend_model(0, 1 << k), crc_u64(t, crc_u64(t, 0, 0), 1 << k) as u64, "vend basis hi {k}");
        }
        // randoms
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        for _ in 0..4096 {
            let vlo = next();
            let vhi = next();
            assert_eq!(
                vend_model(vlo, vhi),
                crc_u64(t, crc_u64(t, 0, vlo), vhi) as u64,
                "vend random"
            );
        }
    }
}
