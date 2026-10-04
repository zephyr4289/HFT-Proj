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

## 5. The CI verdicts (four target draws — the classes split)

| Draw | Run | Silicon | kbench 1t | 11b vend (msg/s) | 11s crc-chain | Δ sustained | kbench vend−rc |
|---|---|---|---|---|---|---|---|
| 1 | 37177689706 | 8573C | 34.05 | **1,185,343,147** | 1,134,784,728 | **+4.45%** | +0.3% |
| 2 | 37178509627 | 8573C | 30.88 | 1,072,927,201 | 1,069,539,994 | +0.32% | +0.3% |
| 4 | 37180175441 | 8573C | 30.42 | 1,071,092,598 | (11n flake†) | — | +1.3% |
| 3 | 37179164401 | 8370C | 27.24 | 946,693,317 | **974,739,551** | **−2.92%** | **−13.8%** |
| 5† | 37181026479 | 8370C | 27.27 | 944,713,489 | 962,875,689 | −1.9% (positional) | −5.1% |

† draw 4's 11n failure: the documented pre-existing prepatch-race flake
(docs/25 §5.1, open since R12) — struck the arm before 11s; unrelated to
vend. Draw 5 ran POST-GATE (commit e4b4090): the 8370C's 11b and 11s BOTH
ran the crc-chain ending — their −1.9% gap is arm-position variance, which
retroactively prices draw 3's sustained penalty at ~−1% vend-specific (the
kernel-level rows are the clean signal: −13.8% / −5.1%). The gate works:
no class runs vend where it loses.

**The reading:** on the record class (8573C — Sapphire Rapids, two 512-bit
datapaths) vend wins the sustained fabric on every completed head-to-head
(+4.45% on the strong instance, +0.32% on the weak one; draw-adjusted
absolute rates agree across draws). On the 8370C (Ice Lake — a single
clmul-issue structure) the ending's +10 clmul/span serialize against the
fold's: the packed loop loses −13.8% and the fabric −2.92%. The kernel
physics is unambiguous at the kbench level — far above draw variance.

**The decision (the R9c→R9d law: a default that hurts ANY class does not
ship):** `vend_enabled`'s default is SILICON-CONDITIONAL — ON for Intel
family-6 model ≥ 0x8F (Sapphire Rapids and newer, where the evidence
lives), OFF on Ice Lake and everything unproven (`vend_supported_cpu`,
a CPUID class table; `HFT_CRC_VEND=1|0` overrides for experiments and
soak arms). The 8370C's r14 behavior is then byte-identical to its r13
default — no class regresses. The kbench attribution pair is now
controlled on every class: `fold512_r` forces vend ON, `fold512_rc`
forces it OFF (the fabric arms carry the per-class default).

## 5b. Local validation (the noisy 2-vCPU SPR sandbox)

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

## 6. The sustained scoreboard (post-verdict)

With the class-conditional default, the 8573C fleet runs vend: the draw-1
instance delivered **1,185,343,147 msg/s** (a +4.45% same-draw dividend
over the crc-chain ending; the R13 five-draw mean dividend was +3.87% for
the fold — the ending's adds on the strong instances). The all-time
sustained record (1,234,801,472, R11/R12 silicon) remains the mark to
beat on a record-quality draw: the R13+R14 stack on draw-1's instance
already matched the R13 record-draw band draw-adjusted. Bronze (≥1.40B)
stays OPEN: the remaining ~15% to the tier lives in the supply side, the
lane-0 scalar tail, and the ring mechanics — not the kernel, whose packed
ceiling now sits at ~34–40 GB/s/worker with the fabric extracting ~50–55%
of it on the real mix.

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

The vend ending ships as a CLASS-SCOPED lever: default ON for SPR+ Intel
(the 8573C evidence: 3 draws, sustained 2/2 head-to-heads positive +
kernel-level 3/3 positive), OFF everywhere else until a draw certifies
the class (the 8370C refutation: −2.92% sustained / −13.8% packed). The
`HFT_CRC_VEND` knob overrides both ways; 11s soaks the OFF path on every
draw; the kbench `fold512_r`/`fold512_rc` pair is now the controlled
kernel-level attribution on every class. Bit-exactness held on every draw
and every arm (HYDRA_BITPARITY `0x881639cead506f25`, D1..D12 incl. the
2101-body reflect differential, allocs=0, full-run SUCCESS on draws 1–3;
draw 4's 11n failure is the documented pre-R14 flake class).
