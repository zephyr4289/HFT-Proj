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

## 5. Front A: Lever B "rxbuild" (the 5B program — design frozen, build next)

The 0.6346 cyc/msg Front A wall is the RX thread's per-frame work: poll()
frame slicing + the per-frame `FrameEntry` construction (bytes/blocks
re-slicing, elig computes, 9-field store). The current design already
builds each entry exactly once (per-turn buffers, one Release publish) —
the remaining lever is to **stop constructing entries on the RX thread at
all**: publish frame-level descriptors by reference and let the workers
walk the frames in place:

* the RX publishes (per turn) only the blob window + frame-slot metadata —
  the per-frame entry build moves into the consumer's scan (which touches
  every field anyway);
* the workers' message-boundary walk vectorizes with the VPADDQ prefix-sum
  scan (4 length prefixes → displacements → parallel addresses,
  < 0.15 cyc/msg — the R10 prior art);
* budget: RX per-frame → ~0.2 cyc/frame; consumer walk stays ≤ 0.46
  cyc/msg — the 5B line at 2.3 GHz.

Constraints that the design must hold: the tombstone/reset turn
arithmetic, the prepatch session-baking windows (never over-patch frames
the consumer has not freed), `ALLOC_DELTA == 0`, identical FrameEntry
stream to the consumer (the D-oracle parity), and `#![forbid(unsafe_code)]`
stays on nf-protocol/nf-arbitrator (the shared-slot machinery lives in
nf-transport as today).

## 6. Route S (the efficiency hedge — partially deployed)

The RX thread already runs as the workers' SMT sibling (the R11/R13
fabric), and the R10 assist ring already converts spare sibling cycles
into fold work (assist_chunks ≈ 5.4% at equilibrium). The Route S
extension for the 2B push: move the workers' serial ITCH parse + FNV
epilogue into the sibling's assist path (bounded < 4.5% p5 penalty per
R8), so the physical cores approach pure fold. This is a hydra.rs
restructure — gated, armed, and priced per draw exactly like 11v. The
supply watch stays: on contended draws the L3 ceiling binds before the
p5 floor does, and no scheduling can fix that (R9) — draw selection is
the mitigation.

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
| R16b | rxbuild (Front A Lever B): publish-by-reference frame descriptors + worker in-place walk + VPADDQ prefix-sum scan | design frozen (§5); build next |
| R16c | Route S extension: serial/FNV absorption into the sibling assist path | scoped (§6); after R16a draw data |
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

| Draw | Stack | kbench 1t (GB/s) | Sustained (msg/s) | Counts |
|---|---|---|---|---|
| 4 | R15 (vend+vtail) | 30.00 | 1,061,800,000 | healthy |
| 5 | R15 | 31.00 | 1,043,112,246 | healthy |
| 6 | R16 (this) | 29.61 | 1,015,310,418 | marginal — no |

Healthy median 1.05B → the 2.0B demand is +91% over the healthy median:
the full R16 program (dfold on healthy draws + Route S absorption + record-
class hosts) carries the distance; no single lever does. Front A on this
draw class stays in the historical contended band (the 5B program rides
R16b, not the kernel).
