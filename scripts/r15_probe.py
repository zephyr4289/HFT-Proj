#!/usr/bin/env python3
"""Probe the rawCRC monomial law empirically: contribution(i, m) for a single
bit at processing-position i in an m-byte message. Fit contribution = VR0 * y^e."""
import sys
sys.path.insert(0, 'scripts')
from r15_tail_derive import T, crc_u8, clmul, clmod, VM, VR0, ypow, M64, M32

def raw_crc(data):
    c = 0
    for b in data:
        c = crc_u8(c, b)
    return c

def bit_crc(m, bitpos):
    """rawCRC of the m-byte message with a single 1 bit at processing-position
    bitpos (byte bitpos//8, bit bitpos%8)."""
    buf = bytearray(m)
    buf[bitpos // 8] |= 1 << (bitpos % 8)
    return raw_crc(bytes(buf))

def ring_log(x, lo=-4096, hi=4096):
    """find e with y^e == x (mod VM), if any"""
    table = {}
    v = 1
    for e in range(0, hi + 1):
        table[v] = e
        v = clmod(clmul(v, 2))
    # negative: walk y^-1
    yinv = None
    # y^32 ≡ VQ => y^-1 ≡ y^31 * VQ^-1
    # brute: solve y * u == 1
    for u in range(1, 1 << 33):
        pass  # too slow; use the positive table: y^-e = y^(N-e) if ord(y)=N known
    # find ord(y): iterate until back to 1
    v = 1; n = 0
    while True:
        v = clmod(clmul(v, 2)); n += 1
        if v == 1: break
        if n > 200000: return None
    order = n
    for e in range(0, order):
        if ypow(e) == x:
            return e if e <= order // 2 else e - order
    return None

print("probing the monomial family (skipping ord(y) brute walk)...")

# monomial family over various message lengths
print("\ncontribution(i, m) — is it VR0 ⊗ y^i (length-independent)?")
for m in [1, 2, 4, 8, 16, 24, 55]:
    ok = True
    for i in range(0, min(m * 8, 40)):
        c = bit_crc(m, i)
        want = clmod(clmul(VR0, ypow(i)))
        if c != want:
            ok = False
            print(f"  m={m} i={i}: got {c:#x} want {want:#x}  log2-diff")
            break
    if ok:
        print(f"  m={m}: contribution(i) == VR0*y^i for all i<40  ✓ LENGTH-INDEPENDENT")

# so what IS the right law? contribution(i) = VR0 * y^i, so
# rawCRC(M) = sum_i M_i * VR0 * y^i = Mhat * VR0  -- NO length factor?!
print("\ncheck rawCRC(M) == Mhat ⊗ VR0 mod VM (no length factor):")
import random
rng = random.Random(7)
for trial in range(200):
    m = rng.randrange(1, 80)
    M = bytes(rng.randrange(256) for _ in range(m))
    Mhat = int.from_bytes(M, 'little')
    want = raw_crc(M)
    got = clmod(clmul(Mhat, VR0))
    if want != got:
        print(f"  FAIL len={m}")
        break
else:
    print("  ✓ rawCRC(M) = M̂ ⊗ VR0 mod VM — NO length factor")

# then the chain law: crc(init=c, D) = ?
print("\nprobing the chain law: crc(init=c, D) = ?")
for d in [0, 1, 2, 3, 8, 16, 24]:
    # c basis: c = 1
    c = 1
    D = bytes(d)
    got = c
    for b in D:
        got = crc_u8(got, b)
    # candidate: c * y^(8d) mod VM?
    cand = clmod(clmul(c, ypow(8 * d)))
    print(f"  d={d}: chain(1, 0^{d}) = {got:#010x}  1*y^{8*d} = {cand:#010x}  match={got==cand}")
