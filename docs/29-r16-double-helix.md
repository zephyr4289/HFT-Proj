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

**Draw 15 (8573C healthy ×2, run 37234491532, shards 5 and 7 — the diet
median pool COMPLETES at four pricings; commit e497e3d).**
Shard 5 (kbench `fold512_r` 1t = **33.46** — the strongest healthy draw
of the program, `2cpu_distinct` 63.69): 11b (diet) **986.4M** at
cyc/span **190.0/189.3** / 11y (B+C) 962.9M at 190.7/187.9 / 11x
(pre-diet) 961.3M at 198.2/193.8 / 11w (ring) **1,098.1M** at
181.3/184.0 / 11n 1,095.5M. Shard 7 (kbench 30.54, pool 60.56):
11b 840.2M at 221.5/203.7 / 11y 810.2M / 11x 840.6M at 228.6/209.4 /
11w 1,084.8M at 192.3/185.7. Bit-exact, allocs=0, all PASS. Verdicts:
1. **The diet's healthy median (4 pricings: +1.4%, +0.9%, +0.0%,
   +2.6%) = +1.15%** — a consistent NEUTRAL-POSITIVE; every draw inside
   the ±3.4% noise, direction positive 3-of-4. The default stands.
2. **The array-path residue is SUPPLY-COUPLED**: at kbench 33.46 the
   diet's worker cost is 190 cyc/span (residue +8 vs the ring's 181);
   at kbench 30.2-30.5 it is 219-221 (residue +24-29). The candidates
   line up: the array's working set (8 slots × 1 MB; ~276 KB/pass/lane
   cycling) is L2/L3-resident where the ring's desc stream is
   L1-resident (16 KB/lane) — on supply-rich draws the latency hides,
   on contended draws it exposes. The null-mode instrument remains the
   next step, but the target moved: the residue to kill is ~+8 cyc on
   supply-rich draws (record-class territory), not ~+24.
3. The strongest-draw ring (1,098.1M) vs diet (986.4M): 0.898 — the
   diet/ring gap narrowed to ~10% on this draw class; on the median
   healthy draw it remains ~18%. The 2B arithmetic (§5.3) still needs
   ring parity first — the fish continues per the class protocol.

**Draw 16 — THE R17 ERA OPENS (8573C healthy, run 37268486513, shard 16
Wave 2; commit aaf80bc: the ring default restore + the null-mode
instrument + the gate reorder; docs/challenge/CHECKLIST.md Phase 0).**
kbench `fold512_r` 1t = **30.44**, `2cpu_distinct` 61.11, THP granted,
bit-exact, allocs=0, all constraints PASS (kbench-first gate: enforced,
healthy draw). The first draw where the DEFAULT arm is the ring:

| arm | stack | rate | worker cyc/span | assist_chunks |
|---|---|---|---|---|
| 11b | ring+distinct (default) | 992.4M | 187.6/187.6 | 597 |
| 11e | prepatch off | 982.4M | — | 558 |
| 11k | **ring+SIBLINGS** | **1,057.9M** | 224.4/224.4 | **245,655** |
| 11t | vtail off | 1,024.5M | 184.4 | 253 |
| 11u | wbatch 64 / 256 | 1,038.8M / 1,035.6M | 183.7 | 59/165 |
| 11v | dfold armed | 1,046.8M | 182.5 | 165 |
| 11w | rxdesc armed (diet) | 792.2M | 210.7 | 4,162 |
| 11z | **NULL-MODE** | **1,306.3M** | **17.1** | 752 |

Verdicts (all single-draw, the ≥3-draw law governs flips):
1. **THE NULL INSTRUMENT'S FIRST READING: the plumbing+protocol floor is
   17.1 cyc/span.** Not 60-80 (the residual theory), not ~100 (Route R's
   kill case), not ~30 — SEVENTEEN. The worker's real-mix 187.6 cyc/span
   is ~91% kernel+endings+supply; the ring protocol, chunk walk, res
   publication, and desc streaming cost almost nothing. **Route R (any
   further ring/diet work) is dead by measurement** — there is nothing
   to reclaim on the submission side. The worker program (Route T
   density, the ending pipeline, supply) owns the entire 170.5 cyc/span
   bucket. Caveat logged honestly: the null stub touches only the
   body's first line, so the 17.1 EXCLUDES supply latency (the real
   loads' stalls live inside the 170.5; the fold512_supply kbench row
   prices that share per draw — CHECKLIST I-1).
2. **A1 CONFIRMED ON THE FIRST SAME-DRAW PRICING: ring+siblings beats
   ring+distinct +6.6%** (1,057.9 vs 992.4) — ROADMAP1 §3.1-A1's
   predicted +5-12% band. Mechanism visible in the telemetry: under
   siblings the assist fires 245,655 chunks (main's surplus converting
   to in-window CRC on its own core — the R11-era record mechanism);
   under distinct it is dormant (597) and main+RX sit on the workers'
   hyperthreads. Needs ≥3 healthy draws before any flip (the law).
3. **A3 IS REAL AND UNEXPLAINED BY PLACEMENT**: ring+siblings at kbench
   30.44 = 1,057.9M vs the R12-era ring+siblings 1,186.1M @ 29.94 =
   **-10.8% at better kbench**, with reflect/vend/vtail all
   neutral-positive on this draw (11r/11s/11t vs 11b). The R15
   `HFT_WORKER_BATCH=128` pacing (post-R12!) prices at +4.6% on BOTH
   sweep points (11u-64/11u-256 vs 11b-128 — suspicious of the 128
   default, awaiting the ≥3-draw median); the R16b assist watermark
   (2048 pending, vs the R12 lane-fullness trigger) prices next via the
   new 11wm arm. The reset_pass unstick changes remain the third
   suspect.
4. rxdesc armed on the same draw: **-20.2%** vs the ring default — the
   fifth consecutive draw in the -18..-21% band. The R17 flip's expected
   dividend is confirmed per-draw: the default arm no longer runs a
   refuted path.
5. dfold (11v) 1,046.8M = **+5.5% vs 11b** on this healthy draw (the
   ledger had it neutral on healthy through the rxdesc era) — the ring
   path's supply shape may favor the 4-chain overlap. Single-draw;
   11v keeps pricing every draw.
6. Section-16 gate reorder: this healthy draw ran the FULL enforcing
   gate (all constraints PASS, r8 pure-ingest PASS at 2.587B, r16 5B
   verdict FAIL-reported as designed); the discard path was not
   exercised on-silicon yet (no noisy Intel draw in this run's waves).
   Front A note for the Phase II program: 0.889 cyc/msg span-median —
   the warm-start lever's target denominator on this draw class.

**Draw 17 (8573C healthy, run 37269680406, Wave-1 shard 9; commit 4144c0b
— the 11wm bisect aboard).** kbench `fold512_r` 1t = **30.61**, THP
granted, bit-exact, allocs=0. Section 16 fired its ONLY failure of the
push on the classic arm's CV (26.07 > 25.0 — a 1-point marginal breach;
span arm tight at CV 10.3%, r8 pure-ingest PASS 3.084B) — the shard went
red but the full arm ladder had already run; the draw counts for the
ledger (the R2 §8 law: single-shard constraint noise never blocks the
fishing push; the scoreboard + artifacts published clean).

| arm | draw 16 | draw 17 | delta vs 11b |
|---|---|---|---|
| 11b ring+distinct | 992.4M | 1,049.6M | — |
| 11k ring+siblings | 1,057.9M | 1,056.1M | +6.6% → **+0.6%** |
| 11e prepatch off | 982.4M | 1,025.8M | −1.0% → −2.3% |
| 11u-64 / 11u-256 | +4.6% / +4.6% | 1,058.5M / 1,058.2M | +0.8% / +0.8% |
| 11wm (A3-b, distinct) | — | 1,053.7M | **+0.4%** |
| 11v dfold | +5.5% | 1,064.4M | +1.4% |
| 11w rxdesc armed | −20.2% | 809.9M | −22.9% (6th straight) |
| 11z null-mode | 1,306.3M @ 17.1 | **1,345.3M @ 17** | **the 17 cyc/span floor replicates** |

Verdicts (two draws in; the law wants three):
1. **The null reading is STABLE: 17 cyc/span on both draws.** The
   plumbing floor is real, small, and replicates — Route R stays dead;
   the worker program (T/endings/supply) owns ~170 cyc/span.
2. **A1 (siblings) is wobbling**: +6.6% then +0.6%. Draw 16's 11b
   (992.4) now looks noise-low (the first-arm position tax) — the
   honest interim read: siblings ≥ distinct on both draws, magnitude
   unclear, median pending the third draw. No flip (the law).
3. **A3-a and A3-b both faded on draw 17** (batch +0.8% both points;
   watermark +0.4% on distinct) — draw 16's +4.6% was likely position
   noise. BUT 11wm priced the watermark on DISTINCT, where the assist
   is dormant (242 chunks) — the R12-era reference shape is SIBLINGS,
   where the assist runs 294,693 chunks. The bisect re-aims: 11wm gets
   HFT_FABRIC_PLACE=siblings on the next push (the watermark question
   only exists where the assist is live).
4. dfold: +5.5% then +1.4% (median +3.5%, direction positive 2-of-2 on
   the ring path — the rxdesc-era neutrality does not transfer; the
   third draw decides whether 11v's case reopens on the healthy class).
5. The section-16 classic-CV breach on a healthy draw (span arm tight)
   is now the dominant shard-red class: the constraint is doing its
   job, the data publishes anyway, and no gate change is warranted
   without a roadmap mandate (R3 §3.1's reorder is in; CV relaxation
   was never on the table).

**Draw 18 (8573C healthy ×2 — a DOUBLE-HEADER, run 37270501728, Wave-1
shards 1 and 10; commit 1ed07cf — the re-aimed siblings-watermark bisect
aboard).** Both shards healthy 8573C: shard 1 kbench `fold512_r` 1t =
**34.77** GB/s (the healthiest host ever drawn — record-class), 2cpu_distinct
69.80, THP granted; shard 10 = **34.64** / 67.19. All arms BIT-EXACT
`0x881639cead506f25`, allocs=0; D1 M1-M17 all PASS on both. Shard 1 went
RED on the r8 pure-ingest gate (1.781B < 2.0B — a bimodal-RX-phase miss,
runs 1.25-2.87B with run-30 at 2.87B) AFTER the full battery ran; counted
per R2 §8 (the draw-17 precedent — constraint noise never blocks the push).

| arm | draw 16 | draw 17 | draw 18a | draw 18b | 4-draw median |
|---|---|---|---|---|---|
| 11b ring+distinct | 992.4M | 1,049.6M | **1,239.1M** | 1,142.9M | 1,096.2M |
| 11k ring+siblings | +6.6% | +0.6% | −0.6% | +3.2% | **+1.9% — wobble, NO FLIP** |
| 11u batch 64/256 | +4.6% | +0.8% | −0.1% | +0.8% | +0.8% — noise |
| 11v dfold | +5.5% | +1.4% | −0.0% | +1.5% | +1.5% — **record-class case CLOSED** |
| 11w rxdesc armed | −20.2% | −22.9% | −21.9% | −20.3% | **−21.1% (7th/8th straight)** |
| 11e prepatch off | −1.0% | −2.3% | −2.5% | −3.1% | −2.4% — armed confirmed 4/4 |
| 11wm wm8192 vs 11k | — | (distinct +0.4%) | **−0.4%** | **−0.7%** | **REFUTED under siblings** |
| 11z null-mode | 1,306.3M | 1,345.3M | 1,510.2M | 1,458.9M | floor replicates ×3/×4 |

Verdicts (the ≥3-draw law is satisfied on every open axis — **Phase 0
closes**):

1. **NEW FLEET BEST 11b: 1,239,070,083 (draw 18a) — ABOVE the standing
   record 1,234,801,472 (+0.35%)**, on the DEFAULT ring+distinct stack,
   BIT-EXACT, allocs=0, 5.00 s, crc_demand 34.26 GB/s. The P0-10
   record-class stage gate (≥1.23B) is MET. Ledger fact, not a claim —
   the claim class is pre-declared per E-1 before any campaign. The
   healthy-band median (1.096B) sits under the 1.15B stage-gate hope:
   kernel-correlated, as physics demands (the 30.4-30.6 band draws
   992-1,050M; the 34.6-34.8 band 1,143-1,239M).
2. **The fabric-efficiency invariant holds on the healthiest host ever**:
   34.26/69.80 = **49.1%** (18b: 31.60/67.19 = 47.0%) — the ~48-49%
   delivered/2-core-ceiling class is stable across the entire ring era.
3. **A1 CLOSED (no flip)**: siblings median +1.9%, sign-inconsistent
   (−0.6%…+6.6%) — distinct stays the default; 11k keeps pricing for
   free. Draw 16's +6.6% was the 11b position-noise-low, as suspected.
4. **A3-b REFUTED**: the R12-era deep-saturation assist watermark (8192)
   does NOT help under siblings (−0.4%/−0.7%, both draws) — the
   vectorized-watermark refutation extends to the placement where the
   assist actually runs (~300K chunks live). Do-not-do ledger entry #8
   amended. **A3-a CLOSED** (batch pacing = +0.8% median, noise). With
   the record-class gate met, the A3 regression question dissolves —
   the residue was draw-class, not code.
5. **dfold's record-class case CLOSED (C4 resolved)**: at kbench
   34.6-34.8 the row prices −0.0%/+1.5% — neutral-positive, inside the
   ±3.4% arm noise. No flip (default stays OFF); 11v remains the
   tripwire. The kernel program's open questions now route entirely
   through T-1 (`fold512_t`) and the I-1 floor rows.
6. **P0-7 CLOSED**: the armed prepatch default confirmed on the ring
   stack — unarmed loses 1.0-3.1% on all four healthy draws.
7. **rxdesc: −21.1% median over 8 consecutive draws** — C1 vindicated
   permanently; the ring default's dividend is one of the most
   replicated facts in the ledger.
8. **Front A**: 18b pure ingest **3,233,997,800 PASS** at 0.711 cyc/msg
   span-median (the best R17-era denominator; draws 16/17: 0.889/0.746;
   the 3.6246B record stands); 18a r8 FAIL 1.781B on a bimodal RX phase
   — the F-1 warm-start lever (the 5B program) is untouched by the
   draw-18 data and remains the Phase II centerpiece.
9. The null floor: busy-worker eval ≈ **17.6/18.1 cyc/span** on
   18a/18b (null sustained 1,510.2M/1,458.9M; margins +21.9%/+27.7% over
   11b) — the third and fourth replications of the instrument; Route R
   stays dead, the worker program owns the gap.

**The draw-18 push (this revision): Phase I instruments + the T-1 kill
test ship with the draw-19 fish.** kbench gains four 1t rows (all on the
packed corpus, no fabric changes, no CI-arm changes — they ride arm 11c):
`fold512_noend` (ending stubbed to a state sum — the ending-stack diet,
priced per draw), `fold512_pre` (the worker's ahead-of-cursor spray
shape; the roadmap's `fold512_nopre` resolved as the r-vs-pre pair — the
kbench baseline never carried a spray to turn off), `fold512_supply`
(the SAME reflect kernel over a ~14.3 MB L3-resident working set — each
draw's actual streaming ceiling for 1.4 KB bodies, the 2B supply gate's
gauge per D-1), and **`fold512_t` — the Route T kill test** (ROADMAP2
§5.2): the transposed-arena twin, `transpose_arena_slot` +
`fold_word_pairs_t` (the no-unpck loop, p5 census 6→4) +
`span_fold_eval_r_t` (the ending reads the ORIGINAL wire body — hazard
#1's ship-first variant), pinned by `t_transpose_arena_parity` (the
exhaustive differential battery) + `t_transpose_arena_public_api` (the
span-class battery). The sink equality with `fold512_r` prints on every
run — the structural proof. **Decision rule: ≥ +8% at 1t on a healthy
draw builds Route T; < +8% kills it and the kernel program ends with a
measurement.** Local preview on the Granite Rapids sandbox (27.56 GB/s
class — BELOW the healthy line, non-deciding): fold512_t 26.85 vs r
27.56 (−2.6%, the unpck deletion pays nothing on a latency-bound host —
exactly the census-vs-latency question the fleet must answer), sink
BIT-EXACT, all four rows live, 30/30 suites green, clippy clean, local
hydra smoke BIT-EXACT allocs=0. One `t_rxdesc_parity_matrix` flake
recorded in a contended first-run suite (passed alone + 3× after) —
consistent with the docs/25 §5.1 prepatch-race class: **I-7's third
strike; its priority rises ahead of any record-fishing push.**

**Draw 19 (8573C healthy, run 37276608228, Wave-1 shard 6; commit 8f2e277 —
the T-1/I-1 instruments' maiden draw).** kbench `fold512_r` 1t = **32.66**
GB/s, 2cpu_distinct 66.28, THP granted; 11b **1,217,168,189** BIT-EXACT
allocs=0 (crc_demand 33.66 GB/s → fabric efficiency 50.8% — the band's top
edge; the kernel-correlation picture holds: the 32.7-kbench class lands
between the 30.5 and 34.7 bands' numbers exactly as physics demands).

**THE T-1 FIRST HEALTHY-DRAW READING (the Route T kill test):**

```text
fold512_r        32.66 GB/s   (sink 0xbedb8ba779de450f)
fold512_t        28.61 GB/s   (sink 0xbedb8ba779de450f — BIT-EXACT)
                 => −12.4% — decisively under the ≥ +8% build bar,
                    and under zero. The pre-declared kill rule fires
                    on its first healthy draw.
fold512_supply   32.58  (−0.2% — L3 streaming from a 14.3 MB working
                        set is FREE at 1t on healthy draws: the D-1
                        supply gate clears with margin on this class)
fold512_pre      32.22  (−1.3% — the worker's spray shape is nearly
                        free at kernel level)
fold512_noend    31.38  (−3.9% — counterintuitive: the ending-stubbed
                        row SLOWER than the full kernel; flagged for
                        replication — suspect the state-sum consume
                        costs more than the vend ending's vector path
                        on this class, or row noise)
```

The local preview (−2.6% on the 27.56-class Granite Rapids sandbox) and
the fleet read (−12.4% on the 32.66-class 8573C) agree in direction:
**deleting the 2 vpunpck does not pay anywhere measured** — the fold step
is not p5-census-bound on the runner fleet; the transposed arena's second
memory stream (the wire-tail reads the ending still performs) and the
layout change cost more than the 2 shuffle uops save. Route T's expected
value collapses; per the law the row keeps riding every draw (zero cost)
and the formal refutation entry lands at ≥3 healthy draws — but the
build decision is already dead unless replication contradicts by >20
points. **Consequence (ROADMAP2 §5.2's own rule): the 2B program falls
to S+residual work with the ~1.6-1.8B honest ceiling** — and the
endings/supply/residual ladder (C-1/C-2, I-1 rows, Route S kill tests)
becomes the only kernel-adjacent path left.

**The battery died at 11n — the prepatch race's THIRD fleet strike.**
Five arms ran (11b 1,217.2M / 11e 1,171.3M −3.8% / 11k 1,178.8M −3.1% /
11l 1,221.5M +0.4% / 11m 1,183.6M −2.8%), then 11n's sustained phase
panicked: `hydra sustained pass count divergence: 505,884 vs 505,849`
(+35 messages) — the docs/25 §5.1 class (submission-side count divergence
in a sweep arm; the second strike was run 37122364077's 11j at −47,297).
The crash killed 11r/11s/11t/11u/11v/11w/11wm/11z — the incomplete-sweep
waste on a HEALTHY draw (E-3's warning, the draw-10 precedent). The same
family struck locally the same day (the contended-suite
`t_rxdesc_parity_matrix` flake). **I-7 (the prepatch-race hardening
round: chaos schedule × forced prepatch × high pass count, the
batch-parity repro pattern) is PROMOTED to the top code priority, ahead
of F-1** — the race now costs real draws, and a mid-record strike during
the 2B campaign would be the scarcest waste of all. Interim verdicts
from the partial sweep corroborate the closed axes: 11k −3.1% (A1's
no-flip, the wobble continues downward), 11e −3.8% (P0-7's 5th
consecutive negative).

**Draw 20 (8573C healthy RECORD-CLASS, run 37277372408, Wave-1 shard 5;
commit 865482e — the draw-20 fish; the battery ran CLEAN through every
arm — no race strike).** kbench `fold512_r` 1t = **34.93** GB/s
(the record class), 2cpu_distinct 69.73; 11b **1,233,118,283** BIT-EXACT
allocs=0; Front A pure ingest **3,256,577,062** PASS (2B gate) @ 0.734
cyc/msg. Full arm table: 11e −2.2% (P0-7's 6th straight — armed prepatch
confirmed again), 11k −1.4% (A1 stays closed), 11l +0.7% / 11m −2.1%
(tripwires quiet), 11n +0.3%, 11r +1.1% / 11s +0.9% / 11t +0.5% (the
kernel rollback triple, all within noise), 11u +0.3%/+0.5% (A3-a stays
closed), 11wm −1.8% (A3-b stays refuted), 11v +0.9% (dfold tripwire),
11w −18.8% (rxdesc, 9th consecutive), 11z null 1,491.3M (~18.7 cyc/span
— the floor replicates a 5th time).

**THE T-1 REPLICATION — ROUTE T'S BUILD DECISION IS DEAD (2/2 healthy
draws):**

```text
fold512_r        34.93 GB/s   (sink 0xbedb8ba779de450f)
fold512_t        30.29 GB/s   (sink 0xbedb8ba779de450f — BIT-EXACT)
                 => −13.3% (draw 19: −12.4%) — the ≥ +8% build bar is
                    missed by ~21 points on both healthy draws. The kill
                    is conclusive; the formal refutation needs the 3rd
                    draw per the law, but no build work happens.
fold512_supply   34.95  (+0.1% — L3 streaming FREE at 1t, 2/2 draws;
                        D-1's ≥28 GB/s gate clears with ~25% margin)
fold512_pre      34.50  (−1.2%)
fold512_noend    34.13  (−2.3% — draw 19's −3.9% replicates in
                        direction; the ending work is a 2-4% kernel-level
                        cost on this class — C-1's pipelining ceiling is
                        modest, as ROADMAP3 §4.3's own math warned)
```

The 2B program's shape is now fully priced: the kernel axis is dead
(Route T), supply is free (D-1), endings are a 2-4% lever (C-1/C-2), and
the null floor holds ~17-19 cyc/span. What remains is S+residual (B-1
wide-store, B-2/B-3 Route S kill tests) and the honest ~1.6-1.8B ceiling
— or the record-class fishing itself (draw 18a's 1.2391B stands as the
fleet best).

**Draw 21 (8573C healthy, run 37280895942, Wave-1 shard 9; commit 866860f
— the I-7 hardening stack's FIRST fleet draw; the battery ran CLEAN
through every arm — no race strike: the fix's target-silicon
validation).** kbench `fold512_r` 1t = **33.02** GB/s (healthy, near the
record band); 11b **1,162,414,834** BIT-EXACT allocs=0 (the
kernel-correlation law holds: 30.4→992-1,050M, 33.0→1,162M,
34.8→1,143-1,239M); Front A pure ingest **3,406,574,091** PASS (2B gate)
@ **0.675 cyc/msg — the best healthy-class denominator of the R17 era**
(the F-1 baseline). Full arm table: 11e −1.0% (armed prepatch confirmed,
7th straight), 11k +2.8% (A1 stays closed — single-draw wobble inside
the ±3.4% band), 11l +0.3% / 11m +0.4% (tripwires quiet), 11n +3.0% /
11r +3.1% / 11s +2.8% / 11t +2.7% (the kernel rollback arms all
positive-side noise on this strong host — the medians stand), 11u +1.4%
(A3-a stays closed), 11wm +2.9% (A3-b stays refuted), 11v +1.3% (dfold
tripwire), 11w −7.0% (rxdesc, 10th consecutive negative — the penalty
compresses on strong hosts; the median stays ~−20%), 11z null
**1,547,027,843** (+33.1% over 11b — the floor scales with host class).

**THE T-1 THIRD READING — ROUTE T'S REFUTATION IS FORMAL (3/3 healthy
draws, the kill rule's own ≥3-draw bar):**

```text
fold512_r        33.02 GB/s   (sink 0xbedb8ba779de450f)
fold512_t        28.91 GB/s   (sink 0xbedb8ba779de450f — BIT-EXACT)
                 => −12.5% (draw 19: −12.4%, draw 20: −13.3%)
                    The transposed-arena no-unpck fold NEVER approached
                    the ≥ +8% build bar on any healthy draw (missed by
                    ~20 points every time); the fold step is not
                    p5-census-bound on the fleet. Route T is CLOSED as
                    a build decision — the kernel program ends with a
                    measurement, exactly as ROADMAP2 §5.2's own rule
                    demanded. The 2B program is S+residual only.
fold512_supply   32.97  (−0.2% — L3 streaming FREE at 1t, 3/3 draws;
                        D-1's ≥28 GB/s gate clears conclusively)
fold512_noend    31.96  (−3.2% — the ending lever stays in its 2-4%
                        band, 3/3)
fold512_pre      32.54  (−1.5% — the prefetch spray is nearly free)
```

Draw 21 also validates I-7 on target silicon: the full battery ran
through every arm on the hardened stack with zero pass-count divergence
(the strike class that killed draw 19's back half and threatened every
record attempt). The race window is closed; the battery is protected.

**Draw 22 — THE F-1 VERDICT DRAW, A TRIPLE-HEADER (run 37284744327,
commit d7b86ad — the F-1 stack's first fleet draw; Wave-1 shards 2, 6,
7, ALL healthy 8573C: kbench `fold512_r` 1t = 34.03 / 30.72 / 33.81;
battery CLEAN through every arm on all three — I-7 holds through its
second fleet draw, zero divergence).** 11b per shard: **1,117,324,394 /
1,075,249,743 / 1,091,274,556** (median 1,091.3M — a mid-band draw, the
kernel-correlation law holds; draw 18a's 1.2391B fleet best stands).
Default Front A (musl 30-run medians): **3,219,220,537 @ 0.7145 /
2,543,776,321 @ 0.9042 / 3,292,976,899 @ 0.6985 cyc/msg** — all PASS
the 2B gate. Arm table (vs own-shard 11b): 11e −1.6%/−2.9%/−4.1%
(armed prepatch confirmed, 8th straight), 11k +0.5%/+1.9%/+0.5% (A1
stays closed), 11l −4.4%/−3.5%/−4.6% (tripwire), 11m −2.3%/−1.5%/−1.7%,
11n +0.0%/+0.3%/+1.1%, 11r +0.7%/+1.5%/+2.8%, 11s +0.5%/+0.7%/+0.4%,
11t +0.8%/+0.5%/+0.3%, 11u −0.1%/−0.0%/+0.6% (A3-a stays closed), 11wm
+1.3%/+2.0%/+0.4% (A3-b stays refuted), 11v −1.0%/+0.7%/+0.3% (dfold
tripwire), 11w −6.7%/−11.3%/−3.3% (rxdesc, 11th straight; rx_fixes
128/0/256 — the check-and-fix law working), 11z null 1,461.1M /
1,371.2M / 1,489.0M (+30.8%/+27.6%/+36.4% over 11b — the floor scales
with host class).

**THE 11wn FIRST READING — THE WARM FRONT A PREMIUM IS DEEPLY NEGATIVE
ON 3/3 INDEPENDENT HEALTHY HOSTS; THE KILL RULE FIRES IN SUBSTANCE AND
THE DECISION GATE ROUTES TO F-2:**

```text
                      default Front A      warm Front A       premium
shard 2 (kbench 34.03)  3,219,220,537       2,130,581,283     −33.8%
                       @ 0.7145 cyc/msg    @ 1.08  cyc/msg
shard 6 (kbench 30.72)  2,543,776,321       1,967,855,253     −22.6%
                       @ 0.9042 cyc/msg    @ 1.17  cyc/msg
shard 7 (kbench 33.81)  3,292,976,899       1,706,781,250     −48.2%
                       @ 0.6985 cyc/msg    @ 1.35  cyc/msg
                       MEDIAN PREMIUM: −33.8%   (sign-unanimous 3/3;
                       7-14x beyond the ±3.4% same-draw noise floor and
                       beyond the ±10% cross-host band — noise cannot
                       explain it; the weakest host regresses LEAST, the
                       strongest MOST — the penalty scales with the rate
                       the W stream must sustain)
warm sustained          949.3M / 937.2M / 922.0M  vs default
                       1,117.3M / 1,075.2M / 1,091.3M
                       = −15.0% / −12.9% / −15.5% (warm FAILS even the
                       1B R8 gate on 3/3 hosts)
zero-fix law            fixes=21,996 (the pass-1 fill exactly), 
                       uncovered=0, last_pass_fixes=0 — HOLDS on 3/3
                       hosts: the mechanism is CORRECT on fleet silicon,
                       BIT-EXACT, allocs=0, no persistent fixes
RX prod_ms (shard 2)    default 3872.4 ms -> warm 3484.5 ms (−10.0%):
                       the RX's OWN production time FELL — the removed
                       slot-push/build µops are real — but total
                       throughput fell with it: the W compare stream
                       (~1.6 MB/pass cycling L2 at up to ~6,300 passes/s
                       ≈ 10+ GB/s of added read traffic) externalizes
                       its cost onto the consumer walk and the fold
                       supply. The risk register's prong (1) fired
                       EXACTLY as written: the W stream binds, C8's
                       working-set warning class materialized.
```

**The F-1 verdict:** the warm start is REFUTED as the Front A 5B lever
on the fleet — not because the check-and-fix law fails (it holds
perfectly: zero fixes on pass ≥ 2, 3/3 hosts) but because the warm
array's read stream is a new working set the span path cannot afford.
The pre-declared kill rule (Front A < +15% healthy, ≥3 draws) opens its
tally at draw 22 with 3/3 independent healthy hosts sign-unanimous at
−22.6…−48.2%; the magnitude makes the remaining replication a formality
(any future 11wn reading extends it — the arm stays aboard as the
pricing instrument). `HFT_RXWARM` stays default-OFF exactly as shipped;
the record path is untouched. The decision gate per the CHECKLIST
routes to **F-2 (publish-by-reference master array)** — F-1's warm
median 2.13B lands far short of the 4.6B build bar, and the roadmap's
own residual (the packed 48-byte warm record, half the stream) cannot
recover a −34% deficit. The v2 note is recorded as closed-with-F-1: the
fundamental added-read-stream cost is the failure mode, not the record
width.

**Draw 23 — THE 11rb FIRST READING, ON TWO NOISY HOSTS (run 37292866128,
commit 616584c — the F-2 stack's first fleet draw; Wave-1 shards 2/6
uploaded, BOTH 8370C: kbench `fold512_r` 1t = 26.96/24.78, both
SECTION16_DISCARD'd at 27.26/25.43 — the R12 noisy-host class, 0/2
healthy readings; battery CLEAN on both, I-7 holds through its third
fleet draw, every gate BIT-EXACT, allocs=0, rx_fixes=0).** The
consolidated scoreboard medians for this draw (sustained ~958M, default
Front A ~2.0-2.2B) sit far under draw 22's healthy trio — that is the
HOST LOTTERY, not a code regression: an 8370C-only draw prices the
whole battery at the weak-host class, exactly as the draw-19-era host
drift taught (11b 935.7M/958.4M is the class's own band).

**THE 11rb FIRST READING — THE PREVIEW IS MASSIVE ON BOTH HOSTS:**

```text
                      default Front A      rxbuild Front A     premium
shard 2 (kbench 26.96)  2,224,675,764       4,548,510,952     +104.5%
                       @ 1.26 cyc/msg     @ 0.61 cyc/msg
shard 6 (kbench 24.78)  2,040,363,664       4,454,621,508     +118.3%
                       @ 1.37 cyc/msg     @ 0.63 cyc/msg
toolchain bound         the battery is musl, the arm is gnu — but the
                       classic arm runs in BOTH (rxbuild does not touch
                       it): gnu ≈ musl within ~2% on both shards → the
                       premium is not a toolchain artifact
the headline            0.61-0.63 cyc/msg on the WEAK host class BEATS
                       the healthy class's best-ever default denominator
                       (0.675, draw 21) — the weak host has the larger
                       RX-share to remove, and removing it more than
                       halves the whole denominator (Amdahl-consistent)
sustained (armed)       954.2M / 976.5M vs own-shard 11b
                       935.7M / 958.4M = +2.0% / +1.9% — NO penalty on
                       the sustained shape (the RX is parallel on its
                       own core; the small gain is the patch-budget
                       headroom coming back)
patch law               span patches=153,972 = 7 × 21,996,
                       last_pass=21,996=frames on BOTH — HOLDS; sustained
                       last_pass 21,996/21,932 (shard 6's −64 = the 5s
                       window cutting the final pass mid-flight — the
                       documented flush-lag artifact class, not a
                       violation)
RX prod_ms (span)       0.8 / 0.7 ms — the RX's measured production cost
                       on the span arm is now LITERALLY rounding error;
                       the mechanism works exactly as designed
verdicts                BIT-EXACT 0x881639cead506f25, allocs=0,
                       rx_fixes=0, R8 pure-ingest PASS at 4.55B/4.45B —
                       the R16 5B gate is in sight on the WEAK class
```

**The draw-23 tally: 0/3 healthy readings — the decision CANNOT open on
this draw.** Both previews are positive at +104.5/+118.3% (the bar is
+15%), sign-unanimous, mechanism-verified, and the local A/B's
+52…+62% under-prices what the fleet shows — but the protocol is the
protocol: the default-flip needs ≥ +15% on ≥ 3 healthy draws, and this
draw had none. The fish continues. The 8370C preview also sharpens the
transfer question: the healthy host's default denominator (0.675) has a
SMALLER RX-share than the weak host's (1.26-1.37), so the healthy
premium will be smaller than +104% — but the ROADMAP1 §6-I1 budget
(Front A 4.8-5.5B on 8573C ≈ +40…+60% over 3.4B) needs only the
mechanism to transfer, not the weak-host multiple.

**Draw 24 — THE 11rb HEALTHY READINGS: 2/2 CLEAR THE BAR (run
37295181423, commit 846284b — the draw-24 fish, battery unchanged from
616584c; Wave-1 shards 6/9 uploaded, BOTH healthy 8573C: kbench
`fold512_r` 1t = 34.31/30.20; battery CLEAN on both — I-7 holds
through its fourth fleet draw).** 11b per shard: **1,194,428,933 /
995,443,342** (the kernel-correlation law holds — the stronger host
prices the whole battery higher; draw 18a's 1.2391B fleet best stands
on the default path). T-1 4th/5th readings: fold512_t 29.88 vs r 34.31
= −12.9% / 26.19 vs 30.20 = −13.2% (Route T stays formally dead).

**THE 11rb HEALTHY READINGS — BOTH CLEAR THE +15% BAR:**

```text
                      default Front A      rxbuild Front A     premium
shard 6 (kbench 34.31)  3,446,213,482       4,603,481,853     +33.6%
                       @ 0.6674 cyc/msg   @ 0.50 cyc/msg
shard 9 (kbench 30.20)  3,099,204,136       3,767,261,217     +21.6%
                       @ 0.7421 cyc/msg   @ 0.61 cyc/msg
sustained (armed)       1,246,447,190 / 1,048,698,948 vs own-shard
                       11b 1,194,428,933 / 995,443,342
                       = +4.4% / +5.3% — shard 6's ARMED rate EXCEEDS
                       the standing fleet-best 11b (1,239,070,083);
                       shard 9 crosses the 1B R8 gate. F-2 is not just
                       a Front A lever — it is a SUSTAINED lever too on
                       healthy silicon (the freed ~1MB entry footprint
                       + the patch-budget headroom coming back to the
                       fold supply)
patch law               span last_pass=frames on both (the pinned law);
                       sustained shard 9 last_pass 22,124 vs frames
                       21,996 (+128 = the rotating-session re-patch
                       count — entries re-patched at session flips,
                       parity BIT-EXACT, the diagnostic counts patches
                       not unique entries)
RX span prod_ms         0.5 / 0.6 ms — sub-millisecond again
the transfer question   ANSWERED: the healthy premium (+21.6…+33.6%) is
                       smaller than the weak-host +104% exactly as
                       Amdahl predicts (the healthy denominator carries
                       a smaller RX-share), yet far above the +15% bar;
                       shard 6's 4.603B reaches 92% of the 5B target
                       and the ROADMAP1 §6-I1 budget band (4.8-5.5B) is
                       in arm's reach with the residual levers
arm context             11e −2.3/−2.5% (9th straight), 11k −2.8/+0.9%,
                       11w −9.7/−4.8% (12th straight — the ring flip
                       stays permanent), 11z null +26.8/+33.6%,
                       11wn −17.0/−13.7% (F-1's instrument stays dead —
                       the refutation replicates)
```

**The decision tally: 2/3 healthy readings at ≥ +15% (+33.6%, +21.6%),
sign-unanimous, plus two noisy-host previews at +104.5/+118.3%.** One
more healthy draw at ≥ +15% triggers the default-flip protocol; a miss
keeps fishing; persistent misses kill. The fish continues — and with
the sustained side now pricing POSITIVE on healthy silicon, the flip
would carry the 2B program upward with it (the armed sustained already
exceeds the fleet-best 11b on a record-class host).

## 10. I-7 — the prepatch-race hardening round (SHIPPED — this revision)

The three-strike class (R9's +39, R12's 11j −47,297, draw 19's +35 on a
healthy draw that killed 8 arms) is root-caused, fixed, and pinned.

**The mechanism (the full chain, each link measured or read from the
code):**

1. Every pass ends with the RX publishing an empty EOS-marker
   publication; the consumer frees it inside `next_batch` before
   returning false. The marker's event-ring slot records the sentinel
   `usize::MAX` — "the whole pass is consumed — patch everything."
2. At the NEXT pass's first publication(s), the consumer cannot have
   freed anything yet (it is still between passes), so the incremental
   prepatch's freed-frontier still maps to that marker — and reads the
   sentinel. It then bakes the NEXT pass's session into the CURRENT
   pass's unconsumed head regions (up to 64 sites per publication, and
   deeper while the consumer lags — the tardiness is the timing
   component that makes the fleet flake rare).
3. Entries built by LATER publications read the over-patched blob and
   carry the foreign session words; the live frame bytes are equally
   wrong for any cold-path re-parse — the documented "the prepatch can
   never be observed mid-flight" contract (render.rs `patch_range`) is
   violated.
4. The consumer's steady scan cold-paths on the session mismatch;
   `session_dispatch` opens a session boundary and sets `State::Init`.
5. The anchor law (`ingest_auto`: `State::Init → self.w = first`,
   UNCONDITIONALLY, before the duplicate check) re-anchors the watermark
   DOWN to the first post-flip frame's `first_seq`. If that frame is a
   duplicate of an already-emitted packet (its primary rode an earlier
   publication), the dup re-emits its messages: **the pass count lands
   at 505,849 + N where N is one packet's message payload — +35 (draw
   19), +39 (R9).** The adjacent-pair test schedules never straddle a
   publication boundary with an early-gated region, which is exactly why
   the class survived every existing suite for 20 rounds.

**The fix (nf-transport/pipeline.rs, the RX thread only):**

* `pass_start_turn` — the current pass's first publication turn, set at
  the auto-advance. The incremental prepatch now SKIPS any freed
  frontier below it: previous-pass turns (including the marker whose
  sentinel the advance's synchronous bake already consumed) carry no
  valid event index for the current pass. The advance's
  `reset_prepatched` tail remains the catch-all, so no legitimate bake
  work is lost — the marker-sentinel window simply closes.
* The overwrite guard is corrected to `ring_covered − t ≥ TOFF_RING`
  (was `>`): at exactly TOFF_RING distance the aliased slot already
  holds the just-recorded turn's own value, and reading it would take a
  LARGER event end as the frontier — the same over-patch class. (Dead
  at NBUF=16 today — the runahead caps the lag at 16 — but it is the
  identical boundary bug and costs nothing to close.)
* No new env, no default flip: the fix lives inside the prepatch, whose
  whole-mechanism rollback (`HFT_PREPATCH=0`, arm 11e) already exists.
  Side effect: the armed path is slightly CHEAPER — the old code
  double-patched the head every pass (64+ sites with the wrong session,
  then re-patched them correctly at the next advance) and forced a
  mid-pass cold path; both are gone.

**The repro harness (the I-7 spec's pattern — chaos × forced prepatch ×
high pass count — plus a deterministic pin):**

* `t_prepatch_marker_sentinel_head_invariant` (nf-transport): an
  ENGINEERED straddle schedule — phase-separated feeds so the 64
  earliest-gated regions each have their primary in publication #1 and
  their duplicate in publication #2 — plus a tardy consumer parked
  through the RX's full runahead. Asserts every entry of every batch
  carries ITS pass's session, in the frame bytes AND the inline words.
  **RED on the unfixed code (pass 1, batch 1: bytes read `…02` — the
  next-NEXT session — where `…01` was baked), GREEN with the fix.**
* `t_i7_prepatch_chaos_sustained_soak` (nf-testkit): the sustained
  auto-advance shape at multi-publication scale on a delayed dual-feed
  (dups straddle publication boundaries organically), 160 passes under
  deterministic LCG chaos — tardy starts (~35% of passes), mid-pass
  stalls, periodic mid-pass abandons — asserting the per-pass reference
  tuple (count, hash, msg_hash) and the per-entry session invariant.
  **RED on the unfixed code (same foreign-session evidence, first
  chaos pass), GREEN with the fix (1.3 s).**

**Validation:** 30/30 workspace suites green; clippy `-D warnings`
clean; the historically-flaky `t_rxdesc_parity_matrix` 3/3 green; local
hydra smoke BIT-EXACT `0x881639cead506f25` allocs=0 with ~3.3k sustained
passes through the fixed window. The fleet re-prices the armed path on
the next draw via 11e (and the whole battery is now protected from the
strike that killed draw 19's back half).

## 11. F-1 — the RX frame-entry warm start (SHIPPED default-off; REFUTED
AS THE FRONT A LEVER AT DRAW 22 — the §9 draw-22 entry: warm premium
−22.6/−33.8/−48.2% on 3/3 healthy hosts, the W stream binds; `HFT_RXWARM`
stays OFF, arm 11wn stays aboard as the pricing instrument; CHECKLIST
F-1 / ROADMAP2 §6.1)

The Front A 5B lever, shipped as specified: `HFT_RXWARM=1` arms it,
unset/0 is the rollback (the classic path verbatim — the default
submission path is untouched until the ≥3-draw evidence lands). Arm
**11wn** prices it per draw: (a) the full sustained soak (bit-parity
asserts + ALLOC_DELTA=0 through the warm path) and (b) hft_bench's span
arm at 5 runs — the warm-vs-default `span_rate` delta on the same draw
is the lever's price.

**The design (the rxdesc check-and-fix law, applied where it wins):**

* **W** — a frame-indexed warm array of the exact `FrameEntry` payload
  (bytes ptr/len, blocks ptr/len, feed, memo, first_seq, sess words,
  elig byte), sized to the schedule's exact per-pass frame count
  (`rendered_frame_count()` — construction-fixed, allocated at RX
  thread start, outside every measured window; the rxdesc arrays'
  pattern). RX-thread-private.
* **The warm walk** — `poll_impl::<WARM>`, a const-generic
  single-sourcing of `poll_clamped`'s ENTIRE pacing skeleton (preamble,
  event walk, tombstone skip, session reads, DLP prefetches); only the
  per-frame EMIT differs. Classic instantiation: the pre-F-1 codegen
  (the record path is untouched at the machine-code level). Warm
  instantiation: the entry is derived IN REGISTERS from the walk's own
  live facts (the frame meta + the frame line's session words — both
  loaded by the walk anyway), COMPARED against W over all ten payload
  fields, FIXED in place on divergence, and the VERIFIED entry is
  stored straight into the mailbox window. The scratch slot push
  (~6 stores/frame, poll side) and the accumulate-loop build (~9 slot
  loads + slice re-derivation + elig chain + construct, build side) are
  REPLACED — never paralleled. Zero added store traffic: the R12b
  sidecar's exact failure mode (added parallel SoA stores collapsing
  Front A 41%) is designed out; nothing new crosses a cache line on the
  steady path.
* **The check IS the correctness** — no memoization: every entry is
  re-derived from the live blob/meta state every pass, and the compare
  proves the remembered payload against that derivation before it is
  published. A divergent schedule (or a blob region that somehow
  escaped a bake) self-corrects via the fix path and is COUNTED
  (`rx_warm_fixes`) — never silently trusted.
* **The pass-boundary rewrite** — at every bake point (the CMD_RESET
  serve, the EOS-park reset serve, the auto-advance) the warm index
  restarts and W's session-derived fields (sess words + the elig byte's
  bit 7) are rewritten from the fresh template. After every bake the
  blob holds the new session everywhere (the advance's
  `reset_prepatched` synchronous tail is the catch-all), so the rewrite
  keeps the steady-state compare clean — and the per-frame check then
  PROVES the assumption. Telemetry windows close at the EOS marker
  (forced — a zero-fix pass must overwrite the previous reading) and at
  the reset serves (conditional — an abandoned partial records its
  count; a serve after a drained EOS preserves it).
* **Telemetry** — `RXWARM_DIAGNOSTIC {label}: enabled={} fixes={}
  uncovered={} last_pass_fixes={}` on every run (diag_summary), plus
  `rx_warm_stats()` for the tests. The steady-state law: fixes == 0 on
  pass n ≥ 2. **Kill rule (CHECKLIST F-1):** `rx_warm_fixes > 0`
  persistent, or Front A < +15% healthy — decided on ≥3 draws per the
  R9c→R9d law, never one.

**The pins (all RED-on-typo-class discipline, all green):**

* `t_rxwarm_parity_vs_classic` — warm vs classic side by side over
  BOTH pacing modes (coalesce 1 and 128), 4 passes of ROTATING
  sessions, every entry of every batch compared over the full payload
  (content compare — the two instances own separate blobs); asserts
  steady-state fixes == 0 from pass 2 and the pass-1 fill count.
* `t_rxwarm_mid_pass_abandon` — the abandoned-pass shape (partial W
  state; the next pass still verifies clean).
* `t_rxwarm_unarmed_constant_session` — hft_bench's span-arm shape
  (plain reset, one session; steady state zero-fix).
* `t_f1_rxwarm_chaos_sustained_soak` (nf-testkit) — the I-7 chaos
  program verbatim (delayed dual-feed, ~35% tardy starts, mid-pass
  stalls, periodic abandons, 40 passes) with the warm start ARMED:
  per-pass tuple parity against the classic legs + the per-entry
  session law + the steady-state zero-fix law from pass 2.

**Local validation** (Granite Rapids sandbox — non-deciding per the
house law, recorded for the ledger): 30/30 workspace suites green,
clippy `-D warnings` clean, fmt clean. Classic default sustained:
BIT-EXACT `0x881639cead506f25`, allocs=0, `RXWARM_DIAGNOSTIC
enabled=false` (the record path untouched). Warm-armed sustained:
BIT-EXACT `0x881639cead506f25`, allocs=0, **fixes=21,996 (exactly the
per-pass frame count — the pass-1 fill), last_pass_fixes=0 through
2,656 rotating-session passes, uncovered=0** — the steady-state law
holds on silicon. Local Front A A/B: classic median 1.599B vs warm
1.463B with ±20% run-to-run variance (warm's best run 1.804B — above
every classic run) — the sandbox is latency-bound with the wrong core
count and placement; W's ~1.6 MB L2-cycling stream costs more than the
removed µops THERE, exactly the profile the CI's 8573C (2 MB L2/core,
RX ~27% idle in the distinct placement) is built to absorb. The fleet
decides.

**The honest risk register:** (1) the W stream is new working set — C8's
warning class; if 11wn prices the warm arm < +15% with the RX's prod_ms
NOT falling, the W stream is binding and the v2 is the packed 48-byte
warm record (half the stream, +4 unpack ops) — documented, not built;
(2) the field-wise compare is ~10 compares, not the roadmap's idealized
2-4 µops raw 64-byte compare (a raw compare over FrameEntry's padding
holes is unsound; the packed-record v2 enables it); (3) the pass-1 fill
is ~22 K fixes on the first pass — one-time, untimed warmup territory,
recorded in the telemetry. Expected per ROADMAP2 §6.1: RX 0.30-0.35 →
0.05-0.10 cyc/msg, Front A 4.6-6.4B on 8573C-class draws.

## 12. F-2 — the publish-by-reference master array (SHIPPED default-off;
CHECKLIST F-2 / ROADMAP1 §6-I1 "Lever B rxbuild" — fired by draw 22's
F-1 refutation)

The Front A 5B lever, round 2 — and this one removes the RX's per-frame
work instead of re-deriving it. `HFT_RXBUILD=1` arms it; unset/0 is the
rollback (the classic path verbatim — `poll_impl::<0>` is the pre-F-1
codegen and the record submission is untouched). If both `HFT_RXWARM`
and `HFT_RXBUILD` are set, rxbuild wins (the warm start is refuted as a
Front A lever anyway; 11wn stays aboard as its pricing instrument).

**The design (ROADMAP1 §6-I1, shipped verbatim):**

1. **Build once per render**: the master `FrameEntry` array is built at
   CONSTRUCTION (`render::build_frame_master` — outside every measured
   window, `ALLOC_DELTA=0`) from the freshly-baked blob: event-ordered
   over the pass's emitted frames, tombstone-free (the consumer's walk
   needs no empty-slot checks), `bytes`/`blocks`/`memo`/`first_seq`/
   `feed` construction-frozen, only `sess_lo`/`sess_hi`/`elig` are
   per-pass state. Sizing is schedule-exact (`rendered_frame_count()`;
   ~22K entries ≈ 1.4 MB for the mini tape).
2. **Per-turn slice publish**: the RX's poll walk becomes COUNT-ONLY
   (`poll_impl::<2>` — the third const instantiation of the shared
   pacing skeleton; no frame line read, no session reads, no slot push,
   no entry build — the DLP prefetch stays). The publication carries
   `(rx_start, len)` master-slice bounds + the clock: mailbox traffic
   collapses from ~64 KB/batch to ~16 B/turn, and the EntryBuf's 1 MB
   entry footprint disappears (C8's working-set law, net-negative).
3. **Session patching, prepatch-extended**: `master_patch_range` is the
   blob prepatch's twin — the SAME consumed-event frontier (the I-7
   `pass_start_turn` floor and the TOFF_RING overwrite guard bound BOTH
   patchers), the master's own cursor `mpp_idx`, the same R9c budget
   pacing (64 entries/step post-publication, 1024 in the advance drain,
   the synchronous tail at the advance = `reset_prepatched`'s twin, the
   full rewrite at the blocking reset serves). The per-entry patch
   writes the new session's compare words and re-derives the elig bit
   from the entry's own static fields (the `warm_rewrite_session`
   formula). Non-patchable entries (HB/EOS and second-session frames
   under `session_split`) never change — the patchable flag mirrors the
   constructor's rule exactly.
4. **The consumer walks the master directly** (`entries()` returns
   `master[rx_start..rx_start+len]` in rxbuild mode) — the steady ladder
   is untouched, no redesign (the vector ladder stays refuted).
5. **Ordering**: the master is shared read-mostly state in the Mailbox
   (the rxdesc records pattern — RX writes, consumer reads, the
   filled[] Release/Acquire pair orders every patch before any
   publication whose read could observe it; the patch frontier
   guarantees no in-flight entry is ever touched).

**PINS** (the F-1 discipline, mirrored): `t_rxbuild_parity_vs_classic`
(both pacing modes, 4 rotating-session passes, every entry compared over
all ten fields against a side-by-side classic transport, plus the patch
law: cumulative patches = whole multiples of the patchable frame count),
`t_rxbuild_mid_pass_abandon` (the abandoned partial's patch state — the
reset serve's full rewrite is the catch-all), `t_rxbuild_constant_session`
(the unarmed span-arm shape), and `t_f2_rxbuild_chaos_sustained_soak`
(the I-7 chaos program verbatim with the master armed: per-entry session
law in the published bytes, per-pass tuple parity vs the classic legs,
and the patch law — deterministic 2,462 × 40 across repeats). The I-7
pins (the engineered-straddle sentinel pin + the prepatch chaos soak)
run unchanged on top of the shared-frontier extension.

**Local validation** (Granite Rapids sandbox — non-deciding, recorded):
30/30 workspace suites green (156 tests: 23 nf-transport incl. the three
new pins, 65 nf-testkit incl. the new soak), clippy `-D warnings` clean.
Classic default sustained: BIT-EXACT `0x881639cead506f25`, allocs=0,
`RXBUILD_DIAGNOSTIC enabled=false patches=0` (the record path
untouched). RXBUILD sustained: BIT-EXACT `0x881639cead506f25`, allocs=0,
patches = 21,996 × boundaries + endpoint partials (the flush-lag
artifact — the visible cumulative is flush-aligned at markers; the soak
pins the exact law). **Local Front A A/B: classic 1.745B/1.538B @
1.83/2.08 cyc/msg vs rxbuild 2.825B/2.336B @ 1.13/1.37 — +62%/+52%, the
RX build removal is worth ~0.70 cyc/msg on the walk-shaped sandbox**
(where F-1's local A/B was ambiguous-to-negative — the direction agrees
with the mechanism this time). Local sustained −10% (the 1-worker
latency-bound shape taxes the patch traffic; the fleet's distinct
placement + the RX idle headroom is the real test — 11rb prices it per
draw and the default stays OFF until the ≥3-draw evidence lands).

**The honest risk register**: (1) the master is a new ~1.4 MB shared
structure — but it REPLACES the mailbox's ~1 MB entry buffers, and its
per-pass traffic is strictly smaller (17 B/entry patches vs full-entry
builds, both off the critical path via the prepatch budget); (2) the
patch RFO at ~9,900 passes/s (the 5B rate) doubles the bake's line
traffic (blob + master) — the prepatch's incremental pacing was built
for exactly this shape, and the sustained arm prices the residue per
draw; (3) the flush-lag telemetry (the visible cumulative vs the
thread-local count) is documented — the soak pins the law where the
structure is clean. Expected per ROADMAP1 §6-I1's budget table: RX
0.20-0.30 → ~0.03 cyc/msg, Front A 4.8-5.5B on 8573C-class draws.
Kill/flip rule per the CHECKLIST: ≥ +15% healthy Front A over ≥ 3 draws
→ the default-flip protocol; short of that → the kill.

**FIRST FLEET READING (draw 23, §9's entry): the preview is massive on
2/2 noisy 8370C hosts — +104.5%/+118.3% Front A (4.55B/4.45B @
0.61/0.63 cyc/msg), sustained +2.0%/+1.9% with the lever armed, patch
law holding, RX span prod_ms at 0.8/0.7 ms, BIT-EXACT, allocs=0.** The
decision tally opens only on healthy draws — and **draw 24 opened it:
2/2 healthy 8573C readings CLEAR the bar (+33.6% on kbench-34.31,
+21.6% on kbench-30.20; the armed sustained +4.4%/+5.3%, shard 6's
armed 1,246,447,190 EXCEEDS the standing fleet-best 11b)**. Tally
2/3 — one more healthy draw at ≥ +15% triggers the default-flip
protocol.
