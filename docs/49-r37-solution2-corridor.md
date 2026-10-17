# 49 — Run 900 / R37: The Blelloch–Galois Tree Fold + The Two-Lane Elision Unlock (solution2.md Part I + II)

The 150B–250B corridor's first landing on `feat/run900-r37-solution2-corridor`
(PR #22), per `docs/solution2.md`'s ordered checklist (§8 items 2–3 + the
Gate 10–12 unlock). The R36 champion's 61.21B wall decomposes into three
theorem-shaped obstructions (§1.3); this run removes the first outright and
the second's blocker:

* **B1 — the serial fold barrier** (322 dependent applies ≈ 2.08 µs/pass,
  the only cross-pass loop-carried dependency — the 65B–70B ceiling):
  DELETED by the tree.
* **B2 — the two-lane coupling** (the R36-E single-lane scope gate, the
  frontier deadlock class docs/48 §1 scoped out): ROOT-CAUSED and fixed
  locally — the elision now arms for both lanes.

## §1 The tree fold (Theorems 19–22, 25 — both rings)

**The law.** Tape order constrains the LEAF sequence of the pass fold's
evaluation, never its bracketing (Theorem 19). The serial left spine
`S_p = S_{p-1} ⊗ R_p` was the unique worst bracketing (depth K = 322); the
Blelloch up-sweep evaluates the same composite at depth ⌈log₂ K⌉ = 9
(Theorem 20(a), Lemma 6), bit-exactly in every coordinate (Theorem 22 —
shape, width, and schedule invariance).

**The engine landing — the fold journal (hydra.rs).** The composite drain's
per-record apply `h ← rotl(h, 13k) ^ B` is replaced by a journal push: the
pass's records accumulate as `(k, B)` leaves (SoA columns: k / B / end, the
`end` the boundary partition key — records never straddle a pending pass
boundary). The boundary snapshot (`complete_boundaries`) flushes: the
`sink::tree_fold_segments` up-sweep over the prefix with `end ≤ boundary`
(the tree over the pass's ~194 leaves) + ONE `fold_compose_apply` hop.
Leaves past the boundary (already drained into the next pass, structurally
empty in the steady law) stay journaled — the composite is h-independent,
so the SPAN_SEED reseed re-bases them exactly. Capacity (512 leaves, 6 KB,
L1-resident per Corollary 19.1) is a performance dial: overflow flushes
early — exact at ANY point by associativity (Theorem 19 remark (ii), the
heterogeneous-shape license). `finish()` flushes the tail (the burst
shape's direct `hash` reads). `HFT_TREE_FOLD=0` restores the serial chain
verbatim (arm 11r37r's attribution).

**The ring polymorphism (Theorem 28, Appendix C).** The engine ring is the
ROTATION monoid — `(k_a, B_a) ⨂ (k_b, B_b) = (k_a+k_b, rotl(B_a, 13k_b) ^
B_b)` — the same semidirect law as the SPANHASH records with the acting
monoid (13k mod 64, rotl) for (x^W, CLMUL). One up-sweep schedule, two
rings, one associativity proof each. The SPANHASH twin lands in spanfold:

* `tree_fold_p64` (§B.1 verbatim) + `tree_fold_p64_avx512` — the vector
  schedule (Theorem 21): every level's independent composes run 4-pair
  batches through `mulmod4x4` (the [S1'] §2.6 kernel,
  results-kept-separate form: 2 batched calls per 4 composes, the integer
  coordinates riding free), scalar tails overlapping the next level.
* `tree_fold_two_level` (§2.7, Corollary 19.1) — runs of ≤ 512 records →
  sub-composites → the pass tree (the 5,122-record maximal variant's L1
  residency law; the u16→u32 degree escape at the level hop, pitfall 7's
  fence).
* `epoch_merge_p64` (§B.2, Theorem 25) — `S ⨂ X_first ⨂ X_second`, two
  composes, static tape order, no negotiation (Part II's kernel surface).

**The port's own bug ledger** (each pinned by a cell): the `mulmod4x4`
result store wrote 64 B into a 4-word stack array (stack smash → `c: 0`
divergence) — caught by `t_r37_tree_vector_dual_path` at K=8, fixed with
the 8-word buffer + even-word extraction. The dual-path cell exists
precisely for this class (the R34 discipline: never assume the vector path).

## §2 The two-lane unlock (Gates 10–12) — the stale-ready fix

The R36-E ship scoped the elision to single-lane: "the two-lane replay
additionally couples to the window publication cadence (the ready gate, the
self-serve pre-publish, the pace spin's fold dependency — the frontier
deadlock class the local slice cannot reproduce)" (docs/48 §1). This run's
local slice DID reproduce it — twice (fold stuck at a pass boundary in lane
1; and at a mid-pass chunk in lane 0), and the debug instrumentation
(allocation-free OnceLock gates — `env::var` in a window path costs
ALLOC_DELTA) root-caused it exactly:

**The deadlock cycle.** The multi-lane eval gap — this lane's cursor sits
BELOW the global publication frontier (the other lane's chunks fill the
gap; with one lane the cursor tracks the frontier exactly) — means Strand
A's refresh condition (`eval >= ready`) never fires on the replay path.
The replay's frontier break (`end > ready`) exits through the liveness
publish's `continue 'batch`, which loops to the batch top WITHOUT
reloading the publication cursor: the worker spins against a STALE local
`ready` while the register already holds the window's SS pre-publication.
The cycle closes through the main thread: its pace gate (pending >
RX_PACE=8192) blocks the submission → the next window's record never
publishes → the fold cannot drain past the boundary → pending stays high.
Circular wait, four parties, no single owner.

**The fix (hydra.rs, the batch top):** while the replay is armed and
Strand A itself won't refresh (`eval < ready`), ONE Acquire of the
publication cursor per chunk-iteration — adopt the register's value and
re-probe the boundary caches (Strand B's refresh). The single-lane shape
never pays the load's branch (its eval tracks the ready). With the fix the
two-worker shape runs green end-to-end locally: BIT-EXACT, allocs=0, the
elision engaged (replayed_passes ≈ 2× passes per worker, live = the
recording orbits, fallbacks=0), the tree engaged (flushes ≈ boundaries,
live_len=0 at rest), every tuple assert.

**The static-order reading (Theorem 23(b)).** The engine's existing
round-robin chunk grid IS the blueprint's static epoch-block assignment
(Def 3.1): the lane owning each chunk is a fixed function of the tape,
never of timing; the per-lane res rings are the SPSC epoch rings
(Thm 26(c)'s `N > D_epoch` sizing is RES_CAP's standing law); the fold's
strict-order drain is the static-order merge. The pace gate itself stays
(flow control, not ordering — Theorem 26(a)'s wait-freedom is about the
epoch protocol; the res-ring capacity bound is sizing, exactly §3.9(ii)).
The blueprint's epoch merge at hash granularity rides the R36-C Galois
merge already in the fabric (the per-pass surface's O(1) apply + the
fail-stop merge law) — the Part-II kernel (`epoch_merge_p64`) lands the
closed form for the engine-side composition when the L4 window widening
asks for it.

## §3 The instrument (Gate 6's measurement)

`spanbench`'s new regime prices both rings on the 322-record pass window
(the champion's fused-call count), with the frozen chunk table as the
corpus (real memory, distinct-per-position digests — no
scalar-evolvable constants) and bit-exactness COMPUTED per iteration:

* The CLMUL ring: serial 4.15 µs → dispatch tree 1.09 µs (3.8× on the
  shared dev slice; the fleet's dedicated core prices the real ratio).
* The engine ring: serial 203 ns (the pure 2-cycle/record dependent
  chain) vs tree 360 ns single-threaded — **the honest accounting**: the
  2.08 µs wall was never the rotl chain itself (203 ns of pure latency)
  but the drain loop's load→assert→apply pipeline stalls around it; the
  journal removes the h-dependency from the loop (the load/assert/push
  pipeline) and the tree's flush (~194 composes, ⌈log₂ K⌉ depth, once per
  pass = ~1–3% duty) replaces the chain's completion latency at the
  boundary. The local same-draw A/B: 11.55B vs 10.07B = **+14.7%** on the
  shared 2-vCPU slice (1-worker shape; the projection for the fleet's
  dedicated-core topology is the L1 band, 77–81B — Gate 6's verdict).
* `PR1_R37_TREE_FOLD_VERDICT` carries both rings' numbers + the
  tree_ok / engine_tree_ok / two_level_ok laws (asserted, exit 0).

## §4 The cells (the 130 → 139 extension, kernel + engine form)

* `t_r37_p64_associativity_triples` (cell +7-extend) — 2¹⁶ random triples +
  the 8 degenerate setter patterns (Appendix A.1's rows).
* `t_r37_tree_vs_serial_shape_fuzz` (cell +19) — the explicit K sweep
  (2..1024, odd tails, the champion's 322) + 10⁴ random-K streams: scalar
  tree == dispatch tree == RANDOM TREE SHAPES == serial chain.
* `t_r37_tree_vector_dual_path` (cell +21) — the vector schedule's bit
  equality (the mulmod4x4 stack-smash catch).
* `t_r37_two_level_tree_and_degree_escape` (cell +20) — 5,122-record
  maximal passes, flat vs two-level at adversarial run lengths; the
  >2¹⁶ degree domain.
* `t_r37_epoch_merge_identity` (cell +34) — the epoch composite identity
  on random splits (incl. empty blocks).
* `t_r37_segment_tree_law` — the rotation ring: 3-factor associativity +
  the SoA tree vs iterated serial applies (K ∈ {2..600}).
* `t_r37_journal_flush_laws` — the FoldJournal protocol: random boundary
  schedules (mid-journal splits, remainders), forced capacity overflows
  (caps 1..64), the serial rollback twin.
* The whole standing matrix (141 nf-testkit cells) runs the tree path BY
  DEFAULT — every hydra cell (fabric, inline, sustained, R36-E, wrap) now
  pins Theorem 22 end-to-end. The workspace total: 248 green, clippy
  `-D warnings` clean.

## §5 The telemetry contract

```
FOLDTREE_DIAGNOSTIC sustained: tree=… flushes=… overflows=… records=…
    compose_max=… live_len=… (R37: the Blelloch-Galois tree fold …
    HFT_TREE_FOLD=0 restores the serial chain)
PR1_R37_TREE_FOLD_VERDICT rate=… tree=true flushes=… overflows=…
    records=… compose_max=… fold_ms=… workers=… (R37: … docs/solution2.md)
spanbench: R37_TREE_P64 / R37_TREE_ENGINE / R37_EPOCH_MERGE +
    PR1_R37_TREE_FOLD_VERDICT … tree_ok=true engine_tree_ok=true
    two_level_ok=true
```

The engagement laws (asserted by the bench, exit 0): the tree armed,
`flushes ≥ passes` (every completed boundary flushes; capacity flushes
ride `overflows`), `live_len == 0` at rest, `records > 0`. The champion
arm (11r37) runs the TWO-WORKER shape (elision + tree); 11r37r prices
the tree's same-draw contribution (HFT_TREE_FOLD=0); the standing 11r36
(one-worker) stays as the η attribution twin — same-draw ratio
11r37/11r36 ÷ 2 = the lane interference efficiency (Gate 12's protocol).

## §6 What this landing is NOT (the honest scope line)

Part III (the affine order-transducer, Theorems 27–31) is NOT landed —
the kernel port (`RowOp`, the segmented `order_scan`, the emit-time
policies) is the next run's Gate 13 work with the engine's 3-stage
pipeline. The L4 window widening (4,096-chunk passes, 8-pass epochs) and
the L5 SMT co-lanes ride after it. The two-worker local rate (322M on
this slice) is a dev-VM topology artifact — both workers pin to the
same vCPU here; the protocol engagement (BIT-EXACT, the elision's
replay dominance, the tree's flush cadence) is what the local run
proves, and the fleet's dedicated-core draws price the corridor
(the projection: L1 77–81B single-lane; L2 ×1.7–1.9 net of η =
134–150B; the corridor floor needs L3).

The next walls, in the blueprint's order: the decode stage's ~6.49 µs
pass floor (L4's amortization), the order-cache probe path (Part III's
push-scan), and the SMT co-lane configuration (L5).
