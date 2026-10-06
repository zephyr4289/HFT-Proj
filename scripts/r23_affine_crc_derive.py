#!/usr/bin/env python3
"""R23 'affine' derivation — the Galois-field span-subtraction frontier.

Stage R23 (branch feat/r23-affine-vector-frontier) derives, proves, and
exhaustively verifies the closed-form algebraic machinery that converts
downstream ITCH span verification from an O(span_length) memory-bound
re-read (~66.6 GB/s of L3 bandwidth to re-read span bytes) into an O(1)
single-projection carry-less multiplication.

THE ARCHITECTURE CONTEXT
========================
The feed handler processes packets P = A || B || C where A is the
MoldUDP64 header prefix (or preceding spans), B is the contiguous span of
emitted ITCH messages (L_B bytes), C the suffix.  Today the worker cores
re-scan B's bytes through `span_crc32c_8lane` to verify the span.  The
ingest core already touches every byte in L1D; if it snapshots its
CUMULATIVE CRC register at span boundaries, the span's own CRC is an
algebraic projection of two 32-bit snapshots:

    Cumulative Packet Stream in L1D Cache (Ingest Core)
    ┌───────────────────────┬─────────────────────────────────┬───────────────────┐
    │   Prefix A (len L_A)  │    Span Body B (len L_B)        │     Suffix C      │
    └───────────────────────┴─────────────────────────────────┴───────────────────┘
                │                           │
                ▼                           ▼
       Prefix Hash raw(A)         Cumulative Hash raw(A || B)
                │                           │
                └───────────────┬───────────┘
                                ▼
          raw(B) = raw(A || B) ^ (raw(A) (x) G[L_B]  mod VM)
                  └──► ONE carry-less multiply + ONE reduction. Zero re-reading.

THE LAW (three representations of one theorem)
==============================================
All integers below encode GF(2)[x] polynomials LSB-first (bit k <-> x^k) —
exactly the PCLMULQDQ / VPCLMULQDQ operand convention used throughout
crcfold.rs.

* Reflected (hardware) register domain — THE operative form.  With
  VM = 0x105EC76F1 (= rev_33(0x11EDC6F41), the R13 ending ring) and
  G[L] = y^(-8L) mod VM (the R15 zeros-advance constant):

      raw(A || B) = ( raw(A) (x) G[L_B] mod VM ) ^ raw(B)
      =>  raw(B)  =  raw(A || B) ^ ( raw(A) (x) G[L_B] mod VM )

  where raw(.) is the reflected CRC32C register, init 0, no final xor —
  bit-identical to the register semantics of every lane of
  `span_crc32c_8lane` and of r15's `raw_crc` reference kernel.

* Normal (MSB-first) representation — the positive-power form stated in
  the R23 directive.  With P(x) = 0x11EDC6F41:

      CRC(A || B) = ( CRC(A) (x) x^(8 L_B) mod P(x) ) ^ CRC(B)
      =>  CRC(B)  =  CRC(A || B) ^ ( CRC(A) (x) x^(8 L_B) mod P(x) )

  The "+"-power (normal) and "−"-power (reflected) forms are the mirror
  duality of one theorem: the reflected hardware representation runs the
  division from the constant-term end, inverting the exponent.  Part P1e
  verifies the normal form with an independent MSB-first engine.

* Full CRC32C (init 0xFFFFFFFF, final xor 0xFFFFFFFF) — the I=F
  cancellation theorem.  Because the standard init equals the final
  inversion, every correction term cancels and the law keeps the same
  shape on FINALIZED CRC32C values:

      full(B) = full(A || B) ^ ( full(A) (x) G[L_B] mod VM )

PROOFS
======
Pencil proof of the reflected law (the computational proofs below pin
every step):

  1. Linearity: the reflected byte step c' = (c >> 8) ^ T[(c ^ b) & 0xFF]
     is affine in (c, b) — its linear part is the one-zero-byte advance
     Z_1, and T[i ^ j] = T[i] ^ T[j] (T is a GF(2)-linear map of its
     index), so the step is Z_1(c) ^ T[b].
  2. Affine processing lemma (P1b): scanning bytes b_1..b_n from any
     register c yields Z_n(c) ^ raw(b_1..b_n).
  3. R15 V1 operator law: Z_r(c) = c (x) G[r] (mod VM), with
     G[r] = y^(-8r) mod VM.
  4. Chain steps 2+3 over A then B:
        raw(A || B) = Z_{L_B}(raw(A)) ^ raw(B)
                    = ( raw(A) (x) G[L_B] ) ^ raw(B)   (mod VM).   ∎

The full-CRC variant follows from full(M) = Z_{L_M}(I) ^ raw(M) ^ F with
I = F = 0xFFFFFFFF (P1d): expanding raw(B) = raw(A||B) ^ Z_{L_B}(raw(A))
in full-space, the two Z_{L_A+L_B}(I) terms and the two F terms cancel
pairwise, and the surviving Z_{L_B}(I) ^ Z_{L_B}(F) collapses to zero
because I ^ F = 0.

THE EMITTED TABLES
==================
AFFINE_POW_128B_TABLE[k-1] = y^(-128 k) mod VM = G[16 k], k = 1..=128 —
    the 16-byte-block advance powers ("x^(128k) in reflected domain"),
    covering block-aligned span lengths 16..=2048 bytes.
AFFINE_POW_BYTE_TABLE[r]   = y^(-8 r)  mod VM = G[r],  r = 0..=15 —
    the byte-level residual powers.
A span of L = 16 k + r bytes advances by
    C(L) = rmul(AFFINE_POW_128B_TABLE[k-1], AFFINE_POW_BYTE_TABLE[r])
(k = 0 is the ring identity 1), so the O(1) projection needs ONE extra
clmul + reduction to compose the length constant, then the projection
clmul + reduction itself.  All constants are <= 32 bits (the R16 width
law): a 32-bit register (x) 32-bit constant product is <= 63 bits —
inside a 64-bit PCLMULQDQ lane with the MSB free; a 64-bit fold-state
lane (x) constant is <= 95 bits — inside the 128-bit VPCLMULQDQ product.
Polynomial degree NEVER overflows the 64-bit or 128-bit register halves.

THE 8-LANE BRIDGE (span_crc32c_8lane in O(1))
=============================================
`span_crc32c_8lane` interleaves EIGHT raw CRC32C registers over 64-byte
blocks (lane k owns the 8-byte words at offsets 8k + 64j) and combines
them with FNV-1a-64.  Because every lane is itself a raw register, the
SAME subtraction law holds per lane: for a span whose boundaries are
64-byte aligned, with cumulative per-lane snapshots CL_k(start) and
CL_k(end),

    lane_k(B) = CL_k(end) ^ ( CL_k(start) (x) G[L_B / 8] mod VM )

(the lane's B-run is L_B/64 words = L_B/8 bytes of lane stream), and the
exact `span_crc32c_8lane(B)` value — lanes + FNV combine — is an O(1)
projection of the cumulative 8-lane state.  Part P3b verifies this
against the scalar reference `span_ref` on a 1,000-case battery.
Misaligned spans re-base the lane phase; the correction constants are
the same G/AT family this file derives (Engineer 2's extension path).

PARTS
  P0   G-table extension to r = 0..=2048 + validation vs shipped tables
  P1   the law: operator law, affine lemma, span subtraction (raw +
       full variants), the normal-representation dual
  P2   the power tables: derivation, cross-anchors vs shipped R15/R21
       constants, composition completeness for ALL L in 0..=2048, width
       audit
  P3   THE exhaustive differential oracle: 10,000 randomized
       variable-length spans (L in [16, 2048], arbitrary patterns and
       boundary alignments), Method A (reference scan) vs Method B
       (cumulative + prefix + the law), differential error count must
       be EXACTLY 0; + the 1,000-case 8-lane bridge battery
  P4   golden anchors (CRC32C("123456789") = 0xE3069283 and the iSCSI
       vector set; the golden fold hashes are preserved structurally —
       this round touches no existing kernel)
  P5   Rust constant emission + live re-verification of the constants
       committed in crates/nf-testkit/src/crcfold.rs

Exit code 0 iff every assertion passes and the differential oracle
reports exactly zero errors.
"""

import random
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from r15_tail_derive import (VM, VR0, VH64, RKHI, RKLO,  # noqa: E402
                             clmul, clmod, rmul, le, M64, M32,
                             crc_u8, crc_u64, raw_crc, span_ref,
                             zr, ypow_raw, G, T)

I32 = 0xFFFFFFFF  # standard CRC32C init / final xor
GMAX = 2048       # span lengths are verified over [0, 2048] bytes


# ── shared: the reflected-CRC32C register scan (tight loop) ─────────────────
def crc_scan(c, buf):
    """Advance reflected CRC32C register `c` over `buf` (the software-exact
    model of the hardware crc32 instruction chain / r15's raw_crc)."""
    t = T
    for b in buf:
        c = (c >> 8) ^ t[(c ^ b) & 0xFF]
    return c


def full_crc32c(buf):
    """Standard CRC32C (init 0xFFFFFFFF, final xor) — hardware semantics."""
    return crc_scan(I32, buf) ^ I32


def fnv_lanes(lanes, ln):
    """The span_crc32c_8lane lane combine: FNV-1a-64 over 8 lanes + length."""
    h = 0xcbf29ce484222325
    for cval in list(lanes) + [ln & M32]:
        h ^= cval
        h = (h * 0x100000001b3) & M64
    return h


def _splitmix(state):
    state[0] = (state[0] + 0x9E3779B97F4A7C15) & M64
    z = state[0]
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M64
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M64
    return z ^ (z >> 31)


def fill(buf, ln, pat, nxt):
    if pat == 0:
        buf[:ln] = bytes(ln)
    elif pat == 1:
        buf[:ln] = b"\xff" * ln
    elif pat == 2:
        buf[:ln] = bytes((i * 131 + 17) & 0xFF for i in range(ln))
    elif pat == 3:
        buf[:ln] = bytes((nxt() & 0xFF) for _ in range(ln))
    else:  # mixed: runs of a constant byte at random lengths
        i = 0
        while i < ln:
            run = min(ln - i, 1 + (nxt() & 0x3F))
            v = nxt() & 0xFF
            buf[i:i + run] = bytes([v]) * run
            i += run


# ── Part 0: the G-table extension G[0..=2048] ───────────────────────────────
def g_extend(n):
    """G[r] = y^(-8r) mod VM for r = 0..=n via the reflected-CRC32C
    zeros-advance of the 1-seed (Z_r(1)) — the r21 extension method."""
    t = T
    g = [1] * (n + 1)
    c = 1
    for r in range(1, n + 1):
        c = (c >> 8) ^ t[c & 0xFF]
        g[r] = c
    return g


def p0_g_extension():
    gx = g_extend(GMAX)
    for r in range(72):
        assert gx[r] == G[r], f"P0 FAIL: G[{r}] shipped {G[r]:#x} vs derived {gx[r]:#x}"
    # inverse-power law probes (G[r] * y^(8r) == 1 mod VM)
    for r in (1, 15, 16, 71, 128, 500, 1000, 2047, 2048):
        assert rmul(gx[r], ypow_raw(8 * r)) == 1, f"P0 FAIL: G[{r}] not y^(-8r)"
    # composition probes (the zeros-advance is a ring homomorphism in r)
    for a, b in ((1, 1), (16, 16), (100, 999), (1024, 1024), (7, 2041)):
        assert rmul(gx[a], gx[b]) == gx[a + b], f"P0 FAIL: G[{a}] (x) G[{b}] != G[{a+b}]"
    # shipped-constant anchors: the extension agrees with R15/R21 literals
    assert gx[8] == VH64 == 0x493C7D27, "P0 FAIL: G[8] != VH64"
    assert gx[16] == VR0 == 0xF20C0DFE, "P0 FAIL: G[16] != VR0"
    assert gx[120] == 0x0D3B6092, "P0 FAIL: G[120] != OFOLD_K8_HI"
    assert gx[128] == 0x6992CEA2, "P0 FAIL: G[128] != OFOLD_K8_LO"
    print(f"P0 G-extension: OK (G[0..={GMAX}]; shipped G[0..72] reproduced verbatim; "
          f"inverse-power law at 9 probes; composition at 5 probes; "
          f"R15/R21 literal anchors G[8]/G[16]/G[120]/G[128] pinned)")
    return gx


# ── Part 1: THE AFFINE SPAN SUBTRACTION LAW ─────────────────────────────────
def p1_the_law(gx):
    rng = random.Random(0x0_F01D_23)

    # P1a: the operator law Z_r(c) = c (x) G[r] on the GF(2)^32 basis and
    # randoms — equality on a basis proves equality everywhere (linearity).
    for r in [0, 1, 2, 3, 15, 16, 17, 31, 32, 63, 64, 65, 100, 128, 1000, 2048]:
        for i in range(32):
            c = 1 << i
            assert zr(c, r) == rmul(c, gx[r]), f"P1a FAIL: basis bit {i}, r={r}"
        for _ in range(8):
            c = rng.randrange(1 << 32)
            assert zr(c, r) == rmul(c, gx[r]), f"P1a FAIL: random, r={r}"
    print("P1a operator law Z_r(c) == c (x) G[r]: OK (16 lengths x 32 basis + 8 randoms)")

    # P1b: the affine processing lemma — scanning bytes from any register c
    # equals Z_n(c) ^ raw(bytes).
    for _ in range(300):
        c = rng.randrange(1 << 32)
        n = rng.randrange(0, 201)
        data = bytes(rng.randrange(256) for _ in range(n))
        assert crc_scan(c, data) == zr(c, n) ^ crc_scan(0, data), "P1b FAIL"
    print("P1b affine processing lemma: OK (300 randoms)")

    # P1c: the span-subtraction law, raw registers — compact structured
    # pre-battery (the exhaustive oracle is Part 3).
    n = 0
    for l_a in (0, 1, 8, 16, 63, 64, 128, 512):
        for l_b in (16, 17, 63, 64, 127, 128, 129, 255, 256, 1343, 1344, 2047, 2048):
            a = bytes(rng.randrange(256) for _ in range(l_a))
            b = bytes(rng.randrange(256) for _ in range(l_b))
            raw_a = crc_scan(0, a)
            raw_ab = crc_scan(raw_a, b)
            want = crc_scan(0, b)
            got = raw_ab ^ rmul(raw_a, gx[l_b])
            assert got == want, f"P1c FAIL: L_A={l_a} L_B={l_b}"
            n += 1
    print(f"P1c raw span subtraction: OK ({n} structured cases)")

    # P1d: the full-CRC32C variant (I = F cancellation).
    for _ in range(300):
        l_a = rng.randrange(0, 200)
        l_b = rng.randrange(1, 300)
        a = bytes(rng.randrange(256) for _ in range(l_a))
        b = bytes(rng.randrange(256) for _ in range(l_b))
        state_a = crc_scan(I32, a)
        state_ab = crc_scan(state_a, b)
        full_a = state_a ^ I32
        full_ab = state_ab ^ I32
        want = crc_scan(I32, b) ^ I32
        got = full_ab ^ rmul(full_a, gx[l_b])
        assert got == want, f"P1d FAIL: L_A={l_a} L_B={l_b}"
    print("P1d full-CRC32C variant (I=F cancellation): OK (300 randoms)")

    # P1e: the normal-representation dual — the directive's literal
    # positive-power law over P(x) = 0x11EDC6F41 with an independent
    # MSB-first engine.
    p_norm = 0x11EDC6F41
    p_low = p_norm & M32

    def nmod(v):
        while v >= (1 << 32):
            v ^= p_norm << (v.bit_length() - 33)
        return v

    def nmul(a, b):
        return nmod(clmul(a, b))

    def xpow(e):
        acc, base = 1, 2
        while e:
            if e & 1:
                acc = nmul(acc, base)
            base = nmul(base, base)
            e >>= 1
        return acc

    def norm_scan(c, buf):
        for b in buf:
            c ^= (b << 24)
            for _ in range(8):
                top = c & 0x80000000
                c = (c << 1) & M32
                if top:
                    c ^= p_low
        return c

    def norm_direct(buf):
        # independent formulation: the CRC definition itself — the remainder
        # of M(x)·x^32 under explicit long division by P(x)
        v = int.from_bytes(buf, "big") << 32
        while v >= (1 << 32):
            v ^= p_norm << (v.bit_length() - 33)
        return v

    for _ in range(60):  # engine self-check: two formulations, one value
        data = bytes(rng.randrange(256) for _ in range(rng.randrange(0, 64)))
        assert norm_scan(0, data) == norm_direct(data), "P1e engine mismatch"
    n = 0
    for _ in range(300):
        l_a = rng.randrange(0, 129)
        l_b = rng.randrange(1, 257)
        a = bytes(rng.randrange(256) for _ in range(l_a))
        b = bytes(rng.randrange(256) for _ in range(l_b))
        norm_a = norm_scan(0, a)
        norm_ab = norm_scan(norm_a, b)
        want = norm_scan(0, b)
        got = norm_ab ^ nmul(norm_a, xpow(8 * l_b))
        assert got == want, f"P1e FAIL: L_A={l_a} L_B={l_b}"
        n += 1
    print(f"P1e normal-domain dual CRC(B) = CRC(A||B) ^ (CRC(A) (x) x^(8L_B) mod P): "
          f"OK ({n} randoms; engines cross-checked)")

    # engine sanity: the tight scan == r15's raw_crc
    for _ in range(50):
        data = bytes(rng.randrange(256) for _ in range(rng.randrange(0, 256)))
        assert crc_scan(0, data) == raw_crc(data), "engine != r15 raw_crc"
    print("    tight scan == r15 raw_crc: OK (50 randoms)")


# ── Part 2: the power tables ────────────────────────────────────────────────
def build_tables(gx):
    t128 = [gx[16 * k] for k in range(1, 129)]  # y^(-128k), k = 1..=128
    tbyte = [gx[r] for r in range(16)]          # y^(-8r),  r = 0..=15
    return t128, tbyte


def affine_const(t128, tbyte, L):
    """C(L) = y^(-8L) mod VM composed from the shipped tables:
    L = 16k + r -> rmul(T128[k-1], TBYTE[r]); L < 16 -> TBYTE[L]."""
    k, r = divmod(L, 16)
    if k == 0:
        return tbyte[r]
    return rmul(t128[k - 1], tbyte[r])


def p2_tables(gx, t128, tbyte):
    # anchors against shipped constants
    assert t128[0] == VR0, "P2 FAIL: T128[1] != VR0 (G[16])"
    assert t128[7] == 0x6992CEA2, "P2 FAIL: T128[8] != OFOLD_K8_LO (G[128])"
    assert tbyte[0] == 1, "P2 FAIL: TBYTE[0] != identity"
    assert tbyte[8] == VH64, "P2 FAIL: TBYTE[8] != VH64 (G[8])"
    # composition completeness: EVERY L in 0..=2048 decomposes exactly
    for L in range(GMAX + 1):
        assert affine_const(t128, tbyte, L) == gx[L], f"P2 FAIL: composition at L={L}"
    # width audit: every constant <= 32 bits (the R16 width law — products
    # with 32-bit registers stay <= 63 bits, with 64-bit fold lanes <= 95)
    for v in t128 + tbyte:
        assert v < (1 << 32), f"P2 FAIL: constant {v:#x} wider than 32 bits"
    print(f"P2 power tables: OK (T128[1]={t128[0]:#08x} == VR0; composition "
          f"verified for ALL L in 0..={GMAX} ({GMAX + 1} lengths); width audit: "
          f"all {len(t128) + len(tbyte)} constants <= 32 bits)")
    # degree-bound report (the register-half safety proof)
    print("    degree bounds: reg(<=32b) (x) const(<=32b) <= 63b < 64b lane;"
          " fold lane(<=64b) (x) const <= 95b < 128b product")


# ── Part 3: THE exhaustive differential oracle ──────────────────────────────
EDGE_LB = [16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 191, 192, 255, 256,
           257, 511, 512, 1023, 1024, 1025, 1343, 1344, 1345, 1536, 2047, 2048]
EDGE_LA = [0, 1, 7, 8, 15, 16, 17, 63, 64, 65, 127, 128, 129, 255, 256, 511, 512]


def p3_oracle(gx, t128, tbyte, n_tests=10_000):
    rng = random.Random(0x523A_F11E)
    state = [0x9E3779B97F4A7C15]
    a_buf = bytearray(512)
    b_buf = bytearray(2048)
    errs = 0
    full_errs = 0
    for i in range(n_tests):
        if i < 2 * len(EDGE_LB):
            L_B = EDGE_LB[i % len(EDGE_LB)]
        elif i % 7 == 0:
            L_B = rng.choice(EDGE_LB)
        else:
            L_B = rng.randrange(16, 2049)
        if i < 2 * len(EDGE_LA):
            L_A = EDGE_LA[i % len(EDGE_LA)]
        else:
            L_A = rng.randrange(0, 513)
        pat = i % 5
        fill(a_buf, L_A, pat, lambda: _splitmix(state))
        fill(b_buf, L_B, pat, lambda: _splitmix(state))
        Ab = bytes(a_buf[:L_A])
        Bb = bytes(b_buf[:L_B])

        # ── Method A (ground truth): scan the raw span bytes through the
        #    reference hardware-reflected CRC32C algorithm.
        raw_b_want = crc_scan(0, Bb)
        full_b_want = crc_scan(I32, Bb) ^ I32

        # ── Method B (the algebraic law): the ingest core's snapshots …
        raw_a = crc_scan(0, Ab)            # prefix hash at span start
        raw_ab = crc_scan(raw_a, Bb)       # cumulative hash at span end
        c_lb = affine_const(t128, tbyte, L_B)
        raw_b_pred = raw_ab ^ rmul(raw_a, c_lb)

        state_a = crc_scan(I32, Ab)         # full-CRC variant snapshots
        state_ab = crc_scan(state_a, Bb)
        full_a = state_a ^ I32
        full_ab = state_ab ^ I32
        full_b_pred = full_ab ^ rmul(full_a, c_lb)

        if raw_b_pred != raw_b_want:
            errs += 1
            if errs <= 3:
                print(f"    RAW MISMATCH: i={i} L_A={L_A} L_B={L_B} "
                      f"want={raw_b_want:#010x} got={raw_b_pred:#010x}")
        if full_b_pred != full_b_want:
            full_errs += 1
            if full_errs <= 3:
                print(f"    FULL MISMATCH: i={i} L_A={L_A} L_B={L_B} "
                      f"want={full_b_want:#010x} got={full_b_pred:#010x}")
        if (i + 1) % 2000 == 0:
            print(f"    … {i + 1}/{n_tests} (errors so far: raw={errs}, full={full_errs})")
    assert errs == 0, f"P3 FAIL: {errs} raw differential errors"
    assert full_errs == 0, f"P3 FAIL: {full_errs} full-CRC differential errors"
    print(f"P3 differential oracle: OK ({n_tests} randomized spans, L_B in "
          f"[16, 2048], L_A in [0, 512], 5 byte patterns, boundary-aligned "
          f"edges; differential error count EXACTLY 0 for raw AND full laws)")
    return n_tests


def lanes_cum(buf, nblocks):
    """The cumulative 8-lane state over the first `nblocks` 64-byte blocks
    (lane k owns the LE qwords at offsets 8k + 64j — span_crc32c_8lane's
    interleave)."""
    ls = [0] * 8
    for j in range(nblocks):
        base = 64 * j
        for k in range(8):
            ls[k] = crc_scan(ls[k], buf[base + 8 * k: base + 8 * k + 8])
    return ls


def p3b_lane_bridge(gx, n_tests=1000):
    """The 8-lane bridge: for 64-byte-aligned spans, the EXACT
    span_crc32c_8lane value is an O(1) per-lane projection of the
    cumulative 8-lane snapshots."""
    rng = random.Random(0x523B_0E57)
    state = [0x9E3779B97F4A7C15]
    pkt = bytearray(64 * 40)
    for i in range(n_tests):
        if i < 24:
            L_B = 64 * [1, 2, 3, 4, 5, 8, 16, 21, 32][i % 9]
        else:
            L_B = 64 * rng.randrange(1, 33)
        L_A = 64 * rng.randrange(0, 9)
        fill(pkt, L_A + L_B, i % 5, lambda: _splitmix(state))
        Ab = bytes(pkt[:L_A])
        Bb = bytes(pkt[L_A:L_A + L_B])

        cl_start = lanes_cum(pkt[:L_A], L_A // 64)
        cl_end = lanes_cum(pkt[:L_A + L_B], (L_A + L_B) // 64)
        adv = gx[L_B // 8]  # the lane's B-run is L_B/8 bytes of lane stream
        pred = [cl_end[k] ^ rmul(cl_start[k], adv) for k in range(8)]
        direct = lanes_cum(Bb, L_B // 64)
        assert pred == direct, f"P3b FAIL: lanes i={i} L_A={L_A} L_B={L_B}"
        assert fnv_lanes(pred, L_B) == span_ref(Bb), \
            f"P3b FAIL: span_crc32c_8lane bridge i={i} L_A={L_A} L_B={L_B}"
    print(f"P3b 8-lane bridge: OK ({n_tests} 64-byte-aligned spans; per-lane "
          f"subtraction reproduces span_crc32c_8lane bit-exact — the FNV "
          f"combine included)")
    return n_tests


# ── Part 4: golden anchors ──────────────────────────────────────────────────
def p4_anchors():
    # Vectors pinned against the hardware crc32 instruction chain itself
    # (probe: 123456789->E3069283, 32x00->8A9136AA, 32xFF->62A8AB43,
    #  00..1F->46DD794E, 1F..00->113FDB5C — SSE4.2 ground truth).
    assert full_crc32c(b"123456789") == 0xE3069283, "P4 FAIL: RFC 3720 check value"
    assert full_crc32c(bytes(32)) == 0x8A9136AA, "P4 FAIL: 32 zero bytes"
    assert full_crc32c(b"\xff" * 32) == 0x62A8AB43, "P4 FAIL: 32 0xFF bytes"
    assert full_crc32c(bytes(range(32))) == 0x46DD794E, "P4 FAIL: incrementing 00..1F"
    assert full_crc32c(bytes(range(31, -1, -1))) == 0x113FDB5C, "P4 FAIL: decrementing 1F..00"
    print("P4 golden anchors: OK (CRC32C('123456789') = 0xE3069283 + the iSCSI "
          "vector set: 32x00 -> 0x8A9136AA, 32xFF -> 0x62A8AB43, "
          "00..1F -> 0x46DD794E, 1F..00 -> 0x113FDB5C — every value pinned "
          "against the hardware crc32 instruction)")
    print("    golden fold hashes 0x881639cead506f25 / 0xF6EF154EFDE905D8: "
          "preserved structurally — this round adds constants + tests only, "
          "touching no existing kernel path (asserted by the crate's own "
          "differential suites and CI)")


# ── Part 5: Rust emission + committed-table re-verification ─────────────────
RUST_DOC = """// ── R23: the affine span-subtraction power tables ──────────────────────────
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
// P(x) = 0x11EDC6F41 — the mirror duality (reflected hardware domain
// tabulates the inverse powers). The law ALSO holds verbatim on FINALIZED
// CRC32C values (init 0xFFFFFFFF, final xor 0xFFFFFFFF) because the init
// and final inversions cancel (I = F).
//
// O(1) span verification recipe (Engineer 2's kernel): the ingest core
// snapshots the cumulative register at span boundaries (prefix hash
// `raw(A)`, cumulative hash `raw(A∥B)`); the worker composes the length
// constant C(L) = rmul(T128[k-1], TBYTE[r]) for L = 16k + r (ONE clmul +
// reduction), then projects the span with ONE VPCLMULQDQ
// (u32 register ⊗ u32 constant, product ≤ 63 bits — inside a 64-bit
// lane), ONE 33-bit reduction mod VM, ONE XOR. Zero payload re-reading.
// For 64-byte-aligned spans the same law applies per lane to the
// cumulative 8-lane state and reconstructs the exact `span_crc32c_8lane`
// value (lane advance constant G[L/8]) — verified by the P3b bridge.
//
// Width law (degree never overflows the register halves): every constant
// is ≤ 32 bits, so register(≤32b) ⊗ constant ≤ 63 bits < 64-bit lane, and
// fold-state lanes (≤64b) ⊗ constant ≤ 95 bits < 128-bit product.
"""


def p5_emit(t128, tbyte):
    out = ["\n" + RUST_DOC]
    out.append("/// Galois field power lookup table: T[k] = x^(128 * k) mod P(x) in reflected")
    out.append("/// domain — the 16-byte-block advance power. AFFINE_POW_128B_TABLE[k-1] =")
    out.append("/// y^(-128·k) mod VM = G[16k] (k ∈ 1..=128, covering 16..=2048 bytes; the")
    out.append("/// k = 0 factor is the ring identity 1, implicit). Anchors: T[1] = VR0,")
    out.append("/// T[8] = OFOLD_K8_LO.")
    out.append("pub const AFFINE_POW_128B_TABLE: [u64; 128] = [")
    for j in range(0, 128, 6):
        out.append("    " + ", ".join(f"0x{v:08X}" for v in t128[j:j + 6]) + ",")
    out.append("];")
    out.append("/// Galois field power lookup table: T[r] = x^(8 * r) mod P(x) in reflected")
    out.append("/// domain — the byte-level residual advance power. AFFINE_POW_BYTE_TABLE[r]")
    out.append("/// = y^(-8r) mod VM = G[r] (r ∈ 0..=15; identity at r = 0; G[8] = VH64).")
    out.append("pub const AFFINE_POW_BYTE_TABLE: [u64; 16] = [")
    for j in range(0, 16, 6):
        out.append("    " + ", ".join(f"0x{v:08X}" for v in tbyte[j:j + 6]) + ",")
    out.append("];")
    text = "\n".join(out)
    print("P5 Rust emission: the constant block below is the committed crcfold.rs")
    print("    source (re-verified against the committed file by P5b):")
    print(text)
    return text


def p5b_verify_committed(t128, tbyte):
    import os
    import re
    rs = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                      "..", "crates", "nf-testkit", "src", "crcfold.rs")
    if not os.path.exists(rs):
        print("P5b committed-table verification: SKIP (crcfold.rs not found — "
              "standalone execution)")
        return
    with open(rs, "r", encoding="utf-8") as fh:
        src = fh.read()
    for name, want in (("AFFINE_POW_128B_TABLE", t128),
                       ("AFFINE_POW_BYTE_TABLE", tbyte)):
        m = re.search(name + r"\s*:\s*\[u64;\s*\d+\]\s*=\s*\[(.*?)\]", src, re.S)
        assert m, f"P5b FAIL: {name} not found in crcfold.rs"
        vals = [int(x, 16) for x in re.findall(r"0x[0-9A-Fa-f]+", m.group(1))]
        assert len(vals) == len(want), \
            f"P5b FAIL: {name} has {len(vals)} entries, expected {len(want)}"
        assert vals == want, f"P5b FAIL: {name} drifted from the derivation"
    print("P5b committed-table verification: OK (crcfold.rs AFFINE_POW_* tables "
          "are bit-exact with this derivation)")


# ── main ────────────────────────────────────────────────────────────────────
def main():
    print("R23 affine derivation — the Galois-field span-subtraction frontier")
    print("=" * 78)
    gx = p0_g_extension()
    p1_the_law(gx)
    t128, tbyte = build_tables(gx)
    p2_tables(gx, t128, tbyte)
    n_oracle = p3_oracle(gx, t128, tbyte)
    n_bridge = p3b_lane_bridge(gx)
    p4_anchors()
    p5_emit(t128, tbyte)
    p5b_verify_committed(t128, tbyte)
    print("=" * 78)
    print(f"VERDICT: PASS — affine span subtraction law proven and pinned "
          f"({n_oracle} differential spans + {n_bridge} bridge spans, "
          f"differential error count EXACTLY 0)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
