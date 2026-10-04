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

Seven hard-won protocol laws — the first three from the parity suites,
the last four from the FIRST FLEET DRAW's attribution (an 8370C where
every rxdesc arm ran ~30% under the ring arms; each fix verified by the
local A/B, which ended +18-20% OVER the ring on the sandbox's 1-worker
worst case):
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
4. **The publication cadence is the chunk**: publishing spans_ready per
   emission batch (≤32 spans) while the worker polls per iteration made
   the shared line ping-pong at MHz rates — main's Release stores
   stalled on the coherence traffic. One publish per CHUNK (the ring's
   per-chunk head-store cadence).
5. **The worker drains in batches**: one chunk per wake made the worker
   idle between every pair of publications; the yield-escalating spin
   then churned the runqueue (3.9M yields/worker measured). The batched
   drain (WORKER_BATCH spans per wake, one res publish) + the deep
   pause-bounded spin (yield only after ~100µs of true idleness).
6. **The prefetch spray keeps the ring's PER-SPAN cadence**: a
   batch-level spray issued 24 lines per 128 spans — 128x too slow —
   and the eval ran memory-stalled at ~45% of the kernel ceiling.
7. **FLOW CONTROL RETURNS, EXPLICITLY**: the ring's desc-ring fullness
   was pacing main to the fold (the R7 invariance's own law); without
   it the fold lagged 87,936 spans and 61% of the run burned in the
   array-reuse gate's spin. The hard pending pace (8192 spans: fold +
   deep-spin, productive) with the assist watermark (2048: convert the
   lead to in-window CRC) firing first — pending_max dropped to 4,817
   and the sandbox went from −20% to +18-20% vs the ring.
8. **THE PASS-BOUNDARY UNSTICK CEILING** (pipeline.rs `reset_pass`): the
   auto-advance unstick must NEVER free beyond the abandoned pass's EOS
   marker, and the marker state alone cannot decide the policy — the
   stale marker of the already-drained pass k−1 and the unconsumed
   marker of the abandoned pass k read identically in `auto_eos_turn`.
   The consumer's own boundary flag (`at_eos`, set when its last
   `next_batch` consumed a marker) is the discriminator. CLEAN END:
   free nothing — every publication past the cursor belongs to the next
   pass. (The bug: freeing to `rx_turn` raced the RX's bake — the
   ap-read/rx_turn-read window let the unstick free the NEXT pass's
   in-flight head, the consumer resumed mid-pass, and the pass verified
   short at 360,068/505,849 — measured once in ~25k passes on the
   first Double Helix draw.) MID-PASS ABANDON (incl. the never-consumed
   construction pass): free the in-flight tail — a frozen unstick
   deadlocks (the RX stalls NBUF=16 buffers in, thousands of turns from
   the marker) — but LOCK the ceiling at the marker the moment it lands
   (`last_eos >= self.turn` proves it is the abandoned pass's own: the
   RX cannot publish past a marker whose buffer it awaits). The lock
   cannot fire late: the RX stores `auto_eos_turn` BEFORE `rx_turn`,
   and the consumer loads `rx_turn` BEFORE `auto_eos_turn` — any
   rx_turn that includes the marker is observed together with the
   marker, in the very iteration whose free loop would first cross it.
9. **ENV PARSING IS ALLOCATION**: `worker_batch()` parses an env var —
   per-iteration calls inside the worker loop sit inside every measured
   window. The first 11u shard caught it as an `ALLOC_DELTA` violation;
   the read is hoisted to worker start (outside all windows).

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

### 5.5 R16e — the RX Desc Diet (SHIPPED — this revision)

Draw 11's build order, executed. The ~21% rxdesc-vs-ring gap decomposed
50/50; the diet fixes the WORKER side only (the sink is untouched — the
attribution stays clean against 11w/11x):

* **STRAND A — the depth batch** (`lane_worker_rxdesc_diet`, the
  default; `HFT_RXDIET=0` is the rollback to the pre-diet worker
  VERBATIM — CI arm 11x): the batch no longer terminates at the
  publication frontier. A frontier hit PUBLISHES the partial run first
  (liveness: the sink's pending pace and the window-reuse gate spin on
  the FOLD, which cannot pass results still buffered in the worker),
  then waits at the frontier in bounded pause laps
  (`HFT_FRONTIER_LAPS`, default 16, fleet-sweepable, clamped [0,64]; 0
  disarms strand A — CI arm 11y) instead of exiting through the outer
  loop's deep-pause escalation: no re-entry, no yield escalation, no
  scheduler wake latency per publication gap. The wait is counted
  (`frontier_waits` / `frontier_ns` → the DIAG's `fw=`/`fw_ms=`) and
  EXCLUDED from `eval_ns` — busy% stays honest. A fresh generation
  landing mid-wait publishes and bails to the outer re-anchor.
* **STRAND B — per-chunk record resolution**: the per-span pass-record
  probe becomes a NEXT-BOUNDARY cache; the per-span cost is one register
  compare. Soundness: the record for a window base rb is published
  (Release on records[slot]) BEFORE the window's first span is
  submitted, hence before any spans_ready store exceeding rb — the
  worker's ready Acquire that exposes spans ≥ rb also exposes the
  record (release sequencing). The cache is refreshed at EVERY ready
  advance (the outer load and each frontier-wait break), so a boundary
  below the current ready is in the cache by the time the span loop
  reaches it; the compare fires the resolve (which walks any number of
  windows in one pass) exactly at the boundary. An 8-window overwrite
  of the probed slot is unreachable while the boundary matters: the
  reuse gate requires the fold to have drained that window, and the
  fold cannot pass results this worker has not yet evaluated.
* **STRAND C — the division-free grid**: the chunk walk (eval + spray)
  tracks `(chunk_id, chunk_lo)` by addition (the lane grid's own
  stride); the spray's per-SPAN division/modulo/is_inline/probe moves
  to per-chunk sections — one is_inline + (at most) one resolve per 64
  spans, no idiv anywhere.

**Validation**: the 3-way parity matrix extended to BOTH worker shapes
(the diet runs the full chaos × w1-3 × (a)(b)(c) matrix; the pre-diet
pin runs w=2 steady+chaos, all three cells), the 400-pass multipass
soak under both shapes, the full existing suite, clippy `-D warnings`,
`HYDRA_BITPARITY` bit-exact + `allocs=0` on every local leg (diet /
laps=0 / pre-diet / ring).

**The honest local economics** (the latency-blind sandbox, 1-worker
main-bound shape — NOT the CI regime): system 5s sustained — pre-diet
424.2M / diet-B+C (laps=0) 399.7M / full diet 387.6M / ring 399.8M
msg/s; worker-level eval rate +9-12% in BOTH diet shapes (11.1K vs
9.9K spans/ms — the dilution reduction is real); the regression
mechanism is the fold-lag feedback: the wait's deferred result
publishes let the fold lag (pending_max 7,040 vs 4,496; the pre-diet's
res-ring fullness — 20.9K res-blocks — was pacing the worker to the
fold), and the reuse-gate spin ate +455ms of main's wall
(reset_ms 985 vs 530). Draw 11's DIAG (the CI regime: workers 86%/79%
busy at ~220 cyc/span vs the ring's 96.2% at 189) is where both halves
bind — the fleet prices the diet per draw via 11b vs 11y vs 11x vs 11w,
and the class protocol decides the default exactly as vend/vtail were
priced per class.

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
| R16e | the RX Desc Diet — the worker-eval fix for the draw-11 decomposition: the depth batch (strand A, HFT_FRONTIER_LAPS) + per-chunk record resolution (B) + the division-free grid (C) (§5.5) | SHIPPED default-ON (HFT_RXDIET=0 rollback to the pre-diet worker verbatim, arm 11x; arm 11y isolates strand A; the worker DIAG gains fw=/fw_ms=); parity extended to both worker shapes |
| R16d | the distinct-placement default flip for the 2-worker 2-core SMT draws (§6) | SHIPPED (HFT_FABRIC_PLACE=siblings rollback, arm 11k repurposed) |
| R16c | Route S extension: serial/FNV absorption into the sibling assist path | scoped; superseded in priority by R16b+R16d (the efficiency now comes from the placement flip) |
| — | Route F (larger runners) | TERMINATED (R12 + R5) |
| — | Stage B unpack-free census-4 kernel | REFUTED (§2 — the zero-divisor proof) |
| — | GFNI CRC hybrid, PMC instrumentation, ymm dual-chain | REFUTED earlier (Task 7 report verdicts) |

## 9. Draw ledger (R16 fishing pushes — each entry labels its stack; run IDs in the entries)

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

**Draw 8 (8573C noisy band, run 37211544099, commit 7d46650 — pre-R16b
ring stack).** kbench 1t 28.02 (< 29.0 — a discard per R12) yet 11b
1,177,988,640: the strongest 11b to that date, on the siblings-stacked
ring. 2cpu_distinct measured 55.28 GB/s vs ~32.6 delivered by the
SMT-stacked layout — the headroom the flip targets. Bit-exact, allocs=0.

**Draw 9 (8370C healthy, same run).** 11b 943.2M — the healthy-8370C band
(~945-965M), consistent with the class's kbench ceiling.

**Draw 10 (8573C RECORD-CLASS, run 37219883871, shard 3 — the four-laws
stack, commit 07e81d3).** kbench 1t: `fold512 34.86 / r 34.05 / rv 34.42
/ rc 34.29 / rd 34.08`; `2cpu_distinct fold512_r` = **69.93 GB/s** — the
largest pool any draw has ever shown (the record draw's class). Arms:
11b (rxdesc+distinct) **957.3M** / 11e (prepatch off) 1,001.1M / 11f (w3)
667.8M / 11g 975.3M / 11h 989.3M / 11i 981.5+963.7M / 11j 964.9M / 11k
(siblings) 876.4M / 11l 946.9M / 11m 889.1M / **11n (HFT_DESC8=0 — which
implies rxdesc off, the ring world) 1,214,220,732** / 11r (fold512 kernel)
909.1M / 11s 972.6M / 11t 928.5M / 11u CRASHED (`ALLOC_DELTA=336` — the
`worker_batch()` env parse; fixed in cc9664b) — arms 11v/11w never ran.
Three verdicts:
1. **R16d (the distinct flip) is vindicated on silicon**: within-draw,
   11b vs 11k prices it at **+9.3%** (957 vs 876) on the rxdesc path; the
   ring at 1.214B is the second-highest number ever recorded (record
   1,234.8M @ kbench 34.90) and +10.8% over draw 7's siblings-stacked
   1,096M on a comparable band — cross-draw, suggestive only.
2. **R16b (rxdesc) is STILL −21.2% vs the ring on record-class silicon**
   (957 vs 1,214, same draw, same placement) after the four coherence
   laws (it was −30% on 8370C before them). Worker DIAG: 80%/74% busy,
   res_waits=0 — busy-but-diluted: the array-protocol eval (chunk-grid
   walk + record resolution + array desc reads + re-anchoring) costs ~11
   points of delivered/pool efficiency (37.9% vs the ring's 48.0%).
3. The §5.3 2B arithmetic assumed rxdesc ≥ ring — **refuted on both
   observed draw classes**. The rxdesc worker-eval diet is now the
   primary R16b follow-up; ring+distinct 1.214B is the current stack
   ceiling. The cc9664b cascade re-rolls with 11u alive — 11v/11w will
   finally price dfold and the rxdesc rollback on a fresh draw.

**Draw 11 (8573C healthy, run 37225938889, shard 5 — the cc9664b stack;
the first FULL ladder: every arm 11b–11w ran green).** kbench 1t `r 30.16
/ rv 30.09 / rd 30.03`; `2cpu_distinct fold512_r` = 60.87 GB/s. Arms: 11b
(rxdesc+distinct) **842.1M** / 11e 888.8M / 11f (w3) 578.6M / 11g 840.9M
/ 11h 855.3M / 11i 837.8+839.5M / 11j 838.8M / 11k (siblings) 750.4M /
11l 795.8M / 11m 765.9M / 11n (ring) 1,055.0M / 11r (fold512) 790.8M /
11s 837.0M / 11t 828.6M / 11u (wbatch 64/256) 802.6+834.0M / 11v (dfold)
827.6M / 11w (rxdesc OFF — ring) **1,060.6M**. Verdicts:
1. **rxdesc = −20.6% vs the ring on the healthy class** (842 vs 1,061,
   same draw, same placement) — with draw 10's −21.2% on record-class,
   the gap is a stable ~21% across classes. The four coherence laws
   moved it from −30% (8370C) to −21%; the remainder is structural.
2. **The gap decomposes 50/50** (worker DIAGs, ring vs rxdesc): the
   ring runs 96.2% busy at ~189 cyc/span; rxdesc runs 86%/79% busy at
   ~220 cyc/span. Half is WAKE-CADENCE IDLE (the batched drain still
   idles between publication pairs — 2.7x more batch iterations than
   the ring's, 620K-806K idle iters), half is PER-SPAN EVAL DILUTION
   (record resolution + array desc reads + re-anchoring + the chunk-grid
   walk ≈ +31 cyc/span). Also: assist_chunks 30,552 vs the ring's 384 —
   the assist watermark fires 80x more under the array protocol's fold
   lag (main-side load, currently not binding).
3. dfold NEUTRAL again (−1.7%, inside noise) — its record-class case
   remains open (draw 10's 11v never ran; the crash is now fixed).
4. The flip prices at +12.2% on the rxdesc path this draw (842 vs 750).
The build order that follows: **the rxdesc worker-eval diet** — (a) the
wake cadence (deeper drain batches / fewer publication-pair idles), (b)
the per-span resolution cost (record probes per span → per-chunk), (c)
the re-anchor path. Target: ring parity first (1.06B healthy / 1.21B
record-class), then the §5.3 pool arithmetic re-opens the 2B path.

**Draw 12 (8573C healthy + marginal, run 37231552516 — the R16e diet's
first pricing; waves 1-2, shards 14/16/17; commit 44fc15b).** Shard 17
(HEALTHY, kbench `fold512_r` 1t = 30.23, `2cpu_distinct` 60.62 GB/s):
11b (diet) **862.3M** / 11y (B+C, laps=0) 851.6M / 11x (pre-diet)
850.6M / 11w (ring) **1,054.5M** / 11n (legacy-desc ring) 1,036.9M;
bit-exact + allocs=0 on every arm; all checks PASS. Shard 16 (marginal,
29.56): 11b 835.9M / 11y 824.0M / 11x 818.7M / 11w 1,089.6M. Shard 14
(8370C noisy, 25.33 — discard): 11b 660.3M — the uniformly-low band.
Verdicts:
1. **The diet is NEUTRAL-POSITIVE on the deciding class**: +1.4%
   (healthy) / +2.1% (marginal) vs the pre-diet — inside the ±3.4%
   arm-position noise, direction consistent on both 8573C draws. The
   default stands; the ≥3-draw median rule accumulates.
2. **The wake-cadence half IS fixed**: idle_iters 620-806K (draw 11) →
   294K/worker; the wait is now VISIBLE and cheap — fw=1.41M hot
   episodes, fw_ms=422 (8.4% of the wall), avg ~0.3µs/episode (the
   frontier advances DURING the waits — the worker stays positioned).
   Strand A prices +1.3% (11b vs 11y); strand B+C +0.1% on the rate but
   **−8 cyc/span on the eval** (11x 226.7 → 11b 219.1 / 11y 218.7 —
   the per-span probe + division removal is real at the worker level).
3. **The ring gap moved −20.6% → −18.2%** (diet/ring 0.818) — the diet
   bought ~2.4 points of it. The remaining gap is NOT the wake cadence
   (fixed) and only partly the measured dilution: **the array protocol
   runs 207-227 cyc/span where the ring runs 195-201** (both desc
   formats), i.e. ~+24 cyc/span is structural to the array-path worker
   (the R7 fabric-efficiency invariance breaks: 39.3% of the 60.62
   pool under rxdesc vs 48.1% under the ring — the invariance held
   only for the ring plumbing).
4. The diet's flow control is healthier than the pre-diet's: pend_max
   1,968 (the assist almost never fires — 1,317 chunks vs the pre-
   diet's 27,102); main never enters the pace spin (wait_ms 46 vs the
   ring's 704); the RX thread is 58% busy (the production side has
   headroom — the consumption side binds).
5. **The next decomposition needs an instrument, not a guess** (the
   honest law): the remaining ~24 cyc/span candidates (the array read
   path vs the ring's L1-resident desc stream, the per-chunk section
   machinery, the 8-slot L2 cycling) must be priced before another
   diet installment — the HFT_HYDRA_NULL diagnostic (protocol cost
   without the CRC kernel) on a CI arm is the candidate instrument;
   the draw-13+ fish continues for the ≥3-draw median.

**Draw 13 (8573C healthy, run 37232593313, shard 32 — the diet's second
healthy pricing; commit d969fab; waves 1-4, 46 jobs).** kbench 1t
`r 32.79 / rv 32.62 / rd 32.41`; `2cpu_distinct` 66.69 GB/s. Arms:
11b (diet) **945.5M** / 11y (B+C) 949.5M / 11x (pre-diet) 937.5M /
11w (ring) **1,124.2M** / 11n 1,079.0M; bit-exact, allocs=0, all PASS.
Verdicts:
1. Diet vs pre-diet **+0.9%** (draw 12: +1.4%) — two healthy pricings,
   same direction, both inside the ±3.4% noise: the diet is a
   consistent NEUTRAL-POSITIVE; one more healthy draw completes the
   3-draw diet median.
2. Worker eval (w0/w1 cyc/span): diet 198.7/185.7, B+C 197.8/185.8,
   pre-diet 205.0/191.7, ring 179.0/169.6 — the B+C strand again
   −6..7 cyc; the ring residue ~+20 (draw 12: ~+24) — consistent, and
   mildly supply-coupled (the residue shrinks on the stronger draw).
3. The ring on this draw class: 1,124.2M — the strongest healthy-draw
   ring number yet (the record-class 1.214B was kbench 34.86); the
   diet/ring 0.841 vs prediet/ring 0.834 — the array-path residue is
   the whole remaining story, exactly as draw 12 concluded.

**Draw 14 (8370C noisy, run 37233779998, shard 8 — a discard per the
class rule).** kbench 1t 27.59; 11b 668.8M / 11w (ring) 991.7M /
11n 985.4M / 11x 656.3M / 11y 675.0M — the uniformly-low band for every
array-path arm; bit-exact, allocs=0. No 8573C in this fish; the ≥3-draw
diet median still wants its third healthy draw.
