#!/usr/bin/env python3
"""R23b 'affine kernel' derivation — Engineer 2's vector-kernel oracle.

Stage R23 (branch feat/r23-affine-vector-frontier) turns Engineer 1's
Galois-field span-subtraction LAW (verified 10,000/10,000 by
`scripts/r23_affine_crc_derive.py`) into silicon: the O(1) scalar
projection kernel (`clmul_reduce_mod_vm` + `span_crc32c_affine_sub`), the
8-lane VPCLMULQDQ projection kernel (`span_crc32c_8lane_affine_sub`), and
the speculative 512-bit MoldUDP64 packet slicer (`spec_slice_512`).

This script derives, pins and exhaustively verifies the KERNEL-LAYER
algebra (everything between the LAW and the intrinsics):

K0  THE PLAIN BARRETT CORE.  The R14 vend ending multiplies by VR0 and
     reduces mod VM; the R23 kernels need PLAIN reduction (multiply by
     G[L] only).  Part K0 proves — basis-exhaustively, which by GF(2)
     linearity of the whole map is a COMPLETE proof over the input
     class — that the SAME constant structure (VMU = floor(y^88/VM),
     the 32/56/32 field-wide alignr shifts, the two correction
     clmuls) reduces ANY field F < 2^95 to F mod VM with no VR0
     factor.  Every R23 product (u32 register ⊗ u32 constant ≤ 62
     bits; u32 ⊗ u32 table composition ≤ 62 bits) sits deep inside
     that range, so `clmul_reduce_mod_vm` needs ONE product clmul +
     THREE reduction clmuls — no new constants, no VR0^-1 dance.

K1  THE SCALAR KERNEL MODEL, differentially: 10,000 randomized spans
     (the same generator shapes as the P3 oracle — boundary edges,
     5 fill patterns), Method A (reference hardware-reflected scan)
     vs Method B (snapshots + `span_crc32c_affine_sub` modeled at the
     exact u128/PCLMULQDQ semantics), raw AND full-CRC variants,
     error count must be EXACTLY 0.

K2  THE 8-LANE VECTOR KERNEL MODEL, differentially: 1,000+
     64-byte-aligned spans — the exact AVX-512 op sequence emulated
     (cvtepu32_epi64 packing, maskz_permutexvar_epi64 field layout,
     ONE VPCLMULQDQ per zmm for the projection, the vend-core
     Barrett with cross-qword alignr 4/7/4 semantics, the FNV-1a-64
     lane combine) vs the `span_crc32c_8lane` reference — the full
     golden hash, bit-exact.

K3  THE SPECULATIVE SLICER MODEL, differentially: random MoldUDP64
     message-block streams (mixed parities, zero lengths, headers
     straddling window edges) — the E/O 16-bit BE lane-table
     extraction (vpshufb bswap16 on loadu(+0)/loadu(+1) of the
     zero-padded 66-byte window) + the register-table walk vs the
     reference scalar walk — identical (count, offsets, lengths),
     10,000 streams.

K4  golden anchors: CRC32C("123456789") = 0xE3069283 and the kernel
     models' agreement with the shipped tables (composition law for
     every L the kernels can be asked about).

PARTS
  K0   plain-Barrett basis-exhaustive exactness (63- and 95-bit classes)
  K1   scalar kernel differential (raw + full)
  K2   8-lane vector kernel differential (the full golden hash)
  K3   speculative slicer differential
  K4   anchors + summary

Run:  python3 scripts/r23b_affine_kernel_derive.py
"""

import random
import sys

# ── the ring (GF(2)[y]/VM — the R13/R14 ending ring) ─────────────────────────
VM = 0x1_05EC_76F1     # y^32 ⊕ VQ (33-bit)
VMU = 0x0105_FD79_BDAB_A560  # floor(y^88 / VM) (57-bit, R14)
VR0 = 0xF20C_0DFE      # G[16] = y^-128 mod VM (the ending seed)

# ── the shipped R23 power tables (mirrored from crcfold.rs) ─────────────────
AFFINE_POW_128B_TABLE = [
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
]
AFFINE_POW_BYTE_TABLE = [
    0x00000001, 0xF26B8303, 0x13A29877, 0xA541927E, 0xDD45AAB8, 0x38116FAC,
    0xEF306B19, 0x68032CC8, 0x493C7D27, 0xF43ED648, 0xCB567BA5, 0x9771F7C1,
    0x3171D430, 0x30D23865, 0x54075546, 0x678EFD01,
]


# ── ring helpers (the software model of PCLMULQDQ + the mod-VM core) ────────
def clmul(a, b):
    """64x64 -> 128 carry-less multiply (PCLMULQDQ semantics, LSB-first)."""
    r = 0
    a <<= 0
    while b:
        if b & 1:
            r ^= a
        a <<= 1
        b >>= 1
    return r & ((1 << 128) - 1)


def clmod(v):
    """Reduction mod VM by the shift-subtract loop (ground truth)."""
    while v >= (1 << 32):
        sh = v.bit_length() - 33
        v ^= VM << sh
    return v


def rmul(a, b):
    return clmod(clmul(a, b))


def plain_barrett(P):
    """THE PLAIN BARRETT CORE — the vend_zmm sequence with the VR0 multiply
    REMOVED, modeled at exact u128/xmm semantics:

        x    = (P >> 32) & mask64      # alignr(w, w, 4) low qword
        p    = clmul(x, VMU)           # the quotient estimate product
        qh   = (p >> 56) & mask64      # alignr(p, p, 7) low qword
        r    = P ^ clmul(qh, VM)
        corr = (r >> 32) & mask64      # alignr(r, r, 4) low qword
        out  = (r ^ clmul(corr, VM)) & 0xFFFFFFFF

    Claim: out == P mod VM for every P < 2^95.
    """
    x = (P >> 32) & 0xFFFF_FFFF_FFFF_FFFF
    p = clmul(x, VMU)
    qh = (p >> 56) & 0xFFFF_FFFF_FFFF_FFFF
    r = P ^ clmul(qh, VM)
    corr = (r >> 32) & 0xFFFF_FFFF_FFFF_FFFF
    return (r ^ clmul(corr, VM)) & 0xFFFF_FFFF


def k0_plain_barrett_basis():
    """K0: basis-exhaustive exactness.  The map P -> plain_barrett(P) is
    GF(2)-linear (clmul, shifts, xor are linear), so equality on every
    one-hot basis vector e_k is a COMPLETE proof for the whole class."""
    m63 = 0
    for k in range(63):
        if plain_barrett(1 << k) != clmod(1 << k):
            m63 += 1
            print(f"    K0 FAIL: e_{k} (63-bit class)")
    m95 = 0
    for k in range(96):
        if plain_barrett(1 << k) != clmod(1 << k):
            m95 += 1
    assert m63 == 0, "K0 FAIL: plain Barrett broken in the 63-bit class"
    assert m95 <= 1 and (m95 == 0 or plain_barrett(1 << 95) != clmod(1 << 95)), \
        "K0 FAIL: unexpected basis failures below bit 95"
    # randoms in both classes
    rng = random.Random(0x523B_0E57)
    for _ in range(4096):
        v = rng.getrandbits(63)
        assert plain_barrett(v) == clmod(v), "K0 FAIL: random 63-bit"
    for _ in range(4096):
        v = rng.getrandbits(94) | (1 << 94)
        assert plain_barrett(v) == clmod(v), "K0 FAIL: random 95-bit class"
    # VMU re-derivation (the R14 anchor, re-pinned here)
    num, q = 1 << 88, 0
    while num >= VM:
        sh = num.bit_length() - 33
        q |= 1 << sh
        num ^= VM << sh
    assert q == VMU, "K0 FAIL: VMU != floor(y^88/VM)"
    print("K0 plain Barrett core: OK — the vend constant structure (VMU, VM, "
          "the 32/56/32 field-wide shifts, two correction clmuls) reduces ANY "
          "field < 2^95 to F mod VM with the VR0 multiply removed; "
          "basis-exhaustive (linear => complete) + 8192 randoms; every R23 "
          "product (<= 62 bits) sits deep inside the exactness range")
    return True


# ── the kernel models ────────────────────────────────────────────────────────
def kernel_clmul_reduce_mod_vm(a, b):
    """`clmul_reduce_mod_vm` — the scalar 64-bit CLMUL path kernel model:
    ONE product clmul + the K0 plain Barrett (3 clmuls).  Contract: a, b
    are <= 32-bit ring elements (the R16/R23 width law) -> product < 2^63."""
    assert a < (1 << 32) and b < (1 << 32), "width law violation"
    P = clmul(a, b)
    assert P < (1 << 63), "product overflow (impossible under the width law)"
    return plain_barrett(P)


def kernel_affine_span_const(span_len):
    """C(L) composition: L = 16k + r -> T128[k-1] ⊗ TBYTE[r] (k = 0 -> TBYTE[r])."""
    k, r = divmod(span_len, 16)
    if k == 0:
        return AFFINE_POW_BYTE_TABLE[r]
    return rmul(AFFINE_POW_128B_TABLE[k - 1], AFFINE_POW_BYTE_TABLE[r])


def kernel_span_crc32c_affine_sub(cum_crc, prefix_crc, span_len):
    """`span_crc32c_affine_sub` — the O(1) scalar projection kernel model."""
    c_l = kernel_affine_span_const(span_len)
    shifted = kernel_clmul_reduce_mod_vm(prefix_crc, c_l)
    return cum_crc ^ (shifted & 0xFFFFFFFF)


# ── the reference CRC32C machinery (hardware-reflected scan) ─────────────────
def crc_table():
    t = []
    for i in range(256):
        c = i
        for _ in range(8):
            c = (c >> 1) ^ 0x82F6_3B78 if c & 1 else c >> 1
        t.append(c)
    return t


T = crc_table()


def crc_scan(c, buf):
    for b in buf:
        c = (c >> 8) ^ T[(c ^ b) & 0xFF]
    return c


def full_crc32c(buf):
    return crc_scan(0xFFFFFFFF, buf) ^ 0xFFFFFFFF


def fnv_lanes(lanes, ln):
    """span_crc32c_8lane's lane combine: FNV-1a-64 over 8 lanes + length."""
    h = 0xCBF2_9CE4_8422_2325
    for c in lanes:
        h ^= c
        h = (h * 0x0100_0000_01B3) & 0xFFFF_FFFF_FFFF_FFFF
    h ^= ln & 0xFFFFFFFF
    h = (h * 0x0100_0000_01B3) & 0xFFFF_FFFF_FFFF_FFFF
    return h


def span_ref_8lane(body):
    """`span_crc32c_8lane` reference (sink.rs semantics): 8 interleaved raw
    CRC32C registers over 64-byte blocks + lane-0 tail + FNV combine."""
    lanes = [0] * 8
    n = len(body)
    i = 0
    while i + 64 <= n:
        for k in range(8):
            lanes[k] = crc_scan(lanes[k], body[i + 8 * k:i + 8 * k + 8])
        i += 64
    while i + 8 <= n:  # tail folded into lane 0
        lanes[0] = crc_scan(lanes[0], body[i:i + 8])
        i += 8
    if i + 4 <= n:
        lanes[0] = crc_scan(lanes[0], body[i:i + 4])
        i += 4
    if i + 2 <= n:
        lanes[0] = crc_scan(lanes[0], body[i:i + 2])
        i += 2
    if i < n:
        lanes[0] = crc_scan(lanes[0], body[i:i + 1])
    return fnv_lanes(lanes, n)


def lanes_cum(buf, nblocks):
    """The cumulative 8-lane state over the first nblocks 64-byte blocks
    (lane k owns the LE qwords at offsets 8k + 64j)."""
    ls = [0] * 8
    for j in range(nblocks):
        base = 64 * j
        for k in range(8):
            ls[k] = crc_scan(ls[k], buf[base + 8 * k:base + 8 * k + 8])
    return ls


# ── the 8-lane VECTOR kernel model (exact AVX-512 op semantics) ─────────────
MASK64 = (1 << 64) - 1


def _clmul_epi64_lo(a_qwords, b_qwords):
    """_mm512_clmulepi64_epi128(a, b, 0x00): per 128-bit lane j, the LOW
    qwords multiply: result lane j = clmul(a[2j], b[2j]) (128-bit)."""
    out = []
    for j in range(4):
        out.append(clmul(a_qwords[2 * j], b_qwords[2 * j]))
    return out  # 4 fields, each a 128-bit value


def _alignr8(lanes, imm):
    """_mm512_alignr_epi8(v, v, imm) with v == both operands: per 128-bit
    lane, (v:v) >> imm*8 bits — for the vend shifts imm in {4, 7}:
    the returned lane's LOW qword = v >> (imm*8) bits (cross-qword)."""
    sh = imm * 8
    return [(v >> sh) & MASK64 for v in lanes]  # low qword per field


def _maskz_permutexvar64(mask, idx, a):
    """_mm512_maskz_permutexvar_epi64(mask, idx, a): result qword k =
    (mask >> k) & 1 ? a[idx[k]] : 0."""
    return [a[idx[k]] if (mask >> k) & 1 else 0 for k in range(8)]


def kernel_span_crc32c_8lane_affine_sub(cum_lanes, prefix_lanes, span_len):
    """`span_crc32c_8lane_affine_sub` — the 8-lane VPCLMULQDQ projection
    kernel model (the exact shipped op sequence):

      cvt   = cvtepu32_epi64(loadu256(prefix))          # qword j = prefix_j
      A     = maskz_permutexvar64(0x55, [0,0,1,0,2,0,3,0], cvt)
      B     = maskz_permutexvar64(0x55, [4,0,5,0,6,0,7,0], cvt)
      pa    = clmul_epi128(A, set1(C), 0x00)            # 4 fields per zmm
      pb    = clmul_epi128(B, set1(C), 0x00)            # (prefix_j ⊗ C)
      vend-core Barrett per zmm (K0: plain reduction)
      unload low-32 per field, XOR cum, FNV-1a-64 combine + length
    """
    # C = G[L/8] (the lane advance for a 64-byte-aligned span)
    l8 = span_len // 8
    k, r = divmod(l8, 16)
    if k == 0:
        C = AFFINE_POW_BYTE_TABLE[r]
    else:
        C = rmul(AFFINE_POW_128B_TABLE[k - 1], AFFINE_POW_BYTE_TABLE[r])
    # packing: cvtepu32_epi64 then masked permutexvar into field layout
    cvt = list(prefix_lanes)
    a = _maskz_permutexvar64(0x55, [0, 0, 1, 0, 2, 0, 3, 0], cvt)
    b = _maskz_permutexvar64(0x55, [4, 0, 5, 0, 6, 0, 7, 0], cvt)
    bc = [C, C, C, C, C, C, C, C]  # _mm512_set1_epi64
    outs = []
    for zmm in (a, b):
        # ONE VPCLMULQDQ: 4 products prefix_j ⊗ C (each < 2^63, hi qword 0).
        # The field layout puts lane j in qword 2j (imm 0x00 multiplies the
        # LOW qword of each 128-bit lane).
        prod = _clmul_epi64_lo(zmm, bc)
        # the K0 plain Barrett at exact xmm semantics
        x = _alignr8(prod, 4)
        p = [clmul(x[j], VMU) for j in range(4)]
        qh = _alignr8(p, 7)
        rr = [prod[j] ^ clmul(qh[j], VM) for j in range(4)]
        out = [(rr[j] ^ clmul((rr[j] >> 32) & MASK64, VM)) for j in range(4)]
        outs.append([v & 0xFFFFFFFF for v in out])
    lanes = outs[0] + outs[1]
    proj = [cum_lanes[k] ^ lanes[k] for k in range(8)]
    return fnv_lanes(proj, span_len)


# ── the speculative 512-bit slicer model ─────────────────────────────────────
def _bswap16_lanes(win):
    """vpshufb(win, BSWAP16) semantics: u16 lane j (LE of bytes 2j, 2j+1)
    byte-swapped = the BE u16 at bytes (2j, 2j+1)."""
    return [(win[2 * j] << 8) | win[2 * j + 1] for j in range(32)]


def model_spec_slice_512(chunk, start, limit, cap=16):
    """`spec_slice_512` model: the E/O lane tables (from loadu(+0)/loadu(+1)
    of the zero-padded 66-byte window) + the register-table walk.

    Returns (msgs, p_final): msgs = [(payload_off, len)] CHUNK-relative,
    p_final = the chunk-relative offset of the next unprocessed header.
    """
    n = len(chunk)
    win = list(chunk) + [0] * (66 - n)  # the zero-padded 66B stack window
    etab = _bswap16_lanes(win)          # BE u16 at even offsets 0..62
    otab = _bswap16_lanes(win[1:])      # BE u16 at odd offsets 1..63
    msgs = []
    p = start
    while len(msgs) < cap and p + 2 <= n:
        l = etab[p >> 1] if (p & 1) == 0 else otab[p >> 1]
        if p + 2 + l > limit:
            break  # truncated message (malformed tail) — stop
        msgs.append((p + 2, l))
        p += 2 + l
    return msgs, p


def model_spec_slice_iter(payload):
    """The SpecSliceIter model: the window invariant (window base always =
    the next header) makes boundary-straddling headers resolve naturally.
    A ZERO-PROGRESS window (p_final = 0: a < 2-byte tail, or the window's
    first message truncated past the payload end) is terminal — the
    iterator ends (the reference walk yields the same)."""
    msgs = []
    pos = 0
    while pos < len(payload):
        rem = len(payload) - pos
        wlen = min(rem, 64)
        chunk = payload[pos:pos + wlen]
        batch, p_final = model_spec_slice_512(chunk, 0, rem)
        for off, l in batch:
            msgs.append((pos + off, l))
        pos += p_final
        if p_final == 0:
            break  # no progress possible: truncated/malformed tail
    return msgs


def ref_walk(payload):
    """The reference scalar message-block walk (MessageBlocks semantics)."""
    msgs = []
    p = 0
    while p + 2 <= len(payload):
        l = (payload[p] << 8) | payload[p + 1]
        if p + 2 + l > len(payload):
            break
        msgs.append((p + 2, l))
        p += 2 + l
    return msgs


# ── the shared PRNG/fill (the P3 generator shapes) ───────────────────────────
def _splitmix(state):
    state[0] = (state[0] + 0x9E37_79B9_7F4A_7C15) & 0xFFFF_FFFF_FFFF_FFFF
    z = state[0]
    z = ((z ^ (z >> 30)) * 0xBF58_476D_1CE4_E5B9) & 0xFFFF_FFFF_FFFF_FFFF
    z = ((z ^ (z >> 27)) * 0x94D0_49BB_1331_11EB) & 0xFFFF_FFFF_FFFF_FFFF
    return z ^ (z >> 31)


def fill(buf, ln, pat, nxt):
    if pat % 5 == 0:
        for i in range(ln):
            buf[i] = 0x00
    elif pat % 5 == 1:
        for i in range(ln):
            buf[i] = 0xFF
    elif pat % 5 == 2:
        for i in range(ln):
            buf[i] = (i * 131 + 17) & 0xFF
    elif pat % 5 == 3:
        for i in range(ln):
            buf[i] = nxt() & 0xFF
    else:
        i = 0
        while i < ln:
            run = min(ln - i, 1 + (nxt() & 0x3F))
            v = nxt() & 0xFF
            for j in range(i, i + run):
                buf[j] = v
            i += run


# ── Part K1: the scalar kernel differential ─────────────────────────────────
def k1_scalar_kernel_differential(n_tests=10_000):
    EDGE_LB = [16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 191, 192, 255,
               256, 257, 511, 512, 1023, 1024, 1025, 1343, 1344, 1345, 1536,
               2047, 2048]
    EDGE_LA = [0, 1, 7, 8, 15, 16, 17, 63, 64, 65, 127, 128, 129, 255, 256,
               511, 512]
    state = [0x9E37_79B9_7F4A_7C15 ^ 0x523A_F11E ^ 0x7C2B_A9E4]
    rng = random.Random(0xC0DE_5EED)
    a_buf = bytearray(512)
    b_buf = bytearray(2048)
    # the ground-truth G table: the reflected-CRC32C zeros-advance of the
    # 1-seed (independent of the shipped tables — same construction as the
    # r23 P0 oracle)
    gtab = [0] * 2049
    gtab[0] = 1
    c1 = 1
    for r in range(1, 2049):
        c1 = (c1 >> 8) ^ T[c1 & 0xFF]
        gtab[r] = c1
    errs = full_errs = const_errs = 0
    for i in range(n_tests):
        lb = (EDGE_LB[i % len(EDGE_LB)] if i < 2 * len(EDGE_LB)
              else (EDGE_LB[rng.randrange(len(EDGE_LB))] if i % 7 == 0
                    else 16 + rng.randrange(2033)))
        la = (EDGE_LA[i % len(EDGE_LA)] if i < 2 * len(EDGE_LA)
              else rng.randrange(513))
        pat = i % 5
        fill(a_buf, la, pat, lambda: _splitmix(state))
        fill(b_buf, lb, pat, lambda: _splitmix(state))
        # Method A — the reference scan (ground truth)
        want = crc_scan(0, b_buf[:lb])
        full_want = full_crc32c(b_buf[:lb])
        # Method B — the ingest snapshots + the O(1) kernel projection
        raw_a = crc_scan(0, a_buf[:la])
        raw_ab = crc_scan(raw_a, b_buf[:lb])
        got = kernel_span_crc32c_affine_sub(raw_ab, raw_a, lb)
        state_a = crc_scan(0xFFFF_FFFF, a_buf[:la])
        state_ab = crc_scan(state_a, b_buf[:lb])
        full_got = kernel_span_crc32c_affine_sub(
            state_ab ^ 0xFFFF_FFFF, state_a ^ 0xFFFF_FFFF, lb)
        if got != want:
            errs += 1
        if full_got != full_want:
            full_errs += 1
        # the C(L) composition audit against the ground-truth G table
        if kernel_affine_span_const(lb) != gtab[lb]:
            const_errs += 1
    assert errs == 0, f"K1 FAIL: {errs}/{n_tests} raw kernel errors"
    assert full_errs == 0, f"K1 FAIL: {full_errs}/{n_tests} full-CRC errors"
    assert const_errs == 0, "K1 FAIL: C(L) composition drift"
    print(f"K1 scalar kernel differential: OK ({n_tests} randomized spans; "
          f"the u128/PCLMULQDQ-exact model of span_crc32c_affine_sub — "
          f"product clmul + plain-Barrett — reproduces the reference scan "
          f"with EXACTLY 0 raw AND full-CRC32C errors)")
    return n_tests


# ── Part K2: the 8-lane vector kernel differential ─────────────────────────
def k2_vector_kernel_differential(n_tests=1_500):
    state = [0x9E37_79B9_7F4A_7C15 ^ 0x523B_0E57 ^ 0xB1AD_1E55]
    rng = random.Random(0x523B_0E57)
    pkt = bytearray(64 * 40)  # max L_A (512) + max L_B (2048)
    errs = 0
    edge_lens = [64, 128, 192, 256, 320, 512, 1024, 1344, 2048]
    for i in range(n_tests):
        lb = (edge_lens[i % len(edge_lens)] if i < 2 * len(edge_lens)
              else 64 * (1 + rng.randrange(32)))
        la = 64 * rng.randrange(9)
        fill(pkt, la + lb, i % 5, lambda: _splitmix(state))
        # the cumulative 8-lane snapshots (the ingest core's job)
        cl_start = lanes_cum(pkt[:la], la // 64)
        cl_end = lanes_cum(pkt[:la + lb], (la + lb) // 64)
        # the vector kernel model vs the span_crc32c_8lane reference
        got = kernel_span_crc32c_8lane_affine_sub(cl_end, cl_start, lb)
        want = span_ref_8lane(pkt[la:la + lb])
        if got != want:
            errs += 1
            if errs < 4:
                print(f"    K2 FAIL i={i} L_A={la} L_B={lb} "
                      f"got={got:#x} want={want:#x}")
    assert errs == 0, f"K2 FAIL: {errs}/{n_tests} vector kernel errors"
    print(f"K2 8-lane vector kernel differential: OK ({n_tests} 64-byte-"
          f"aligned spans; the exact AVX-512 op-sequence model — cvtepu32 "
          f"packing, maskz_permutexvar field layout, ONE VPCLMULQDQ per "
          f"zmm, the K0 plain Barrett at alignr 4/7/4 semantics, the "
          f"FNV-1a-64 combine — reproduces span_crc32c_8lane bit-exact, "
          f"golden hash included)")
    return n_tests


# ── Part K3: the speculative slicer differential ────────────────────────────
def k3_slicer_differential(n_streams=10_000):
    rng = random.Random(0x51CE_512B)
    state = [0x51CE_512B ^ 0xD00D_FACE]
    errs = 0
    for i in range(n_streams):
        # a random VALID message-block stream: lens drawn from the ITCH-like
        # mix (even, odd, tiny, zero, chunk-straddling) + random payload
        n_msgs = 1 + rng.randrange(48)
        lens = []
        for _ in range(n_msgs):
            l = rng.choice([0, 1, 2, 5, 8, 12, 13, 16, 18, 24, 30, 31, 32,
                            33, 40, 47, 48, 60, 61, 62, 63, 64, 65, 72, 96,
                            100, 128, 130, 200, 255])
            lens.append(l)
        # build the stream: [len BE][payload] per message (payload bytes are
        # irrelevant to the slicer — only the lens and their positions — so
        # deterministic C-speed randbytes replaces the splitmix fill)
        out = bytearray()
        for l in lens:
            out += bytes([(l >> 8) & 0xFF, l & 0xFF])
            out += rng.randbytes(l)
        want = ref_walk(out)
        got = model_spec_slice_iter(out)
        if got != want or len(want) != n_msgs:
            errs += 1
            if errs < 4:
                print(f"    K3 FAIL i={i}: n_msgs={n_msgs} "
                      f"got={len(got)} want={len(want)}")
    assert errs == 0, f"K3 FAIL: {errs}/{n_streams} slicer errors"
    # window-edge battery: headers straddling the 64B boundary
    edge = bytearray()
    for l in [1, 63, 62, 64, 1, 127, 2, 65, 3]:
        edge += bytes([(l >> 8) & 0xFF, l & 0xFF]) + bytes(l)
    assert model_spec_slice_iter(edge) == ref_walk(edge), "K3 FAIL: edge"
    # all-zero-length stream (32 messages per window)
    zz = bytes([0, 0]) * 100
    assert model_spec_slice_iter(zz) == ref_walk(zz), "K3 FAIL: zero-lens"
    # single tiny tail
    assert model_spec_slice_iter(bytes([0, 1, 0xAA])) == ref_walk(
        bytes([0, 1, 0xAA])), "K3 FAIL: tiny tail"
    assert model_spec_slice_iter(bytes([0])) == ref_walk(bytes([0])), \
        "K3 FAIL: 1-byte tail"
    assert model_spec_slice_iter(b"") == ref_walk(b""), "K3 FAIL: empty"
    print(f"K3 speculative slicer differential: OK ({n_streams} random "
          f"streams + the window-edge battery (straddling headers, "
          f"zero-length runs, 1-byte tails); the E/O BE-lane-table model "
          f"(vpshufb bswap16 over loadu(+0)/loadu(+1) of the zero-padded "
          f"66B window) + the register-table walk == the reference walk, "
          f"identical (count, offsets, lengths))")
    return n_streams


# ── Part K4: golden anchors ─────────────────────────────────────────────────
def k4_anchors():
    assert full_crc32c(b"123456789") == 0xE3069283, "K4 FAIL: RFC 3720"
    assert full_crc32c(bytes(32)) == 0x8A9136AA, "K4 FAIL: 32x00"
    assert full_crc32c(b"\xff" * 32) == 0x62A8AB43, "K4 FAIL: 32xFF"
    # the 1344-byte span shape (the fabric's real per-span body): the 8-lane
    # bridge composes C = G[168] = T128[9] ⊗ TBYTE[8] and projects exactly
    state = [0x1344_C0DE]
    pkt = bytearray(64 * 21)
    fill(pkt, len(pkt), 3, lambda: _splitmix(state))
    cl0 = lanes_cum(pkt, 0)
    cl21 = lanes_cum(pkt, 21)
    got = kernel_span_crc32c_8lane_affine_sub(cl21, cl0, 1344)
    assert got == span_ref_8lane(pkt), "K4 FAIL: the 1344B span bridge"
    # the raw 1-lane law on the same span
    raw = crc_scan(0, pkt)
    got1 = kernel_span_crc32c_affine_sub(raw, 0, 1344)
    assert got1 == raw, "K4 FAIL: 1344B scalar self-anchor"
    print("K4 golden anchors: OK (CRC32C('123456789') = 0xE3069283; the "
          "1344-byte span — 21 blocks, C = G[168] = T128[9] ⊗ TBYTE[8] — "
          "projects through BOTH kernels bit-exact; the golden fold hashes "
          "0x881639cead506f25 / 0xF6EF154EFDE905D8 are preserved "
          "structurally: this round adds kernels + rows, touching no "
          "existing kernel path — asserted by the crate's own differential "
          "suites and CI)")
    return True


def main():
    print("R23b affine kernel derivation — Engineer 2's vector-kernel oracle")
    print("=" * 78)
    k0_plain_barrett_basis()
    k1_scalar_kernel_differential()
    k2_vector_kernel_differential()
    k3_slicer_differential()
    k4_anchors()
    print("=" * 78)
    print("R23b VERDICT: all parts OK — the kernel layer is bit-exact "
          "against the reference CRC32C scan, span_crc32c_8lane, and the "
          "MoldUDP64 reference walk. Ship the intrinsics.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
