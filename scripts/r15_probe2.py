#!/usr/bin/env python3
"""Focused probe: (a) yinv correctness, (b) Z_r as ring multiply, (c) per-length
law validity, (d) VH64^2 =? VR0, (e) direction of the shift."""
import sys
sys.path.insert(0, 'scripts')
from r15_tail_derive import clmul, clmod, VM, VR0, VH64, ypow, M64
import random

rng = random.Random(42)

print("(a) yinv correctness: y^-e * y^e == 1 ?")
for e in [1, 8, 64, 128, 312, 576]:
    ok = clmod(clmul(ypow(-e), ypow(e))) == 1
    print(f"    e={e}: {ok}")

print("\n(b) Z_r(c) == c ⊗ Z_r(1) for random c?  (Z_r = r-byte zeros update)")
# build Z_r via table updates
def make_T():
    t = []
    for i in range(256):
        c = i
        for _ in range(8):
            c = (c >> 1) ^ 0x82F63B78 if c & 1 else c >> 1
        t.append(c)
    return t
T = make_T()
def zr(c, r):
    for _ in range(r):
        c = (c >> 8) ^ T[c & 0xFF]
    return c
for r in [1, 2, 3, 5, 8, 16, 24, 55, 71]:
    z1 = zr(1, r)
    ok = all(zr(c, r) == clmod(clmul(c, z1)) for c in [rng.randrange(1 << 32) for _ in range(20)])
    print(f"    r={r}: Z_r(1)={z1:#010x}  mult-consistent={ok}")

print("\n(c) per-length law: rawCRC(M) == M̂ ⊗ VR0 ⊗ y^(128-8m) ?")
def raw_crc(data):
    c = 0
    for b in data:
        c = (c >> 8) ^ T[(c ^ b) & 0xFF]
    return c
for m in list(range(1, 26)) + [28, 32, 40, 48, 55, 64, 71, 80]:
    ok = True
    for _ in range(10):
        M = bytes(rng.randrange(256) for _ in range(m))
        Mhat = int.from_bytes(M, 'little')
        got = clmod(clmul(clmod(clmul(Mhat, VR0)), ypow(128 - 8 * m)))
        if raw_crc(M) != got:
            ok = False
            break
    print(f"    m={m}: {'OK' if ok else 'FAIL'}")

print("\n(d) VH64^2 == VR0 ?  VH64*VH64 mod VM =", hex(clmod(clmul(VH64, VH64))), " VR0 =", hex(VR0))
print("    y^64 =", hex(ypow(64)), " y^-64 =", hex(ypow(-64)), " y^128 =", hex(ypow(128)), " y^-128 =", hex(ypow(-128)))
print("    VR0 == y^-128 ?", clmod(clmul(ypow(-128), 1)) == VR0 or ypow(-128) == VR0)
print("    VH64 == y^-64 ?", ypow(-64) == VH64, "  VH64 == y^64 ?", ypow(64) == VH64)

print("\n(e) Z_r(1) vs y-powers:")
for r in [1, 8, 16]:
    print(f"    Z_{r}(1) = {zr(1, r):#010x}   y^-{8*r} = {ypow(-8*r):#010x}   y^{8*r} = {ypow(8*r):#010x}")
