#!/usr/bin/env python3
"""Brute-force which fold_extra arrangement reproduces the reference at len=200."""
import sys, itertools
sys.path.insert(0, 'scripts')
from r15_tail_derive import crc_u64, clmul, le, M64, RKHI, RKLO, vend

body = bytes((i * 131 + 17) & 0xFF for i in range(200))
s0, s1, s2, t0 = le(body, 0), le(body, 64), le(body, 128), le(body, 192)
ref0 = crc_u64(crc_u64(crc_u64(crc_u64(0, s0), s1), s2), t0)
print(f"ref lane0 = {ref0:08x}")

def step(v_hi, v_lo, u_hi, u_lo, hi_const, lo_const):
    r = clmul(v_hi, hi_const) ^ clmul(v_lo, lo_const) ^ (u_lo ^ (u_hi << 64))
    return r >> 64, r & M64

found = []
for kc, lc, uorder, seedorder in itertools.product([RKHI, RKLO], [RKHI, RKLO],
                                                    [0, 1], [0, 1]):
    # seed: state = unit0 = [s0 || s1]: lo = s0, hi = s1 (stream order)
    if seedorder == 0:
        v_lo, v_hi = s0, s1
    else:
        v_lo, v_hi = s1, s0
    # unit1 = [s2 || t0]
    if uorder == 0:
        u_lo, u_hi = s2, t0
    else:
        u_lo, u_hi = t0, s2
    nh, nl = step(v_hi, v_lo, u_hi, u_lo, kc, lc)
    # ending: double-crc
    got = crc_u64(crc_u64(0, nl), nh)
    # also vend ending
    got_vend = vend(nl, nh)
    tag = f"hi⊗{('RKHI' if kc==RKHI else 'RKLO'):5s} lo⊗{('RKHI' if lc==RKHI else 'RKLO'):5s} " \
          f"u={('s2,t0' if uorder==0 else 't0,s2')} seed={('s0,s1' if seedorder==0 else 's1,s0')}"
    if got == ref0:
        found.append(tag + "  [crc ending ✓]")
    if got_vend == ref0:
        found.append(tag + "  [vend ending ✓]")

print("\nmatching arrangements:")
for f in found:
    print("  ", f)
if not found:
    print("  NONE — the divergence is elsewhere")
    # try: maybe fold_extra seeds when wp==0 only, and for wp>=1 the FIRST extra
    # unit also goes through fold_step? or the state after the loop isn't the seed?
    # print diagnostics
    v_lo, v_hi = s0, s1
    r = clmul(v_hi, RKHI) ^ clmul(v_lo, RKLO) ^ (s2 ^ (t0 << 64))
    print(f"   state lo={r & M64:016x} hi={r >> 64:016x}")
    print(f"   crc ending: {crc_u64(crc_u64(0, r & M64), r >> 64):08x}")
    print(f"   vend ending: {vend(r & M64, r >> 64):08x}")
