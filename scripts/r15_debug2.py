#!/usr/bin/env python3
"""Debug len=200 (B=3, tail=8): per-lane comparison of the current model."""
import sys
sys.path.insert(0, 'scripts')
from r15_tail_derive import (crc_u16, crc_u32, crc_u64, crc_u8, M64, M32,
                             clmul, le, fold_states, vend, RKHI, RKLO)

body = bytes((i * 131 + 17) & 0xFF for i in range(200))
ln = len(body); blocks = ln // 64; tail = ln % 64
print(f"len={ln} blocks={blocks} tail={tail}")

def ref_lanes(body):
    ln = len(body); lanes = [0] * 8; i = 0
    while i + 64 <= ln:
        for k in range(8):
            lanes[k] = crc_u64(lanes[k], int.from_bytes(body[i+8*k:i+8*k+8], 'little'))
        i += 64
    while i + 8 <= ln:
        lanes[0] = crc_u64(lanes[0], int.from_bytes(body[i:i+8], 'little')); i += 8
    if i + 4 <= ln:
        lanes[0] = crc_u32(lanes[0], int.from_bytes(body[i:i+4], 'little')); i += 4
    if i + 2 <= ln:
        lanes[0] = crc_u16(lanes[0], int.from_bytes(body[i:i+2], 'little')); i += 2
    if i < ln:
        lanes[0] = crc_u8(lanes[0], body[i])
    return lanes

rl = ref_lanes(body)
st, wp = fold_states(body)

# lane 0: fold_extra(w, t0)
v0_lo, v0_hi = st[0] & M64, st[0] >> 64
w = le(body, 64 * (blocks - 1)); t0 = le(body, 64 * blocks)
r = clmul(v0_hi, RKHI) ^ clmul(v0_lo, RKLO) ^ (t0 ^ (w << 64))
nv_lo, nv_hi = r & M64, r >> 64
got0 = crc_u64(crc_u64(0, nv_lo), nv_hi)
print(f"lane 0: model={got0:08x} ref={rl[0]:08x} match={got0 == rl[0]}")

# lanes 1..7: vend + odd word
for k in range(1, 8):
    v = st[k]
    got = crc_u64(vend(v & M64, v >> 64), le(body, 64 * (blocks - 1) + 8 * k))
    print(f"lane {k}: model={got:08x} ref={rl[k]:08x} match={got == rl[k]}")

# what SHOULD lane 0's unit structure be? stream = q0(b0),q0(b1),q0(b2),tail[0..8)
# units: [q0(b0)||q0(b1)], [q0(b2)||tail0]  — check against direct chain:
s = [le(body, 8*0), le(body, 64+0), le(body, 128+0), le(body, 192)]
direct = crc_u64(crc_u64(crc_u64(crc_u64(0, s[0]), s[1]), s[2]), s[3])
print(f"lane 0 direct 4-qword chain: {direct:08x}  ref: {rl[0]:08x}")

# vend of seeded pair-0 state then chain: does [q0(b0)||q0(b1)] fold == vend state?
v01 = crc_u64(crc_u64(0, s[0]), s[1])
seed_state = s[0] ^ (s[1] << 64)
print(f"vend(seed [s0,s1]) = {vend(s[0], s[1]):08x} vs chain {v01:08x}")
