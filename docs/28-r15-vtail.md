# R15 — The Vectorized Tail ("vtail")

**Program:** the R14 verdict's own queue (docs/27 §7: "the lane-0 scalar
tail — ~15-20 serial cyc/span, vectorizable into the state via a
length-indexed constant table per tail case") + the dossier's Vector
Target 1.

**Baseline to beat (8573C, R13+R14 stack):** sustained full verification
1,234,210,590 msg/s (the R13 record draw; the R14 stack draw-adjusted
matches it); the real-mix span pays ~93 cyc over the packed loop's ~91,
of which the lane-0 tail chain is the largest single owned share.

---

## 1. The algebra (derived, then pinned — scripts/r15_tail_derive.py)

The lane-0 tail is the LAST serial dependency chain the kernel owns after
R14: the state extract (storeu + reload), the `fold_extra` unit chain
(one serial xmm fold step per 16 tail bytes, ~10-12 cyc each), the 2-3
chained `crc32`, and the `chain_bytes` r0 tail. Everything is linear over
GF(2), so the whole composition can be absorbed into the vend input field
in ONE shot:

**The laws (each verified numerically, V1..V4):**

* **L-Z (zeros-update):** `Z_r(c) = c ⊗ G[r] (mod VM)` where `Z_r` is the
  r-byte zeros-update and `G[r] := Z_r(1)` — the empirical table. The
  ring element `G[r]` equals `y^(-8r)` (verified: `G[m] ⊗ y^(8m) = 1`).
* **L-M (message law):** `rawCRC(M) = M̂(y) ⊗ VR0 ⊗ y^(128-8|M|) (mod VM)`
  — the length factor is a pure y-power; the per-length seeds satisfy
  `s_r ⊗ y^(8r) = VR0 ⊗ y^128` (positive-power verifiable, no inverses).
* **L-V (vend ring product, R14's own law):** `vend(F) = F_ring ⊗ VR0
  (mod VM)` for every field F ≤ 95 bits.

**The composition:** lane 0's post-loop value is the chain
`Z_r(vend(V0)) ⊕ rawCRC(R0)` with `R0` = the remaining byte stream
(`|R0| = r = 8·(B%2) + tail ≤ 71`). Solving for the vend input:

```
F0 = (V0_lo ⊗ G[r]) ⊕ (V0_hi ⊗ KH[r]) ⊕ Σ_d (D_d ⊗ AT[t_d])
KH[r] = G[r] ⊗ y^64 mod VM        (the pre-reduced structural y^64)
AT[t] = y^(128-8t) mod VM         (t = bytes from datum d's start to the end of R0)
```

Every constant is ≤ 32 bits, so every clmul product — and their XOR —
stays ≤ 95 bits: exactly vend's Barrett-verified input range. The
derivation is `scripts/r15_tail_derive.py`; its differential is the
house-discipline corpus (exhaustive lengths 0..600 × 4 patterns + the
representative long sizes = 2419 bodies, 100% bit-exact against the
scalar kernel) and `t_vtail_constants_derivation` re-derives all 216
table entries at test time from the table-driven reference (a
transcription typo cannot survive).

**The anchors that confirm the structure:** `G[8] = VH64` (the R14
ending constant re-emerging), `G[16] = VR0`, `KH[8] = 1` (the hi-qword's
y^64 lift exactly cancels), `AT[16] = 1`, `AT[8] = y^64`.

## 2. What ships

* `finish_span_r_vtail` — lane 0's tail composed into `vend_xmm` (the
  xmm-domain Barrett): 2 lift clmuls + up to 9 INDEPENDENT data clmuls
  (qwords + the partial byte group as the high bytes of the last u64),
  replacing the extract → fold_extra chain → chained crc32 (25-60 serial
  cyc at r ≥ 16 → ~22 cyc, with the data clmuls off the critical path
  entirely — their inputs are pure memory loads available during the fold).
* **The r ≥ 16 gate** (the dispatcher): spans with r ≤ 8 keep the old
  path — they have NO fold_extra chain to eliminate, and their 2-3
  chained crc32 (~17-21 cyc, p1 work parallel to the fold's p5) beat the
  composed vend_xmm chain (~22 cyc) while keeping p5 free. The gate is
  the packed-loop evidence made law.
* Lanes 1..7: the R14 path VERBATIM. The first design absorbed their
  odd-block words into the zmm states (2 clmul + 1 ternlog + 1 load per
  register — the algebra closes identically, `F_k = (V_lo ⊗ G[8]) ⊕ V_hi
  ⊕ (w_k ⊗ y^64)`); it was REFUTED locally (below) and removed.
* `HFT_CRC_VEND=0` (R13 rollback) and `HFT_CRC_VTAIL=0` (R14-shape
  rollback) — the knob stack; defaults follow the R14 class gate (SPR+).
* kbench: the `fold512_r` / `fold512_rv` / `fold512_rc` triple (vend /
  vend+vtail / crc-chain, all forced) on every topology row.
* ci.sh arm **11t** (`HFT_CRC_VTAIL=0` soak — the 11r/11s precedent).
* D11 extended: every body pins all THREE ending paths explicitly.

## 3. The refutations kept (negative results are deliverables)

1. **The lanes-1..7 zmm absorption — REFUTED locally.** The 7 odd-block
   `crc_u64` words are p1 uops (1c throughput, independent) that run
   PARALLEL to the fold's p5 clmuls. Absorbing them into the states moves
   ~9 uops onto the SATURATED p5: the packed loop measured 19.5/19.7/19.9
   (vend) vs 19.1/19.3/19.3 GB/s (full vtail) — a consistent -2-3%.
   Reverted; the R14 shape stands for lanes 1..7.
2. **vtail for r ≤ 8 — REFUTED locally.** The packed corpus (SPAN=1344:
   B=21 odd, tail=0, r=8) is the old path's BEST case (no fold_extra)
   and the vtail's worst: lane-0-only still measured 18.6/18.6/18.8 vs
   18.3/18.4/18.3 GB/s. The r ≥ 16 gate is the fix: the vtail only runs
   where the serial chain it eliminates exists.
3. **The negative-power ring algebra — first derivation REFUTED by its
   own solver.** The y^(-1) construction (y^-1 = y^31 ⊗ VQ^-1) verified
   inconsistent (y^-e ⊗ y^e ≠ 1); every "law" that needed negative
   powers (the rawCRC length law beyond 16 bytes) failed its probe. The
   fix: NO negative powers anywhere — the byte-granularity negative
   shifts ARE the empirical zeros-update table `G[r] = Z_r(1)`, and the
   length law's residual is `AT[t] ⊗ y^(8t) = y^128` (all positive).
   This is the R13 lesson (docs/26 §4.2) repeating: the published/
   empirical constants beat hand-derived closed forms.

## 4. Local evidence (the noisy 2-vCPU GNR-class sandbox)

* Correctness: `t_fold_differential_exhaustive` + D11 (all three paths
  forced) + the full battery green; HYDRA_BITPARITY
  `0x881639cead506f25` BIT-EXACT on every fabric arm (default, VTAIL=0,
  VEND=0).
* Packed kbench (r=8 → the gate routes to the old path): fold512_r ≈
  fold512_rv within noise (18.5-18.9 GB/s both) — the row pair now
  documents the gate's parity on this corpus.
* Real-mix fabric A/B (5 draw pairs, ±5-10% VM noise): vtail-on mean
  348.6M vs vtail-off 347.8M msg/s (excluding one 422M quiet-window
  outlier) — DEAD EVEN locally. The sandbox is latency-blind (the VM's
  memory supply stalls dominate the 93-cyc span gap the tail lives in);
  the 8573C's bare memory is where the ending latency binds. **CI
  decides** (the standing rule): 11b (default) vs 11t (VTAIL=0) on ≥3
  draws of the target class, kbench attribution printed alongside.

## 5. Claim scope

The vtail ships as a CLASS-SCOPED, GATED lever: default ON only where
vend is ON (SPR+), only for r ≥ 16 spans, with the R14-shape rollback
one env away and the 11t soak pricing it per draw. The math is
exhaustively pinned (2419-body differential + 216-constant test-time
re-derivation); the SPEED claim waits for the fleet. Bronze (≥1.40B) is
NOT claimed by this lever alone — the R15 queue's other two items (the
RX per-frame entry build, the supply/ring rebalance) are the follow-ups
this same branch carries.

## 6. The first fleet verdicts (four target draws, 2026-10-04)

| # | Silicon | Gates | 11b (default) | vtail attribution | kbench 1t r/rv/rc |
|---|---|---|---|---|---|
| 1 | 8370C (noisy host) | R8 FAIL (noise) | 865.4M (−10% vs band) | n/a (OFF on class) | 27.6 / 27.7 / 28.8 |
| 2 | 8370C (healthy) | ALL PASS | 945.9M | 11t 915.1M — Δ = arm-position noise (identical configs: ±3.4% floor) | — |
| 3 | 8370C (marginal) | R8 FAIL (−0.9%) | 889.2M | n/a | — |
| 4 | **8573C** (deciding) | **ALL PASS** | **1,061.8M** (Front A 3.234B) | **11t 1,057.9M → vtail +0.37% (within noise)** | 30.00 / 29.99 / 29.39 |

* **Bit-exactness: 4/4 draws, every arm** — `0x881639cead506f25`
  (the R15 stack's invariant held on every certified draw, including
  the arms that force each of the three ending paths).
* **The 8573C draw (the deciding class):** a mid-band instance (kbench
  30.0 vs the record draw's 34.9 GB/s). The vtail's sustained delta is
  +0.37% — WITHIN the ±3.4% identical-config arm-position variance the
  8370C draw-2 measured (11b 945.9 vs 11t 915.1 on byte-identical code
  paths). One draw is not a verdict (the challenge's own rule 9: ≥3);
  the ledger records NEUTRAL-on-draw-1. The class-conditional default
  stands — no class regresses (the R9c→R9d law), the physics targets
  the r≥16 serial chain, and the rollback is one env away.
* **The vend (R14) also washed this draw** (11s 1,061.2M ≈ 11b) — on a
  record-class instance it measured +4.45%; draw variance dominates on
  mid-band instances. The stack holds R13/R14 territory draw-adjusted:
  reflect beats mirror +4.4% on this draw too (11r 1,017.3M).
* **The 11u batch sweep:** 64 → 1,049.8M, 256 → 1,051.7M vs the 128
  default's 1,061.8M — no signal; the R8 shape stands.
* **The R8 pure-ingest gate's two marginal misses** (1.67B on a
  uniformly-low host; 1.982B at −0.9%) are pre-existing gate strictness
  on noisy draws — hft_bench executes none of the R15 code (the
  count+span sink), and the same class passed at 3.2B+ on healthy draws.
