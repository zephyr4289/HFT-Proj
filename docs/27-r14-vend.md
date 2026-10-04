# R14 — The Vector Barrett Ending ("vend")

**Program:** the R13 verdict's own queued follow-up (docs/26 §6: "A vector
Barrett/fold-down ending (all-zmm, ~6-8 clmuls per span, 8 extracts) is the
designed R13b follow-up") — the per-span ending stack that docs/23 §5
priced at ~26% of a real-mix span, re-priced by the R13 record draw's
worker telemetry at **~93 cyc/span over the packed loop's ~91** (the
real-mix span: 184 cyc measured vs the packed kbench's ~91 at the same
kernel density — the endings + supply + ring eat the other half).

**Baseline to beat (8573C, R13 reflect):** sustained full verification
1,234,210,590 msg/s (34.13 GB/s CRC demand, workers 98.5% busy); kbench
`fold512_r` 1t 33.94 GB/s on the record draw.

---

## 1. The problem, in the record draw's own numbers

The R13 record run's DIAG (run 37154454987, 11b): workers eval_ms 4926/4924
of 5000 (98.5% busy) delivering 34.13 GB/s of CRC — **~17 GB/s per worker,
50% of each worker's own 33.94 GB/s packed ceiling**. The per-span
arithmetic: 58.6M spans per worker per 4.926s = 184 cyc/span at 2.3 GHz,
against the packed loop's ~91. The gap is the real mix: the endings' serial
dependency (store/reload → 16 chained `crc32_u64` → the FNV-1a imul chain),
the supply waits, and the ring mechanics. The ending is the only piece of
that stack the KERNEL owns — and its share grows as the fold gets faster.

The R13 reflect ending (per span): 2 `storeu` + 16 u64 reloads + **16
chained `crc32` instructions** (2 per lane, all on p1, 3c latency each) +
8 result lanes feeding a 9-step serial FNV. The extract storm and the
p1 chain are pure overhead the vector unit can eat.

## 2. The algebra (derived, then pinned — scripts/solve_vend*.py)

The ending's per-lane value is DEFINED by the crc-chain:
`raw = crc32_u64(crc32_u64(0, V_lo), V_hi)` — true for ANY (V_lo, V_hi),
independent of the fold's internal convention. The map is GF(2)-linear.
Empirically identifying its structure:

1. **The monomial family is a Galois LFSR.** `R_j := crc(y^j)` satisfies
   `R_{j+1} = (R_j << 1) ^ (VQ if R_j >= 2^31)` with
   **`VQ = 0x05ec76f1`** — pinned across j ∈ [0, 190) (16-byte family).
   The register is a left-shift LFSR: bit i = coefficient of y^i in
   `GF(2)[y]/VM`, `VM = y^32 + VQ`.
2. **The map is a ring product.** `raw(V) = (r0 ⊗ V) mod VM` with
   **`r0 = VR0 = 0xf20c0dfe`** (the family's degree-0 seed) — verified
   basis-exhaustively. Split over the qwords:
   `W = (V_lo ⊗ VR0) ⊕ (V_hi ⊗ VH64)`, `VH64 = r0·y^64 mod VM =
   0x493c7d27` — **numerically RKHI again** (the R13 fold constant
   re-emerging from independent algebra: the fold's 128-degree unit lift
   and the ending's y^64 shift are the same ring operation).
3. **The reduction is a Barrett with byte-aligned shifts.** W ≤ 95 bits;
   the quotient path needs field-wide (cross-qword!) shifts — a per-qword
   `vpsrlq` CORRUPTS them; `vpalignr` per 128-bit lane is the correct
   primitive. The solver's grid over (shift-triple, quotient constant)
   found **exactly one** byte-aligned exact solution:

```
W  = (V_lo ⊗ VR0) ⊕ (V_hi ⊗ VH64)     ≤ 94 bits   2 clmul + 1 xor
X  = field >> 32   = vpalignr(W, W, 4)             1 p5
P  = X.field.lo ⊗ VMU                  ≤ 119 bits  1 clmul   (VMU = floor(y^88/VM), 57b)
qh = field >> 56   = vpalignr(P, P, 7)             1 p5
R  = W ⊕ (qh.field.lo ⊗ VM)            ≤ 98 bits   1 clmul + 1 xor
r  = R ⊕ ((field>>32).lo ⊗ VM)                     1 clmul + 1 alignr + 1 xor
out = low32(r)   — the q̂−1 correction; qh structurally ≤ 63 bits
                    (W ≤ 94 after VH64's 30-bit width), all products
                    field-safe (≤ 119 bits)
```

Verified: **0/128 basis, 0/300k random, 0 edge patterns** under the exact
vector semantics (every clmul reads only its field's low qword — the
masking hazard was checked explicitly). `t_vend_constants_derivation`
re-derives VQ, VR0, VH64, VMU from the table reference and re-proves the
composed structure basis-exhaustively at test time; the differential sweep
pins BOTH ending paths (`span_fold_eval_r_forced(body, vend)`) on every
body of the exhaustive corpus.

## 3. What ships

* `vend_zmm` — the in-register ending above; per zmm (4 lanes):
  **5 clmul + 2 vpalignr + 3 logic**; per span (2 zmm): 10 clmul + 4
  alignr + 6 logic + 2 stores + 7 u32 reloads — replacing 2 stores + 16
  u64 reloads + **16 chained crc32 (p1)**. Lane 0 keeps its scalar
  continuation/ending (its state is post-tail scalar; 2 chained crc32,
  overlappable); the odd-block last words and r0 byte chains are unchanged
  (the value definition fixes them; the R15 candidate list keeps their
  vectorization open).
* `HFT_CRC_VEND=0` rollback (default ON); `finish_span_r` reads the switch
  once per span; the fold loop is untouched.
* Attribution: `CrcKernel::eval_rpath(body, vend)` +
  `span_fold_eval_r_forced`; kbench **`fold512_rc`** rows (1t / 2cpu_smt /
  2cpu_distinct — the crc-chain ending forced) vs `fold512_r` (vend
  default) on the same draw; ci.sh arm **11s** (`HFT_CRC_VEND=0` fabric
  soak). The fabric (eval + eval_pair) picks vend up automatically.

## 4. The port ledger (the honest trade)

| | crc-chain ending (R13) | vend (R14) |
|---|---|---|
| p5 | 0 | **+10/span** (5 clmul × 2 zmm) |
| p1 | 16 crc32 | **0** |
| p23 | 2 stores + 16 u64 loads | 2 stores + 7 u32 loads |
| p015 | ~6 | ~9 |
| serial latency | store-fwd → 2×3c crc → FNV | ~4-dep clmul chain → FNV |

The +10 p5/span lands on the workers' shared SMT port — but the real-mix
p5 occupancy is ~34%/worker (63 of 184 cyc; 68% pair aggregate), so the
ending's p5 buys issue slots, p1 slots, and latency that the real mix
actually starves on. The packed loop's p5 (69%/thread) rises to ~76% —
still under saturation.

## 5. Local validation (the noisy 2-vCPU SPR sandbox)

* kbench A/B (packed corpus, 1t): `fold512_r` (vend) **23.96** vs
  `fold512_rc` (crc-chain) **22.85 GB/s — +4.8%**, sinks IDENTICAL
  (`0xbedb8ba779de450f`) across fold512/fold512_r/fold512_rc.
* `cargo test --workspace`, clippy `-D warnings`: green (7/7 crcfold
  tests incl. the vend derivation).
* Full battery (D1..D12, window sweep, 17/17 matrix, replay golden,
  ALLOC_DELTA=0, HYDRA_BITPARITY bit-exact): run before push — see the
  worklog for the verdicts.
* The sandbox's absolute rates are VM-crippled (5.7 B/cyc) — the +4.8%
  is the kernel-level signal; **CI decides the sustained claim** (≥3
  draws of the target class, the challenge's standing rule).

## 6. Projections (the honest bands)

* Packed kbench 1t: 91 → ~78-82 cyc/span ⇒ **~37-40 GB/s** (+9-15%).
* Real-mix sustained: the ending's ~25-30 cyc/span of exposed serial work
  drops to ~10-12 ⇒ per-span 184 → ~165-172 ⇒ **+7-11% sustained** on the
  R13 reflect baseline ⇒ **1.32-1.37 B msg/s** — approaching the Bronze
  tier (≥1.40B) but not through it alone; the remaining gap is the supply
  side + ring mechanics + the lane-0 scalar tail (the R15 queue below).
* Front A: untouched (no CRC in the ingest path).

## 7. The R15 queue (what this round did NOT do)

* **The lane-0 scalar tail** (odd-block words + r0 bytes + the SSE-clmul
  continuation): ~15-20 serial cyc/span, vectorizable into the state
  (a length-indexed constant table per tail case, the desc8-anchor
  precedent) — the next per-span bite.
* **The RX per-frame entry build** (Front A's co-wall, docs/25 §7 /
  docs/26 §6): untouched, still the ingest ceiling.
* **The supply side** (main's scan + ring flush pacing at >1.3B rates):
  the R13/R14 kernel gains shift the balance toward supply — the next
  fabric rebalance point.

## 8. Claim scope

No rate claims until CI certifies on the target class (≥3 draws). The
default-ON flip is justified by: bit-exactness proven at every level
(basis-exhaustive algebra + both-path differential + identical sinks on
the packed corpus), the +4.8% local kernel-level attribution, and the
one-env-var rollback. The scoring follows the R13 challenge's tiers
(Bronze ≥1.40B remains OPEN after this lever alone).
