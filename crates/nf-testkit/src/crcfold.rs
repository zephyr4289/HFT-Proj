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

/// R16: the M² step pair for the dual-stream fold (`dfold`,
/// scripts/r16_ufold_derive.py). The R13 step is ring multiplication by
/// K = RKLO mod VM — which IS [`VR0`] (the P1' class law: `RKHI == K ⊗
/// y^64 mod VM`, verified on 500 randoms + the 1778-body differential) —
/// so advancing TWO blocks per step multiplies by K², realized as ONE
/// 2-clmul step with the reduced pair below. Both constants are <= 32
/// bits: every state field's clmul products stay <= 96 bits (state hi
/// <= 32 bits — strictly tighter than RKLO's 36-bit unreduced form).
/// The dual-stream MERGE pair is (VR0, RKHI) itself — see
/// [`fold_word_pairs_r2`].
pub const DFOLD_K2_LO: u64 = 0x3DA6_D0CB;
pub const DFOLD_K2_HI: u64 = 0xBA4F_C28E;

// ── R21: the octo-stream (T=8) fold constant table ────────────────────────
//
// Derived, verified and emitted by `scripts/r21_ofold_derive.py` (P0/P1/P2:
// the 2178-body differential against the scalar kernel, mirroring the R16
// dfold program). The class law P1' (R16) generalizes: ONE reduced-pair
// 2-clmul step advances a state by k fold units, the pair being
// (K^k (x) y^64, K^k) with K = RKLO mod VM = VR0 — the ending ring's
// inverse powers of y. In the G-table law (R15): K^k = y^(-128k) = G[16k]
// and K^k (x) y^64 = y^(-128k+64) = G[16k-8]. The octo fold needs k = 1..8;
// the shipped tables stop at G[71], so the script extends them to G[128]
// (`OFOLD_G_EXT`, re-derived at test time by `t_ofold_constants_derivation`).
// Every constant is <= 32 bits — the R16 width law (state hi <= 33 bits,
// every composed ending field <= 96 bits, the vend exactness range).
//
// The step pair (k = 8): eight block-pair units per stream step.
pub const OFOLD_K8_HI: u64 = 0x0D3B_6092;
pub const OFOLD_K8_LO: u64 = 0x6992_CEA2;
/// The offset-1 merge pair: K^1 (x) y^64 = G[8] (= RKHI), K^1 = G[16]
/// (= VR0) — the dfold merge pair re-emerging as offset 1.
pub const OFOLD_MG1_HI: u64 = 0x493C_7D27;
pub const OFOLD_MG1_LO: u64 = 0xF20C_0DFE;
/// The offset-2 merge pair: K^2 (x) y^64 = G[24], K^2 = G[32] — the
/// DFOLD_K2 step pair re-emerging as offset 2 (the same ring elements).
pub const OFOLD_MG2_HI: u64 = 0xBA4F_C28E;
pub const OFOLD_MG2_LO: u64 = 0x3DA6_D0CB;
/// The offset-3 merge pair: K^3 (x) y^64 = G[40], K^3 = G[48].
pub const OFOLD_MG3_HI: u64 = 0xDDC0_152B;
pub const OFOLD_MG3_LO: u64 = 0x1C29_1D04;
/// The offset-4 merge pair: K^4 (x) y^64 = G[56], K^4 = G[64].
pub const OFOLD_MG4_HI: u64 = 0x9E4A_DDF8;
pub const OFOLD_MG4_LO: u64 = 0x740E_EF02;
/// The offset-5 merge pair: K^5 (x) y^64 = G[72], K^5 = G[80].
pub const OFOLD_MG5_HI: u64 = 0x39D3_B296;
pub const OFOLD_MG5_LO: u64 = 0x083A_6EEC;
/// The offset-6 merge pair: K^6 (x) y^64 = G[88], K^6 = G[96].
pub const OFOLD_MG6_HI: u64 = 0x0715_CE53;
pub const OFOLD_MG6_LO: u64 = 0xC49F_4F67;
/// The offset-7 merge pair: K^7 (x) y^64 = G[104], K^7 = G[112].
pub const OFOLD_MG7_HI: u64 = 0x47DB_8317;
pub const OFOLD_MG7_LO: u64 = 0x2AD9_1C30;
/// The G-table extension G[72..=128] (57 entries, `y^(-8r) mod VM`) — the
/// r15 table's continuation, re-derived and pinned at test time.
pub const OFOLD_G_EXT: [u32; 57] = [
    0x39D3B296, 0xB430C84D, 0xFEE761A7, 0x767F362C, 0x6D883E38, 0xBA579940,
    0x41C14A25, 0x150D5B88, 0x083A6EEC, 0xAE7B5DA4, 0x657F59E4, 0x24CF405C,
    0x1C42DA43, 0x5237AC92, 0x73C1BB4C, 0x0C4B13D7, 0x0715CE53, 0x42723CE9,
    0x9BC001EA, 0x88494023, 0x3365346A, 0x0A17DE6E, 0xCDB43B9B, 0x0BECE317,
    0xC49F4F67, 0xB5C868C6, 0xE5990944, 0x860413AA, 0xC92F998D, 0x3D175832,
    0xD1E52E1E, 0xBCF79D66, 0x47DB8317, 0xC4D37807, 0xD40EB793, 0x812C0154,
    0x963E61CD, 0x7C335476, 0x574580B1, 0x4029B44A, 0x2AD91C30, 0x30C990AD,
    0x1D5330E5, 0xD6DCEF36, 0x169472B6, 0x94A20153, 0x42E18B26, 0x065E88BD,
    0x0D3B6092, 0x739EB780, 0x8285A5CF, 0x9D1C9F45, 0x7417153F, 0x6E846280,
    0x8298BF1A, 0x7B3E77E8, 0x6992CEA2,
];
/// The offset-indexed merge-pair lookup (index c = the offset in fold
/// units, c = 0 unused — the base stream merges by identity).
pub const OFOLD_MG_HI: [u64; 8] = [
    0, OFOLD_MG1_HI, OFOLD_MG2_HI, OFOLD_MG3_HI, OFOLD_MG4_HI, OFOLD_MG5_HI,
    OFOLD_MG6_HI, OFOLD_MG7_HI,
];
/// The offset-indexed merge-pair lookup (low qwords; index 0 unused).
pub const OFOLD_MG_LO: [u64; 8] = [
    0, OFOLD_MG1_LO, OFOLD_MG2_LO, OFOLD_MG3_LO, OFOLD_MG4_LO, OFOLD_MG5_LO,
    OFOLD_MG6_LO, OFOLD_MG7_LO,
];

/// R22: the TRI-STREAM step pair for the natural-domain fold — the k=3
/// entry of the G-table law (R21, scripts/r21_ofold_derive.py): ONE
/// reduced-pair 2-clmul step advances a state by THREE fold units, the
/// pair being (K^3 (x) y^64, K^3) = (G[40], G[48]) with K = RKLO mod VM
/// = VR0. Both alias the octo-stream offset-3 merge pair — the same ring
/// elements (the tables are ONE law); named separately so the tri
/// kernel's derivation reads self-contained.
pub const TRI_K3_HI: u64 = OFOLD_MG3_HI;
pub const TRI_K3_LO: u64 = OFOLD_MG3_LO;

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

// ── R15: the vtail (vectorized tail) constant tables ──────────────────────
//
// Derived, verified and emitted by `scripts/r15_tail_derive.py` (V1..V5;
// the exhaustive 2419-body differential against the scalar kernel). The
// algebra: with everything in the ring GF(2)[y]/VM and `vend(F) = F ⊗ VR0
// (mod VM)` (the R14 ring-product law, re-verified for F < 2^95), the
// lane-k value after the fold loop is
//
//   lane = Z_r(vend(V)) ⊕ rawCRC(R)     [chain decomposition]
//        = (V_ring ⊗ G[r]) ⊗ VR0 ⊕ R̂ ⊗ y^(128-8r) ⊗ VR0
//
// so a modified field `F = (V_lo ⊗ G[r]) ⊕ (V_hi ⊗ KH[r]) ⊕ Σ data`
// satisfies `vend(F) = lane` exactly, where R is the lane's remaining
// byte stream (lane 0: the extra units + r0 bytes, |R| = 8·(B%2)+tail;
// lanes 1..7 with B odd: the single unpaired word, r = 8) and each data
// datum (qword or partial byte group) multiplies by `AT[t]`, t = the
// bytes from the datum's start to the end of R. All constants are ≤ 32
// bits, so every clmul product stays ≤ 95 bits — vend's Barrett-verified
// input range. The KH table pre-reduces the hi-qword's structural y^64
// factor (`KH[r] = G[r] ⊗ y^64 mod VM`), which is why KH[8] = 1: lanes
// 1..7's hi qword passes through unmultiplied.

/// `G[r] = Z_r(1) = y^(-8r) mod VM` — the r-byte zeros-update constant
/// (the LFSR advance of a 1-seed by r zero bytes), r ∈ 0..=71.
pub const VTAIL_G: [u32; 72] = [
    0x00000001, 0xF26B8303, 0x13A29877, 0xA541927E, 0xDD45AAB8, 0x38116FAC,
    0xEF306B19, 0x68032CC8, 0x493C7D27, 0xF43ED648, 0xCB567BA5, 0x9771F7C1,
    0x3171D430, 0x30D23865, 0x54075546, 0x678EFD01, 0xF20C0DFE, 0x5FE4DC5F,
    0x0F69022B, 0xB93B4CE7, 0x3743F7BD, 0x0D0A7DED, 0x5C15EEB4, 0x75D3F038,
    0xBA4FC28E, 0x2E34CB9D, 0x2DAE840F, 0x5E3E92A0, 0xA2158B34, 0xF7DBCB25,
    0x15BB4109, 0x78A7608D, 0x3DA6D0CB, 0x5A392B2F, 0x7EF48BD1, 0x21C69623,
    0x33CCBBBC, 0xFF6571A2, 0x438FA020, 0x20FE017E, 0xDDC0152B, 0xB9E9E5F0,
    0xF3D78690, 0x925B2B91, 0x6051243F, 0x6E9024B1, 0x401061EE, 0x4F08075C,
    0x1C291D04, 0xC786BE02, 0xE1FCF649, 0x39283A86, 0xA46EF4AA, 0xC90DF36A,
    0x0AEDB6A9, 0xDAF383DC, 0x9E4ADDF8, 0x79297D67, 0xB575DEF4, 0x34418DB4,
    0x75BBA45B, 0xC8D9CA4C, 0x0CF00BA6, 0x84E6A245, 0x740EEF02, 0xE14F7E18,
    0x9A66D0DE, 0x7F31385C, 0x1C19243B, 0xA976FBAE, 0x0E9A7C7A, 0x1A74E649,
];
/// `KH[r] = G[r] ⊗ y^64 mod VM` — lane-0's hi-qword lift constant (the
/// pre-reduced structural y^64; KH[8] = 1 — lanes 1..7's hi qword is raw).
pub const VTAIL_KH: [u32; 72] = [
    0xA9CDDA0D, 0xBF818109, 0x780D5A4D, 0xFE2B5C35, 0x05EC76F1, 0x01000000,
    0x00010000, 0x00000100, 0x00000001, 0xF26B8303, 0x13A29877, 0xA541927E,
    0xDD45AAB8, 0x38116FAC, 0xEF306B19, 0x68032CC8, 0x493C7D27, 0xF43ED648,
    0xCB567BA5, 0x9771F7C1, 0x3171D430, 0x30D23865, 0x54075546, 0x678EFD01,
    0xF20C0DFE, 0x5FE4DC5F, 0x0F69022B, 0xB93B4CE7, 0x3743F7BD, 0x0D0A7DED,
    0x5C15EEB4, 0x75D3F038, 0xBA4FC28E, 0x2E34CB9D, 0x2DAE840F, 0x5E3E92A0,
    0xA2158B34, 0xF7DBCB25, 0x15BB4109, 0x78A7608D, 0x3DA6D0CB, 0x5A392B2F,
    0x7EF48BD1, 0x21C69623, 0x33CCBBBC, 0xFF6571A2, 0x438FA020, 0x20FE017E,
    0xDDC0152B, 0xB9E9E5F0, 0xF3D78690, 0x925B2B91, 0x6051243F, 0x6E9024B1,
    0x401061EE, 0x4F08075C, 0x1C291D04, 0xC786BE02, 0xE1FCF649, 0x39283A86,
    0xA46EF4AA, 0xC90DF36A, 0x0AEDB6A9, 0xDAF383DC, 0x9E4ADDF8, 0x79297D67,
    0xB575DEF4, 0x34418DB4, 0x75BBA45B, 0xC8D9CA4C, 0x0CF00BA6, 0x84E6A245,
];
/// `AT[t] = y^(128-8t) mod VM` — the data constant for a datum (qword or
/// partial byte group) with t bytes from its start to the end of the
/// remaining stream R. Index 0 unused.
pub const VTAIL_AT: [u32; 72] = [
    0x00000000, 0xF838CD50, 0x51DDE21E, 0xBC77A5AA, 0xC915EA3B, 0xA9A3F760,
    0x616F3095, 0xA738873B, 0xA9CDDA0D, 0xBF818109, 0x780D5A4D, 0xFE2B5C35,
    0x05EC76F1, 0x01000000, 0x00010000, 0x00000100, 0x00000001, 0xF26B8303,
    0x13A29877, 0xA541927E, 0xDD45AAB8, 0x38116FAC, 0xEF306B19, 0x68032CC8,
    0x493C7D27, 0xF43ED648, 0xCB567BA5, 0x9771F7C1, 0x3171D430, 0x30D23865,
    0x54075546, 0x678EFD01, 0xF20C0DFE, 0x5FE4DC5F, 0x0F69022B, 0xB93B4CE7,
    0x3743F7BD, 0x0D0A7DED, 0x5C15EEB4, 0x75D3F038, 0xBA4FC28E, 0x2E34CB9D,
    0x2DAE840F, 0x5E3E92A0, 0xA2158B34, 0xF7DBCB25, 0x15BB4109, 0x78A7608D,
    0x3DA6D0CB, 0x5A392B2F, 0x7EF48BD1, 0x21C69623, 0x33CCBBBC, 0xFF6571A2,
    0x438FA020, 0x20FE017E, 0xDDC0152B, 0xB9E9E5F0, 0xF3D78690, 0x925B2B91,
    0x6051243F, 0x6E9024B1, 0x401061EE, 0x4F08075C, 0x1C291D04, 0xC786BE02,
    0xE1FCF649, 0x39283A86, 0xA46EF4AA, 0xC90DF36A, 0x0AEDB6A9, 0xDAF383DC,
];

// ── R23: the affine span-subtraction power tables ──────────────────────────
//
// Derived, verified and emitted by `scripts/r23_affine_crc_derive.py`
// (branch feat/r23-affine-vector-frontier). THE LAW (all products are
// carry-less, LSB-first — PCLMULQDQ operand semantics):
//
//   raw(A ∥ B) = ( raw(A) ⊗ G[L_B] mod VM ) ⊕ raw(B)
//   ⟹ raw(B)   = raw(A ∥ B) ⊕ ( raw(A) ⊗ G[L_B] mod VM )
//
// with G[L] = y^(-8L) mod VM (the R15 zeros-advance constant; the V1
// operator law Z_r(c) = c ⊗ G[r]). `raw` is the reflected CRC32C register
// (init 0, no final xor) — the register semantics of every
// `span_crc32c_8lane` lane. In the normal (MSB-first) representation the
// same law reads CRC(B) = CRC(A∥B) ⊕ (CRC(A) ⊗ x^(8·L_B) mod P(x)) with
// P(x) = 0x11EDC6F41 — the mirror duality (the reflected hardware domain
// tabulates the inverse powers). The law ALSO holds verbatim on FINALIZED
// CRC32C values (init 0xFFFFFFFF, final xor 0xFFFFFFFF) because the init
// and final inversions cancel (I = F):
//
//   full(B) = full(A ∥ B) ⊕ ( full(A) ⊗ G[L_B] mod VM )
//
// O(1) span verification recipe (Engineer 2's kernel): the ingest core
// snapshots the cumulative register at span boundaries (prefix hash
// `raw(A)`, cumulative hash `raw(A∥B)`); the worker composes the length
// constant C(L) = rmul(T128[k-1], TBYTE[r]) for L = 16k + r (ONE clmul +
// reduction), then projects the span with ONE VPCLMULQDQ (u32 register ⊗
// u32 constant, product ≤ 63 bits — inside a 64-bit lane), ONE 33-bit
// reduction mod VM, ONE XOR. Zero payload re-reading. For 64-byte-aligned
// spans the same law applies per lane to the cumulative 8-lane state and
// reconstructs the exact `span_crc32c_8lane` value (lane advance constant
// G[L/8]) — verified by the derivation script's P3b bridge battery.
//
// Width law (degree never overflows the register halves): every constant
// is ≤ 32 bits, so register(≤32b) ⊗ constant ≤ 63 bits < 64-bit lane, and
// fold-state lanes (≤64b) ⊗ constant ≤ 95 bits < 128-bit product.

/// Galois field power lookup table: T[k] = x^(128 * k) mod P(x) in reflected
/// domain — the 16-byte-block advance power. `AFFINE_POW_128B_TABLE[k-1] =
/// y^(-128·k) mod VM = G[16k]` (k ∈ 1..=128, covering 16..=2048 bytes; the
/// k = 0 factor is the ring identity 1, implicit). Anchors: T[1] = VR0,
/// T[2] = DFOLD_K2_LO, T[3..7] = OFOLD_MG{3..7}_LO, T[8] = OFOLD_K8_LO —
/// the table is the K-power ladder of the R13 fold ring.
pub const AFFINE_POW_128B_TABLE: [u64; 128] = [
    0xF20C0DFE, 0x3DA6D0CB, 0x1C291D04, 0x740EEF02, 0x083A6EEC, 0xC49F4F67,
    0x2AD91C30, 0x6992CEA2, 0x7E908048, 0x1B3D8F29, 0xF1D0F55E, 0xA87AB8A8,
    0x8462D800, 0x71D111A8, 0xFFD852C6, 0xDCB17AA4, 0xF37C5AEE, 0x6051D5A2,
    0x18B0D4FF, 0x21F3D99C, 0x8F158014, 0xA00457F7, 0x8D6D2C43, 0x00AC29CF,
    0xE9ADF796, 0x96638B34, 0xE0E9F351, 0x9AF01F2D, 0x2CFF42CF, 0x88F25A3A,
    0x4E36F0B0, 0xBD6F81F8, 0x91C9BD4B, 0x885F087B, 0x4C144932, 0x52148F02,
    0xA3C6F37A, 0xD7C0557F, 0x63DED06A, 0x4D56973C, 0x9669C9DF, 0xE417F38A,
    0x4B9E0F71, 0xD104B8FC, 0x5B397730, 0xE78EB416, 0x61FF0E01, 0x8D96551C,
    0x0BF80DD2, 0x8821ABED, 0x6A45D2B2, 0xD8D26619, 0xDE87806C, 0x14338754,
    0x5BD2011F, 0xDD07448E, 0xDDE8F5B9, 0xA3E3E02C, 0xD73C7BEA, 0x80FF0093,
    0x8FE4C34D, 0xDF99FC11, 0x6C23E841, 0xFE314258, 0x0D8373A0, 0x19E3635E,
    0x29F268B4, 0x1DC0632A, 0x1614F396, 0x9E2993D3, 0x6BEBD73C, 0x63AE91E6,
    0xF8C9DA7A, 0x945A19C1, 0xEE8213B7, 0x93781DC7, 0xCCC4A1B9, 0xA2C2D971,
    0x1CAD4452, 0x74922601, 0xC55F7EAB, 0xA1962329, 0x2D370749, 0x397D84A1,
    0x79113270, 0xBC817803, 0x88EB3C07, 0x6E4CB630, 0x71971D5C, 0xF33B8BC6,
    0x9FB3BBC0, 0x6EF22B23, 0xCE2DF768, 0xE53A4FC7, 0xBE60A91A, 0x1DFA0A15,
    0x8EC52396, 0x0E766B11, 0x475846A4, 0xB2A3DFA6, 0xDC1A160C, 0x79AFDF1C,
    0x07AC6E46, 0x15F85253, 0x1BEC24DD, 0x4C36CD5B, 0xE0A22E29, 0x7C2B6ED9,
    0x06FF88FD, 0xF7317CF0, 0x61B6E40B, 0xDE8A97F8, 0x88F61445, 0xD4520E9E,
    0x0C592BD5, 0x38EDFAF3, 0x72CBFCDB, 0x348331A5, 0xC3977C19, 0xDAFAEA7C,
    0x73DB4C04, 0x72675CE8, 0x3EC2FF83, 0xE8C7A017, 0xCF4BFAEF, 0x6BDE1AC7,
    0xAE1175C2, 0xF7506984,
];
/// Galois field power lookup table: T[r] = x^(8 * r) mod P(x) in reflected
/// domain — the byte-level residual advance power. `AFFINE_POW_BYTE_TABLE[r]
/// = y^(-8r) mod VM = G[r]` (r ∈ 0..=15; identity at r = 0; G[8] = VH64).
pub const AFFINE_POW_BYTE_TABLE: [u64; 16] = [
    0x00000001, 0xF26B8303, 0x13A29877, 0xA541927E, 0xDD45AAB8, 0x38116FAC,
    0xEF306B19, 0x68032CC8, 0x493C7D27, 0xF43ED648, 0xCB567BA5, 0x9771F7C1,
    0x3171D430, 0x30D23865, 0x54075546, 0x678EFD01,
];

// ── R23b: the O(1) affine span kernels (Engineer 2) ─────────────────────────
//
// Derived, pinned and exhaustively verified by
// `scripts/r23b_affine_kernel_derive.py` (the kernel-layer oracle on top of
// Engineer 1's `r23_affine_crc_derive.py` law oracle). THE KEY STRUCTURAL
// RESULT (part K0, basis-exhaustive — GF(2) linearity makes it a COMPLETE
// proof): the R14 vend Barrett constant structure (VMU, VM, the 32/56/32
// field-wide shifts, the two correction clmuls) reduces ANY field F < 2^95
// to F mod VM with the VR0 ending-multiply REMOVED. Every R23 product —
// u32 register ⊗ u32 constant, or u32 table ⊗ u32 table — is ≤ 62 bits,
// deep inside that exactness range, so `clmul_reduce_mod_vm` needs ONE
// product clmul + THREE reduction clmuls and NO new constants, no
// VR0^-1 compensation, no length-dependent reduction ladder.
//
// Kernel inventory (all bit-exact against the reference CRC32C scan and
// `span_crc32c_8lane` — the K1/K2 differentials, 0 errors):
//
//   * `affine_span_const(L)`          — C(L) = G[L] composed from the
//     shipped tables in ONE clmul + reduction (the k = 0 factor is the
//     ring identity 1 — table hit only).
//   * `clmul_reduce_mod_vm(a, b)`     — the scalar 64-bit CLMUL ring
//     product: PCLMULQDQ on x86_64, portable u128 shift-multiply
//     elsewhere. Contract: both operands ≤ 32 bits (the width law).
//   * `span_crc32c_affine_sub`        — THE O(1) single-register span
//     projection: cum ⊕ (prefix ⊗ C(L) mod VM). Zero payload re-reading,
//     two clmul chains, no allocation.
//   * `span_crc32c_8lane_affine_sub`  — the 8-lane VPCLMULQDQ projection:
//     ONE VPCLMULQDQ per zmm advances FOUR lanes (the 8 lanes ride two
//     zmm fields), each followed by the plain-Barrett reduction and the
//     inline FNV-1a-64 lane combine — the EXACT `span_crc32c_8lane` value
//     for 64-byte-aligned spans, in O(1), without reading a single span
//     byte. `HFT_CRC_AFFINE_VEC=0` is the rollback knob (the scalar
//     per-lane path); the default follows the fold512 class gate.

/// R23b: compose the span-length advance constant C(L) = G[L]
/// = y^(-8L) mod VM for L = 16k + r, k ∈ 0..=128, r ∈ 0..=15 — ONE
/// carry-less multiply + ONE reduction (k = 0 is the table-only path).
/// Bounds: L ≤ 2048 (the shipped table horizon; the fabric's span bodies
/// are 1344 B). Both factors are ≤ 32 bits — the width law holds.
#[inline(always)]
pub fn affine_span_const(span_len: usize) -> u64 {
    let k = span_len / 16;
    let r = span_len % 16;
    debug_assert!(span_len <= 2048, "span_len {span_len} beyond the R23 table horizon");
    if k == 0 {
        // SAFETY: r ∈ 0..=16 by construction (the table bound).
        unsafe { *AFFINE_POW_BYTE_TABLE.get_unchecked(r) }
    } else {
        // SAFETY: k-1 ∈ 0..=127 (the table bound, pinned above).
        let t128 = unsafe { *AFFINE_POW_128B_TABLE.get_unchecked(k - 1) };
        let tr = unsafe { *AFFINE_POW_BYTE_TABLE.get_unchecked(r) };
        // SAFETY: both factors ≤ 32 bits — the width-law contract.
        unsafe { clmul_reduce_mod_vm(t128, tr) }
    }
}

/// R23b: the scalar 64-bit CLMUL ring product `a ⊗ b mod VM`.
///
/// Contract (the R16/R23 width law — enforced by debug_assert):
/// `a` and `b` are ≤ 32-bit ring elements, so the product is ≤ 62 bits —
/// inside the K0 plain-Barrett exactness range (< 2^95) with 33 bits of
/// headroom. On x86_64 this is 1 PCLMULQDQ (the product) + 3 PCLMULQDQ
/// (the plain Barrett: quotient estimate, first correction, final
/// correction) + byte shifts — ~4-5 dependent clmuls, no memory, no
/// allocation. PCLMULQDQ presence follows the crate's standing x86_64
/// runner contract (Westmere 2010+; `kbench`'s probe aborts otherwise —
/// same precedent as `fold_step_u128_r`). Non-x86_64 falls back to the
/// portable u128 shift-multiply + the same reduction (tests pin both).
///
/// Returns the FULL reduced value in the low 32 bits; bits ≥ 32 are
/// cleared (unlike the vend ending, there is no seed multiply here).
#[inline(always)]
#[allow(clippy::missing_safety_doc)]
pub unsafe fn clmul_reduce_mod_vm(a: u64, b: u64) -> u64 {
    debug_assert!(a < (1 << 32) && b < (1 << 32), "width law: operands must be ≤ 32 bits");
    #[cfg(target_arch = "x86_64")]
    {
        clmul_reduce_mod_vm_pcl(a, b)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        clmul_reduce_mod_vm_sw(a, b)
    }
}

/// The K0 plain Barrett at exact xmm semantics (the vend core with the
/// VR0 multiply removed). `prod` (< 2^63, hi qword zero) reduces to its
/// residue mod VM in 3 clmuls + 2 byte shifts + 2 xors.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
#[allow(clippy::missing_safety_doc)]
pub(crate) unsafe fn clmul_reduce_mod_vm_pcl(a: u64, b: u64) -> u64 {
    use std::arch::x86_64::*;
    // SAFETY: PCLMULQDQ per the crate's runner contract (see the
    // public doc above); all lane operands are in-range by the width
    // law, so every product fits its 128-bit register. The caller
    // (clmul_reduce_mod_vm) is compiled only on x86_64.
    unsafe {
        let va = _mm_set_epi64x(0, a as i64);
        let vb = _mm_set_epi64x(0, b as i64);
        let prod = _mm_clmulepi64_si128(va, vb, 0x00); // P = a ⊗ b (< 2^63)
        let x = _mm_srli_si128(prod, 4); // low qword = P >> 32 (< 2^31)
        let vmu = _mm_set_epi64x(0, VMU as i64);
        let p = _mm_clmulepi64_si128(x, vmu, 0x00); // (P>>32) ⊗ VMU
        let qh = _mm_srli_si128(p, 7); // low qword = p >> 56
        let vm = _mm_set_epi64x(0, VM as i64);
        let qvm = _mm_clmulepi64_si128(qh, vm, 0x00); // q̂ ⊗ VM
        let r = _mm_xor_si128(prod, qvm);
        let corr = _mm_srli_si128(r, 4); // low qword = r >> 32
        let out = _mm_xor_si128(r, _mm_clmulepi64_si128(corr, vm, 0x00));
        _mm_cvtsi128_si32(out) as u32 as u64 // low 32 = P mod VM
    }
}

/// The portable u128 model of the same kernel (the non-x86_64 path AND
/// the differential oracle's ground model in tests — pinned against the
/// hardware path on x86_64 by `t_affine_kernel_models`).
#[cfg(any(test, not(target_arch = "x86_64")))]
#[inline(always)]
pub(crate) fn clmul_reduce_mod_vm_sw(a: u64, b: u64) -> u64 {
    // 64x64 -> 128 carry-less multiply, LSB-first (PCLMULQDQ semantics).
    let mut prod = 0u128;
    let mut aa = a as u128;
    let mut bb = b;
    while bb != 0 {
        if bb & 1 != 0 {
            prod ^= aa;
        }
        aa <<= 1;
        bb >>= 1;
    }
    // The K0 plain Barrett (u128 semantics of the xmm sequence above).
    let x = (prod >> 32) & 0xFFFF_FFFF_FFFF_FFFF;
    let mut p = 0u128;
    let mut xv = x;
    let mut vmu = VMU as u128;
    while vmu != 0 {
        if vmu & 1 != 0 {
            p ^= xv;
        }
        xv <<= 1;
        vmu >>= 1;
    }
    let qh = (p >> 56) & 0xFFFF_FFFF_FFFF_FFFF;
    let r = prod ^ clmul128(qh, VM as u128);
    let corr = (r >> 32) & 0xFFFF_FFFF_FFFF_FFFF;
    (r ^ clmul128(corr, VM as u128)) as u64 & 0xFFFF_FFFF
}

/// 128-bit software carry-less multiply (LSB-first).
#[cfg(any(test, not(target_arch = "x86_64")))]
#[inline(always)]
fn clmul128(a: u128, b: u128) -> u128 {
    let mut r = 0u128;
    let mut aa = a;
    let mut bb = b;
    while bb != 0 {
        if bb & 1 != 0 {
            r ^= aa;
        }
        aa <<= 1;
        bb >>= 1;
    }
    r
}

/// R23b Task 1 — THE O(1) affine span projection, scalar / 64-bit CLMUL
/// path. THE LAW (Engineer 1's oracle, 10,000/10,000):
///
/// ```text
/// raw(B) = raw(A ∥ B) ⊕ ( raw(A) ⊗ G[L_B] mod VM )
/// ```
///
/// The ingest core snapshots the cumulative raw register at span
/// boundaries (prefix hash `raw(A)`, cumulative hash `raw(A ∥ B)`); this
/// kernel projects the span's own register in TWO clmul chains (C(L)
/// composition + the projection multiply) + ONE XOR — zero payload
/// re-reading, zero allocation, no table misses. The law holds verbatim
/// on finalized CRC32C values too (the I = F cancellation), so callers
/// may feed raw or full snapshots consistently.
///
/// Bounds: `span_len` ≤ 2048 (the shipped table horizon). Panics in
/// debug builds beyond it (release: the projection is undefined — the
/// fabric's span bodies are 1344 B).
#[inline(always)]
#[allow(clippy::missing_safety_doc)]
pub unsafe fn span_crc32c_affine_sub(cum_crc: u32, prefix_crc: u32, span_len: usize) -> u32 {
    // 1. Compose the length multiplier C(L) in 1 CLMUL (+1 for k > 0).
    let c_l = affine_span_const(span_len);
    // 2. Multiply the prefix register by C(L) and XOR with the cumulative
    //    register (0 byte reads). SAFETY: width law — both ≤ 32 bits.
    let shifted_prefix = clmul_reduce_mod_vm(prefix_crc as u64, c_l);
    cum_crc ^ (shifted_prefix as u32)
}

/// R23b: the 8-lane vector gate. `HFT_CRC_AFFINE_VEC=1|0` overrides;
/// otherwise the default follows the fold512 class gate (the kernel needs
/// AVX-512F + BW + VPCLMULQDQ — the same class `fold512_available()`
/// pins). Read once per span (OnceLock), never in a step loop.
pub fn affine_vec_enabled() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| match std::env::var("HFT_CRC_AFFINE_VEC").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => fold512_available(),
    })
}

/// R23b Task 1 — the 8-lane O(1) affine span projection: the EXACT
/// `span_crc32c_8lane(B)` golden hash (8 raw lane registers + the
/// FNV-1a-64 combine) for a 64-byte-aligned span, computed from the two
/// 8-lane snapshots WITHOUT reading a single span byte.
///
/// THE 8-LANE BRIDGE (Engineer 1's P3b battery): every lane of
/// `span_crc32c_8lane` is itself a raw register, so for L_B a multiple
/// of 64 (each lane's B-run = L_B/8 bytes of lane stream):
///
/// ```text
/// lane_k(B) = CL_k(end) ⊕ ( CL_k(start) ⊗ G[L_B / 8] mod VM )
/// ```
///
/// Vector shape (AVX-512F/BW + VPCLMULQDQ): `cvtepu32_epi64` packs the
/// prefix lanes, `maskz_permutexvar_epi64` lays them out 4-per-zmm in
/// 128-bit field position (value in the LOW qword), ONE VPCLMULQDQ per
/// zmm multiplies all 4 fields by the broadcast advance constant, the
/// K0 plain-Barrett reduces all 4 products per zmm in parallel, and the
/// unload XORs the cumulative lanes and runs the inline FNV-1a-64
/// combine. Scalar fallback (no AVX-512): the same law per lane through
/// `clmul_reduce_mod_vm` — bit-identical output (the K2 differential
/// covers BOTH paths).
///
/// Bounds: `span_len` must be a multiple of 64 and ≤ 16384 (L/8 ≤ 2048,
/// the table horizon). Panics in debug builds otherwise.
#[inline]
pub fn span_crc32c_8lane_affine_sub(
    cum_lanes: &[u32; 8],
    prefix_lanes: &[u32; 8],
    span_len: usize,
) -> u64 {
    debug_assert!(
        span_len % 64 == 0 && span_len / 8 <= 2048,
        "8-lane bridge: span_len must be 64-byte aligned and ≤ 16384 (got {span_len})"
    );
    // The lane advance: G[L/8] composed from the shipped tables.
    let l8 = span_len / 8;
    let k = l8 / 16;
    let r = l8 % 16;
    let c_adv = if k == 0 {
        // SAFETY: r < 16 by construction.
        unsafe { *AFFINE_POW_BYTE_TABLE.get_unchecked(r) }
    } else {
        // SAFETY: k-1 ≤ 127 and r < 16 (the table bounds); the width law
        // holds for both factors.
        let t128 = unsafe { *AFFINE_POW_128B_TABLE.get_unchecked(k - 1) };
        let tr = unsafe { *AFFINE_POW_BYTE_TABLE.get_unchecked(r) };
        // SAFETY: width law.
        unsafe { clmul_reduce_mod_vm(t128, tr) }
    };
    if affine_vec_enabled() {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: affine_vec_enabled() gates the AVX-512F/BW + VPCLMULQDQ
        // class; c_adv is a ≤ 32-bit ring element (the width law).
        unsafe {
            return span_crc32c_8lane_affine_sub_vec(cum_lanes, prefix_lanes, c_adv, span_len);
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            unreachable!("affine_vec_enabled() is false on non-x86_64")
        }
    }
    // The scalar fallback: the same law, per lane.
    let mut lanes = [0u32; 8];
    for j in 0..8 {
        // SAFETY: width law — prefix ≤ 32 bits, c_adv ≤ 32 bits.
        let p = unsafe { clmul_reduce_mod_vm(prefix_lanes[j] as u64, c_adv) };
        lanes[j] = cum_lanes[j] ^ (p as u32);
    }
    fnv_lanes_64(&lanes, span_len)
}

/// The `span_crc32c_8lane` lane combine: FNV-1a-64 over the 8 lane
/// registers + the span length (bit-identical to sink.rs's tail).
#[inline(always)]
fn fnv_lanes_64(lanes: &[u32; 8], len: usize) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &c in lanes.iter() {
        h ^= c as u64;
        h = h.wrapping_mul(0x0100_0000_01B3);
    }
    h ^= (len as u32) as u64;
    h.wrapping_mul(0x0100_0000_01B3)
}

/// R23c (Engineer 3's pipeline wiring): the WORKER's O(1) golden-value
/// evaluation from the rxdesc affine tag — the EXACT
/// `span_crc32c_8lane(body)` value reconstructed from the 8 lane
/// REGISTER TAGS (the tag kernel's lanes are the reference kernel's own
/// lane registers, tail folded into lane 0; the combine is the
/// reference's FNV-1a-64 tail) — for ARBITRARY span lengths, ZERO
/// payload reads, nine multiplies.
///
/// Relation to Engineer 2's kernels: for 64-byte-aligned spans this is
/// value-identical to [`span_crc32c_8lane_affine_sub`] with a zero
/// prefix projection (the lanes ARE the cumulative registers at the
/// span's own origin — the K2 law with prefix = 0; pinned by
/// `t_r23c_tag_core`). The pipeline's real spans (mean 1364.6 B, only
/// ~2.7% 64-multiples) ride this combine; the (prefix, cum) raw-CRC
/// triple in the same tag is verified per-span by the SCALAR affine law
/// ([`span_crc32c_affine_sub`]) as the integrity check.
#[inline]
pub fn span_crc32c_8lane_from_tags(lanes: &[u32; 8], len: usize) -> u64 {
    fnv_lanes_64(lanes, len)
}

/// The AVX-512 vector core of [`span_crc32c_8lane_affine_sub`] (the
/// K2-verified op sequence; `c_adv` = G[L/8], precomposed by the caller).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,vpclmulqdq")]
unsafe fn span_crc32c_8lane_affine_sub_vec(
    cum_lanes: &[u32; 8],
    prefix_lanes: &[u32; 8],
    c_adv: u64,
    span_len: usize,
) -> u64 {
    use std::arch::x86_64::*;
    unsafe {
        // Broadcast the advance constant; pack the prefix lanes 8-wide
        // then interleave into field position (value in LOW qword of each
        // 128-bit lane, high qword zero — the vend field contract).
        let bc = _mm512_set1_epi64(c_adv as i64);
        let pre = _mm256_loadu_si256(prefix_lanes.as_ptr() as *const __m256i);
        let cvt = _mm512_cvtepu32_epi64(pre); // qword j = prefix lane j
        let idxa = _mm512_set_epi64(0, 3, 0, 2, 0, 1, 0, 0); // qw [0,0,1,0,2,0,3,0]
        let idxb = _mm512_set_epi64(0, 7, 0, 6, 0, 5, 0, 4); // qw [4,0,5,0,6,0,7,0]
        let fa = _mm512_maskz_permutexvar_epi64(0x55, idxa, cvt);
        let fb = _mm512_maskz_permutexvar_epi64(0x55, idxb, cvt);
        // ONE VPCLMULQDQ per zmm: 4 fields × (prefix_j ⊗ C) (< 2^63,
        // high qword of each field zero — the width law).
        let pa = _mm512_clmulepi64_epi128(fa, bc, 0x00);
        let pb = _mm512_clmulepi64_epi128(fb, bc, 0x00);
        // The K0 plain Barrett (the vend-core structure, seed multiply
        // removed), field-wide cross-qword shifts:
        //   x = w >> 32; p = x ⊗ VMU; qh = p >> 56; r = w ⊕ qh⊗VM;
        //   out = r ⊕ (r>>32)⊗VM  — low 32 bits of each field = the lane.
        let bvmu = _mm512_set1_epi64(VMU as i64);
        let bvm = _mm512_set1_epi64(VM as i64);
        let xa = _mm512_alignr_epi8(pa, pa, 4);
        let pqa = _mm512_clmulepi64_epi128(xa, bvmu, 0x00);
        let qha = _mm512_alignr_epi8(pqa, pqa, 7);
        let ra = _mm512_xor_si512(pa, _mm512_clmulepi64_epi128(qha, bvm, 0x00));
        let oa = _mm512_xor_si512(ra, _mm512_clmulepi64_epi128(_mm512_alignr_epi8(ra, ra, 4), bvm, 0x00));
        let xb = _mm512_alignr_epi8(pb, pb, 4);
        let pqb = _mm512_clmulepi64_epi128(xb, bvmu, 0x00);
        let qhb = _mm512_alignr_epi8(pqb, pqb, 7);
        let rb = _mm512_xor_si512(pb, _mm512_clmulepi64_epi128(qhb, bvm, 0x00));
        let ob = _mm512_xor_si512(rb, _mm512_clmulepi64_epi128(_mm512_alignr_epi8(rb, rb, 4), bvm, 0x00));
        // Unload: field j's residue sits in the LOW 32 bits of qword 2j.
        // Stack store + reload (zero allocation, store-forwarded).
        let mut bufa = [0u64; 8];
        let mut bufb = [0u64; 8];
        _mm512_storeu_si512(bufa.as_mut_ptr() as *mut __m512i, oa);
        _mm512_storeu_si512(bufb.as_mut_ptr() as *mut __m512i, ob);
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for k in 0..4 {
            let v = (bufa[2 * k] as u32) ^ cum_lanes[k];
            h ^= v as u64;
            h = h.wrapping_mul(0x0100_0000_01B3);
        }
        for k in 0..4 {
            let v = (bufb[2 * k] as u32) ^ cum_lanes[4 + k];
            h ^= v as u64;
            h = h.wrapping_mul(0x0100_0000_01B3);
        }
        h ^= (span_len as u32) as u64;
        h.wrapping_mul(0x0100_0000_01B3)
    }
}

// ── R23b Task 2: the speculative slicer's AVX-512 table builder ─────────────
//
// moldudp64::spec_slice_512 consumes the E/O BE-u16 lane tables; this
// crate (the workspace's SIMD home) owns the raw-intrinsic builder and
// injects it through moldudp64::install_spec_vec_tables — nf-protocol is
// #![forbid(unsafe_code)] by law. The hook type is a plain safe fn; the
// unsafe stays here. Table contract (the K3 oracle's exact model):
//   e_tab[j] = the BE u16 at window bytes (2j, 2j+1)
//   o_tab[j] = the BE u16 at window bytes (2j+1, 2j+2)
// built from the ZERO-PADDED 66-byte stack window so the +1 load stays
// in-bounds by construction (padding is never read as a length: the
// walk only touches lanes whose both header bytes are real).

/// The per-u16 byte-swap shuffle matrix (four identical 16-byte lanes:
/// 1,0,3,2,5,4,7,6,9,8,11,10,13,12,15,14) — `vpshufb` with this turns
/// every LE u16 lane into the BE u16 of the same byte pair.
#[cfg(target_arch = "x86_64")]
static SPEC_BSWAP16: [u8; 64] = [
    1, 0, 3, 2, 5, 4, 7, 6, 9, 8, 11, 10, 13, 12, 15, 14, //
    1, 0, 3, 2, 5, 4, 7, 6, 9, 8, 11, 10, 13, 12, 15, 14, //
    1, 0, 3, 2, 5, 4, 7, 6, 9, 8, 11, 10, 13, 12, 15, 14, //
    1, 0, 3, 2, 5, 4, 7, 6, 9, 8, 11, 10, 13, 12, 15, 14,
];

/// Install the AVX-512 E/O table builder into the speculative slicer
/// (R23b Task 2). Call once at startup — the fabric spawn / harness /
/// kbench main — before the first window is sliced. Returns false (and
/// installs nothing) when the silicon lacks the AVX-512F+BW class or a
/// builder is already installed; the slicer then keeps its scalar path
/// (bit-identical output, the K3/t12 differentials pin both).
pub fn install_spec_slice_vec() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        if !(std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw"))
        {
            return false;
        }
        nf_protocol::moldudp64::install_spec_vec_tables(spec_slice_vec_entry)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// The safe hook entry: the target_feature call is gated by the
/// install-time feature detection above.
#[cfg(target_arch = "x86_64")]
fn spec_slice_vec_entry(chunk: &[u8], e_tab: &mut [u16; 32], o_tab: &mut [u16; 32]) {
    // SAFETY: install_spec_slice_vec() verified avx512f+avx512bw before
    // installing this entry; the builder is memory-safe by the table
    // contract (the +1 load stays inside its zero-padded 66-byte window).
    unsafe { spec_slice_vec_tables_avx512(chunk, e_tab, o_tab) }
}

/// The E/O BE-u16 lane-table builder: E = vpshufb(v, bswap16) over the
/// window, O = the same over the +1-shifted window — 63 candidate
/// message headers extracted in ~5 vector ops.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw")]
unsafe fn spec_slice_vec_tables_avx512(chunk: &[u8], e_tab: &mut [u16; 32], o_tab: &mut [u16; 32]) {
    use std::arch::x86_64::*;
    unsafe {
        // The zero-padded 66-byte stack window: the +1 load reads bytes
        // 1..=64 — in-bounds by construction for any chunk ≤ 64 bytes.
        let mut win = [0u8; 66];
        win[..chunk.len()].copy_from_slice(chunk);
        let bswap16 = _mm512_loadu_si512(SPEC_BSWAP16.as_ptr() as *const __m512i);
        let v = _mm512_loadu_si512(win.as_ptr() as *const __m512i);
        let v1 = _mm512_loadu_si512(win.as_ptr().add(1) as *const __m512i);
        let e = _mm512_shuffle_epi8(v, bswap16);
        let o = _mm512_shuffle_epi8(v1, bswap16);
        _mm512_storeu_si512(e_tab.as_mut_ptr() as *mut __m512i, e);
        _mm512_storeu_si512(o_tab.as_mut_ptr() as *mut __m512i, o);
    }
}


/// R15: the vectorized-tail master switch. `HFT_CRC_VTAIL=1|0` overrides;
/// otherwise the default follows the vend class gate (the vtail replaces
/// the vend path's scalar lane-0 continuation + the lanes-1..7 odd-word
/// crc chain, so it only ever runs where vend runs — Intel SPR+ per the
/// R14 draw evidence). Read once per span (OnceLock), never in a step
/// loop. The kbench `fold512_rv` row forces it ON; `HFT_CRC_VTAIL=0` is
/// the documented rollback (CI arm 11t).
pub fn vtail_enabled() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| match std::env::var("HFT_CRC_VTAIL").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => vend_supported_cpu(),
    })
}

/// R16: the dual-stream fold's master switch. `HFT_CRC_DFOLD=1|0`;
/// default OFF on every class until >= 3 healthy-draw verdicts certify
/// it (the house law — the 11r/11s/11t precedent; CI arm 11v is the
/// attribution soak). dfold is CLASS-exact — the merged states are
/// ring-congruent, not value-identical, to the sequential kernel's
/// states — so it requires the class endings: dispatch forces vend + the
/// vtail composed-field lane-0 path for ALL r, and resolves to OFF when
/// vend is off. `HFT_CRC_DFOLD=0` is the documented rollback (the
/// default IS the rollback until a class certifies).
pub fn dfold_enabled() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| match std::env::var("HFT_CRC_DFOLD").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => false,
    })
}

/// R21: the octo-stream fold's master switch. `HFT_CRC_OFOLD=1|0`;
/// default OFF on every class until >= 3 healthy-draw verdicts certify
/// it (the house law — the dfold precedent). ofold is CLASS-exact —
/// the merged states are ring-congruent, not value-identical, to the
/// sequential kernel's states — so it requires the class endings:
/// dispatch forces vend + the vtail composed-field lane-0 path for ALL
/// r, and resolves to OFF when vend is off (dfold precedence: when both
/// axes arm, ofold wins — it is the deeper split and subsumes dfold's
/// chain-depth effect). `HFT_CRC_OFOLD=0` is the documented rollback
/// (the default IS the rollback until a class certifies).
pub fn ofold_enabled() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| match std::env::var("HFT_CRC_OFOLD").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => false,
    })
}

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

/// R17/T-1: bake-time wire -> arena transposition for one span body
/// (Route T, ROADMAP2 §5.2 — the kill-test/twin groundwork). For each
/// full 128 B fold unit j the arena stores the unit's (even, odd)
/// lane-pure images in the exact order [`CrcKernel::eval_rpath_t`]
/// loads them:
///
/// * `arena[128j .. 128j+64]`     = `unpacklo_epi64(n0, n1)` as bytes
/// * `arena[128j+64 .. 128j+128]` = `unpackhi_epi64(n0, n1)` as bytes
///
/// with `n0 = body[128j .. 128j+64]`, `n1 = body[128j+64 .. 128j+128]`
/// (per 128-bit lane i: lo = `(n0.qw[2i], n1.qw[2i])`,
/// hi = `(n0.qw[2i+1], n1.qw[2i+1])`). The trailing partial unit (the
/// body's bytes past the last full 128 B — lane-0 continuation and
/// ending bytes) is NOT transposed: the ending path keeps reading the
/// ORIGINAL wire body (ROADMAP2 §5.2 hazard #1's ship-first variant —
/// zero algebra changes). Pure storage permutation, no algebra — safe
/// code, runs in the untimed init window.
pub fn transpose_arena_slot(body: &[u8], arena: &mut [u8]) {
    let wp = body.len() / 128;
    assert!(
        arena.len() >= 128 * wp,
        "arena slot too small: {} < {}",
        arena.len(),
        128 * wp
    );
    for j in 0..wp {
        let s = 128 * j;
        let (n0, n1) = (&body[s..s + 64], &body[s + 64..s + 128]);
        let (ev, od) = arena[s..s + 128].split_at_mut(64);
        for i in 0..4 {
            let (lo, hi) = (16 * i, 16 * i + 8);
            ev[lo..hi].copy_from_slice(&n0[lo..hi]);
            ev[hi..lo + 16].copy_from_slice(&n1[lo..hi]);
            od[lo..hi].copy_from_slice(&n0[hi..lo + 16]);
            od[hi..lo + 16].copy_from_slice(&n1[hi..lo + 16]);
        }
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
    /// stream), then merged back into the single-stream state class
    /// before the endings. Same value as [`Self::eval`] on every input
    /// (the differential suite pins it). The point is latency: the
    /// two-stream kernel's per-register chain is one
    /// `clmul -> xor -> clmul -> xor` dependency per 128 body bytes, and
    /// measured fold512 rates sit at ~9 cycles per step — right where
    /// that chain binds. Three chains give the out-of-order engine 50%
    /// more slack per step at the same issue cost per byte; whether the
    /// kernel is chain-bound or port-bound is exactly what the kbench
    /// `fold512_tri` row decides.
    ///
    /// R22: the arm initially shipped the NATURAL-domain tri here (the
    /// class-exact K^3 shape with the forced composed-field endings).
    /// R22.1 (the run #455 fleet verdict, 16 consolidated draws): that
    /// shape is fleet-REJECTED — tri_r/fold512_rc = 0.67–0.86 on the Zen
    /// 5 draws (the forced vend+vtail-all-r endings cost 12–25% on AMD,
    /// which the split's ~9% chain saving cannot recover), −9..−14%
    /// SUSTAINED on the same-draw 11b-vs-11l arms, −2..−6% on Emerald.
    /// The `Reflect` arm now runs the MIRROR-domain tri — the VALUE-EXACT
    /// shape (its states are bit-identical to the sequential mirror
    /// kernel's, so the endings stay the cheap mirror path, no forced
    /// class machinery): parity-to−3% packed on Zen 5, parity on Emerald,
    /// and the only T=3 configuration whose latency-slack mechanism
    /// survives the L3-bound fabric question. The 11l armed soak prices
    /// it through the worker loop; the natural class-exact axis remains
    /// first-class via [`Self::eval_tri_r`] (the kbench `fold512_tri_r`
    /// row).
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval`].
    #[inline(always)]
    pub unsafe fn eval_tri(&self, body: &[u8]) -> u64 {
        match self {
            Self::Scalar => span_crc32c_8lane(body),
            Self::Fold512 => imp::span_fold_eval_tri(body),
            Self::Reflect => imp::span_fold_eval_tri(body),
        }
    }

    /// R22: evaluate one span through the NATURAL-domain tri-stream (the
    /// class-exact K^3 shape — [`imp::span_fold_eval_tri_r`], forced
    /// vend+vtail-all-r endings). Split out from [`Self::eval_tri`] by
    /// the R22.1 fleet ruling (the class-exact shape is kbench
    /// attribution-only as a worker default: the forced composed-field
    /// endings lose on AMD — see `eval_tri`'s R22.1 note); the kbench
    /// `fold512_tri_r` row keeps pricing it per draw.
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval`].
    #[inline(always)]
    pub unsafe fn eval_tri_r(&self, body: &[u8]) -> u64 {
        match self {
            Self::Scalar => span_crc32c_8lane(body),
            Self::Fold512 => imp::span_fold_eval_tri(body),
            Self::Reflect => imp::span_fold_eval_tri_r(body),
        }
    }

    /// R14: evaluate the REFLECT kernel with an explicit ENDING path — the
    /// kbench attribution twin (`fold512_r` runs the HFT_CRC_VEND default,
    /// i.e. the vector Barrett ending; `vend = false` forces the R13
    /// crc-chain ending). R15: the vtail axis rides `vtail_enabled()`
    /// inside the forced path (HFT_CRC_VTAIL=0 pins the R14 tail shape).
    /// Values identical on every input (the sweeps assert it); only the
    /// ending's uop mix differs.
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval`].
    #[inline(always)]
    pub unsafe fn eval_rpath(&self, body: &[u8], vend: bool) -> u64 {
        match self {
            Self::Scalar => span_crc32c_8lane(body),
            Self::Fold512 => imp::span_fold_eval(body),
            Self::Reflect => {
                imp::span_fold_eval_r_forced(body, vend, vend && vtail_enabled())
            }
        }
    }

    /// R15: the FULL forced path — both the vend and vtail axes explicit
    /// (the kbench attribution triple: `fold512_r` = vend/no-vtail (the
    /// R14 shape), `fold512_rv` = vend/vtail (the R15 shape), `fold512_rc`
    /// = crc-chain (the R13 rollback)). Values identical on every input.
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval`].
    #[inline(always)]
    pub unsafe fn eval_rpath3(&self, body: &[u8], vend: bool, vtail: bool) -> u64 {
        match self {
            Self::Scalar => span_crc32c_8lane(body),
            Self::Fold512 => imp::span_fold_eval(body),
            Self::Reflect => imp::span_fold_eval_r_forced(body, vend, vtail),
        }
    }

    /// R16: the FULL forced path + the dual-stream fold axis (the kbench
    /// attribution quad's fourth entry: `fold512_rd` = dfold — the T=2
    /// block-parity shape, forced vend+vtail-all-r). `dfold && !vend`
    /// resolves as dfold OFF. Values identical on every input.
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval`].
    #[inline(always)]
    pub unsafe fn eval_rpath4(&self, body: &[u8], vend: bool, vtail: bool, dfold: bool) -> u64 {
        match self {
            Self::Scalar => span_crc32c_8lane(body),
            Self::Fold512 => imp::span_fold_eval(body),
            Self::Reflect => imp::span_fold_eval_r_forced_d(body, vend, vtail, dfold),
        }
    }

    /// R21: the FULL forced path + the octo-stream fold axis (the kbench
    /// attribution row: `fold512_ro` = ofold — forced vend+vtail-all-r,
    /// the T=8 block-parity shape). `ofold && !vend` resolves as ofold
    /// OFF; ofold preempts dfold when both arm (the deeper split).
    /// Values identical on every input.
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval`].
    #[inline(always)]
    pub unsafe fn eval_rpath5(
        &self,
        body: &[u8],
        vend: bool,
        vtail: bool,
        dfold: bool,
        ofold: bool,
    ) -> u64 {
        match self {
            Self::Scalar => span_crc32c_8lane(body),
            Self::Fold512 => imp::span_fold_eval(body),
            Self::Reflect => imp::span_fold_eval_r_forced_o(body, vend, vtail, dfold, ofold),
        }
    }

    /// R17/T-1: the TRANSPOSED-arena twin of [`Self::eval_rpath3`] (Route
    /// T, ROADMAP2 §5.2 — the `fold512_t` kbench kill-test row). `arena`
    /// must point at the body's transposed slot: `128*wp` bytes
    /// (`wp = body.len()/64/2`) holding each 128 B fold unit
    /// PRE-INTERLEAVED in the exact lane order the fold consumes — the
    /// `unpacklo/hi(n0, n1)` materialized at bake time by
    /// [`transpose_arena_slot`]. The fold loop then loads the even/odd
    /// unit registers directly: the 2 `vpunpckqdq` per 128 B step are
    /// DELETED (p5 census 6 -> 4). The tail/ending path reads the
    /// ORIGINAL wire `body` (hazard #1's ship-first variant), so the
    /// value is bit-identical to [`Self::eval_rpath3`] BY CONSTRUCTION
    /// (a pure storage permutation; the 8-lane value definition is
    /// untouched). `t_transpose_arena_parity` pins it.
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval_rpath3`]; additionally
    /// `arena` must be readable for `128 * (body.len()/64/2)` bytes and
    /// hold the [`transpose_arena_slot`] image of `body`.
    #[inline(always)]
    pub unsafe fn eval_rpath_t(
        &self,
        body: &[u8],
        arena: *const u8,
        vend: bool,
        vtail: bool,
    ) -> u64 {
        match self {
            Self::Scalar => span_crc32c_8lane(body),
            Self::Fold512 => imp::span_fold_eval(body),
            Self::Reflect => imp::span_fold_eval_r_t(body, arena, vend, vtail),
        }
    }

    /// R17/I-1: the fold-loop-only floor row (`fold512_noend`) — the
    /// ending stack stubbed to a state sum. Prices the ending +
    /// lane-0-continuation share in isolation (fold512_r minus this row
    /// = the ending diet, per draw). NOT a CRC value (state sum, not the
    /// finished span value) — kbench telemetry only, never a fabric path.
    ///
    /// # Safety
    /// Same feature contract as [`Self::eval_rpath3`].
    #[inline(always)]
    pub unsafe fn eval_rpath_noend(&self, body: &[u8]) -> u64 {
        match self {
            Self::Scalar => span_crc32c_8lane(body),
            Self::Fold512 => imp::span_fold_eval(body),
            Self::Reflect => imp::span_fold_eval_r_noend(body),
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════
// x86_64 implementation
// ══════════════════════════════════════════════════════════════════════════

#[cfg(target_arch = "x86_64")]
pub(crate) mod imp {
    use super::{
        DFOLD_K2_HI, DFOLD_K2_LO, KP128, KP192, FOLD_MIN_LEN, KP256, KP320, KP384, KP448,
        OFOLD_K8_HI, OFOLD_K8_LO, OFOLD_MG_HI, OFOLD_MG_LO, RKHI, RKLO, TRI_K3_HI, TRI_K3_LO,
        VH64, VM, VMU, VR0,
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

    /// R21: the OCTO-STREAM (T=8) natural-domain block-pair loop. One
    /// span's word-pair units split mod-8 across EIGHT independent
    /// (even, odd) state pairs — sixteen independent clmul chains over
    /// ONE sequential load stream, the dfold/tri-stream latency lever
    /// taken to the directive's 8-accumulator shape (Task 1: break the
    /// 3-4 cycle serial clmul latency floor; 512 bytes per accumulator
    /// generation, 1024 bytes per full 8-stream cycle).
    ///
    /// # The math
    ///
    /// Stream m folds block-pairs {m, m+8, m+16, ...}; between its
    /// consecutive units sit SEVEN units of the other streams, so its
    /// fold step multiplies the state by K^8 in the ending ring
    /// (K = RKLO mod VM = VR0) — ONE 2-clmul step with the reduced pair
    /// (OFOLD_K8_HI, OFOLD_K8_LO) = (K^8 (x) y^64, K^8) = (G[120],
    /// G[128], scripts/r21_ofold_derive.py P0/P1). After T_m units the
    /// stream state is `V_m = sum_t U_{m+8t} (x) K^(8(T_m-1-t))`; the
    /// full single-stream state is `V = sum_m V_m (x) K^(C_m)` with the
    /// offset `C_m = (wp-1-m) mod 8` (the same derivation as the
    /// tri-stream table, mod 8; units after stream m's last unit). Each
    /// merge is ONE 2-clmul step with the reduced pair (OFOLD_MG{c}_HI,
    /// OFOLD_MG{c}_LO) = (K^c (x) y^64, K^c); the base stream (the one
    /// owning the body's LAST unit) merges by identity, and empty
    /// streams merge to zero. The merged states are CLASS-exact (ring
    /// congruent to the sequential kernel's states — the P1' law), so
    /// the vend/vtail composed-field ending stack runs unchanged, with
    /// the dfold dispatch law: vend + vtail-all-r forced for ALL r.
    ///
    /// SAFETY: `p` must hold >= 128*wp bytes; requires the AVX-512 +
    /// VPCLMULQDQ feature contract (callers gate it). `wp >= 1` by the
    /// FOLD_MIN_LEN dispatcher gate.
    #[inline(always)]
    unsafe fn fold_word_octets_r(p: *const u8, wp: usize) -> FoldStates {
        let khi8 = _mm512_set1_epi64(OFOLD_K8_HI as i64);
        let klo8 = _mm512_set1_epi64(OFOLD_K8_LO as i64);
        debug_assert!(wp >= 1);
        // Seed stream m with pair m (streams beyond wp stay zero).
        let mut st: [FoldStates; 8] = [
            FoldStates { even: _mm512_setzero_si512(), odd: _mm512_setzero_si512(), units: 0 },
            FoldStates { even: _mm512_setzero_si512(), odd: _mm512_setzero_si512(), units: 0 },
            FoldStates { even: _mm512_setzero_si512(), odd: _mm512_setzero_si512(), units: 0 },
            FoldStates { even: _mm512_setzero_si512(), odd: _mm512_setzero_si512(), units: 0 },
            FoldStates { even: _mm512_setzero_si512(), odd: _mm512_setzero_si512(), units: 0 },
            FoldStates { even: _mm512_setzero_si512(), odd: _mm512_setzero_si512(), units: 0 },
            FoldStates { even: _mm512_setzero_si512(), odd: _mm512_setzero_si512(), units: 0 },
            FoldStates { even: _mm512_setzero_si512(), odd: _mm512_setzero_si512(), units: 0 },
        ];
        for m in 0..8usize {
            if m >= wp {
                break;
            }
            // SAFETY: 128*(m+1) <= 128*wp bytes are in bounds.
            let n0 = _mm512_loadu_si512(p.add(128 * m) as *const _);
            let n1 = _mm512_loadu_si512(p.add(128 * m + 64) as *const _);
            st[m] = FoldStates {
                even: _mm512_unpacklo_epi64(n0, n1),
                odd: _mm512_unpackhi_epi64(n0, n1),
                units: 1,
            };
        }
        // Steps: pair q = 8, 9, ... feeds stream (q-8) % 8 — an unrolled
        // 8-iteration block keeps the mapping division-free; sixteen
        // independent clmul chains run over ONE sequential load stream.
        let mut q = 8usize;
        while q + 7 < wp {
            for m in 0..8usize {
                // SAFETY: 128*(q+m+1) <= 128*wp bytes are in bounds.
                let n0 = _mm512_loadu_si512(p.add(128 * (q + m)) as *const _);
                let n1 = _mm512_loadu_si512(p.add(128 * (q + m) + 64) as *const _);
                fold_step_r(
                    &mut st[m],
                    _mm512_unpacklo_epi64(n0, n1),
                    _mm512_unpackhi_epi64(n0, n1),
                    khi8,
                    klo8,
                );
            }
            q += 8;
        }
        // Tail pairs (0..=6 of them): stream m takes pair q iff q < wp.
        while q < wp {
            let m = q % 8;
            // SAFETY: 128*(q+1) <= 128*wp bytes are in bounds.
            let n0 = _mm512_loadu_si512(p.add(128 * q) as *const _);
            let n1 = _mm512_loadu_si512(p.add(128 * q + 64) as *const _);
            fold_step_r(
                &mut st[m],
                _mm512_unpacklo_epi64(n0, n1),
                _mm512_unpackhi_epi64(n0, n1),
                khi8,
                klo8,
            );
            q += 1;
        }
        // Merge (once per span): base = the stream owning the LAST unit;
        // every other live stream folds in with its offset pair. The
        // merges are mutually independent (each multiplies only the
        // INCOMING stream's state) — 14 independent clmuls + a 3-deep
        // XOR tree, off the hot loop. The offset table is the
        // OFOLD_MG_HI/LO lookup (index c = (wp-1-m) mod 8).
        let base = (wp - 1) % 8;
        let mut acc = st[base];
        for m in 0..8usize {
            if m == base || m >= wp {
                continue;
            }
            let c = (wp - 1 - m) % 8;
            debug_assert!(c >= 1 && c <= 7);
            let khi = _mm512_set1_epi64(OFOLD_MG_HI[c] as i64);
            let klo = _mm512_set1_epi64(OFOLD_MG_LO[c] as i64);
            acc.even = merge_stream(acc.even, st[m].even, khi, klo);
            acc.odd = merge_stream(acc.odd, st[m].odd, khi, klo);
        }
        acc.units = wp;
        acc
    }

    /// R22: the TRI-STREAM (T=3) natural-domain block-pair loop — the
    /// worker drain's default fold shape (the directive's fold512_tri
    /// wiring; the Zen 5 kbench row priced T=3 at 64.90 GB/s vs 59.33
    /// sequential — the sweet spot between chain latency and merge
    /// overhead at the real ~1,344B span mix, where the T=8 merge's 14
    /// extra clmuls LOSE). One span's word-pair units split mod-3 across
    /// THREE independent (even, odd) state pairs — six independent clmul
    /// chains over ONE sequential load stream.
    ///
    /// # The math (the octo kernel's P1' law at k=3)
    ///
    /// Stream m folds block-pairs {m, m+3, m+6, ...}; between its
    /// consecutive units sit TWO units of the other streams, so its step
    /// multiplies the state by K^3 in the ending ring (K = RKLO mod VM =
    /// VR0) — ONE 2-clmul step with the reduced pair (TRI_K3_HI,
    /// TRI_K3_LO) = (K^3 (x) y^64, K^3) = (G[40], G[48]). After T_m
    /// units the stream state is `V_m = sum_t U_{m+3t} (x)
    /// K^(3(T_m-1-t))`; the full single-stream state is `V = sum_m V_m
    /// (x) K^(C_m)` with the offset `C_m = (wp-1-m) mod 3` (the octo
    /// derivation mod 3; the mirror-domain tri's y^256/y^128/y^0 offset
    /// table re-emerging as ring powers). Each merge is ONE 2-clmul step
    /// with the reduced pair (OFOLD_MG{c}_HI, OFOLD_MG{c}_LO) = (K^c (x)
    /// y^64, K^c), c in {1, 2}; the base stream (the one owning the
    /// body's LAST unit) merges by identity, empty streams merge to
    /// zero. The merged states are CLASS-exact (ring congruent to the
    /// sequential kernel's states — the P1' law), so the vend/vtail
    /// composed-field ending stack runs unchanged, with the dfold
    /// dispatch law: vend + vtail-all-r forced for ALL r.
    ///
    /// SAFETY: `p` must hold >= 128*wp bytes; requires the AVX-512 +
    /// VPCLMULQDQ feature contract (callers gate it). `wp >= 1` by the
    /// FOLD_MIN_LEN dispatcher gate.
    #[inline(always)]
    unsafe fn fold_word_triples_r(p: *const u8, wp: usize) -> FoldStates {
        let khi3 = _mm512_set1_epi64(TRI_K3_HI as i64);
        let klo3 = _mm512_set1_epi64(TRI_K3_LO as i64);
        debug_assert!(wp >= 1);
        // Seed stream m with pair m (streams beyond wp stay zero).
        let mut st: [FoldStates; 3] = [
            FoldStates { even: _mm512_setzero_si512(), odd: _mm512_setzero_si512(), units: 0 },
            FoldStates { even: _mm512_setzero_si512(), odd: _mm512_setzero_si512(), units: 0 },
            FoldStates { even: _mm512_setzero_si512(), odd: _mm512_setzero_si512(), units: 0 },
        ];
        for m in 0..3usize {
            if m >= wp {
                break;
            }
            // SAFETY: 128*(m+1) <= 128*wp bytes are in bounds.
            let n0 = _mm512_loadu_si512(p.add(128 * m) as *const _);
            let n1 = _mm512_loadu_si512(p.add(128 * m + 64) as *const _);
            st[m] = FoldStates {
                even: _mm512_unpacklo_epi64(n0, n1),
                odd: _mm512_unpackhi_epi64(n0, n1),
                units: 1,
            };
        }
        // Steps: pair q = 3, 4, 5, ... feeds stream (q-3) % 3 — an
        // unrolled 3-iteration block keeps the mapping division-free;
        // six independent clmul chains run over ONE sequential load
        // stream.
        let mut q = 3usize;
        while q + 2 < wp {
            for m in 0..3usize {
                // SAFETY: 128*(q+m+1) <= 128*wp bytes are in bounds.
                let n0 = _mm512_loadu_si512(p.add(128 * (q + m)) as *const _);
                let n1 = _mm512_loadu_si512(p.add(128 * (q + m) + 64) as *const _);
                fold_step_r(
                    &mut st[m],
                    _mm512_unpacklo_epi64(n0, n1),
                    _mm512_unpackhi_epi64(n0, n1),
                    khi3,
                    klo3,
                );
            }
            q += 3;
        }
        // Tail pairs (0..=1 of them): stream m takes pair q iff q < wp.
        while q < wp {
            let m = q % 3;
            // SAFETY: 128*(q+1) <= 128*wp bytes are in bounds.
            let n0 = _mm512_loadu_si512(p.add(128 * q) as *const _);
            let n1 = _mm512_loadu_si512(p.add(128 * q + 64) as *const _);
            fold_step_r(
                &mut st[m],
                _mm512_unpacklo_epi64(n0, n1),
                _mm512_unpackhi_epi64(n0, n1),
                khi3,
                klo3,
            );
            q += 1;
        }
        // Merge (once per span): base = the stream owning the LAST unit;
        // every other live stream folds in with its offset pair (the
        // OFOLD_MG_HI/LO lookup at c = (wp-1-m) mod 3 in {1, 2}; the
        // base merges by identity). The merges are mutually independent
        // (each multiplies only the INCOMING stream's state) — 4
        // independent clmuls + a 3-deep XOR tree, off the hot loop.
        let base = (wp - 1) % 3;
        let mut acc = st[base];
        for m in 0..3usize {
            if m == base || m >= wp {
                continue;
            }
            let c = (wp - 1 - m) % 3;
            debug_assert!(c >= 1 && c <= 2);
            let khi = _mm512_set1_epi64(OFOLD_MG_HI[c] as i64);
            let klo = _mm512_set1_epi64(OFOLD_MG_LO[c] as i64);
            acc.even = merge_stream(acc.even, st[m].even, khi, klo);
            acc.odd = merge_stream(acc.odd, st[m].odd, khi, klo);
        }
        acc.units = wp;
        acc
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

    /// R17/Route T: the no-unpck block-pair loop over a TRANSPOSED arena
    /// (ROADMAP2 §5.2 — the `fold512_t` kill test). Identical recurrence,
    /// census and value semantics as [`fold_word_pairs_r`]; the only
    /// difference is that the 2 `vpunpckqdq` per 128 B step are GONE —
    /// the arena stores each unit's (even, odd) lane-pure images at bake
    /// time ([`super::transpose_arena_slot`]), so the loop loads them
    /// directly: 2 loads + 4 VPCLMULQDQ + 2 `vpternlogq` per step (p5
    /// census 4). The states are bit-identical to [`fold_word_pairs_r`]
    /// on the same body BY CONSTRUCTION (pure storage permutation — the
    /// Stage-B unpack-free kills do not apply: no unmixing is ever
    /// needed, each load lane IS a lane-pure qword).
    ///
    /// SAFETY: `p` must hold >= 128*wp bytes in the transposed layout;
    /// requires the AVX-512 + VPCLMULQDQ feature contract (callers gate
    /// it).
    #[inline(always)]
    unsafe fn fold_word_pairs_t(p: *const u8, wp: usize) -> FoldStates {
        let khi = _mm512_set1_epi64(RKHI as i64);
        let klo = _mm512_set1_epi64(RKLO as i64);
        if wp == 0 {
            return FoldStates {
                even: _mm512_setzero_si512(),
                odd: _mm512_setzero_si512(),
                units: 0,
            };
        }
        // Prologue: arena unit 0 is the pre-unpacked image of wire pair
        // 0 — seed the states directly.
        // SAFETY: 128 <= 128*wp bytes are in bounds (wp >= 1).
        let mut st = FoldStates {
            even: _mm512_loadu_si512(p as *const _),
            odd: _mm512_loadu_si512(p.add(64) as *const _),
            units: 1,
        };
        for j in 1..wp {
            // SAFETY: 128*(j+1) <= 128*wp bytes are in bounds.
            let u_even = _mm512_loadu_si512(p.add(128 * j) as *const _);
            let u_odd = _mm512_loadu_si512(p.add(128 * j + 64) as *const _);
            fold_step_r(&mut st, u_even, u_odd, khi, klo);
        }
        st
    }

    /// R16: the dual-stream (T=2 block-parity) block-pair loop. Set A
    /// consumes the EVEN blocks, set B the ODD; every state is stepped
    /// every OTHER block, so the four chains (A.even/A.odd/B.even/B.odd)
    /// each get a two-block latency budget — the loop converts from the
    /// measured latency-bound ~9 cyc/step (2 chains vs the 6-cyc clmul
    /// latency) toward the p5-throughput floor (6 p5 uops per 128 B).
    /// The step advances each state by TWO blocks, i.e. multiplies by
    /// K² in the ending ring (K = RKLO mod VM = VR0 — the P1' class
    /// law), realized with the reduced pair (DFOLD_K2_HI, DFOLD_K2_LO).
    ///
    /// The MERGE: the block-parity split leaves the early set deficient
    /// by exactly ONE single-block advance M (ring-mult by K), whose
    /// reduced pair is (VR0, RKHI) — so the merge is literally ONE
    /// [`fold_step_r`] with the OTHER set's states as the injected
    /// units (4 clmul + 2 ternlog, once per span, off the hot loop).
    /// The merged states are ring-CONGRUENT to the sequential kernel's
    /// states (P2: 1778-body differential, scripts/r16_ufold_derive.py),
    /// so the entire vend/vtail ending stack runs unchanged — but the
    /// equality is CLASS-only, which is why dfold dispatch forces the
    /// class endings (see [`super::dfold_enabled`]).
    ///
    /// SAFETY: `p` must hold >= 128*wp bytes; requires the AVX-512 +
    /// VPCLMULQDQ feature contract (callers gate it). `wp >= 1` by the
    /// FOLD_MIN_LEN dispatcher gate.
    #[inline(always)]
    unsafe fn fold_word_pairs_r2(p: *const u8, wp: usize) -> FoldStates {
        let khi2 = _mm512_set1_epi64(DFOLD_K2_HI as i64);
        let klo2 = _mm512_set1_epi64(DFOLD_K2_LO as i64);
        debug_assert!(wp >= 1);
        // Prologue: seed set A with block 0's raw units (the sequential
        // kernel's prologue verbatim).
        // SAFETY: 128*1 <= 128*wp bytes are in bounds (wp >= 1).
        let n0 = _mm512_loadu_si512(p as *const _);
        let n1 = _mm512_loadu_si512(p.add(64) as *const _);
        let mut a = FoldStates {
            even: _mm512_unpacklo_epi64(n0, n1),
            odd: _mm512_unpackhi_epi64(n0, n1),
            units: 1,
        };
        if wp == 1 {
            return a;
        }
        // SAFETY: 128*2 <= 128*wp bytes are in bounds (wp >= 2).
        let n0 = _mm512_loadu_si512(p.add(128) as *const _);
        let n1 = _mm512_loadu_si512(p.add(192) as *const _);
        let mut b = FoldStates {
            even: _mm512_unpacklo_epi64(n0, n1),
            odd: _mm512_unpackhi_epi64(n0, n1),
            units: 1,
        };
        // Main loop: two blocks per iteration, A takes the even one, B
        // the odd — four independent chains, no per-iteration branch.
        let mut j = 2usize;
        while j + 1 < wp {
            // SAFETY: 128*(j+2) <= 128*wp bytes are in bounds.
            let n0 = _mm512_loadu_si512(p.add(128 * j) as *const _);
            let n1 = _mm512_loadu_si512(p.add(128 * j + 64) as *const _);
            fold_step_r(
                &mut a,
                _mm512_unpacklo_epi64(n0, n1),
                _mm512_unpackhi_epi64(n0, n1),
                khi2,
                klo2,
            );
            // SAFETY: 128*(j+2) <= 128*wp bytes are in bounds.
            let n0 = _mm512_loadu_si512(p.add(128 * (j + 1)) as *const _);
            let n1 = _mm512_loadu_si512(p.add(128 * (j + 1) + 64) as *const _);
            fold_step_r(
                &mut b,
                _mm512_unpacklo_epi64(n0, n1),
                _mm512_unpackhi_epi64(n0, n1),
                khi2,
                klo2,
            );
            j += 2;
        }
        if j < wp {
            // wp odd: the last block is even-indexed -> set A (A holds
            // ceil(wp/2) blocks, B floor(wp/2) — the parity bookkeeping
            // the merge below resolves).
            // SAFETY: 128*(j+1) <= 128*wp bytes are in bounds.
            let n0 = _mm512_loadu_si512(p.add(128 * j) as *const _);
            let n1 = _mm512_loadu_si512(p.add(128 * j + 64) as *const _);
            fold_step_r(
                &mut a,
                _mm512_unpacklo_epi64(n0, n1),
                _mm512_unpackhi_epi64(n0, n1),
                khi2,
                klo2,
            );
        }
        // Merge (once per span). The set whose LAST block sits at global
        // index wp-1 is exact; the other is one M short:
        //   wp even: A's blocks end at wp-2 -> V = M(A) ⊕ B
        //   wp odd:  B's blocks end at wp-2 -> V = A ⊕ M(B)
        // The reduced M pair is (khi = RKHI, klo = VR0) — P1'-verified.
        let khim = _mm512_set1_epi64(RKHI as i64);
        let klom = _mm512_set1_epi64(VR0 as i64);
        if wp % 2 == 0 {
            fold_step_r(&mut a, b.even, b.odd, khim, klom);
            a.units = wp;
            a
        } else {
            fold_step_r(&mut b, a.even, a.odd, khim, klom);
            b.units = wp;
            b
        }
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

    /// R15: the vend ending in the xmm domain (one 128-bit field) — the
    /// same Barrett structure as [`vend_zmm`], for lane 0's composed tail
    /// field. Input must be ≤ 95 bits wide (the vend exactness range, the
    /// same contract every vend input satisfies).
    #[inline(always)]
    unsafe fn vend_xmm(v: __m128i) -> u32 {
        let kr0 = _mm_set1_epi64x(VR0 as i64);
        let kh64 = _mm_set1_epi64x(VH64 as i64);
        let kmu = _mm_set1_epi64x(VMU as i64);
        let km = _mm_set1_epi64x(VM as i64);
        let w = _mm_xor_si128(
            _mm_clmulepi64_si128(v, kr0, 0x00),
            _mm_clmulepi64_si128(v, kh64, 0x01),
        );
        let x = _mm_alignr_epi8(w, w, 4);
        let p = _mm_clmulepi64_si128(x, kmu, 0x00);
        let qh = _mm_alignr_epi8(p, p, 7);
        let r = _mm_xor_si128(w, _mm_clmulepi64_si128(qh, km, 0x00));
        let out = _mm_xor_si128(r, _mm_clmulepi64_si128(_mm_alignr_epi8(r, r, 4), km, 0x00));
        _mm_cvtsi128_si32(out) as u32
    }

    /// R15 (vtail): finish one span with LANE 0's post-loop tail absorbed
    /// into its vend input — no scalar lane-0 continuation, no store/reload
    /// extract, no chained crc32. Lanes 1..7 keep the R14 vend shape
    /// (the odd-word crc_u64 chaining is p1 work that runs PARALLEL to the
    /// fold's p5 clmuls — absorbing it into the zmm states measured as a
    /// local regression by moving it onto the saturated p5; the R14 shape
    /// stands). See the table docs above for the algebra;
    /// `scripts/r15_tail_derive.py` is the derivation + the exhaustive
    /// 2419-body differential.
    ///
    /// Per span (typical span: B odd, tail ≈ 16, r = 24):
    /// * lane 0: 2 lift clmuls + ≤9 independent data clmuls + `vend_xmm`
    ///   (replacing the serial extract → fold_extra chain → 2-3 chained
    ///   crc32 ≈ 15-25 serial cyc — the R15 target's ~15-20 cyc).
    /// * r == 0 (len an exact 128-multiple): no tail terms — F0 is the
    ///   pure state lift (R15 shipped the 2-crc ending here for economy;
    ///   the R16 dfold dispatch routes ALL r through this path because
    ///   its states are class-exact only — see `dfold_enabled`).
    /// * lanes 1..7: the R14 path verbatim.
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    unsafe fn finish_span_r_vtail(body: &[u8], st: FoldStates) -> u64 {
        let len = body.len();
        let p = body.as_ptr();
        let blocks = len / 64;
        let tail = len % 64;
        let odd = blocks % 2 == 1;
        // Lane 0's remaining stream length: the unpaired word (B odd) +
        // the tail bytes. (wp >= 1 always — the FOLD_MIN_LEN gate.)
        let r = 8 * (blocks % 2) + tail;

        // ---- lane 0: F0 = (V0_lo ⊗ G[r]) ⊕ (V0_hi ⊗ KH[r]) ⊕ Σ data ----
        // (r <= 71 by construction — the table bounds. The R15 ship gate
        // kept this path at r >= 16 for ECONOMY; the dfold dispatch uses
        // it for all r because the formula is exact on every r — the R15
        // V5 differential verified it below 16 too.)
        debug_assert!(r <= 71);
        let v0 = _mm512_castsi512_si128(st.even);
        let mut f0 = _mm_xor_si128(
            _mm_clmulepi64_si128(v0, _mm_set1_epi64x(super::VTAIL_G[r] as i64), 0x00),
            _mm_clmulepi64_si128(v0, _mm_set1_epi64x(super::VTAIL_KH[r] as i64), 0x01),
        );
        let mut t = r;
        if odd {
            // The unpaired word w = qword 0 of block B-1 (t = r).
            // SAFETY: 64*(B-1)+8 <= len (block B-1 is full; B >= 3 by
            // the FOLD_MIN_LEN gate).
            let w = (p.add(64 * (blocks - 1)) as *const u64).read_unaligned();
            f0 = _mm_xor_si128(
                f0,
                _mm_clmulepi64_si128(
                    _mm_set_epi64x(0, w as i64),
                    _mm_set1_epi64x(super::VTAIL_AT[t] as i64),
                    0x00,
                ),
            );
            t -= 8;
        }
        let mut base = 64 * blocks;
        while t >= 8 {
            // SAFETY: t >= 8 guarantees base+8 <= 64*B + tail = len.
            let q = (p.add(base) as *const u64).read_unaligned();
            f0 = _mm_xor_si128(
                f0,
                _mm_clmulepi64_si128(
                    _mm_set_epi64x(0, q as i64),
                    _mm_set1_epi64x(super::VTAIL_AT[t] as i64),
                    0x00,
                ),
            );
            t -= 8;
            base += 8;
        }
        // The partial byte group (t = r mod 8 bytes): the LAST t bytes of
        // the body, as the high t bytes of the u64 ending at len.
        // SAFETY: len >= 192 > 8 (FOLD_MIN_LEN gate).
        if t > 0 {
            let v = ((p.add(len - 8) as *const u64).read_unaligned()) >> (64 - 8 * t);
            f0 = _mm_xor_si128(
                f0,
                _mm_clmulepi64_si128(
                    _mm_set_epi64x(0, v as i64),
                    _mm_set1_epi64x(super::VTAIL_AT[t] as i64),
                    0x00,
                ),
            );
        }
        let lane0 = vend_xmm(f0);

        // ---- lanes 1..7: the R14 vend path verbatim ----
        let mut lanes = [0u32; 8];
        {
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
        }
        lanes[0] = lane0;
        // Lanes 1..7: the odd-block last words (p1 crc chain, parallel to
        // the fold's p5 work — the R14 economics).
        if odd {
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

    /// R15: the full attribution/rollback dispatch. `vend && vtail` takes
    /// the vectorized-tail path (the default on SPR+); `vend` alone is the
    /// R14 shape (the scalar lane-0 continuation + crc-chain odd words);
    /// otherwise the R13 crc-chain ending.
    ///
    /// The vtail only engages when there is a serial tail chain to
    /// eliminate: r = 8·(B%2) + tail ≥ 16 (the fold_extra unit chain).
    /// For r ≤ 8 the old path's 2-3 chained crc32 (~17-21 cyc, no
    /// fold_extra) beat the composed vend_xmm chain (~22 cyc) AND keep
    /// p1 (not the fold-saturated p5) busy — the measured packed-loop and
    /// sandbox-fabric evidence behind the gate.
    ///
    /// R16: `dfold` overrides the economics gate — the dual-stream states
    /// are CLASS-exact only, so their lane-0 ending MUST be the composed
    /// vtail field (a class formula) for every r; the value-based R13/R14
    /// continuations would read wrong representatives.
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    unsafe fn finish_span_r_inner3(
        body: &[u8],
        st: FoldStates,
        vend: bool,
        vtail: bool,
        dfold: bool,
    ) -> u64 {
        if vend && (vtail || dfold) {
            let blocks = body.len() / 64;
            let r = 8 * (blocks % 2) + body.len() % 64;
            if dfold || r >= 16 {
                return finish_span_r_vtail(body, st);
            }
        }
        finish_span_r_inner(body, st, vend)
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
    /// R21: the ofold axis rides the env gate (HFT_CRC_OFOLD; the dfold
    /// precedence law — ofold wins when both arm).
    ///
    /// # Safety
    /// Requires AVX-512F/BW, VPCLMULQDQ, GFNI, SSE4.2.
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_r(body: &[u8]) -> u64 {
        span_fold_eval_r_forced_o(
            body,
            super::vend_enabled(),
            super::vtail_enabled(),
            super::dfold_enabled(),
            super::ofold_enabled(),
        )
    }

    /// R14: the reflect kernel with an EXPLICIT ending path (the kbench
    /// attribution twin + the differential suite's pin of BOTH paths).
    /// R15: `vtail` adds the third axis — `vend && vtail` runs the
    /// vectorized-tail path, `vend && !vtail` the R14 shape, `!vend` the
    /// R13 crc-chain ending. R16: the dfold axis rides the env gate
    /// (this entry is the back-compat shim; see `_forced_d`).
    ///
    /// # Safety
    /// Same feature contract as [`span_fold_eval_r`].
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_r_forced(body: &[u8], vend: bool, vtail: bool) -> u64 {
        span_fold_eval_r_forced_d(body, vend, vtail, super::dfold_enabled())
    }

    /// R16: the FULL forced path — vend, vtail AND the dual-stream fold
    /// axis explicit (the kbench attribution quad: `fold512_r` =
    /// vend/no-vtail (the R14 shape), `fold512_rv` = vend/vtail (the R15
    /// shape), `fold512_rc` = crc-chain (the R13 rollback), `fold512_rd`
    /// = dfold (the R16 dual-stream shape — forced vend+vtail, all r)).
    /// `dfold && !vend` resolves as dfold OFF (the class-ending
    /// requirement). Values identical on every input.
    ///
    /// # Safety
    /// Same feature contract as [`span_fold_eval_r`].
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_r_forced_d(
        body: &[u8],
        vend: bool,
        vtail: bool,
        dfold: bool,
    ) -> u64 {
        span_fold_eval_r_forced_o(body, vend, vtail, dfold, false)
    }

    /// R21: the FULL forced path — vend, vtail, dfold AND the octo-stream
    /// fold axis explicit (the kbench attribution row `fold512_ro` = ofold —
    /// forced vend+vtail, all r; the T=8 block-parity shape). `ofold &&
    /// !vend` resolves as ofold OFF (the class-ending requirement); ofold
    /// PREEMPTS dfold when both arm (the deeper split — 16 chains vs 4 —
    /// subsumes dfold's chain-depth effect). Values identical on every
    /// input (the R21 P2 differential, 2178 bodies + the Rust sweeps).
    ///
    /// # Safety
    /// Same feature contract as [`span_fold_eval_r`].
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_r_forced_o(
        body: &[u8],
        vend: bool,
        vtail: bool,
        dfold: bool,
        ofold: bool,
    ) -> u64 {
        if body.len() < FOLD_MIN_LEN {
            return span_crc32c_8lane(body);
        }
        let wp = body.len() / 64 / 2;
        let ofold = ofold && vend;
        let dfold = dfold && vend && !ofold;
        // SAFETY: 128*wp <= len (feature contract + caller bounds).
        let st = if ofold {
            fold_word_octets_r(body.as_ptr(), wp)
        } else if dfold {
            fold_word_pairs_r2(body.as_ptr(), wp)
        } else {
            fold_word_pairs_r(body.as_ptr(), wp)
        };
        // The ofold states are CLASS-exact only — the same dispatch law as
        // dfold: the composed-field lane-0 ending for ALL r.
        finish_span_r_inner3(body, st, vend, vtail || dfold || ofold, dfold || ofold)
    }

    /// R22: the natural-domain TRI-STREAM fold kernel (single span) — the
    /// class-exact K^3 shape. Bit-exact with `span_crc32c_8lane` (the P2
    /// differential + the exhaustive sweeps); the tri states are
    /// CLASS-exact, so the composed-field endings run for ALL r — the
    /// dfold/ofold dispatch law. The ofold axis preempts when armed (the
    /// deeper split — the precedence law); the class endings are FORCED
    /// on every fold-class silicon.
    /// R22.1 (the run #455 fleet verdict, 16 draws): this shape as a WORKER
    /// DEFAULT is REJECTED — the forced composed-field endings cost 12-25%
    /// on Zen 5 (same-draw rc 61.6-66.5 GB/s vs tri_r 41.9-45.9), -9..-14%
    /// sustained on the same-draw 11b-vs-11l arms, -2..-6% on Emerald. The
    /// kernel stays in the tree as the class-exact attribution axis (the
    /// kbench `fold512_tri_r` row via [`super::CrcKernel::eval_tri_r`]); it
    /// is NOT a worker default. See docs/34 §6.
    ///
    /// # Safety
    /// Requires AVX-512F/BW, VPCLMULQDQ, GFNI, SSE4.2.
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_tri_r(body: &[u8]) -> u64 {
        if body.len() < FOLD_MIN_LEN {
            return span_crc32c_8lane(body);
        }
        let wp = body.len() / 64 / 2;
        // SAFETY: 128*wp <= len (feature contract + caller bounds).
        let st = if super::ofold_enabled() {
            fold_word_octets_r(body.as_ptr(), wp)
        } else {
            fold_word_triples_r(body.as_ptr(), wp)
        };
        // The tri states are CLASS-exact only — the same dispatch law as
        // dfold/ofold: the composed-field lane-0 ending for ALL r.
        finish_span_r_inner3(body, st, true, true, true)
    }

    /// R17/T-1: the TRANSPOSED-arena eval (the `fold512_t` kbench row —
    /// the Route T kill test, ROADMAP2 §5.2). Fold loop over the arena
    /// image of `body` (no unpck), ending stack over the ORIGINAL wire
    /// body — bit-identical to [`span_fold_eval_r_forced`] by
    /// construction; `t_transpose_arena_parity` pins it.
    ///
    /// # Safety
    /// Same feature contract as [`span_fold_eval_r_forced`]; additionally
    /// `arena` must be readable for `128 * (body.len()/64/2)` bytes and
    /// hold the [`super::transpose_arena_slot`] image of `body`.
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_r_t(
        body: &[u8],
        arena: *const u8,
        vend: bool,
        vtail: bool,
    ) -> u64 {
        if body.len() < FOLD_MIN_LEN {
            return span_crc32c_8lane(body);
        }
        let wp = body.len() / 64 / 2;
        // SAFETY: 128*wp <= len; arena holds the transposed image
        // (caller contract).
        let st = fold_word_pairs_t(arena, wp);
        finish_span_r_inner3(body, st, vend, vtail, false)
    }

    /// R17/I-1: the fold-loop-only floor (`fold512_noend` kbench row) —
    /// the ending stubbed to a state sum. NOT a CRC value.
    ///
    /// # Safety
    /// Same feature contract as [`span_fold_eval_r_forced`].
    #[target_feature(enable = "avx512f,avx512bw,vpclmulqdq,gfni,sse4.2,pclmulqdq")]
    pub unsafe fn span_fold_eval_r_noend(body: &[u8]) -> u64 {
        if body.len() < FOLD_MIN_LEN {
            return span_crc32c_8lane(body);
        }
        let wp = body.len() / 64 / 2;
        // SAFETY: 128*wp <= len (feature contract + caller bounds).
        let st = fold_word_pairs_r(body.as_ptr(), wp);
        let mut e = [0u64; 8];
        let mut o = [0u64; 8];
        _mm512_storeu_si512(e.as_mut_ptr() as *mut _, st.even);
        _mm512_storeu_si512(o.as_mut_ptr() as *mut _, st.odd);
        let mut acc = st.units as u64;
        for v in e.iter().chain(o.iter()) {
            acc ^= v;
        }
        acc
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
        // R16: the dfold axis rides the env gates exactly like the
        // single-span path (both spans take the same shape — the parity
        // bookkeeping is per-span and independent). R21: the ofold axis
        // rides too, preempting dfold when both arm (the dispatch law).
        let vend = super::vend_enabled();
        let ofold = super::ofold_enabled() && vend;
        let dfold = super::dfold_enabled() && vend && !ofold;
        let vtail = super::vtail_enabled() || dfold || ofold;
        // SAFETY: 128*(wpa) <= a.len() (FOLD_MIN_LEN gate).
        let sta = if ofold {
            fold_word_octets_r(a.as_ptr(), a.len() / 64 / 2)
        } else if dfold {
            fold_word_pairs_r2(a.as_ptr(), a.len() / 64 / 2)
        } else {
            fold_word_pairs_r(a.as_ptr(), a.len() / 64 / 2)
        };
        // SAFETY: 128*(wpb) <= b.len().
        let stb = if ofold {
            fold_word_octets_r(b.as_ptr(), b.len() / 64 / 2)
        } else if dfold {
            fold_word_pairs_r2(b.as_ptr(), b.len() / 64 / 2)
        } else {
            fold_word_pairs_r(b.as_ptr(), b.len() / 64 / 2)
        };
        let va = finish_span_r_inner3(a, sta, vend, vtail, dfold || ofold);
        let vb = finish_span_r_inner3(b, stb, vend, vtail, dfold || ofold);
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
    pub unsafe fn span_fold_eval_r_forced(body: &[u8], _vend: bool, _vtail: bool) -> u64 {
        span_crc32c_8lane(body)
    }

    #[inline(always)]
    pub unsafe fn span_fold_eval_r_forced_d(
        body: &[u8],
        _vend: bool,
        _vtail: bool,
        _dfold: bool,
    ) -> u64 {
        span_crc32c_8lane(body)
    }

    #[inline(always)]
    pub unsafe fn span_fold_eval_r_forced_o(
        body: &[u8],
        _vend: bool,
        _vtail: bool,
        _dfold: bool,
        _ofold: bool,
    ) -> u64 {
        span_crc32c_8lane(body)
    }

    #[inline(always)]
    pub unsafe fn span_fold_eval_r_t(
        body: &[u8],
        _arena: *const u8,
        _vend: bool,
        _vtail: bool,
    ) -> u64 {
        span_crc32c_8lane(body)
    }

    #[inline(always)]
    pub unsafe fn span_fold_eval_r_noend(body: &[u8]) -> u64 {
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

    #[inline(always)]
    pub unsafe fn span_fold_eval_tri_r(body: &[u8]) -> u64 {
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

    /// R17/T-1: the PUBLIC transposed-arena API (`CrcKernel::eval_rpath_t`)
    /// mirrors `eval_rpath3` on the span-class battery (the fabric-shaped
    /// lengths: MTU-class 1344, power boundaries, FOLD_MIN_LEN edges), and
    /// the noend floor row is smoke-checked (state-sum value, not a CRC —
    /// determinism + non-panic only, by design).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn t_transpose_arena_public_api() {
        if !fold512_available() {
            eprintln!("(fold512 unavailable on this CPU — transpose API test skipped)");
            return;
        }
        let kernel = CrcKernel::Reflect;
        let mut body = [0u8; 4200];
        let mut seed = 0x0E12_578C_31A4_90F7u64;
        let mut next = || {
            seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let mut arena = [0u8; 128 * 34];
        for len in [
            0usize, 1, 63, 64, 65, 127, 128, 129, 191, 192, 193, 255, 256, 383, 384, 512, 1000,
            1344, 1345, 1408, 2047, 2048, 2049, 3000, 4095, 4199,
        ] {
            for (i, e) in body[..len].iter_mut().enumerate() {
                *e = (next() >> ((i % 8) * 8)) as u8;
            }
            let b = &body[..len];
            transpose_arena_slot(b, &mut arena);
            for (vend, vtail) in [(true, false), (true, true), (false, false)] {
                unsafe {
                    let want = kernel.eval_rpath3(b, vend, vtail);
                    let got = kernel.eval_rpath_t(b, arena.as_ptr(), vend, vtail);
                    assert_eq!(
                        got, want,
                        "public transpose parity break: len={len} vend={vend} vtail={vtail}"
                    );
                }
            }
            // The noend floor row: deterministic, non-panic (state sum —
            // NOT a CRC value; equality with the CRC is not asserted by
            // design).
            let a = unsafe { kernel.eval_rpath_noend(b) };
            let b2 = unsafe { kernel.eval_rpath_noend(b) };
            assert_eq!(a, b2, "noend row not deterministic at len={len}");
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
            let grv = unsafe { imp::span_fold_eval_r_forced(body, true, false) };
            assert_eq!(want, grv, "reflect vend ending diverged at len={}", body.len());
            let grc = unsafe { imp::span_fold_eval_r_forced(body, false, false) };
            assert_eq!(want, grc, "reflect crc-chain ending diverged at len={}", body.len());
            // R15: the vectorized-tail path (vend + vtail), independent of
            // the HFT_CRC_VEND / HFT_CRC_VTAIL defaults.
            let gvt = unsafe { imp::span_fold_eval_r_forced(body, true, true) };
            assert_eq!(
                want, gvt,
                "reflect vtail diverged at len={}",
                body.len()
            );
            // R16: the dual-stream fold (dfold) — forced vend + vtail-all-r
            // (the class-ending shape), independent of the HFT_CRC_DFOLD
            // default. Same value on every body (the P2 differential).
            let gd = unsafe { imp::span_fold_eval_r_forced_d(body, true, true, true) };
            assert_eq!(want, gd, "reflect dfold diverged at len={}", body.len());
            // R21: the octo-stream fold (ofold) — forced vend + vtail-all-r
            // (the class-ending shape), independent of the HFT_CRC_OFOLD
            // default. Same value on every body (the P2 differential).
            let go = unsafe { imp::span_fold_eval_r_forced_o(body, true, true, false, true) };
            assert_eq!(want, go, "reflect ofold diverged at len={}", body.len());
            // R21: ofold PREEMPTS dfold when both arm (the dispatch law) —
            // still the same value.
            let god = unsafe { imp::span_fold_eval_r_forced_o(body, true, true, true, true) };
            assert_eq!(want, god, "reflect ofold+dfold diverged at len={}", body.len());
            // R22: the natural-domain TRI-STREAM fold (the worker drain's
            // default shape) — forced vend + vtail-all-r (the class-ending
            // shape). Same value on every body (the P2 differential).
            let gt3 = unsafe { imp::span_fold_eval_tri_r(body) };
            assert_eq!(want, gt3, "reflect tri_r diverged at len={}", body.len());
            // R17/T-1: the transposed-arena twin (Route T, ROADMAP2 §5.2) —
            // identical value on every body and both ending shapes, by
            // construction (pure storage permutation; the ending reads the
            // ORIGINAL wire body).
            {
                let mut arena = [0u8; 128 * 34];
                super::transpose_arena_slot(body, &mut arena);
                let gt = unsafe { imp::span_fold_eval_r_t(body, arena.as_ptr(), true, false) };
                assert_eq!(want, gt, "transpose twin diverged at len={}", body.len());
                let gtt = unsafe { imp::span_fold_eval_r_t(body, arena.as_ptr(), true, true) };
                assert_eq!(want, gtt, "transpose twin (vtail) diverged at len={}", body.len());
                let gtc = unsafe { imp::span_fold_eval_r_t(body, arena.as_ptr(), false, false) };
                assert_eq!(want, gtc, "transpose twin (crc-chain) diverged at len={}", body.len());
            }
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

    /// R16: re-derive the dual-stream fold constants at test time in the
    /// ENDING ring GF(2)[y]/VM (scripts/r16_ufold_derive.py's P1'/P2, the
    /// 1778-body differential). Pins the whole dfold algebra:
    ///   * P1' class law: the R13 step is ring multiplication by
    ///     K = RKLO mod VM, and K == VR0 (the ending seed re-emerging);
    ///   * RKHI == K ⊗ y^64 (the merge pair IS (VR0, RKHI));
    ///   * the M² pair: DFOLD_K2_LO == K², DFOLD_K2_HI == K² ⊗ y^64;
    ///   * spot check of the law itself on one-hot states.
    /// A transcription typo in either constant cannot survive.
    #[test]
    fn t_dfold_constants_derivation() {
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
        fn clmod(mut v: u128) -> u128 {
            while v >= (1 << 32) {
                let sh = (128 - v.leading_zeros() as i32 - 33) as u32;
                v ^= (VM as u128) << sh;
            }
            v
        }
        fn rmul(a: u64, b: u64) -> u64 {
            clmod(clmul(a, b)) as u64
        }
        fn ypow(mut e: u32) -> u64 {
            let mut r = 1u64;
            let mut base = 2u64;
            while e != 0 {
                if e & 1 != 0 {
                    r = rmul(r, base);
                }
                base = rmul(base, base);
                e >>= 1;
            }
            r
        }
        // P1': K = RKLO mod VM == VR0 (the fold advance and the ending
        // seed are the same ring element — the R13/R14 convergence).
        let k = clmod(RKLO as u128) as u64;
        assert_eq!(k, VR0, "K = RKLO mod VM must equal VR0");
        // RKHI == K ⊗ y^64 — the merge pair is (VR0, RKHI) itself.
        let y64 = ypow(64);
        assert_eq!(rmul(k, y64), RKHI, "RKHI != K (x) y^64");
        // The M² step pair.
        let k2 = rmul(k, k);
        assert_eq!(k2, DFOLD_K2_LO, "DFOLD_K2_LO != K^2 mod VM");
        assert_eq!(rmul(k2, y64), DFOLD_K2_HI, "DFOLD_K2_HI != K^2 (x) y^64");
        // The class law on one-hot states: clmod(M(V)) == rmul(clmod(V), K)
        // with M(V) = clmul(V_hi, RKHI) ^ clmul(V_lo, RKLO).
        for bit in [0u32, 1, 31, 32, 63, 64, 95, 96, 127] {
            let v = 1u128 << bit;
            let m = clmul((v >> 64) as u64, RKHI) ^ clmul(v as u64, RKLO);
            assert_eq!(
                clmod(m),
                clmod(clmul(clmod(v) as u64, k)),
                "P1' class law broken at state bit {bit}"
            );
        }
    }

    /// R21: re-derive the octo-stream fold constants at test time (the
    /// ending ring GF(2)[y]/VM — scripts/r21_ofold_derive.py's P0/P1, the
    /// 2178-body differential). Pins the whole ofold algebra:
    ///   * the G-table extension: G[r] = Z_r(1) (the r-byte zeros-update
    ///     of the reflected CRC32C LFSR) reproduced for r = 0..=128, with
    ///     the shipped VTAIL_G[0..72] and OFOLD_G_EXT[72..=128] pinned
    ///     against the recurrence;
    ///   * the merge/step pairs: OFOLD_MG{c}_LO == K^c == G[16c],
    ///     OFOLD_MG{c}_HI == K^c (x) y^64 == G[16c-8] for c = 1..7, and
    ///     the T=8 step pair (OFOLD_K8_HI, OFOLD_K8_LO) == (G[120],
    ///     G[128]);
    ///   * the P1' class law at the octo power on one-hot states: ONE
    ///     K^8-pair application == EIGHT stepwise R13 advances.
    /// A transcription typo in any constant cannot survive.
    #[test]
    fn t_ofold_constants_derivation() {
        // The reflected-CRC32C byte table (same construction as ref_crc32c).
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
        let t = table();
        // G[r] = the seed-1 state advanced by r zero bytes.
        let mut g = [0u64; 129];
        g[0] = 1;
        for r in 1..=128 {
            let prev = g[r - 1] as u32;
            g[r] = ((prev >> 8) ^ t[(prev & 0xFF) as usize]) as u64;
        }
        // The shipped tables pin the recurrence.
        for r in 0..72 {
            assert_eq!(g[r], VTAIL_G[r] as u64, "G[{r}] != VTAIL_G[{r}]");
        }
        for i in 0..57 {
            assert_eq!(g[72 + i], OFOLD_G_EXT[i] as u64, "G[{}] != OFOLD_G_EXT", 72 + i);
        }
        // The ring helpers (GF(2)[y]/VM).
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
        fn clmod(mut v: u128) -> u128 {
            while v >= (1 << 32) {
                let sh = (128 - v.leading_zeros() as i32 - 33) as u32;
                v ^= (VM as u128) << sh;
            }
            v
        }
        fn rmul(a: u64, b: u64) -> u64 {
            clmod(clmul(a, b)) as u64
        }
        fn ypow(mut e: u32) -> u64 {
            let mut r = 1u64;
            let mut base = 2u64;
            while e != 0 {
                if e & 1 != 0 {
                    r = rmul(r, base);
                }
                base = rmul(base, base);
                e >>= 1;
            }
            r
        }
        let y64 = ypow(64);
        let k = clmod(RKLO as u128) as u64;
        assert_eq!(k, VR0, "K = RKLO mod VM must equal VR0");
        // Every merge pair IS (K^c (x) y^64, K^c) AND the G-table powers.
        let mut kk = 1u64;
        for c in 1..=7usize {
            kk = rmul(kk, k);
            assert_eq!(OFOLD_MG_LO[c], kk, "OFOLD_MG{c}_LO != K^{c}");
            assert_eq!(OFOLD_MG_LO[c], g[16 * c], "OFOLD_MG{c}_LO != G[{}]", 16 * c);
            assert_eq!(
                OFOLD_MG_HI[c],
                rmul(kk, y64),
                "OFOLD_MG{c}_HI != K^{c} (x) y^64"
            );
            assert_eq!(OFOLD_MG_HI[c], g[16 * c - 8], "OFOLD_MG{c}_HI != G[{}]", 16 * c - 8);
        }
        // The T=8 step pair: K^8 = G[128], K^8 (x) y^64 = G[120].
        kk = rmul(kk, k);
        assert_eq!(OFOLD_K8_LO, kk, "OFOLD_K8_LO != K^8");
        assert_eq!(OFOLD_K8_LO, g[128], "OFOLD_K8_LO != G[128]");
        assert_eq!(OFOLD_K8_HI, g[120], "OFOLD_K8_HI != G[120]");
        assert_eq!(OFOLD_K8_HI, rmul(kk, y64), "OFOLD_K8_HI != K^8 (x) y^64");
        // P1' at the octo power, one-hot states: ONE reduced K^8-pair
        // application == EIGHT stepwise R13 advances, in class terms.
        for bit in [0u32, 1, 31, 32, 63, 64, 95, 96, 127] {
            let v = 1u128 << bit;
            let got = clmod(clmul((v >> 64) as u64, OFOLD_K8_HI) ^ clmul(v as u64, OFOLD_K8_LO));
            let mut cls = clmod(v) as u64;
            for _ in 0..8 {
                cls = rmul(cls, k);
            }
            assert_eq!(
                got,
                cls as u128,
                "P1' octo class law broken at state bit {bit}"
            );
        }
        // R22: the T=3 tri-stream step pair re-emerges as the octo offset-3
        // merge pair (the k=3 entry of the SAME law) — the natural-domain
        // tri kernel's step constants, pinned here end to end.
        assert_eq!(TRI_K3_HI, OFOLD_MG3_HI, "TRI_K3_HI != OFOLD_MG3_HI");
        assert_eq!(TRI_K3_LO, OFOLD_MG3_LO, "TRI_K3_LO != OFOLD_MG3_LO");
        assert_eq!(TRI_K3_LO, g[48], "TRI_K3_LO != G[48]");
        assert_eq!(TRI_K3_HI, g[40], "TRI_K3_HI != G[40]");
        // P1' at the tri power, one-hot states: ONE reduced K^3-pair
        // application == THREE stepwise R13 advances, in class terms.
        for bit in [0u32, 1, 31, 32, 63, 64, 95, 96, 127] {
            let v = 1u128 << bit;
            let got = clmod(clmul((v >> 64) as u64, TRI_K3_HI) ^ clmul(v as u64, TRI_K3_LO));
            let mut cls = clmod(v) as u64;
            for _ in 0..3 {
                cls = rmul(cls, k);
            }
            assert_eq!(
                got,
                cls as u128,
                "P1' tri class law broken at state bit {bit}"
            );
        }
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

    /// R15: pin the vtail (vectorized-tail) tables from the ground up —
    /// `scripts/r15_tail_derive.py` is the derivation; this re-derives
    /// every entry independently at test time so a transcription typo in
    /// any of the 216 constants cannot survive:
    ///
    /// 1. `G[r]` — the r-byte zeros-update of a 1-seed, computed through
    ///    an independent table-driven reference (NOT the shipped kernel);
    ///    plus the ring law `G[m] ⊗ y^(8m) == 1` (positive powers only).
    /// 2. `KH[r]` — `G[r] ⊗ y^64 mod VM` (the pre-reduced structural
    ///    y^64; KH[8] == 1 pins the lanes-1..7 shortcut).
    /// 3. `AT[t]` — `y^(128-8t) mod VM` via the law `AT[t] ⊗ y^(8t) ==
    ///    y^128` (all positive powers — no inverses anywhere).
    #[test]
    fn t_vtail_constants_derivation() {
        // independent reflected-CRC32C table (same construction as the
        // reference in t_vend_constants_derivation)
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
        // 1. G[r]: the zeros-update of 1 by r bytes.
        let mut c = 1u32;
        for r in 0..72usize {
            assert_eq!(c, VTAIL_G[r], "VTAIL_G[{r}] re-derivation");
            c = (c >> 8) ^ t[(c & 0xFF) as usize];
        }
        // ring helpers (positive powers only)
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
        fn rmul(a: u128, b: u128) -> u128 {
            let mut v = clmul(a, b);
            while v >= (1u128 << 32) {
                let sh = (128 - v.leading_zeros()) - 33;
                v ^= (VM as u128) << sh;
            }
            v
        }
        let y = |e: u32| -> u128 {
            let mut r = 1u128;
            let mut base = 2u128;
            let mut e = e;
            while e != 0 {
                if e & 1 != 0 {
                    r = rmul(r, base);
                }
                base = rmul(base, base);
                e >>= 1;
            }
            r
        };
        // G[m] ⊗ y^(8m) == 1
        for m in 1..72usize {
            assert_eq!(
                rmul(VTAIL_G[m] as u128, y(8 * m as u32)),
                1,
                "G[{m}] == y^(-8m)"
            );
        }
        // 2. KH[r] == G[r] ⊗ y^64; the KH[8] == 1 anchor.
        let y64 = y(64);
        for r in 0..72usize {
            assert_eq!(
                VTAIL_KH[r] as u128,
                rmul(VTAIL_G[r] as u128, y64),
                "KH[{r}] == G[{r}] * y^64"
            );
        }
        assert_eq!(VTAIL_KH[8], 1, "KH[8] anchor (lanes 1..7 raw hi qword)");
        // 3. AT[t] ⊗ y^(8t) == y^128 (t >= 1), i.e. AT[t] == y^(128-8t);
        //    plus the structural anchors AT[16] == 1 and AT[8] == y^64.
        let y128 = y(128);
        for t in 1..72usize {
            assert_eq!(
                rmul(VTAIL_AT[t] as u128, y(8 * t as u32)),
                y128,
                "AT[{t}] == y^(128-8t)"
            );
        }
        assert_eq!(VTAIL_AT[16], 1, "AT[16] anchor");
        assert_eq!(VTAIL_AT[8] as u128, y64, "AT[8] == y^64 anchor");
        assert_eq!(VTAIL_G[8] as u128, VH64 as u128, "G[8] == VH64 anchor");
        assert_eq!(VTAIL_G[16] as u128, VR0 as u128, "G[16] == VR0 anchor");
    }

    /// R23: the affine span-subtraction derivation oracle
    /// (`scripts/r23_affine_crc_derive.py`). Pins the whole algebra:
    ///   * **table re-derivation**: the reflected-CRC32C zeros-advance of
    ///     the 1-seed extended to `G[0..=2048]` (the same recurrence the
    ///     ofold test uses), with the shipped `AFFINE_POW_*` tables pinned
    ///     entry-for-entry against it and against `VTAIL_G` — a
    ///     transcription typo cannot survive;
    ///   * **algebraic identities**: the inverse-power law `G[r] ⊗ y^(8r)
    ///     == 1`, the composition law `C(16k + r) == T128[k-1] ⊗ TBYTE[r]`
    ///     for EVERY L ∈ 0..=2048, the shipped-constant anchors (T128[1]
    ///     == VR0, T128[2] == DFOLD_K2_LO, T128[8] == OFOLD_K8_LO,
    ///     TBYTE[8] == VH64), and the width audit (every constant ≤ 32
    ///     bits — products never overflow the 64/128-bit register halves);
    ///   * **THE LAW, differentially**: 4,096 randomized synthetic spans
    ///     (deterministic PRNG, stack buffers, L_B ∈ [16, 2048], L_A ∈
    ///     [0, 512], boundary-aligned edges, 5 byte patterns) — Method A
    ///     (the reference hardware-reflected CRC32C scan of the raw span
    ///     bytes) vs Method B (the ingest snapshots `raw(A)`,
    ///     `raw(A∥B)` + the O(1) affine projection), for BOTH the raw
    ///     registers and the full CRC32C values (the I=F cancellation);
    ///   * **the 8-lane bridge**: 256 64-byte-aligned spans reconstruct
    ///     the exact `span_crc32c_8lane` lane values (and the FNV-1a
    ///     combine) as per-lane O(1) projections of the cumulative 8-lane
    ///     snapshots — cross-checked against the production kernel on
    ///     x86_64;
    ///   * **hardware anchors**: CRC32C("123456789") == 0xE3069283 and
    ///     the software scan == the `_mm_crc32_*` instruction chain.
    #[test]
    fn t_affine_span_derivation_oracle() {
        // ── the ring helpers (GF(2)[y]/VM) ──
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
        fn clmod(mut v: u128) -> u128 {
            while v >= (1u128 << 32) {
                let sh = (128 - v.leading_zeros()) - 33;
                v ^= (VM as u128) << sh;
            }
            v
        }
        fn rmul(a: u64, b: u64) -> u64 {
            clmod(clmul(a, b)) as u64
        }
        fn ypow(mut e: u32) -> u64 {
            let mut r = 1u64;
            let mut base = 2u64;
            while e != 0 {
                if e & 1 != 0 {
                    r = rmul(r, base);
                }
                base = rmul(base, base);
                e >>= 1;
            }
            r
        }
        // C(L) = y^(-8L) mod VM composed from the shipped tables.
        fn c_of(l: usize) -> u64 {
            let (k, r) = (l / 16, l % 16);
            if k == 0 {
                AFFINE_POW_BYTE_TABLE[r]
            } else {
                rmul(AFFINE_POW_128B_TABLE[k - 1], AFFINE_POW_BYTE_TABLE[r])
            }
        }

        // ── the reflected-CRC32C byte table + the seeded raw scan ──
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
        let t = table();
        fn crc_scan(mut c: u32, t: &[u32; 256], buf: &[u8]) -> u32 {
            for &b in buf {
                c = (c >> 8) ^ t[((c ^ b as u32) & 0xFF) as usize];
            }
            c
        }

        // ── 1. G[0..=2048] re-derivation + shipped-table pinning ──
        let mut g = [0u64; 2049];
        g[0] = 1;
        {
            let mut c = 1u32;
            for r in 1..=2048usize {
                c = (c >> 8) ^ t[(c & 0xFF) as usize];
                g[r] = c as u64;
            }
        }
        for k in 1..=128usize {
            assert_eq!(
                AFFINE_POW_128B_TABLE[k - 1],
                g[16 * k],
                "AFFINE_POW_128B_TABLE[{k}] != G[{}]",
                16 * k
            );
        }
        for r in 0..16usize {
            assert_eq!(AFFINE_POW_BYTE_TABLE[r], g[r], "AFFINE_POW_BYTE_TABLE[{r}] != G[{r}]");
        }
        for r in 0..72usize {
            assert_eq!(VTAIL_G[r] as u64, g[r], "G[{r}] drift vs shipped VTAIL_G");
        }
        // the shipped-constant anchors: the table IS the K-power ladder
        assert_eq!(AFFINE_POW_128B_TABLE[0], VR0, "T128[1] == VR0 (G[16])");
        assert_eq!(AFFINE_POW_128B_TABLE[1], DFOLD_K2_LO, "T128[2] == DFOLD_K2_LO (G[32])");
        assert_eq!(AFFINE_POW_128B_TABLE[7], OFOLD_K8_LO, "T128[8] == OFOLD_K8_LO (G[128])");
        assert_eq!(AFFINE_POW_BYTE_TABLE[8], VH64, "TBYTE[8] == VH64 (G[8])");
        // the inverse-power law at probes
        for r in [1usize, 8, 16, 128, 1000, 2048] {
            assert_eq!(rmul(g[r], ypow(8 * r as u32)), 1, "G[{r}] == y^(-8r)");
        }
        // the composition law for EVERY length in 0..=2048
        for l in 0..=2048usize {
            assert_eq!(c_of(l), g[l], "composition C({l}) != G[{l}]");
        }
        // the width audit: degree never overflows the register halves
        for v in AFFINE_POW_128B_TABLE.iter().chain(AFFINE_POW_BYTE_TABLE.iter()) {
            assert!(*v < (1u64 << 32), "constant {v:#x} exceeds 32 bits");
        }

        // ── 2. THE LAW, differentially: 4,096 randomized synthetic spans ──
        struct Sm(u64);
        impl Sm {
            fn next(&mut self) -> u64 {
                self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            }
        }
        fn fill(buf: &mut [u8], sm: &mut Sm, pat: u32) {
            match pat % 5 {
                0 => buf.fill(0x00),
                1 => buf.fill(0xFF),
                2 => {
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = ((i * 131 + 17) & 0xFF) as u8;
                    }
                }
                3 => {
                    for b in buf.iter_mut() {
                        *b = sm.next() as u8;
                    }
                }
                _ => {
                    let mut i = 0usize;
                    while i < buf.len() {
                        let run = (buf.len() - i).min(1 + (sm.next() as usize & 0x3F));
                        let v = sm.next() as u8;
                        buf[i..i + run].fill(v);
                        i += run;
                    }
                }
            }
        }
        const EDGE_LB: [usize; 27] = [
            16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 191, 192, 255, 256, 257, 511, 512,
            1023, 1024, 1025, 1343, 1344, 1345, 1536, 2047, 2048,
        ];
        const EDGE_LA: [usize; 17] = [0, 1, 7, 8, 15, 16, 17, 63, 64, 65, 127, 128, 129, 255, 256, 511, 512];
        let mut sm = Sm(0x9E37_79B9_7F4A_7C15 ^ 0x523A_F11E);
        let mut a_buf = [0u8; 512];
        let mut b_buf = [0u8; 2048];
        let mut errs = 0usize;
        let mut full_errs = 0usize;
        for i in 0..4096usize {
            let lb = if i < 2 * EDGE_LB.len() {
                EDGE_LB[i % EDGE_LB.len()]
            } else if i % 7 == 0 {
                EDGE_LB[(sm.next() % EDGE_LB.len() as u64) as usize]
            } else {
                16 + (sm.next() % 2033) as usize
            };
            let la = if i < 2 * EDGE_LA.len() {
                EDGE_LA[i % EDGE_LA.len()]
            } else {
                (sm.next() % 513) as usize
            };
            let pat = (i % 5) as u32;
            fill(&mut a_buf[..la], &mut sm, pat);
            fill(&mut b_buf[..lb], &mut sm, pat);
            // Method A — the reference hardware-reflected CRC32C scan of
            // the raw span bytes (ground truth; ref_crc32c cross-check below)
            let want = crc_scan(0, &t, &b_buf[..lb]);
            let full_want = crc_scan(0xFFFF_FFFF, &t, &b_buf[..lb]) ^ 0xFFFF_FFFF;
            // Method B — the ingest snapshots + the O(1) affine projection
            let raw_a = crc_scan(0, &t, &a_buf[..la]);
            let raw_ab = crc_scan(raw_a, &t, &b_buf[..lb]);
            let c_lb = c_of(lb);
            let got = (raw_ab as u64 ^ rmul(raw_a as u64, c_lb)) as u32;
            // the full-CRC32C variant (init 0xFFFFFFFF / final xor)
            let state_a = crc_scan(0xFFFF_FFFF, &t, &a_buf[..la]);
            let state_ab = crc_scan(state_a, &t, &b_buf[..lb]);
            let full_a = state_a ^ 0xFFFF_FFFF;
            let full_ab = state_ab ^ 0xFFFF_FFFF;
            let full_got = (full_ab as u64 ^ rmul(full_a as u64, c_lb)) as u32;
            if got != want {
                errs += 1;
            }
            if full_got != full_want {
                full_errs += 1;
            }
        }
        assert_eq!(errs, 0, "raw affine span subtraction: {errs}/4096 differential errors");
        assert_eq!(
            full_errs, 0,
            "full-CRC32C affine span subtraction: {full_errs}/4096 differential errors"
        );
        // the shared reference-kernel cross-check + the raw golden vector
        assert_eq!(crc_scan(0, &t, b"123456789"), ref_crc32c(b"123456789"));
        assert_eq!(crc_scan(0, &t, b"123456789"), 0x58E3_FA20);

        // ── 3. the 8-lane bridge: span_crc32c_8lane in O(1) for aligned spans ──
        fn fnv_lanes(lanes: &[u32; 8], ln: usize) -> u64 {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for &c in lanes {
                h ^= c as u64;
                h = h.wrapping_mul(0x0100_0000_01B3);
            }
            h ^= (ln as u32) as u64;
            h.wrapping_mul(0x0100_0000_01B3)
        }
        let mut pkt = [0u8; 64 * 40];
        for i in 0..256usize {
            let lb = 64 * (1 + (sm.next() % 32) as usize);
            let la = 64 * ((sm.next() % 9) as usize);
            fill(&mut pkt[..la + lb], &mut sm, (i % 5) as u32);
            let mut cl_start = [0u32; 8];
            let mut cl_end = [0u32; 8];
            let na = la / 64;
            let nab = (la + lb) / 64;
            for j in 0..nab {
                let base = 64 * j;
                for k in 0..8usize {
                    let w = &pkt[base + 8 * k..base + 8 * k + 8];
                    cl_end[k] = crc_scan(cl_end[k], &t, w);
                    if j < na {
                        cl_start[k] = crc_scan(cl_start[k], &t, w);
                    }
                }
            }
            // the lane's B-run is L_B/64 words = L_B/8 bytes of lane stream
            let adv = g[lb / 8];
            let mut pred = [0u32; 8];
            for k in 0..8usize {
                pred[k] = (cl_end[k] as u64 ^ rmul(cl_start[k] as u64, adv)) as u32;
            }
            let bb = &pkt[la..la + lb];
            let mut direct = [0u32; 8];
            for j in 0..(lb / 64) {
                let base = 64 * j;
                for k in 0..8usize {
                    direct[k] = crc_scan(direct[k], &t, &bb[base + 8 * k..base + 8 * k + 8]);
                }
            }
            assert_eq!(pred, direct, "bridge per-lane projection i={i}");
            let v = fnv_lanes(&pred, lb);
            // the software 8-lane model == the production kernel on x86_64
            #[cfg(target_arch = "x86_64")]
            assert_eq!(
                v,
                crate::sink::span_crc32c_8lane(bb),
                "bridge span_crc32c_8lane value i={i}"
            );
        }

        // ── 4. hardware anchors (x86_64: the instruction chain is truth) ──
        #[cfg(target_arch = "x86_64")]
        {
            use std::arch::x86_64::*;
            let mut c: u32 = 0xFFFF_FFFF;
            c = unsafe { _mm_crc32_u64(c as u64, u64::from_le_bytes(*b"12345678")) } as u32;
            c = unsafe { _mm_crc32_u8(c, b'9') };
            assert_eq!(c ^ 0xFFFF_FFFF, 0xE306_9283, "hw anchor CRC32C(\"123456789\")");
            // the instruction chain == the software table scan (raw semantics)
            let mut buf = [0u8; 300];
            for _ in 0..40 {
                for b in buf.iter_mut() {
                    *b = sm.next() as u8;
                }
                let n = (sm.next() as usize) % 301;
                let data = &buf[..n];
                let mut hwc: u32 = 0;
                let mut i = 0usize;
                while i + 8 <= n {
                    let w: [u8; 8] = data[i..i + 8].try_into().unwrap();
                    hwc = unsafe { _mm_crc32_u64(hwc as u64, u64::from_le_bytes(w)) } as u32;
                    i += 8;
                }
                if i + 4 <= n {
                    let w: [u8; 4] = data[i..i + 4].try_into().unwrap();
                    hwc = unsafe { _mm_crc32_u32(hwc, u32::from_le_bytes(w)) };
                    i += 4;
                }
                if i + 2 <= n {
                    let w: [u8; 2] = data[i..i + 2].try_into().unwrap();
                    hwc = unsafe { _mm_crc32_u16(hwc, u16::from_le_bytes(w)) };
                    i += 2;
                }
                if i < n {
                    hwc = unsafe { _mm_crc32_u8(hwc, data[i]) };
                }
                assert_eq!(hwc, crc_scan(0, &t, data), "hw chain != software raw scan");
            }
        }
    }

    /// R23b: pin the SHIPPED hardware kernel against the portable u128
    /// model — `clmul_reduce_mod_vm` (the PCLMULQDQ plain Barrett on
    /// x86_64) vs `clmul_reduce_mod_vm_sw`, basis-exhaustive over the
    /// one-hot operands (GF(2)-linear => complete) plus random pairs.
    /// The K0 structure (scripts/r23b_affine_kernel_derive.py) pinned the
    /// model itself against the shift-subtract ground truth.
    #[test]
    fn t_affine_kernel_models() {
        let basis = [0u64, 1, 2, 4, 1 << 8, 1 << 15, 1 << 16, 1 << 24, 1 << 31];
        let mut pairs: Vec<(u64, u64)> = Vec::new();
        for &a in &basis {
            for &b in &basis {
                pairs.push((a, b));
            }
        }
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        for _ in 0..4096 {
            pairs.push((next() >> 32, next() >> 32)); // 32-bit operands
        }
        for (a, b) in pairs {
            // SAFETY: width law — a, b < 2^32 by construction.
            let hw = unsafe { clmul_reduce_mod_vm(a, b) };
            let sw = clmul_reduce_mod_vm_sw(a, b);
            assert_eq!(hw, sw, "kernel model drift at a={a:#x} b={b:#x}");
        }
    }

    /// R23c (the pipeline wiring's bit-exactness pin): the rxdesc tag
    /// kernel — `nf_transport::rxdesc::affine_frame_tag` (the RX ledger
    /// fill AND the sink's fallback both produce tags through it) —
    /// against the reference `span_crc32c_8lane` and a table-driven raw
    /// CRC32C scan, over an edge-length sweep + pattern mix. Pins ALL
    /// four facts: (1) the lane registers reproduce the reference value
    /// through `span_crc32c_8lane_from_tags`; (2) the (prefix, cum)
    /// mid-boundary pair equals the direct scans; (3) the raw_crc target
    /// equals the independent second-half scan AND Engineer 2's scalar
    /// affine projection (the worker's integrity check's exactness);
    /// (4) for 64-multiples, the tag lanes through Engineer 2's
    /// 8-lane affine kernel (zero prefix) give the same value.
    #[test]
    fn t_r23c_tag_core() {
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
        let t = table();
        fn crc_scan(mut c: u32, t: &[u32; 256], buf: &[u8]) -> u32 {
            for &b in buf {
                c = (c >> 8) ^ t[((c ^ b as u32) & 0xFF) as usize];
            }
            c
        }
        struct Sm(u64);
        impl Sm {
            fn next(&mut self) -> u64 {
                self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            }
        }
        let mut sm = Sm(0x243F_6A88_85A3_08D3 ^ 0x1319_8A2E);
        // The pipeline's real span shape first (edge lengths around the
        // MTU-1400 bodies: 1217..=1380), then the structural edges.
        let mut lens: Vec<usize> = (1217..=1380).collect();
        lens.extend([0, 1, 2, 7, 8, 9, 15, 16, 17, 63, 64, 65, 127, 128, 129, 255, 256, 257, 511, 512, 1023, 1024, 1344, 2047, 2048]);
        let mut body = vec![0u8; 2048];
        for (i, &len) in lens.iter().enumerate() {
            // Deterministic pattern mix (the Sm stream + per-case fills).
            match i % 5 {
                0 => body[..len].fill(0xA5),
                1 => body[..len].fill(0x5A),
                2 => {
                    for (j, b) in body[..len].iter_mut().enumerate() {
                        *b = ((j * 197 + 43) & 0xFF) as u8;
                    }
                }
                3 => {
                    for b in &mut body[..len] {
                        *b = sm.next() as u8;
                    }
                }
                _ => {
                    let mut j = 0usize;
                    while j < len {
                        let run = (len - j).min(1 + (sm.next() as usize & 0x1F));
                        let v = sm.next() as u8;
                        body[j..j + run].fill(v);
                        j += run;
                    }
                }
            }
            let tag = nf_transport::rxdesc::affine_frame_tag(&body[..len]);
            // (1) The lanes + the combine == the reference golden value.
            let want = span_crc32c_8lane(&body[..len]);
            let got = span_crc32c_8lane_from_tags(&tag.lanes, len);
            assert_eq!(got, want, "r23c lanes diverged at len={len}");
            // (2) The raw triple vs the direct scans.
            let h = len / 2;
            let prefix = crc_scan(0, &t, &body[..h]);
            let cum = crc_scan(0, &t, &body[..len]);
            let raw2 = crc_scan(0, &t, &body[h..len]);
            assert_eq!(tag.prefix_crc, prefix, "r23c prefix at len={len}");
            assert_eq!(tag.cum_crc, cum, "r23c cum at len={len}");
            assert_eq!(tag.raw_crc, raw2, "r23c raw at len={len}");
            // (3) The worker's integrity check is exact: the scalar
            // affine projection of the pair == the independent scan.
            if len - h <= 2048 {
                // SAFETY: width law — raw registers and C(len−h) ≤ 32 bits.
                let proj = unsafe { span_crc32c_affine_sub(tag.cum_crc, tag.prefix_crc, len - h) };
                assert_eq!(proj, raw2, "r23c affine projection at len={len}");
            }
            // (4) The 64-multiple equivalence with Engineer 2's 8-lane
            // affine kernel (the zero-prefix projection — the shipped
            // kernel's law applied to the tag's own lane registers).
            if len % 64 == 0 && len > 0 {
                let zero = [0u32; 8];
                let v = span_crc32c_8lane_affine_sub(&tag.lanes, &zero, len);
                assert_eq!(v, want, "r23c 8-lane affine equivalence at len={len}");
            }
        }
    }

    /// R23b: the K1 differential — the SHIPPED `span_crc32c_affine_sub`
    /// (snapshots + the O(1) projection) vs the reference
    /// hardware-reflected scan, raw AND full-CRC32C variants.
    #[test]
    fn t_affine_kernel_scalar_differential() {
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
        let t = table();
        fn crc_scan(mut c: u32, t: &[u32; 256], buf: &[u8]) -> u32 {
            for &b in buf {
                c = (c >> 8) ^ t[((c ^ b as u32) & 0xFF) as usize];
            }
            c
        }
        struct Sm(u64);
        impl Sm {
            fn next(&mut self) -> u64 {
                self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            }
        }
        fn fill(buf: &mut [u8], sm: &mut Sm, pat: u32) {
            match pat % 5 {
                0 => buf.fill(0x00),
                1 => buf.fill(0xFF),
                2 => {
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = ((i * 131 + 17) & 0xFF) as u8;
                    }
                }
                3 => {
                    for b in buf.iter_mut() {
                        *b = sm.next() as u8;
                    }
                }
                _ => {
                    let mut i = 0usize;
                    while i < buf.len() {
                        let run = (buf.len() - i).min(1 + (sm.next() as usize & 0x3F));
                        let v = sm.next() as u8;
                        buf[i..i + run].fill(v);
                        i += run;
                    }
                }
            }
        }
        const EDGE_LB: [usize; 27] = [
            16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 191, 192, 255, 256, 257, 511, 512,
            1023, 1024, 1025, 1343, 1344, 1345, 1536, 2047, 2048,
        ];
        const EDGE_LA: [usize; 17] = [0, 1, 7, 8, 15, 16, 17, 63, 64, 65, 127, 128, 129, 255, 256, 511, 512];
        let mut sm = Sm(0x9E37_79B9_7F4A_7C15 ^ 0x523A_F11E ^ 0x7C2B_A9E4);
        let mut a_buf = [0u8; 512];
        let mut b_buf = [0u8; 2048];
        let mut errs = 0usize;
        let mut full_errs = 0usize;
        for i in 0..4096usize {
            let lb = if i < 2 * EDGE_LB.len() {
                EDGE_LB[i % EDGE_LB.len()]
            } else if i % 7 == 0 {
                EDGE_LB[(sm.next() % EDGE_LB.len() as u64) as usize]
            } else {
                16 + (sm.next() % 2033) as usize
            };
            let la = if i < 2 * EDGE_LA.len() {
                EDGE_LA[i % EDGE_LA.len()]
            } else {
                (sm.next() % 513) as usize
            };
            let pat = (i % 5) as u32;
            fill(&mut a_buf[..la], &mut sm, pat);
            fill(&mut b_buf[..lb], &mut sm, pat);
            let want = crc_scan(0, &t, &b_buf[..lb]);
            let full_want = crc_scan(0xFFFF_FFFF, &t, &b_buf[..lb]) ^ 0xFFFF_FFFF;
            let raw_a = crc_scan(0, &t, &a_buf[..la]);
            let raw_ab = crc_scan(raw_a, &t, &b_buf[..lb]);
            // SAFETY: the width law holds (raw registers and C(L) ≤ 32 bits).
            let got = unsafe { span_crc32c_affine_sub(raw_ab, raw_a, lb) };
            let state_a = crc_scan(0xFFFF_FFFF, &t, &a_buf[..la]);
            let state_ab = crc_scan(state_a, &t, &b_buf[..lb]);
            // SAFETY: width law (the I = F cancellation carries it verbatim).
            let full_got = unsafe {
                span_crc32c_affine_sub(
                    state_ab ^ 0xFFFF_FFFF,
                    state_a ^ 0xFFFF_FFFF,
                    lb,
                )
            };
            if got != want {
                errs += 1;
            }
            if full_got != full_want {
                full_errs += 1;
            }
        }
        assert_eq!(errs, 0, "K1 raw scalar kernel: {errs}/4096 errors");
        assert_eq!(full_errs, 0, "K1 full-CRC scalar kernel: {full_errs}/4096 errors");
        // the C(L) composition over the whole table horizon
        let mut g = [0u64; 2049];
        g[0] = 1;
        {
            let mut c = 1u32;
            for r in 1..=2048usize {
                c = (c >> 8) ^ t[(c & 0xFF) as usize];
                g[r] = c as u64;
            }
        }
        for l in 0..=2048usize {
            assert_eq!(affine_span_const(l), g[l], "affine_span_const({l}) != G[{l}]");
        }
    }

    /// R23b: the K2 differential — the SHIPPED
    /// `span_crc32c_8lane_affine_sub` vs `crate::sink::span_crc32c_8lane`
    /// (the golden 8-lane hash), on x86_64 BOTH the vector path (the
    /// process default gate) and the scalar per-lane law.
    #[test]
    fn t_affine_kernel_8lane_differential() {
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
        let t = table();
        fn crc_scan(mut c: u32, t: &[u32; 256], buf: &[u8]) -> u32 {
            for &b in buf {
                c = (c >> 8) ^ t[((c ^ b as u32) & 0xFF) as usize];
            }
            c
        }
        struct Sm(u64);
        impl Sm {
            fn next(&mut self) -> u64 {
                self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            }
        }
        fn fill(buf: &mut [u8], sm: &mut Sm, pat: u32) {
            match pat % 5 {
                0 => buf.fill(0x00),
                1 => buf.fill(0xFF),
                2 => {
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = ((i * 131 + 17) & 0xFF) as u8;
                    }
                }
                3 => {
                    for b in buf.iter_mut() {
                        *b = sm.next() as u8;
                    }
                }
                _ => {
                    let mut i = 0usize;
                    while i < buf.len() {
                        let run = (buf.len() - i).min(1 + (sm.next() as usize & 0x3F));
                        let v = sm.next() as u8;
                        buf[i..i + run].fill(v);
                        i += run;
                    }
                }
            }
        }
        // the cumulative 8-lane state over the first nblocks 64B blocks
        // (sink::span_crc32c_8lane's interleave: lane k owns bytes 8k+64j)
        fn lanes_cum(buf: &[u8], nblocks: usize, t: &[u32; 256]) -> [u32; 8] {
            let mut ls = [0u32; 8];
            for j in 0..nblocks {
                let base = 64 * j;
                for k in 0..8usize {
                    ls[k] = crc_scan(ls[k], t, &buf[base + 8 * k..base + 8 * k + 8]);
                }
            }
            ls
        }
        let mut sm = Sm(0x9E37_79B9_7F4A_7C15 ^ 0x523B_0E57 ^ 0xB1AD_1E55);
        let mut pkt = [0u8; 64 * 40];
        let mut errs = 0usize;
        let mut scalar_errs = 0usize;
        const EDGE: [usize; 9] = [64, 128, 192, 256, 320, 512, 1024, 1344, 2048];
        for i in 0..512usize {
            let lb = if i < 2 * EDGE.len() {
                EDGE[i % EDGE.len()]
            } else {
                64 * (1 + (sm.next() as usize % 32))
            };
            let la = 64 * (sm.next() as usize % 9);
            fill(&mut pkt[..la + lb], &mut sm, (i % 5) as u32);
            let cl_start = lanes_cum(&pkt[..la], la / 64, &t);
            let cl_end = lanes_cum(&pkt[..la + lb], (la + lb) / 64, &t);
            let want = crate::sink::span_crc32c_8lane(&pkt[la..la + lb]);
            // the shipped kernel (vector path where the gate says so)
            let got = span_crc32c_8lane_affine_sub(&cl_end, &cl_start, lb);
            if got != want {
                errs += 1;
                if errs < 4 {
                    panic!("K2 vector kernel i={i} L_A={la} L_B={lb} got={got:#x} want={want:#x}");
                }
            }
            // the scalar per-lane law (the fallback path, pinned directly)
            let l8 = lb / 8;
            let (k, r) = (l8 / 16, l8 % 16);
            let c_adv = if k == 0 {
                AFFINE_POW_BYTE_TABLE[r]
            } else {
                // SAFETY: width law — both table factors ≤ 32 bits.
                unsafe { clmul_reduce_mod_vm(AFFINE_POW_128B_TABLE[k - 1], AFFINE_POW_BYTE_TABLE[r]) }
            };
            let mut lanes = [0u32; 8];
            for j in 0..8usize {
                // SAFETY: width law.
                let p = unsafe { clmul_reduce_mod_vm(cl_start[j] as u64, c_adv) };
                lanes[j] = cl_end[j] ^ (p as u32);
            }
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for &c in lanes.iter() {
                h ^= c as u64;
                h = h.wrapping_mul(0x0100_0000_01B3);
            }
            h ^= (lb as u32) as u64;
            h = h.wrapping_mul(0x0100_0000_01B3);
            if h != want {
                scalar_errs += 1;
            }
        }
        assert_eq!(errs, 0, "K2 vector kernel: {errs}/512 errors");
        assert_eq!(scalar_errs, 0, "K2 scalar per-lane law: {scalar_errs}/512 errors");
    }

    /// R23b Task 2: the AVX-512 E/O table builder + the FULL vector-path
    /// slicer differential — install the builder (the hook into
    /// nf-protocol's `spec_slice_512`), then pin the E/O tables against
    /// the software model and the whole `SpecSliceIter` against the
    /// reference `MessageBlocks` walk on random packets (mixed parities,
    /// zero-length messages, window-straddling headers).
    #[test]
    fn t_spec_slice_vec_differential() {
        // Install the vector builder (idempotent; may already be in).
        let installed = install_spec_slice_vec();
        #[cfg(target_arch = "x86_64")]
        {
            let hw_ok = std::arch::is_x86_feature_detected!("avx512f")
                && std::arch::is_x86_feature_detected!("avx512bw");
            assert_eq!(
                installed, hw_ok,
                "install_spec_slice_vec() must succeed exactly on AVX-512F/BW silicon"
            );
        }
        if !nf_protocol::moldudp64::spec_vec_installed() {
            // No vector path on this silicon — the scalar differential
            // (moldudp64's t12) already covers the walk; nothing to pin.
            return;
        }
        // 1. The E/O tables vs the software model (basis windows).
        let mut e_tab = [0u16; 32];
        let mut o_tab = [0u16; 32];
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for wlen in [2usize, 3, 17, 33, 63, 64] {
            let mut win = [0u8; 66];
            for b in win.iter_mut().take(wlen) {
                *b = next() as u8;
            }
            // SAFETY: gated by install-time feature detection.
            unsafe { spec_slice_vec_tables_avx512(&win[..wlen], &mut e_tab, &mut o_tab) };
            for j in 0..32usize {
                // The tables come from the ZERO-PADDED window: a lane's
                // value mixes a real byte with a zero pad byte when the
                // pair straddles the real end — exactly what the builder
                // computes (padding is never read as a length by the walk).
                let want_e = ((win[2 * j] as u16) << 8) | win[2 * j + 1] as u16;
                let want_o = ((win[2 * j + 1] as u16) << 8) | win[2 * j + 2] as u16;
                assert_eq!(e_tab[j], want_e, "E[{j}] wlen={wlen}");
                assert_eq!(o_tab[j], want_o, "O[{j}] wlen={wlen}");
            }
        }
        // 2. The vector-path COMPOSITION (builder + register-table walk,
        //    the exact spec_slice_512 vector branch) vs the reference walk.
        use nf_protocol::moldudp64::{
            parse, spec_slice_walk, spec_vec_builder, Parsed, SpecMsg, HEADER_LEN,
        };
        let build = spec_vec_builder().expect("the builder is installed");
        let session = *b"NFTESTSESS";
        for i in 0..2000u32 {
            let count = 1 + (next() % 48) as u16;
            let start_seq = next() % 1_000_000_000;
            let mut raw: Vec<u8> = Vec::with_capacity(4096);
            raw.extend_from_slice(&session);
            raw.extend_from_slice(&start_seq.to_be_bytes());
            raw.extend_from_slice(&count.to_be_bytes());
            let mut offs_lens: Vec<(usize, u16)> = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let l = match next() % 10 {
                    0 => 0u16,
                    1 => (next() % 64) as u16,
                    2 => 1 + (next() % 63) as u16,
                    3 => 64 + (next() % 192) as u16,
                    _ => 8 + (next() % 56) as u16,
                };
                offs_lens.push((raw.len() + 2 - HEADER_LEN, l));
                raw.extend_from_slice(&l.to_be_bytes());
                for _ in 0..l {
                    raw.push(next() as u8);
                }
            }
            let blocks_region = &raw[HEADER_LEN..];
            let parsed = parse(&raw).expect("valid random packet");
            let reference: Vec<(usize, u16)> = match parsed {
                Parsed::Data { blocks, .. } => blocks
                    .map(|b| {
                        (
                            b.data.as_ptr() as usize - blocks_region.as_ptr() as usize,
                            b.data.len() as u16,
                        )
                    })
                    .collect(),
                _ => panic!("expected Data"),
            };
            assert_eq!(reference, offs_lens, "reference walk drift i={i}");
            // The SpecSliceIter window loop, driven through the EXPLICIT
            // vector composition (window invariant + zero-progress rule).
            let mut spec: Vec<(usize, u16)> = Vec::with_capacity(count as usize);
            let mut pos = 0usize;
            'outer: while pos < blocks_region.len() {
                let rem = blocks_region.len() - pos;
                let wlen = rem.min(64);
                let chunk = &blocks_region[pos..pos + wlen];
                let mut et = [0u16; 32];
                let mut ot = [0u16; 32];
                build(chunk, &mut et, &mut ot);
                let mut batch = [SpecMsg { off: 0, len: 0 }; 16];
                let (n, pf) = spec_slice_walk(&et, &ot, wlen, 0, rem, &mut batch);
                for m in batch[..n].iter() {
                    spec.push((pos + m.off as usize, m.len));
                }
                pos += pf;
                if pf == 0 {
                    break 'outer;
                }
            }
            assert_eq!(spec, reference, "vector composition drift i={i}");
        }
    }
}
