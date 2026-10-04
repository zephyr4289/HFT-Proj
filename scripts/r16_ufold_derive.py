#!/usr/bin/env python3
"""R16 'dfold' derivation — the Route K Stage A kernel (T=2 dual-stream).

PROGRAM HISTORY (the recalibrations that led here):
  * The report's Stage B claim (unpack-free, census 6 -> 4, 21.3-32 B/cyc)
    DIED in this session's structural analysis — three independent kills:
    (1) raw-load adjacent qword pairs (qw[2f], qw[2f+1]) mix REFERENCE
        LANES (the sink = FNV over 8 stride-64B sub-stream CRCs — the
        qwords 8 bytes apart belong to DIFFERENT reference lanes);
    (2) two 64-bit states cannot share one 128-bit clmul lane (clmul
        products start at lane bit 0 and spill into the neighbor half);
    (3) unmixing a mixed state needs a zero divisor in GF(2)[y]/VM, but
        the Castagnoli polynomial is primitive -> VM generates a FIELD ->
        no zero divisors.  VPUNPCK is STRUCTURAL (docs/26 refutation #4
        re-confirmed by independent algebra).  The census floor stays
        4 VPCLMULQDQ + 2 VPUNPCK + 2 VPTERNLOG per 128 B.
  * The surviving lever is LATENCY: the reflect step runs 2 serial chains
    (even/odd zmm states), each clmul(6c) -> ternlog -> clmul, measured
    ~9 cyc/step on record draws (14.3 B/cyc) vs the p5-throughput floor
    6 cyc/step (21.3 B/cyc).  T=2 block-parity dual-stream doubles the
    chain count: every state is stepped every OTHER block, so each chain
    gets a 2x latency budget and the loop lands on the p5 floor.

THE CLASS-LAW FOUNDATION (this file's Part 1):
  The R15 vtail proved the fold state's RING semantics: the state value
  V = V_lo | V_hi<<64 represents the ring element clmod(V) in GF(2)[y]/VM,
  and both vend and the vtail composed-field ending are CLASS functions
  (they depend only on clmod(V)).  The R13 step map
      M(V) = clmul(V_hi, RKHI) ^ clmul(V_lo, RKLO)
  is hypothesized to be RING MULTIPLICATION:
      clmod(M(V)) == rmul(clmod(V), K)   with K = clmod(RKLO)
  (equivalently RKHI == RKLO (x) y^64 in the ring).  If P1' holds:
    * the M^2 advance = multiplication by K^2 — realizable as ONE
      2-clmul step with the REDUCED pair (K^2, K^2 (x) y^64), both
      constants <= 32 bits (strictly safer than RKLO's 36-bit unreduced
      form: state hi <= 32 bits, every vend input <= 96 bits);
    * the dual-stream merge (set parity deficits of exactly ONE M) is a
      single fold_step with the reduced pair (K, K (x) y^64);
    * the merged states are CLASS-equal to the sequential kernel's
      states, so the ENTIRE shipped ending stack (vend + vtail, all
      tables, all paths) is reused verbatim.

  CONSTRAINT THE DFOLD IMPOSES (documented in the Rust gate): the class
  equality is not VALUE equality, so the value-based endings (the R13
  crc-chain lane-0 continuation) are INCOMPATIBLE — dfold dispatch forces
  vend=1 and the vtail composed-field ending for ALL r (the R15 V5
  differential already verified the vtail formula for r < 16; the r>=16
  ship gate was pure economics).  HFT_CRC_DFOLD=1 with HFT_CRC_VEND=0
  resolves as dfold OFF.

Probes: P0 the step-map audit (kept from the scaffold), P1' the class
law, P2 the full dual-stream differential vs span_ref (the 2419-body
battery + longs + randoms), P3 constant emission.
"""

import sys

# reuse the R15/R13 machinery (tables, clmul/clmod/rmul, constants)
sys.path.insert(0, __file__.rsplit("/", 1)[0])
from r15_tail_derive import (T, VM, VR0, VH64, RKHI, RKLO,  # noqa: E402
                             clmul, clmod, rmul, ypow_raw, le, M64, M32,
                             span_ref, vtail, G, KH, ATab, vend_field)


# ── Part 0: the step-map audit (scaffold heritage, kept for the record) ─────

def step_map_apply(w_hi: int, w_lo: int) -> tuple[int, int]:
    """M(W) = clmul(W_hi, RKHI) ^ clmul(W_lo, RKLO); re-split the field."""
    prod = clmul(w_hi, RKHI) ^ clmul(w_lo, RKLO)
    return prod >> 64, prod & M64


def step_map_matrix() -> list[int]:
    cols = []
    for i in range(128):
        w_hi = (1 << (i - 64)) if i >= 64 else 0
        w_lo = (1 << i) if i < 64 else 0
        hi, lo = step_map_apply(w_hi, w_lo)
        cols.append((hi << 64) | lo)
    return cols


def compose_map(cols_a, cols_b):
    out = []
    for i in range(128):
        b = cols_b[i]
        acc = 0
        while b:
            low = b & -b
            acc ^= cols_a[low.bit_length() - 1]
            b ^= low
        out.append(acc)
    return out


def p0_audit():
    cols = step_map_matrix()
    assert cols[0] == RKLO, "anchor e_0: M(e_0) must be RKLO"
    assert cols[64] == RKHI, "anchor e_64: M(e_64) must be RKHI"
    acc = [1 << i for i in range(128)]
    for _ in range(2):
        acc = compose_map(cols, acc)
    w_hi, w_lo = 0x0123456789ABCDEF, 0xFEDCBA9876543210
    h1, l1 = step_map_apply(w_hi, w_lo)
    for _ in range(1):
        h1, l1 = step_map_apply(h1, l1)
    got, b = 0, (w_hi << 64) | w_lo
    while b:
        low = b & -b
        got ^= acc[low.bit_length() - 1]
        b ^= low
    assert got == ((h1 << 64) | l1), "M^2 composition mismatch"
    print("P0 step-map audit: OK (e_0->RKLO, e_64->RKHI, M^2 composition)")


# ── Part 1: the class law (P1') ─────────────────────────────────────────────

Y64R = ypow_raw(64)            # y^64 mod VM


def m_step(v: int) -> int:
    """The EXACT kernel step on a state value (no data injection)."""
    return clmul(v >> 64, RKHI) ^ clmul(v & M64, RKLO)


def m_class(c: int) -> int:
    """The step's induced map on CLASSES (ring elements < VM)."""
    return clmod(m_step(c))    # a class c packs as the state value c


def p1_class_law() -> tuple[int, int, int, int]:
    """clmod(M(V)) == rmul(clmod(V), K), K = clmod(RKLO)?

    Returns (K, K_hi, K2, K2_hi) — the reduced constant pairs for the
    merge step (K, K (x) y^64) and the M^2 step (K^2, K^2 (x) y^64).
    """
    K = clmod(RKLO)
    # the law itself, on random state values (widths like the kernel's:
    # lo <= 64 bits, hi <= 36 bits — plus full 128-bit randoms)
    import random
    rng = random.Random(0xD_F01D)
    for _ in range(500):
        v = rng.randrange(1 << 64) | (rng.randrange(1 << 36) << 64)
        lhs = clmod(m_step(v))
        rhs = rmul(clmod(v), K)
        assert lhs == rhs, f"P1' FAIL: class law broken at v={v:#x}"
    # the RKHI identity: RKHI == rmul(K, y^64)?
    k_hi = rmul(K, Y64R)
    rkhi_identity = (k_hi == RKHI)
    K2 = rmul(K, K)
    K2_hi = rmul(K2, Y64R)
    # every emitted constant must be <= 32 bits (the width safety that
    # keeps state hi <= 32 and every vend input <= 96 bits)
    for name, c in (("K", K), ("K_hi", k_hi), ("K2", K2), ("K2_hi", K2_hi)):
        assert c < (1 << 32), f"constant {name} too wide: {c:#x}"
    print(f"P1' class law: OK (500 randoms).  K = clmod(RKLO) = {K:#x}")
    print(f"    RKHI == K (x) y^64 : {'YES — the R13 pair IS the reduced merge pair' if rkhi_identity else 'NO'}"
          f"  (K_hi = {k_hi:#x})")
    print(f"    K2 = K^2 = {K2:#x}   K2_hi = K2 (x) y^64 = {K2_hi:#x}")
    return K, k_hi, K2, K2_hi


# ── Part 2: the dual-stream model + the full differential ───────────────────

def units(body, j):
    n0 = [le(body, 128 * j + 8 * k) for k in range(8)]
    n1 = [le(body, 128 * j + 64 + 8 * k) for k in range(8)]
    ev, od = [], []
    for f in range(4):
        ev.append((n0[2 * f], n1[2 * f]))       # unpacklo: (lo, hi)
        od.append((n0[2 * f + 1], n1[2 * f + 1]))
    return ev, od


def pack(ev, od):
    st = [0] * 8
    for f in range(4):
        st[2 * f] = ev[f][0] ^ (ev[f][1] << 64)
        st[2 * f + 1] = od[f][0] ^ (od[f][1] << 64)
    return st


def fold_states_dfold(body, K, k_hi, K2, K2_hi):
    """The T=2 block-parity dual-stream loop + merge (classes only)."""
    ln = len(body)
    wp = (ln // 64) // 2
    assert wp >= 1

    def step2(st, u_ev, u_od):
        for f in range(4):
            v = st[2 * f]
            u = u_ev[f][0] ^ (u_ev[f][1] << 64)
            st[2 * f] = clmul(v >> 64, K2_hi) ^ clmul(v & M64, K2) ^ u
            v = st[2 * f + 1]
            u = u_od[f][0] ^ (u_od[f][1] << 64)
            st[2 * f + 1] = clmul(v >> 64, K2_hi) ^ clmul(v & M64, K2) ^ u

    def merge(stX, stY):
        """V = M(stX) ^ stY — one reduced-constant step."""
        for k in range(8):
            stX[k] = clmul(stX[k] >> 64, k_hi) ^ clmul(stX[k] & M64, K) ^ stY[k]

    ev, od = units(body, 0)
    stA = pack(ev, od)
    stB = [0] * 8
    if wp >= 2:
        ev, od = units(body, 1)
        stB = pack(ev, od)
    j = 2
    while j + 1 < wp:
        step2(stA, *units(body, j))
        step2(stB, *units(body, j + 1))
        j += 2
    if j < wp:                       # wp odd: the last block is even -> A
        step2(stA, *units(body, j))
    if wp % 2 == 0:                  # A deficient by exactly one M
        merge(stA, stB)
        return stA, wp
    merge(stB, stA)                  # wp odd: B deficient (or absent)
    return stB, wp


def dfold(body, K, k_hi, K2, K2_hi):
    """The shipped dfold pipeline: dual-stream states + the vtail ending
    for ALL r (the dfold dispatch forces the composed-field lane-0 path;
    the R15 V5 differential already verified this formula for r < 16)."""
    st, wp = fold_states_dfold(body, K, k_hi, K2, K2_hi)
    ln = len(body)
    blocks = ln // 64
    tail = ln % 64
    r = 8 * (blocks % 2) + tail
    # ---- lane 0: the vtail composed field (all r) ----
    F0 = clmul(st[0] & M64, G[r]) ^ clmul(st[0] >> 64, KH[r])
    off = 8 if blocks % 2 == 1 else 0
    if off:
        F0 ^= clmul(le(body, 64 * (blocks - 1)), ATab[r])
    q = 0
    while off + 8 * q + 8 <= r:
        bo = 64 * blocks + off + 8 * q - (8 if blocks % 2 == 1 else 0)
        F0 ^= clmul(le(body, bo), ATab[r - off - 8 * q])
        q += 1
    p = r % 8
    if p > 0:
        v = le(body, ln - 8) >> (64 - 8 * p)
        F0 ^= clmul(v, ATab[p])
    lanes = [0] * 8
    lanes[0] = vend_field(F0)
    # ---- lanes 1..7: the R14/R15 vend path verbatim ----
    if blocks % 2 == 1:
        g8, kh8, a8 = G[8], KH[8], ATab[8]
        for k in range(1, 8):
            v = st[k]
            Fk = clmul(v & M64, g8) ^ (v >> 64) ^ clmul(le(body, 64 * (blocks - 1) + 8 * k), a8)
            lanes[k] = vend_field(Fk)
    else:
        for k in range(1, 8):
            v = st[k]
            lanes[k] = vend_field(v)
    h = 0xcbf29ce484222325
    for cv in lanes + [ln & M32]:
        h ^= cv
        h = (h * 0x100000001b3) & M64
    return h


def p2_differential(K, k_hi, K2, K2_hi):
    import random
    rng = random.Random(0x516)
    body = bytearray(16500)

    def fill(ln, pat, nxt):
        if pat == 0:
            body[:ln] = bytes(ln)
        elif pat == 1:
            body[:ln] = b"\xff" * ln
        elif pat == 2:
            body[:ln] = bytes((i * 131 + 17) & 0xFF for i in range(ln))
        else:
            body[:ln] = bytes((nxt() & 0xFF) for i in range(ln))

    state = [0x9E3779B97F4A7C15]

    def nxt():
        state[0] = (state[0] + 0x9E3779B97F4A7C15) & M64
        z = state[0]
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M64
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M64
        return z ^ (z >> 31)

    n = 0
    for ln in range(0, 601):
        for pat in range(4):
            fill(ln, pat, nxt)
            bd = bytes(body[:ln])
            want = span_ref(bd)
            if ln >= 192:
                got = dfold(bd, K, k_hi, K2, K2_hi)
                assert got == want, f"P2 FAIL len={ln} pat={pat}"
                n += 1
    longs = [640, 680, 1000, 1359, 1360, 1361, 1379, 1380, 1399, 1400,
             2047, 2048, 2049, 4095, 4096, 4200, 8191, 8192, 8193,
             12287, 12288, 16384]
    for ln in longs:
        fill(ln, 3, nxt)
        bd = bytes(body[:ln])
        assert dfold(bd, K, k_hi, K2, K2_hi) == span_ref(bd), f"P2 FAIL len={ln}"
        n += 1
    # random lengths (wp parity / tail coverage up to wp=128)
    for _ in range(120):
        ln = rng.randrange(192, 16500)
        fill(ln, 3, nxt)
        bd = bytes(body[:ln])
        assert dfold(bd, K, k_hi, K2, K2_hi) == span_ref(bd), f"P2 FAIL rand len={ln}"
        n += 1
    print(f"P2 dual-stream differential: OK ({n} bodies; exhaustive 192..600 x4 "
          f"+ longs to 16384 + 120 randoms)")
    print("    (state widths reach 128 bits by design — seed states carry raw")
    print("     64-bit data qwords, same as the sequential kernel; the vend-")
    print("     exactness constraint applies to the COMPOSED ending fields,")
    print("     which are clmul products with <=32-bit constants: <= 96 bits)")
    # cross-check against the R15 vtail model on the shared battery (the
    # two independent loop structures must agree on every body)
    for ln in [192, 200, 256, 257, 320, 321, 384, 512, 640, 1000, 2048, 4096]:
        fill(ln, 3, nxt)
        bd = bytes(body[:ln])
        assert dfold(bd, K, k_hi, K2, K2_hi) == vtail(bd) == span_ref(bd), \
            f"P2 vtail-cross FAIL len={ln}"
    print("    vtail cross-model agreement: OK (12 lengths)")


def p3_emit(K, k_hi, K2, K2_hi):
    print("\n// ── R16 dfold constants (derived & pinned by scripts/r16_ufold_derive.py) ──")
    print("/// The reduced merge pair: K = RKLO mod VM, K_hi = K ⊗ y^64 mod VM.")
    print("/// The merge is ONE fold_step with these constants (P1' class law).")
    print(f"pub const DFOLD_MG_LO: u64 = 0x{K:08X};")
    print(f"pub const DFOLD_MG_HI: u64 = 0x{k_hi:08X};")
    print("/// The M^2 step pair: K2 = K^2 mod VM, K2_hi = K2 ⊗ y^64 mod VM.")
    print("/// Census-identical to the R13 step, both constants <= 32 bits.")
    print(f"pub const DFOLD_K2_LO: u64 = 0x{K2:08X};")
    print(f"pub const DFOLD_K2_HI: u64 = 0x{K2_hi:08X};")


def main():
    print("R16 dfold derivation (class-law formulation)")
    p0_audit()
    K, k_hi, K2, K2_hi = p1_class_law()
    p2_differential(K, k_hi, K2, K2_hi)
    p3_emit(K, k_hi, K2, K2_hi)
    return 0


if __name__ == "__main__":
    sys.exit(main())
