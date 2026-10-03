# R13 — Breaking the Port-5 Wall: The Natural-Domain Fold

**Program:** docs/challenge/CHALLENGE-R13-p5-wall.md — "Break the Port-5 Wall".
**Baseline to beat (8573C @ 2.3 GHz, 2 phys cores × SMT):** sustained full verification 1,234,801,472 msg/s (34.15 GB/s); Front A 3,624,572,766 msg/s.
**The lever:** the mirror-domain plumbing around the clmul — 2 GFNI bit-reverses + 2 `vpshufb` bswaps per 128 B step — removed by re-deriving the fold in the natural (reflected-representation) domain.

---

## 1. Step 0 — The port decomposition (the hypothesis, measured)

No `perf` on the runners (no binary; `perf_event_paranoid=2`), so the decomposition was
built from three independent instruments on the SAME uarch class (the sandbox is a
Sapphire Rapids Xeon — Golden Cove, like the 8573C):

**(a) Static census** (objdump of the shipped `fold_word_pairs` loop, unrolled ×2 by LLVM):

| uops per 128 B step | count | port (Golden Cove, 512-bit) |
|---|---|---|
| `vmovdqu64` (loads) | 2 | p23 (load ports) |
| `vgf2p8affineqb` (bit-reverse) | 2 | p0 |
| `vpunpck{l,h}qdq` (unit interleave) | 2 | **p5** |
| `vpshufb` / `vpermt2b` (qword bswap) | 2 | **p5** |
| `vpclmulqdq` | 4 | **p5** |
| xor / `vpternlogq $0x96` (LLVM already fuses) | 2 | p0/p5-flex |

**p5 total: 8 uops per step** against a measured ~9 cycles/step (kbench `fold512 1t` =
32.96 GB/s = 14.28 B/cyc on the record 8573C draw). The challenge's table is confirmed
almost exactly — the only correction is that LLVM had already fused the 3-way XORs.

**(b) Port probe** (`scripts/r13_port_probe.rs`, 16 independent self-chained registers,
RDTSC, min-of-5; mix probes reveal port sharing by summation):

```
vpclmulqdq zmm  : 0.889 cyc/op      vpclmulqdq ymm: 0.889    vpclmulqdq xmm: 0.889
vpshufb zmm     : 0.893             vgf2p8affine zmm: 0.889  vpternlogq zmm: 0.445
MIX clmul_z8+unpck_z8 : 0.556       MIX clmul_z8+gfni_z8   : 0.445
MIX clmul_z8+ternlog_z8: 0.445       MIX clmul_z8+clmul_y8 : 0.889
```

Readings (relative — the sandbox's TSC-to-core ratio cancels in comparisons):
- **zmm = ymm = xmm clmul share the same port(s)** (the z+y mix runs at the z-only
  rate). There is NO width-hybrid port spreading to be had on this part.
- **GFNI runs on a different port than clmul** (the mix halves): the 2 affines were
  never p5 work — they were free capacity on p0.
- `vpxorq`/`vpunpcklqdq` solo probes are unreliable in this harness (LLVM reassociates
  xor chains and unpck-with-fixed-operand is idempotent — both partially collapse;
  documented as instrument artifacts, not silicon facts).

**(c) The perturbation differential on the REAL kernel loop** (the kill criterion):

```
CURRENT mirror step : 9.867 cyc/step -> 12.97 B/cyc
  +1 vpshufb zmm    : 10.818 cyc/step   (+0.95 cyc per added p5 op)
```

Adding ONE p5-family op to the real fold step costs **0.95 cycles** — p5 is ≥ 95 %
busy. **The hypothesis survives its own kill criterion: the wall is p5, and the wall
is the mirror-domain plumbing, not the clmul.** (The clmul floor of 4 p5 uops/step
remains — the 8 B/clmul invariant is structural: any n-bit state lift by y^n needs
n/64 clmuls, so 4 zmm-clmuls per 128 B per 8 lanes is the minimum; interleaving
state count does not reduce it, only re-times it.)

## 2. The fix — the natural-domain (reflected-representation) fold

### 2.1 Why the plumbing existed

docs/21 §2 chose the mirror domain so the ENDING collapses to two `crc32`
instructions. The price: every unit needs `rev128` = per-byte bit-reverse (GFNI, p0)
+ per-qword byte-swap (`vpshufb`, **p5**) + the unpack interleave that arranges it.
The bswaps are 2 of the 8 p5 uops — 25 % of the wall spent moving mirrors around.

### 2.2 The derivation (the honest version)

`raw(D) = rev32(X̄·y^32 mod P)` (docs/21's own formula; re-verified against the
table reference). A natural-data fold must compute an equivalent quantity with the
units as RAW little-endian loads. Four designs were killed on the way (all preserved
in `scripts/r13_reflect_derive.py`'s history and §4 below): my independent
back-to-front and front-to-back derivations kept hitting the re-anchoring wall
(length-dependent y^(8n) factors, 31-bit mirror misalignment), and an exact GF(2)
linear-algebra solve for the constants proved **no 2-clmul lift + crc-chain ending
exists in the conventions I had assumed**.

The resolution: the published standard form. **ISA-L's CRC32C `fold_1x128b` pair**
(`crc/crc_const.asm`) is exactly our 128-degree unit lift:

- `RKHI = 0x493c7d27` — multiplies the state's HIGH qword; independently
  re-derived as **`rev32(y^95 mod P)`** (the reversed-power convention — the
  constants are mirrors of polynomial powers, which is why naive `x^e mod q`
  searches fail).
- `RKLO = 0xec1068c50` — multiplies the state's LOW qword (33-bit: carries the
  polynomial's y^32 term). No simple closed form found; pinned by the differential
  and the derivation test's convention anchor (`rev33(y^96 mod P) = 0x14cd00bd6`,
  ISA-L's combine constant).

The kernel (`crcfold.rs`, `fold_word_pairs_r`):

```
per 128 B step:  2 loads + 2 vpunpckqdq + 4 vpclmulqdq + 2 vpternlogq $0x96
                 (NO GFNI, NO vpshufb — 6 p5 uops, down from 8)
V ← (V_hi ⊗ RKHI) ⊕ (V_lo ⊗ RKLO) ⊕ U        — units enter as raw LE qwords
ending per lane: c = crc32_u64(crc32_u64(0, V_lo), V_hi)   — no rev64, 2 instructions
```

Same state layout (even/odd lane split), same loop shape, same lane-0 tail
decomposition (crossing units, tail units, r0 chaining), same lanes-1..7 odd-block
word chaining, same FNV combine — the value definition is untouched. The derivation
and full-pipeline simulation live in `scripts/r13_reflect_derive.py`
(**2584/2584** lengths × patterns bit-exact against the scalar reference before any
Rust was written); `t_reflect_constants_derivation` re-derives RKHI at test time.

### 2.3 Measured (sandbox SPR, same-uarch relative comparisons)

```
port-probe fold-step simulations (pure loops, L2 buffer):
  CURRENT mirror step : 9.867 TSC-cyc/step
  REFLECT candidate   :  6.843 TSC-cyc/step   (+44 % step density)
kbench (full kernel, packed-SPAN corpus, noisy shared vCPU, 3 draws):
  fold512   : 21.79 / 21.56 / 23.42 GB/s
  fold512_r : 22.67 / 22.73 / 24.59 GB/s      (+4..+9 % full-kernel, sinks bit-equal)
hydra sustained (1 worker, sandbox):
  crc_kernel=reflect : 372.2M msg/s
  crc_kernel=fold512 : 363.2M msg/s           (+2.5 %)
```

The pure-loop +44 % dilutes to single digits end-to-end on the sandbox because (a)
the endings (~26 % of real-span cycles, docs/23 §5) improve only modestly, and (b)
the shared vCPU's noise floor. On the 8573C the kernel-density ratio is what the
`fold512` vs `fold512_r` kbench rows will price on the same draw.

## 3. What ships

- `CrcKernel::Reflect` — the natural-domain fold; **default ON** on fold-class
  silicon (`HFT_CRC_KERNEL=reflect|fold512|scalar` overrides; `fold512` is the
  documented rollback).
- Production paths: `eval` + `eval_pair` (the fabric's two-span schedule) on the
  reflected kernel; `eval2`/`eval_tri` remain the mirror kernel's attribution arms.
- kbench: `fold512_r` and `fold512_r_pair` rows (1t / 2cpu_distinct / 2cpu_smt).
- ci.sh **11r**: the mirror-kernel rollback soak (the 11m/11n precedent) — per-draw
  attribution of the kernel flip against 11b's default.
- D11 extended: `D11 REFLECT_KERNEL_DIFFERENTIAL_PASSED ... on 2101 bodies`
  (exhaustive lengths × patterns + mismatched eval_pairs, against the scalar kernel
  AND transitively the reference table CRC).

## 4. Refutations kept (negative results are deliverables)

1. **The ymm hybrid (challenge §3.C) — REFUTED locally.** The port probe's
   `clmul_z8+clmul_y8` mix runs at the zmm-only rate: 256-bit clmul shares the same
   port(s) as 512-bit on this part, so the hybrid's only benefit would be unpck on
   p1 — and the doubled instruction stream (≈20+ uops/step, front-end bound) eats
   it: the simulated ymm step measured 8.29 vs 6.84 TSC-cyc/step for zmm reflect.
2. **My independent reflected-domain derivations — REFUTED (four ways).**
   Back-to-front natural anchoring picks up a length-dependent y^(8n) factor;
   front-to-back natural with pure-power constants provably cannot close (the
   GF(2) solver returned INCONSISTENT for every {modulus form} × {ending} guess —
   the crc-chain ending is mirror-domain-specific); the per-qword mirror state
   transform needs a structural 31-bit realignment of the 97-bit products. The
   published reversed-power constants are the only form that closes.
3. **`RKLO` closed form — NOT FOUND** (searched rev32/rev33 of x^e for e ≤ 700 over
   all six modulus forms). Documented; the differential + convention anchors carry
   the proof burden instead.
4. **The clmul floor stands.** 4 zmm-clmuls per 128 B is invariant under state
   interleaving (the n-bit-lift-needs-n/64-clmuls law). The remaining p5 budget
   after this fix is 6 uops/step (4 clmul + 2 unpck); the unpcks are structural
   (vpclmulqdq's 128-bit-lane granularity vs the block layout's 64-bit-lane
   granularity — the transpose is information-theoretically forced).

## 5. The challenge scoreboard (what CI decides)

| Tier | Target | Mechanism |
|---|---|---|
| Bronze | ≥ 1.40 B sustained | kernel density 14.28 → ~20 B/cyc lifts the worker-pair SMT cap from ~33.6 to ~45+ GB/s; fold stops binding at ~1.2 B |
| Silver | ≥ 1.60 B | + the RX/ingest co-wall holding (Front A must not regress — it shares no code with the kernel) |
| Gold | ≥ 1.80 B | + the assist flywheel converting the freed worker cycles into main-side folds |
| Obsidian | ≥ 2.00 B | the docs/24 §8.2 structural-budget claim — the budget itself moves with the kernel density |

Rules held: same value definition (`span_crc32c_8lane` semantics pinned by D11 +
the exhaustive sweep), same schedule/corpus (`sample-mini.itch`, `MtuBound(1400)`
dual-feed, 505,849 msgs/pass), 100 % in-window byte verification, no cross-pass
memoization, `ALLOC_DELTA=0`, bit-exact goldens `0x881639cead506f25` /
`0xF6EF154EFDE905D8` (all verified locally post-change), `#![forbid(unsafe_code)]`
untouched on `nf-protocol`/`nf-arbitrator`, and every rate claim above waits for
CI draws on target silicon with the kbench attribution printed alongside.

## 6. Verdicts — five target draws, reflect 5/5

| Draw | Run | Silicon | 11b reflect (msg/s) | 11r mirror (msg/s) | Δ sustained | kbench 1t mirror / reflect |
|---|---|---|---|---|---|---|
| 1 | 37154454987 | 8573C | **1,234,210,590** | 1,201,821,534 | **+2.70 %** | 34.86 / 33.94 GB/s |
| 2 | 37155810659 | 8573C | 1,091,571,575 | 1,049,043,185 | **+4.05 %** | 31.70 / 31.07 |
| 5 | 37159212735 | 8573C | 1,049,273,600 | 1,000,599,480 | **+4.86 %** | 29.03 / 29.38 |
| 3 | 37156989499 | 8370C | 915,495,427 | 876,028,387 | **+4.50 %** | 23.40 / 25.03 |
| 4 | 37158012917 | 8370C | **988,654,797** (class record) | 934,319,782 | **+5.82 %** | 26.87 / 30.12 |

**Challenge rule 9 satisfied: ≥3 independent draws of the deciding class
(3× 8573C, mean +3.87 %) plus a second-class confirmation (2× 8370C, mean
+5.16 %). The reflect kernel wins every sustained head-to-head.**

- **The class split on the packed kbench corpus is the physics payoff:**
  the 8370C (Ice Lake — the tighter port structure the challenge itself
  flagged: single clmul port) rewards the p5 relief EVERYWHERE: +7.0 % and
  +12.1 % on the pure packed loop, the largest kernel-level jump since
  GIGAHFT. The 8573C (Golden Cove/SPR) has loop slack the mirror kernel
  could hide in (packed −2 % on draws 1-2, +1.2 % on draw 5's weaker
  instance) — but the real-mix fabric still favors reflect on every draw:
  the ending machinery (the mirror's `rev64` pass + its store/reload
  latency) is paid per SPAN, and the real corpus is span-shaped, not
  loop-shaped. The scoreboard gate is the sustained fabric; both classes
  agree.
- **Draw 4's 988,654,797 msg/s is the all-time 8370C-class record**
  (previous best 938.7M, R10b) — set with bit-exact goldens and allocs=0.
- Draw 1's 1,234,210,590 matches the all-time sustained record (1,2348B,
  R11 draw) on its instance; the same-draw mirror rollback prices the
  kernel's share at +2.7 %.
- Front A (pure ingest — no CRC in path, the kernel cannot touch it):
  PASS on all five draws (2.25–2.80B; instance-variance band, unchanged
  behavior).
- Correctness on every draw and every arm: HYDRA_BITPARITY BIT-EXACT
  (0x881639cead506f25), D1..D12 incl. `D11 REFLECT_KERNEL_DIFFERENTIAL_
  PASSED` (2101 bodies), 17/17 matrix (0xF6EF154EFDE905D8), window sweep,
  ALLOC_DELTA=0, thp-granted.

**Scoreboard vs the challenge tiers:** reflect ships as a real, replicated
+2.7–5.8 % sustained win across both target classes — but Bronze
(≥ 1.40 B) is NOT reached by this lever alone. The honest physics: on the
8573C the fold kernel was not the only binding constraint at 1.2 B (the
sandbox's +44 % step-density did not carry to the SPR packed loop), and
the challenge's own §5 prediction holds — with fold density improved and
the endings cheaper, the remaining walls are the RX per-frame entry build
(Front A's co-wall) and the supply side. The 8370C result (+12 % kernel
density) shows the lever's full size where ports ARE the wall. The next
frontier for the 1.4–1.8 B tiers is the RX build, per docs/25 §7's R13
candidate list.
