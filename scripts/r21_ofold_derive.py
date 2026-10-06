#!/usr/bin/env python3
"""R21 'ofold' derivation — the octo-stream (T=8) natural-domain fold.

The R16 dfold proved the class law P1' for the step map
    M(V) = clmul(V_hi, RKHI) ^ clmul(V_lo, RKLO)
as RING MULTIPLICATION by K = clmod(RKLO) in GF(2)[y]/VM: advancing a
state by k fold units is ring multiplication by K^k, realizable as ONE
2-clmul step with the REDUCED pair (K^k, K^k (x) y^64).  The dfold
shipped k=2 (the M^2 step pair DFOLD_K2_*).  This file derives and
verifies the OCTO-STREAM extension:

  * the T=8 step pair  (K^8, K^8 (x) y^64)          — 8 units per step;
  * the merge pairs    (K^c, K^c (x) y^64), c=1..7  — the inter-stream
    offsets (stream m's units sit C_m = (wp-1-m) mod 8 units before the
    stream whose last unit is the body's last).

Both reduce to the R15 G-table law: K^k = y^(-128k) = G[16k] and
K^k (x) y^64 = y^(-128k+64) = G[16k-8].  The shipped tables stop at
G[71]; the octo fold needs G[72..=128].  Part 0 extends the table with
the SAME table-driven reflected-CRC32C advance (c = (c >> 8) ^ T[c & 0xFF]
from seed 1) and validates the extension against the shipped G[0..72].

Parts:
  P0  G-table extension + validation against r15_tail_derive.G
  P1  the class law for every emitted constant: clmod(M^k(V)) ==
      rmul(clmod(V), K^k) on random states, k = 1..8 (P1' at all octo
      powers), plus the width audit (every constant <= 32 bits)
  P2  the full octo-stream differential vs the scalar reference
      span_ref: exhaustive lengths 192..700 x 4 patterns + longs to
      16384 + 120 randoms — the same battery the R16 P2 differential
      certified dfold with
  P3  Rust constant emission

Constants are emitted as OFOLD_* (step + the 7 merge pairs).  The Rust
port re-derives G[72..=128] at test time (t_ofold_constants_derivation)
and the differential sweep pins every body bit-exact.
"""

import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from r15_tail_derive import (VM, VR0, VH64, RKHI, RKLO,  # noqa: E402
                             clmul, clmod, rmul, le, M64, M32,
                             span_ref, vtail, G, KH, ATab, vend_field)


# ── Part 0: the G-table extension ────────────────────────────────────────────

def g_extend(n: int) -> list[int]:
    """G[r] = y^(-8r) mod VM for r = 0..n-1, via the reflected-CRC32C
    table advance from seed 1 (Z_r(1), docs/27 §2 — the r-byte
    zeros-update of the reflected LFSR)."""
    t = [0] * 256
    for i in range(256):
        c = i
        for _ in range(8):
            c = (c >> 1) ^ (0x82F6_3B78 if c & 1 else 0)
        t[i] = c

    def adv(c: int) -> int:
        return (c >> 8) ^ t[c & 0xFF]

    g = [1]
    for _ in range(n - 1):
        g.append(adv(g[-1]))
    return g


def p0_g_extension() -> list[int]:
    g128 = g_extend(129)
    # the extension must reproduce the SHIPPED table exactly
    for r in range(72):
        assert g128[r] == G[r], f"P0 FAIL: G[{r}] shipped {G[r]:#x} vs derived {g128[r]:#x}"
    # inverse-powers law spot check: G[r] * y^(8r) == 1 (mod VM)
    def ypow(k: int) -> int:
        acc = 1
        base = 2  # y mod VM
        e = k
        while e:
            if e & 1:
                acc = rmul(acc, base)
            base = rmul(base, base)
            e >>= 1
        return acc

    for r in (1, 8, 16, 32, 64, 100, 128):
        assert rmul(g128[r], ypow(8 * r)) == 1, f"P0 FAIL: G[{r}] not the y^-{8*r} inverse"
    print(f"P0 G-extension: OK (129 entries; shipped G[0..72] reproduced "
          f"verbatim; inverse-powers law pinned at 7 probes)")
    return g128


# ── Part 1: the class law at the octo powers ────────────────────────────────

def m_step(v: int) -> int:
    return clmul(v >> 64, RKHI) ^ clmul(v & M64, RKLO)


def p1_class_law(g128: list[int]):
    K = clmod(RKLO)
    assert K == VR0, f"P1 FAIL: K {K:#x} != VR0 {VR0:#x}"
    import random
    rng = random.Random(0x0_F01D_21)
    powers = [(g128[16 * k - 8], g128[16 * k]) for k in range(1, 9)]
    # width audit (the vend-input safety: constants <= 32 bits keeps every
    # composed ending field <= 96 bits — the R16 width law)
    for k, (khi, klo) in enumerate(powers, start=1):
        assert khi < (1 << 32) and klo < (1 << 32), \
            f"P1 FAIL: k={k} constant too wide: khi={khi:#x} klo={klo:#x}"
    # the pairs ARE (K^k (x) y^64, K^k): klo_k == K^k (stepwise rmul),
    # khi_k == K^k (x) y^64
    base, e = 2, 64
    acc = 1
    while e:
        if e & 1:
            acc = rmul(acc, base)
        base = rmul(base, base)
        e >>= 1
    y64 = acc
    Kk = 1
    for k, (khi, klo) in enumerate(powers, start=1):
        Kk = rmul(Kk, K)
        assert klo == Kk, f"P1 FAIL: k={k} klo {klo:#x} != K^{k} {Kk:#x}"
        assert khi == rmul(Kk, y64), f"P1 FAIL: k={k} khi != K^{k} (x) y^64"
    # the law itself, on random FULL-WIDTH states shaped like the
    # kernel's (lo <= 64 bits, hi <= 33 — the seeded-raw and post-step
    # shapes): ONE reduced-pair application == k stepwise M advances,
    # in CLASS terms.
    for _ in range(300):
        v = rng.randrange(1 << 64) | (rng.randrange(1 << 33) << 64)
        c0 = clmod(v)
        for k, (khi, klo) in enumerate(powers, start=1):
            # k stepwise advances from c0
            cls = c0
            for _ in range(k):
                cls = rmul(cls, K)
            # one reduced-pair application on the raw state
            got = clmod(clmul(v >> 64, khi) ^ clmul(v & M64, klo))
            assert got == cls, \
                f"P1 FAIL: reduced pair k={k} class law broken at v={v:#x}"
    print(f"P1 class law: OK (300 randoms x k=1..8 reduced pairs vs stepwise "
          f"advances; pairs ARE (K^c (x) y^64, K^c); all constants <= 32 bits)")
    return powers


# ── Part 2: the octo-stream model + the full differential ───────────────────

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


def fold_states_ofold(body, K8_hi, K8, MG):
    """The T=8 octo-stream loop + merge (mod-8 block-pair streams)."""
    ln = len(body)
    wp = (ln // 64) // 2
    assert wp >= 1

    def step8(st, u_ev, u_od):
        for f in range(4):
            v = st[2 * f]
            u = u_ev[f][0] ^ (u_ev[f][1] << 64)
            st[2 * f] = clmul(v >> 64, K8_hi) ^ clmul(v & M64, K8) ^ u
            v = st[2 * f + 1]
            u = u_od[f][0] ^ (u_od[f][1] << 64)
            st[2 * f + 1] = clmul(v >> 64, K8_hi) ^ clmul(v & M64, K8) ^ u

    st = [[0] * 8 for _ in range(8)]
    for m in range(min(8, wp)):
        ev, od = units(body, m)
        st[m] = pack(ev, od)
    q = 8
    while q < wp:
        m = q % 8
        ev, od = units(body, q)
        step8(st[m], ev, od)
        q += 1
    # Merge: base = the stream owning the body's LAST unit; every other
    # live stream folds in with its offset pair.  C_m = (wp-1-m) mod 8.
    base = (wp - 1) % 8
    acc = st[base][:]
    for m in range(min(8, wp)):
        if m == base:
            continue
        c = (wp - 1 - m) % 8
        khi, klo = MG[c - 1]  # MG[0] is the offset-1 pair (list is 0-indexed)
        for k in range(8):
            v = st[m][k]
            acc[k] ^= clmul(v >> 64, khi) ^ clmul(v & M64, klo)
    return acc, wp


def ofold(body, K8_hi, K8, MG):
    """The shipped-ofold pipeline: octo-stream states + the vtail ending
    for ALL r (the class-exact states force the composed-field lane-0
    path — the dfold dispatch law)."""
    st, wp = fold_states_ofold(body, K8_hi, K8, MG)
    ln = len(body)
    blocks = ln // 64
    tail = ln % 64
    r = 8 * (blocks % 2) + tail
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


def p2_differential(K8_hi, K8, MG):
    import random
    rng = random.Random(0x516_21)
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
    for ln in range(192, 701):
        for pat in range(4):
            fill(ln, pat, nxt)
            bd = bytes(body[:ln])
            want = span_ref(bd)
            got = ofold(bd, K8_hi, K8, MG)
            assert got == want, f"P2 FAIL len={ln} pat={pat}"
            n += 1
    longs = [640, 680, 1000, 1359, 1360, 1361, 1379, 1380, 1399, 1400,
             2047, 2048, 2049, 4095, 4096, 4200, 8191, 8192, 8193,
             12287, 12288, 16384]
    for ln in longs:
        fill(ln, 3, nxt)
        bd = bytes(body[:ln])
        assert ofold(bd, K8_hi, K8, MG) == span_ref(bd), f"P2 FAIL len={ln}"
        n += 1
    for _ in range(120):
        ln = rng.randrange(192, 16500)
        fill(ln, 3, nxt)
        bd = bytes(body[:ln])
        assert ofold(bd, K8_hi, K8, MG) == span_ref(bd), f"P2 FAIL rand len={ln}"
        n += 1
    # cross-model agreement with the R15 vtail + R16 dfold shapes on the
    # shared battery (three independent loop structures, one value)
    from r16_ufold_derive import dfold
    K = clmod(RKLO)
    k_hi = rmul(K, _ypow(64))
    K2 = rmul(K, K)
    K2_hi = rmul(K2, _ypow(64))
    for ln in [192, 200, 256, 257, 320, 321, 384, 512, 640, 1000, 2048, 4096]:
        fill(ln, 3, nxt)
        bd = bytes(body[:ln])
        assert ofold(bd, K8_hi, K8, MG) == vtail(bd) == dfold(bd, K, k_hi, K2, K2_hi) \
            == span_ref(bd), f"P2 cross FAIL len={ln}"
    print(f"P2 octo-stream differential: OK ({n} bodies; exhaustive 192..700 x4 "
          f"+ longs to 16384 + 120 randoms)")
    print("    vtail/dfold cross-model agreement: OK (12 lengths)")


def _ypow(k: int) -> int:
    acc, base, e = 1, 2, k
    while e:
        if e & 1:
            acc = rmul(acc, base)
        base = rmul(base, base)
        e >>= 1
    return acc


def p3_emit(g128, powers):
    names = ["MG1", "MG2", "MG3", "MG4", "MG5", "MG6", "MG7"]
    print("\n// ── R21 ofold constants (derived & pinned by scripts/r21_ofold_derive.py) ──")
    print("/// The T=8 step pair: K^8 = G[128], K^8 (x) y^64 = G[120].")
    print(f"pub const OFOLD_K8_HI: u64 = 0x{powers[7][0]:08X};")
    print(f"pub const OFOLD_K8_LO: u64 = 0x{powers[7][1]:08X};")
    for c in range(1, 8):
        khi, klo = powers[c - 1]
        print(f"/// The offset-{c} merge pair: K^{c} (x) y^64 = G[{16*c-8}], K^{c} = G[{16*c}].")
        print(f"pub const OFOLD_{names[c-1]}_HI: u64 = 0x{khi:08X};")
        print(f"pub const OFOLD_{names[c-1]}_LO: u64 = 0x{klo:08X};")
    print("/// The G-table extension (the Rust test re-derives G[72..=128] and")
    print("/// checks these against the advance recurrence).")
    print("pub const OFOLD_G_EXT: [u32; 57] = [")
    for r in range(72, 129):
        print(f"    0x{g128[r]:08X},")
    print("];")


def main():
    print("R21 ofold derivation (octo-stream class-law formulation)")
    g128 = p0_g_extension()
    powers = p1_class_law(g128)
    p2_differential(powers[7][0], powers[7][1], powers)
    p3_emit(g128, powers)
    return 0


if __name__ == "__main__":
    sys.exit(main())
