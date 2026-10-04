#!/usr/bin/env python3
"""Debug: which lane diverges at len=192? Compare per-lane values."""
import sys
sys.path.insert(0, 'scripts')
import r15_tail_derive as D
from r15_tail_derive import (T, crc_u8, crc_u16, crc_u32, crc_u64, M64, M32,
                             clmul, clmod, le, span_ref, fold_states, vend, G)

# build a 192-byte body
body = bytes((i * 131 + 17) & 0xFF for i in range(192))
ln = len(body); blocks = ln // 64; tail = ln % 64
print(f"len={ln} blocks={blocks} tail={tail} wp={blocks//2}")

# reference lanes (the scalar kernel's per-lane values)
def ref_lanes(body):
    ln = len(body)
    lanes = [0] * 8
    i = 0
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
print("ref lanes:", [f"{x:08x}" for x in rl])

# vend sanity: vend(vlo,vhi) == crc_u64(crc_u64(0,vlo),vhi)?
import random
rng = random.Random(1)
for _ in range(50):
    a, b = rng.randrange(1<<64), rng.randrange(1<<64)
    assert vend(a, b) == crc_u64(crc_u64(0, a), b), "vend model broken!"
print("vend model == crc-chain: OK")

# fold states
st, wp = fold_states(body)
print("wp =", wp)
for k in range(8):
    v = st[k]
    # lane k's stream (B=3): qword k of blocks 0,1 folded (wp=1) + w_k (block 2)
    # if wp covers all: vend(V) == crc over [q_k(0), q_k(1)] then chain w_k
    got = vend(v & M64, v >> 64)
    # expected: crc chain over q_k(block0), q_k(block1), w_k
    q0 = le(body, 8*k); q1 = le(body, 64 + 8*k); q2 = le(body, 128 + 8*k)
    want_chain = crc_u64(crc_u64(crc_u64(0, q0), q1), q2)
    want_partial = crc_u64(vend(v & M64, v >> 64), q2)
    print(f"  lane {k}: vend(V)={got:08x}  full-chain={want_chain:08x}  "
          f"vend+chain(w)={want_partial:08x}  ref={rl[k]:08x}  "
          f"match={'vend+chain' if want_partial == rl[k] else ('full-chain' if want_chain == rl[k] else 'NONE')}")
