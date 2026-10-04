# R16 "Double Helix" — The 2B Sustained / 5B Ingest Program

> Targets: **≥ 2.0B msg/s sustained full verification** (record 1,234,801,472)
> and **≥ 5.0B msg/s pure ingest, Front A** (record 3,624,572,766), on the
> standard CI fleet's 4-vCPU draw (2 physical cores + SMT) — no silicon
> shopping, no bypasses, the challenge §4 nine rules intact.

## 1. The Research Verdicts (R12 / R5 / R7 — the external report)

* **R12 (target ruling)**: the mission is strictly confined to the standard
  4-vCPU fleet configuration (2 physical cores, SMT on). Larger runners or
  private multi-socket infrastructure do not count toward the claim.
  **Route F (fleet scaling) is TERMINATED.** The 2.0B must come from
  software architecture: Route K (kernel step density) + Route S (sibling
  scheduling).
* **R12 (reproducibility protocol)**: the mandatory three healthy draws must
  match microarchitecture CLASS (SPR/EMR — the Golden Cove/Raptor Cove
  family exemplified by the 8573C; 8370C Ice Lake draws cannot aggregate
  toward an 8573C-class claim). A draw is **healthy** iff kbench `fold512_r`
  1t ≥ 30.0 GB/s; draws below 29.0 are flagged noisy and discarded. The
  2.0B threshold must hold on the **median healthy draw**, not an isolated
  top-decile outlier. A Front A 5.0B claim formally elevates the pure-ingest
  gate from 2.0B to 5.0B at submission time.
* **R5 (larger-runner policy)**: larger runners are org/enterprise-only,
  billed per minute on public repos too, provide NO microarchitecture
  pinning (same AMD/Intel fishing pool), and ARM64 hosted runners have no
  AVX-512/VPCLMULQDQ. Route F is dead from the infrastructure side as well.
* **R7 (fleet telemetry harvest)**: across 50-shard pushes ~50% of shards
  land on non-target AMD hosts; of the Intel half, ~36% are 8370C and ~14%
  8573C; healthy mid-band-or-better 8573C draws occur at ~1-in-10..12
  shards (3–5 candidates per push). The decisive discovery is the
  **fabric-efficiency invariance**: delivered multi-core CRC bandwidth =
  **48.9% ± 0.3%** of the 2-core kbench ceiling on every healthy draw
  (single-core delivered ≈ 98% of bare-metal kbench). Fabric overhead is
  stable — **plumbing is not a lever; kernel step density is.**
* Combined with the earlier PDF report (Task 7 verdicts): GFNI CRC hybrid
  REFUTED (affine is intra-byte, p0 anyway); PMC counters DEAD on hosted
  runners (perf_event_paranoid=2 — kbench differential probes are the only
  instrument); Route S ALIVE (sibling scalar costs the p5 vector loop
  < 4.5%); L3 contended supply 25–35 GB/s/core is the 2B co-wall.

## 2. The Stage B Death Certificate (unpack-free is REFUTED)

The external report's Route K Stage B claim — "transposing fold constants
eliminates VPUNPCK, p5 uops 6 → 4 per 128 B, density 14.3 → 21.3–32 B/cyc,
~52 GB/s single-core" — **does not survive structural analysis.** Three
independent kills, any one of which is fatal:

1. **The raw-load mixing kill.** A 64 B load's 128-bit lanes hold ADJACENT
   qword pairs `(qw[2f], qw[2f+1])` — 8 bytes apart. But the span value is
   FNV over EIGHT stride-64B reference-lane CRCs (`span_crc32c_8lane`: lane
   k = the sub-stream `{64b + 8k}`); the two qwords of a raw pair belong to
   DIFFERENT reference lanes (2f and 2f+1). An unpack-free state
   irreversibly mixes two reference lanes.
2. **The lane-granularity kill.** Splitting into 16 half-lane (64-bit)
   states fails because clmul products begin at lane bit 0: a 64-bit
   state's ≤96-bit product spills across the qword boundary into the
   neighbor state's half of the 128-bit clmul lane. VPCLMULQDQ simply
   cannot multiply two independent 64-bit values in one lane.
3. **The zero-divisor kill.** Unmixing a mixed state at the ending would
   need a ring element X ≠ 0 with B ⊗ X ≡ 0 for an entire data subspace —
   a zero divisor in GF(2)[y]/VM. The Castagnoli polynomial is primitive,
   so VM generates a FIELD: no zero divisors exist. The mixed information
   cannot be separated by any linear ending.

**Consequence: VPUNPCK is STRUCTURAL** (docs/26 refutation #4 re-confirmed
by independent algebra — this time with the full lane-semantics proof).
The p5 census floor for lane-pure CRC-32 on zmm is
**4 VPCLMULQDQ + 2 VPUNPCK + 2 VPTERNLOG per 128 B = 6 p5 uops →
21.3 B/cyc ceiling** (~51–55 GB/s/core at 2.4–2.6 GHz). The 2B demand of
55.32 GB/s delivered CRC must therefore come from: (a) converting the
measured ~9 cyc/step latency-bound loop to the 6-cyc p5 floor (**Stage A**,
this revision), (b) raising fabric efficiency 49% → 55%+ by absorbing
serial/FNV/RX work onto SMT siblings (**Route S**), and (c) healthy-draw
selection per the class protocol. On record-class draws (kbench 34.9,
uncontended L3) the post-Stage-A budget closes with margin; on median
healthy draws it is knife-edge — the fleet decides.

## 3. Stage A: The Dual-Stream Fold ("dfold") — R16a

### 3.1 The class-law foundation (P1')

The R15 vtail proved the fold state's ring semantics: a state value
`V = V_lo | V_hi<<64` represents the element `clmod(V)` in GF(2)[y]/VM,
and both vend and the vtail composed-field ending are CLASS functions.
The R16 solver (scripts/r16_ufold_derive.py) verified the missing law:

```text
clmod(M(V)) == rmul(clmod(V), K),   K = clmod(RKLO) = 0xF20C0DFE = VR0
```

**The R13 step is ring multiplication by VR0** — the fold's advance
constant and the R14 ending's seed constant are the same ring element
(the R13/R14 convergence), and `RKHI == K ⊗ y^64 mod VM` exactly. 500
random-state probes + the full differential pin it.

### 3.2 The design

* **T=2 block-parity split**: set A consumes even 128 B blocks, set B odd.
  Both step with **M² = multiplication by K²**, realized as ONE 2-clmul
  step with the reduced pair `DFOLD_K2_LO = 0x3DA6D0CB`,
  `DFOLD_K2_HI = 0xBA4FC28E` (both ≤ 32 bits — state hi ≤ 32 bits,
  strictly tighter than RKLO's 36-bit unreduced form). Census per 128 B is
  IDENTICAL to the R13 kernel; the chain count doubles (4: A.even, A.odd,
  B.even, B.odd), so every chain's latency budget doubles — the
  latency-bound ~9 cyc/step (2 chains vs the 6-cyc VPCLMULQDQ latency)
  converts toward the 6-cyc p5 throughput floor = **21.3 B/cyc**.
* **The merge**: the block-parity split leaves one set deficient by exactly
  ONE single-block advance M — whose reduced pair is **(VR0, RKHI)** — so
  the merge is literally one `fold_step_r` with the OTHER set's states as
  the injected units (4 clmul + 2 ternlog, once per span, off the hot
  loop). Zero new merge code, zero new merge constants.
* **The ending is reused verbatim** — with one constraint. The merged
  states are ring-CONGRUENT to the sequential kernel's states, not
  value-identical, so the value-based endings (the R13 crc-chain lane-0
  continuation) are INCOMPATIBLE. dfold dispatch therefore forces
  `vend + the vtail composed-field lane-0 path for ALL r` (the R15 V5
  differential already verified the vtail formula below r=16; the r≥16
  ship gate was pure economics). `HFT_CRC_DFOLD=1` with `HFT_CRC_VEND=0`
  resolves as dfold OFF.
* **Validation**: the solver's P2 differential — 1,778 bodies (exhaustive
  lengths 192..600 × 4 patterns + longs to 16,384 + 120 randoms) all
  bit-exact vs `span_ref`, plus dfold==vtail==ref cross-model agreement;
  the Rust suite pins the same in `t_fold_differential_exhaustive`
  (dfold-forced on every body) and `t_dfold_constants_derivation`
  (re-derives K/K²/K⊗y^64 and the class law at test time).
* **Rollout**: default OFF on every class (the house law); CI arm **11v**
  is the armed attribution soak; kbench rows `fold512_rd` (1t +
  2cpu_distinct + 2cpu_smt) are the kernel-level twin — `fold512_rd` vs
  `fold512_rv` on the same draw IS the chain-depth effect at identical
  census. Rollback: `HFT_CRC_DFOLD=0` (the default IS the rollback).
  Local sandbox read (latency-blind VM, 14 cyc/step vs CI's 9):
  rd 20.70 vs rv 21.07 GB/s — loop-neutral here, as expected; the fleet
  decides on real silicon.

### 3.3 The honest local economics note

Forcing the vtail ending on r < 16 spans (~19% of lengths by the len%128
distribution) trades ~5 cyc/span of ending economy for class-compatibility.
On the local sandbox that is the entire −3.3% (r vs rv); the CI draws with
vend+vtail already default (SPR+) pay only the r<16 residue. If the fleet
prices dfold positive but the r<16 residue shows, a follow-up can pin a
value-exact small-r lane-0 path for the dfold states (solver-derived).

## 4. The 2B Budget Math (post-verdicts)

```text
Demand:    2.0B msg/s × 27.66 B/msg = 55.32 GB/s delivered CRC
Ceiling:   2 cores × 21.3 B/cyc × 2.30–2.60 GHz  = 98–111 GB/s fold pool
           (record-draw kbench 34.9 → post-Stage-A projection 45–55 1t)
Efficiency: 48.9% today → 55–65% needed with Route S absorption
Supply:    contended L3 25–35 GB/s/core is the co-wall (R9);
           55.32 aggregate = 79–92% of the 2-core contended band
Verdict:   record-class draws close with margin; median healthy draws are
           knife-edge — exactly what the ≥3-draw class protocol prices.
```

## 5. R16b "rxdesc" — the array-driven submission (SHIPPED — this revision)

### 5.1 The main-side wall, measured

The sustained record (1,234,801,472) was MAIN-THREAD-BOUND: the R11
`distinct` placement experiment moved the workers to their own physical
cores (they folded +15% more spans) and the sustained rate stayed flat —
the submitting core could not feed them faster. Its budget (~1.86
cyc/msg) decomposes as the ladder (~0.63), the ordered fold (~0.15), and
the per-span descriptor submission into the per-lane SPSC rings
(~0.4-0.6: the desc store, the chunk-open anchor write, the space
checks, the backpressure spin). The 2B demand at 2.3 GHz allows ≤ 1.15
cyc/msg — the submission cost had to go.

### 5.2 The design that shipped (and the one that did not)

The first design — the RX thread PREFILLS a frame-indexed descriptor
array while slicing — was **refuted by the parity matrix within hours**:
the canonical schedule is DUAL-FEED, half its frames are duplicates the
ladder skips, so the frame index and the span index diverge immediately
(measured: fixes == span count == 8,640 on the "steady" schedule — every
entry wrong). The shipped design is the **WARM START**: the schedule is
deterministic — every pass replays the SAME span sequence over the SAME
blob — so each submission window opens by COPYING the previous window's
8-byte span descriptors `(offset:u32 | len:u16)` into its array slot (one
memcpy of the previous window's span count, ~1µs/pass). The untimed
reference pass pays the full check-and-fix once; every measured pass
finds every entry already correct. **The check is the correctness**: each
span's entry is compared against the actual body (one 8-byte load +
compare on the steady path) and fixed in place on divergence — the ring
protocol's cost and semantics on the slow path, zero stores on the fast
path. Measured on the local smoke: rx_fixes = 5,184 TOTAL across
thousands of passes (vs 8,640 PER PASS without the warm start).

The protocol (nf-transport/src/rxdesc.rs — the full ownership proof):
8 array slots circulate sink-driven (`last_slot + 1`); a slot is reused
only after this sink's fold drained its earlier window (the reuse gate);
workers poll one `spans_ready` cursor, walk their chunk-grid chunks
(THE SAME GRID as the ring protocol — the fold's chunk-ordered drain and
the fold-order assert are unchanged), resolve each span's pass record
and read descriptors straight from the arrays. The work-assist survives:
chunks taken inline are marked in a chunk-state ring (the mark stores
`chunk_id + 1` — the value check makes wrap aliasing impossible; the
ring is cleared at each sink's ACTIVATION — chunk ids restart at 0 per
generation), and the fold waits at marked chunks (the lane's results
hold only later chunks there). The straddling window boundary (the pass
span count is not chunk-aligned) seals a partial inline chunk and the
next window's open re-claims the continuation.

Three hard-won protocol laws, each caught by the parity suites:
1. **The gen-activation clear**: sink CONSTRUCTION order does not match
   consumption order (the sustained bench builds its main sink before
   its ref sink) — marks must clear at the first window OPEN, not at
   construction.
2. **PASS_RING 8 → 32**: the array protocol removed the desc-ring
   backpressure, so the fold may lag up to the 8-window array gate when
   the workers are throughput-bound — the harvest ring must hold that
   lag (the ring protocol capped it at ~1 pass).
3. **The fold's inline wait**: the worker skips marked chunks and
   publishes BEYOND them — the fold must not drain the lane while
   `fold_pos` sits inside a marked chunk (the ring protocol never had
   later chunks' results ahead of the cursor).

Rollback: `HFT_RXDESC=0` (CI arm 11w; the default IS the new path).
Attribution: the `R16B_RXDESC_VERDICT rx_fixes=... assist_chunks=...`
telemetry line on every sustained run. Validation: the 3-way parity
matrix (sequential == check+fix == forced-inline == pipelined ×
steady/chaos × worker counts 1-3, plus the zero-fixes fast-path assert
on the warm-started second pass), a 400-pass multipass soak (the record
ring wraps 50x, the reuse gate binds, chunks straddle every boundary),
and the full existing hydra suite on the legacy path.

### 5.3 The 2B arithmetic (post-R16b/R16d)

```text
Main budget:  ladder 0.63 + fold 0.15 + rxdesc (load+cmp/span ≈ 0.03
              amortized) + poll ≈ 0.85 cyc/msg  → ceiling ~2.7B @ 2.3 GHz
Worker pool:  2cpu_distinct 59-61 GB/s on healthy draws (kbench 30.7)
              × ~0.95 (scalar-sibling theft, R8's <4.5% class)
              ≈ 56-58 GB/s  → 2.03-2.10B msg/s of CRC capacity
Demand:       2.0B × 27.66 B/msg = 55.32 GB/s
Verdict:      the strands close together — median healthy draws pass with
              single-digit-percent margin, record-class draws (kbench 34.9
              → pool ~66 GB/s) with ~20%. The fleet prices it per draw.
```

### 5.4 Front A (the 5B program — Lever B remainder)

The 0.6346 cyc/msg Front A wall is the RX thread's per-frame work: poll()
frame slicing + the per-frame `FrameEntry` construction. R16b's arrays
are the SPAN side; the FRAME side (publish frame-level descriptors by
reference + the consumer's VPADDQ prefix-sum message-boundary walk,
< 0.15 cyc/msg) remains the Front A lever — the next installment.
Constraints unchanged: the tombstone/reset arithmetic, the prepatch
windows, `ALLOC_DELTA == 0`, the D-oracle parity, and
`#![forbid(unsafe_code)]` on nf-protocol/nf-arbitrator.

## 6. R16d — the placement flip (SHIPPED with R16b — the Double Helix)

Neither strand moves the number alone: rxdesc without the flip leaves
the workers SMT-stacked at the ~32.8 GB/s `2cpu_smt` ceiling; the flip
without rxdesc leaves the system main-bound at ~1.23B (the R11
refutation). Together: `fabric_placement` now defaults to DISTINCT on
exactly the 2-worker / 2-physical-core / SMT shape (the 4-vCPU draws)
— workers own physical cores, the scalar threads (main, RX) become the
SMT siblings, each stealing issue slots from one worker (the R8 class
bound: < 4.5% if no p5 vector shuffles and no L1D pressure). Every
other shape keeps the R8 default (≥3 workers, non-SMT hosts, ≥4
physical cores). Rollback: `HFT_FABRIC_PLACE=siblings` (CI arm 11k,
repurposed from the R11 experiment — distinct IS the default now). The
supply watch stays: on contended draws the L3 ceiling binds before the
p5 floor does (R9) — draw selection per the R12 protocol is the
mitigation, and the kbench `2cpu_distinct` row prices each draw's pool.

## 7. The Record-Claim Protocol (R12-encoded)

1. Harvest ≥ 3 independent healthy draws of the SAME class (8573C-class
   SPR/EMR; kbench `fold512_r` 1t ≥ 30.0 GB/s each; sub-29.0 discards).
2. On each: the default-config 11b sustained verdict + the armed arms
   (11v dfold attribution) with `HYDRA_BITPARITY == 0x881639cead506f25`
   bit-exact on every arm and `ALLOC_DELTA == 0`.
3. The claim is the MEDIAN healthy draw's sustained rate ≥ 2.0B; raw logs
   + kbench side-by-side in the evidence ledger (docs/28 §6 format).
4. Front A 5.0B claim: same class protocol; the pure-ingest gate is
   formally elevated 2.0B → 5.0B in the same change that submits it.

## 8. Ledger

| Revision | Change | Verdict |
|---|---|---|
| R16a | dfold (this doc): T=2 dual-stream fold, class endings forced, `HFT_CRC_DFOLD`, arm 11v, kbench `fold512_rd` | shipped default-OFF; draw 6 neutral (supply-bound host); healthy-draw pricing pending |
| R16b | rxdesc — the array-driven submission: warm-started span arrays + check-and-fix + the chunk-state assist (§5) | SHIPPED default-ON (HFT_RXDESC=0 rollback, arm 11w, R16B_RXDESC telemetry); the RX-frame-prefill variant REFUTED by the parity matrix (dual-feed divergence — §5.2) |
| R16d | the distinct-placement default flip for the 2-worker 2-core SMT draws (§6) | SHIPPED (HFT_FABRIC_PLACE=siblings rollback, arm 11k repurposed) |
| R16c | Route S extension: serial/FNV absorption into the sibling assist path | scoped; superseded in priority by R16b+R16d (the efficiency now comes from the placement flip) |
| — | Route F (larger runners) | TERMINATED (R12 + R5) |
| — | Stage B unpack-free census-4 kernel | REFUTED (§2 — the zero-divisor proof) |
| — | GFNI CRC hybrid, PMC instrumentation, ymm dual-chain | REFUTED earlier (Task 7 report verdicts) |

## 9. Draw ledger (R16 stack, commit 8e95fe1 — run 37210099462)

**Draw 6 — 8573C, contended band (kbench `fold512_r` 1t = 29.61 GB/s; below
the 30.0 healthy line, above the 29.0 discard line → marginal, does NOT
count toward the record protocol):**

```text
kbench 1t:   fold512_r 29.61 / rv 29.64 / rc 29.56 / rd 29.62 GB/s
             (all four sink-identical 0xbedb8ba779de450f — the dfold is
              BIT-EXACT on target silicon; dead even ±0.1% — the draw's
              kernel sits ON the contended L3 supply ceiling, exactly the
              R7 prediction: no compute lever can show on this host)
2cpu:        distinct 59.17 / smt 29.41 GB/s (SMT adds nothing — consistent)
11b default: 1,015,310,418 sustained (bit-exact, allocs=0, R8 1B gate PASS)
11v dfold:   1,015,433,364 sustained (+0.01% — dead even, supply-bound)
HYDRA_BITPARITY 0x881639cead506f25 BIT-EXACT; ALL CHECKS PASSED
```

Verdict: **dfold neutral on a contended draw** — the lever prices only on
healthy draws (kbench ≥ 30) where the loop runs latency-bound instead of
supply-bound. The fleet continues fishing.

**Healthy 8573C draw statistics (the R12 protocol's counting pool):**

| Draw | Stack | kbench 1t (GB/s) | Sustained (msg/s) | 11v dfold | Counts |
|---|---|---|---|---|---|
| 4 | R15 (vend+vtail) | 30.00 | 1,061,800,000 | — | healthy |
| 5 | R15 | 31.00 | 1,043,112,246 | — | healthy |
| 6 | R16 | 29.61 | 1,015,310,418 | 1,015.4M (+0.01%) | marginal — no |
| 7 | R16 | 30.66 | 1,096,132,337 | 1,093.6M (−0.23%) | healthy |

Healthy median 1.062B → the 2.0B demand is +88% over the healthy median:
the full R16 program (dfold on record-class draws + Route S absorption)
carries the distance; no single lever does. Front A on this draw class
stays in the historical contended band (the 5B program rides R16b).

**Draw 7 (healthy, run 37210847710, shard 11) — the reframe.** kbench 1t:
`r 30.66 / rv 30.82 / rd 30.49` (−0.6% — parity perfect, sinks identical).
11b 1,096.1M (the best healthy-draw number yet), 11v 1,093.6M (−0.23%,
inside the ±3.4% arm noise). **Verdict so far: dfold NEUTRAL on contended
AND healthy draws** — at 30.66 GB/s ≈ 12.8 B/cyc ≈ 10 cyc/step, the
kbench-realistic span corpus is bound by span-level supply/endings, not
step latency. The scaffold's honest decomposition anticipated the
possibility; the fleet has now priced it on two draw classes. **The dfold's
remaining open case is the record-class host** (kbench ≥ 34: there the
9 cyc/step ≈ the pure-loop latency bound — the one regime where chain
depth can bind). Record-class hosts are ~2% of shards — the fleet keeps
fishing with the 11v arm armed. Consequence for the program ordering:
**Route S (fabric efficiency 49% → 55%+) is promoted to the primary 2B
lever** (docs/29 §6), with dfold as the record-class contingency.
