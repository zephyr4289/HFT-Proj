# ROADMAP — Breaking the 2B Verification / 5B Ingest Barriers

**Repo:** `zephyr4289/HFT-Proj` · **Branch under review:** `r15-frontier` (PR #7, "R15: Frontier Breakthrough") · **Prepared:** 2026-10-05

**Scope of this document.** I read the full doc tree (`docs/00`–`docs/29`, `context.md`, `PROMPT.md`), the complete commit/PR history, the PR #7 diff (21 files, +5,241/−242), and the hot paths in `crcfold.rs`, `hydra.rs`, `rxdesc.rs`, `pipeline.rs`, `render.rs`, `gates.rs`, and `ci.sh`. This roadmap is written against **measured numbers from your own draws**, not folklore. It does three things:

1. Explains **precisely why the walls hold where they hold** (1.2348B sustained verify / 3.625B Front A), with the budget math done to the cycle.
2. Audits PR #7 honestly: what each lever proved, what it refuted, and **three anomalies in your own draw data that nobody has priced** — one of which is probably worth +5–12% on the headline number by itself.
3. Lays out the two programs (Verification → 2B, Ingest → 5B) as staged, instrument-first engineering with kill criteria, because your own discipline (Law: "no default changes without class evidence", "an instrument, not a guess") is the reason this repo got this far.

Everything below respects the 9-rule law of `PROMPT.md §1.5` (bit-exact, same value definition, no memoization, `ALLOC_DELTA=0`, `#![forbid(unsafe_code)]` on protocol/arbitrator, ≥3 healthy draws, no silicon shopping).

---

## 0. TL;DR — The Verdict Table

| Target | Record | Honest physics verdict | The levers that close the gap |
|---|---|---|---|
| **Sustained full verification ≥ 2.0B** | 1.2348B (R11/R12 draw, kbench 34.9) | **Knife-edge ×3.** Needs (a) kernel step 9 → ≤7 cyc (census floor is 6, zero-divisor proof kills census-4), (b) main-core serial 1.42 → ≤1.15 cyc/msg, (c) L3 supply ≥ ~28 GB/s/core on a *median* healthy draw. Each alone is buildable; all three landing simultaneously on the median draw is the hard part. Realistic engineered frontier: **1.4–1.6B**; 2.0B only if *everything* lands on a strong draw. | Route S fold-offload (never built), dfold verdict on record-class silicon (never run), real-mix kernel/attribution work, supply probe + L2-stripe contingency |
| **Pure ingest (Front A) ≥ 5.0B** | 3.6246B (0.6346 cyc/msg) | **Genuinely reachable.** The wall is the RX's per-frame `FrameEntry` build (~30–40 µops/frame, proven co-wall by the R12c ladder experiment). Lever B "rxbuild" (publish-by-reference master entry array) is *already designed in your own docs* and removes it. Post-Lever-B budget closes at ~0.40–0.45 cyc/msg → **5.1–5.7B** on 8573C-class draws. | Lever B rxbuild + reset-path amortization at ~9,900 passes/s + consumer walk economics |

**Do these three things first (this week, all cheap):**

1. **Flip the default submission path back to the ring** (`HFT_RXDESC` default → `0`). Your own draws price rxdesc at **−10% to −21% vs the ring on both silicon classes** (draw 10: 957 vs 1,214; draw 11: 842 vs 1,061; draw 12: 862 vs 1,054; draw 13: 945 vs 1,124; draw 15: 986 vs 1,098). Every fishing draw's headline 11b number is currently running a refuted path. Keep rxdesc as the armed soak (11w's inverse).
2. **Arm 11z: ring + siblings, same draw.** The R16d `distinct` placement flip was **only ever priced on the rxdesc path** (+9.3%/+12.2%, draws 10/11). Cross-era comparison at matched kbench says the ring path likely *loses* to distinct: R12-era ring+siblings scored 1,186.1M @ kbench 29.94 and 1,234.8M @ 34.9, while R16-era ring+distinct scored 1,054.5M @ 30.23 and 1,214.2M @ 34.86 — a −1.7% to −11% anomaly the kernel levers (reflect/vend/vtail, all positive) cannot explain. One CI arm settles it.
3. **Ship the null-mode + real-mix kbench instruments** (§4). The next lever choice (Route S vs kernel vs supply) is undecidable without them — this is your own law from draw 12: *"the next decomposition needs an instrument, not a guess."*

---

## 1. Where the Program Actually Stands (verified state)

### 1.1 The machine and the harness

- Corpus: `sample-mini.itch`, 15,000,000 bytes, **505,849 msgs/pass**, sha256-pinned. Dual-feed `MtuBound(1400)` schedule → **10,992 spans/pass** (all ≥ 512 B, ~46 msg/span, ~1.3 KB bodies), virtual-clock paced, pre-rendered THP-backed blob (14.3 MB, aliased dual-feed).
- Silicon fished: **Intel 8573C** (Sapphire Rapids / Golden Cove, 2.30 GHz, 2 phys × SMT = 4 vCPU, dual 512-bit datapaths, kbench `fold512_r` 1t 30–35 GB/s) and **8370C** (Ice Lake / Sunny Cove, 2.79 GHz, single clmul port, kbench 24–28 GB/s). ~50% of shards land AMD (not claim-eligible), healthy 8573C ≈ 1-in-10–12 shards.
- The record stack (R11/R12): siblings placement + SPSC desc rings + Desc8 + armed prepatch + deep assist ring + `fold512` kernel → **1,234,801,472 msg/s sustained, 34.15 GB/s delivered CRC, bit-exact `0x881639cead506f25`, allocs=0**. Front A record **3,624,572,766 msg/s** (0.6346 cyc/msg).
- PR #7 stack adds: reflect kernel (R13, +2.7–5.8% sustained), vend ending (R14, +4.45% on record-class, class-gated), vtail (R15, neutral ±0.37%), dfold (R16a, neutral on contended+healthy, **open on record-class**), rxdesc arrays (R16b, **refuted**), distinct flip (R16d, priced on rxdesc only), rxdesc diet (R16e, +1.15% median — still −10…−18% vs ring).

### 1.2 The PR #7 draw ledger (the honest bar)

| Draw | Silicon | kbench 1t (GB/s) | 11b (default stack) | Best ring arm (11w/11n) | Verdict |
|---|---|---|---|---|---|
| 4 (R15) | 8573C healthy | 30.00 | 1,061.8M | — | vtail +0.37% (noise) |
| 5 (R15) | 8573C healthy | 31.00 | 1,043.1M | — | — |
| 6 (R16) | 8573C marginal | 29.61 | 1,015.3M | — | dfold +0.01% (supply-bound) |
| 7 (R16) | 8573C healthy | 30.66 | 1,096.1M | — | dfold −0.23% (noise) |
| 10 (R16) | 8573C **record-class** | 34.86 | 957.3M (rxdesc+distinct) | **11n ring 1,214.2M** | rxdesc −21.2%; flip +9.3% (rxdesc only) |
| 11 (R16) | 8573C healthy | 30.16 | 842.1M | **11w ring 1,060.6M** | rxdesc −20.6%, gap 50/50 wake-cadence/dilution |
| 12 (R16e) | 8573C healthy | 30.23 | 862.3M (diet) | **11w 1,054.5M** | diet +1.4%; gap −18.2% |
| 13 (R16e) | 8573C healthy | 32.79 | 945.5M | **11w 1,124.2M** | diet +0.9%; ring residue ~+20 cyc/span |
| 15 (R16e) | 8573C healthy ×2 | 33.46 / 30.54 | 986.4M / 840.2M | **11w 1,098.1M / 1,084.8M** | diet +2.6%/−0.1%; residue supply-coupled (+8 cyc on strong draws) |
| — (R12 era) | 8573C | 29.94 | **1,186.1M** (ring+siblings) | — | the pre-flip reference point |
| — (R11 era) | 8573C **record** | 34.90 | **1,234.8M** (ring+siblings) | — | the all-time record |

**The single most important observation in this whole roadmap:** in the R16 era, *the default arm has never once been the fastest arm on the draw*. The ring (11w/11n) wins every head-to-head, usually by double digits. The branch's own headline metric has been running handicapped for 10 draws.

### 1.3 What the healthy-draw median looks like today

Per the R12 claim protocol (median of ≥3 healthy 8573C draws, kbench ≥ 30.0):

- Default stack (rxdesc+diet+distinct): healthy median ≈ **0.94–0.99B**
- Ring stack (HFT_RXDESC=0), same draws: ≈ **1.05–1.12B**
- R12-era ring+siblings at matched kbench: ≈ **1.19–1.23B**

The gap between what the branch ships as default and what the codebase has already demonstrated is **~20%**. Closing that is configuration, not engineering.

---

## 2. The Physics — Why the Walls Hold Exactly Where They Hold

### 2.1 Sustained verification: the four-way ledger at 2B

Demand at 2.0B msg/s:

```
CRC demand      = 2.0e9 msg/s × 27.66 B/msg      = 55.32 GB/s delivered
Main-core budget = 4.6 Gcyc/s ÷ 2.0e9 msg/s      = ≤ 1.15 cyc/msg total serial path
Worker span budget = 55.32 GB/s ÷ (2 workers × 1378 B/span)
                 = 20.1M spans/s/worker           = ≤ 114 cyc/span @ 2.3 GHz
```

Supply today (record draw decomposition, R13-era telemetry + current draws):

| Component | Today | Floor/optimum | Gap |
|---|---|---|---|
| Fold step (128 B/step, ~10.77 steps/span) | 9–10 cyc/step (14.3–12.8 B/cyc) | **6 cyc** = p5 census floor (4 clmul + 2 unpck); census-4 (unpack-free) **refuted by the zero-divisor proof** | 1.5× — but only reachable if latency-bound; dfold says healthy draws are NOT |
| Per-span endings (vend + vtail + FNV seed) | ~25–35 cyc | ~20 (algebra is exhausted; constants pinned) | small |
| Dispatch + ring + prefetch bookkeeping | ~15–25 cyc | ~15 (chunked, desc8, O(1) everything) | small |
| Supply stalls (L3-resident 14.3 MB blob) | ~20–30 cyc/span | ~10 if L2-resident (impossible: 2 MB L2 vs 7.5 MB/worker stream) | **structural on contended draws** |
| **Total real-mix span** | **169–190 cyc** (draws 13–15 DIAG) | **~114 needed at 2B** | **~1.5× overall** |
| Main-core serial path | ~1.42 cyc/msg (ladder 0.63 + submission 0.4–0.6 + ordered fold 0.15 + poll/pacing ~0.15) | ≤ 1.15 at 2B | −20% needed |
| Fabric delivery efficiency (delivered ÷ 2-core packed kbench pool) | **48.9% ± 0.3%** on every healthy draw ever measured | ≥ 56% needed at 2B with a 21.3 B/cyc kernel; ≥ 79% with today's kernel | see §2.2 — this number is not what it looks like |

### 2.2 The reframe: the 48.9% "fabric inefficiency" is mostly NOT plumbing

This is the most consequential analysis in this roadmap, because it decides where the next month of work goes.

The R7 harvest codified "fabric-efficiency invariance": delivered CRC = 48.9% ± 0.3% of the 2-core **packed-kbench** ceiling on every healthy draw, concluding "plumbing is not a lever; kernel step density is." That conclusion is right, but the *reason* is stronger than stated. Decompose the 48.9%:

1. **The real-mix span is structurally ~2× the packed-loop span.** Packed kbench: uniform 1344 B spans, hot L2 buffer, zero dispatch — ~91–94 cyc/span on healthy silicon. Real fabric: ~1.3 KB spans from an L3-resident blob with per-span endings, desc-ring reads, res-ring writes, and assist interactions — **169–190 cyc/span measured on the same draws** (worker DIAG, 96%+ busy under the ring). Workers are ~98.5% busy delivering half the packed ceiling *because the real span costs half again more*, not because the cores are idle.
2. **Cross-check from the R9 era:** real-mix packed control 25.51 GB/s vs real fabric 26.61 GB/s = **104% extraction of the real-mix ceiling** (the assist's main-core CRC pushes delivery *above* the worker-only ceiling). The fabric plumbing was already ~saturated two generations ago.
3. Therefore the efficiency invariance says: **to move delivered CRC you must make the real-mix span itself cheaper** (kernel step, endings, supply) **or add CRC capacity the packed ceiling doesn't count** (main-core assist, i.e. Route S).

Corollary for the 2B budget: at 2B, main has **zero surplus** (its serial path needs ~100% of its 1.15 cyc/msg budget just to feed), so the assist channel that delivered 34.15 > 33.57 at the R11 record is gone. Workers must deliver the full 55.32 GB/s → 114 cyc/span → every component must sit at its optimistic floor simultaneously (step 6–7 cyc, endings 20, dispatch 15, supply 10–15). That is the precise, honest shape of the 2B wall: **three knife-edges stacked** — kernel density, main-core budget, and L3 supply — with no single lever carrying the claim.

### 2.3 Front A (pure ingest): the wall is one struct

Front A = RX thread (poll → per-frame `FrameEntry` build → mailbox publish) ∥ consumer (ladder scan → dup rejection → watermark → span emission → count+span sink). No CRC workers.

- Measured record: 0.6346 cyc/msg; the RX is the **proven** co-wall (R12c: consumer ladder cut to ~2.5 µops/frame and Front A only recovered to RX parity).
- The RX per-frame cost is dominated by building an ~8-field, ~64–72 B `FrameEntry` (two fat slices, an `Option<FrameMemo>`, seq + two session words + elig byte) **per frame per pass** — at MtuBound(1400) that's ~169M frames/s at 5B, i.e. ~1.5–2 Gcyc/s of pure struct-building on one sibling, every pass, for bytes that are identical pass-to-pass except the 10 session bytes.
- 5B needs 0.46 cyc/msg @ 2.3 GHz. Post-Lever-B budget (§6): consumer ~0.32–0.36 + RX ~0.10–0.15 → **closes with margin**.
- Secondary walls nobody has measured at 5B rates (must be instrumented, not assumed): the reset/bake path runs every ~101 µs (≈9,900 passes/s — 3.4× the sustained arm's bake rate, the same shape that lotteried Front A in R10b); the mailbox round-trip granularity; the pacing walk.

Memory is *not* a Front A wall: ~15–20 GB/s of L1/L2-local entry/header traffic at 5B — trivial against the 25–35 GB/s/core L3 band.

---

## 3. PR #7 Audit — What Each Lever Actually Proved

| Lever | Mechanism | CI verdict | Status |
|---|---|---|---|
| R15 vtail | lane-0 tail absorbed into vend field, 216-constant table, r≥16 gate | +0.37% on one deciding draw (inside ±3.4% arm noise); bit-exact 5/5 | **Neutral.** Keep (default ON, gated) — it removes the last serial chain; its value shows only when endings bind |
| R16a dfold | T=2 block-parity split, 4 chains, K² step | Neutral on contended (draw 6) AND healthy (draw 7); **11v never ran on a record-class draw** (draw 10 crash; draws 11+ neutral again) | **Open — one experiment left** (§5-V3). The only silicon where the 9-cyc step is latency-bound rather than supply-bound is kbench ≥ 34 |
| R16b rxdesc | array-driven submission, warm start, check-and-fix | **−21.2% (record-class), −20.6% (healthy), −18.2% post-diet** vs ring, stable across classes | **Refuted as the default.** The §5.3 arithmetic that assumed rxdesc ≥ ring is dead. Keep the code + telemetry as an attribution instrument |
| R16e diet | depth batch, per-chunk record resolution, division-free grid | +1.4/+0.9/+0.0/+2.6% (median +1.15%) vs pre-diet; recovers ~2.4 pts of the 21-pt gap | **Correct work, wrong substrate.** The remaining ~+8–24 cyc/span residue is structural to array-path workers (L2/L3-resident arrays vs the ring's L1-resident desc stream) |
| R16d flip | workers→distinct cores, main/RX→SMT siblings | +9.3% (draw 10), +12.2% (draw 11) — **both on the rxdesc path only** | **Unpriced on the ring path** — and cross-era evidence (§0 item 2, §3.1) suggests it regresses the ring. Must A/B same-draw before it touches any default |
| Supply sweep 11u | HFT_WORKER_BATCH 64/256 | no signal | closed |

### 3.1 The three anomalies in your own data (unpriced, cheap to price)

**A1 — the placement flip has never touched the ring path it may hurt.**
Every "distinct wins" attribution (draw 10: 11b 957 vs 11k 876; draw 11: 842 vs 750) ran the *rxdesc* submission path. On the ring path the only same-era evidence is cross-era: at kbench 29.94–30.23, ring+**siblings** (R12 era) = 1,186.1M vs ring+**distinct** (R16 era) = 1,054.5M (−11%); at kbench 34.86–34.90, 1,234.8M vs 1,214.2M (−1.7%). The R11 §8.1 mechanism is exactly the one that would hurt the ring under distinct: main+RX land on the *workers'* hyperthreads, the supply side (ladder+submission at 72%+ duty) loses its L1/L2-local sibling and steals issue slots from the cores doing the fold, and the assist ring — which converts main's surplus into CRC — starves. The R16d flip fixed a main-side submission wall that **rxdesc created** (no rings → no backpressure pacing → fold lag); with the ring restored as default, the flip's premise partially evaporates. **Action: arm 11z = `HFT_RXDESC=0 HFT_FABRIC_PLACE=siblings` on every draw, ≥3 healthy 8573C draws, then apply the R9c→R9d law per class.** This is potentially +5–12% on the headline number for one CI arm.

**A2 — prepatch-armed is losing on the R16-era stack (0.4–5.5%), and nobody re-priced it.**
Draw 10: 11e (unarmed) 1,001.1 vs 11b (armed) 957.3. Draw 11: 888.8 vs 842.1. Draw 13: 949.5 vs 945.5. The R12 flip to armed-default was decided on the ring stack 8/8 — but the worker/submission timing changed materially since (rxdesc, diet, batch drains). If the ring comes back as default, re-price 11e on it for three draws before trusting the stale flip. Cheap: the arm already exists.

**A3 — the ring stack's absolute numbers regressed vs R12-era at matched kbench, beyond placement.**
Even after A1 is priced, the R16-era ring numbers (1,054–1,124 healthy) sit below R12-era (1,186 at kbench 29.94) by more than the kernel levers should allow (reflect/vend/vtail are all measured positive). Candidates, in order of suspicion: (a) the R15 `HFT_WORKER_BATCH=128` drain cap now sits in `lane_worker` (the ring worker) — it was swept only on the rxdesc path (11u) and adds per-batch instrumentation (`Instant::now()`, stats, re-space check) every 128 spans; (b) the R16b/R16e sink-side changes (assist watermark 2048, RX_PACE 8192, window bookkeeping) now execute on the ring path even when rxdesc is off; (c) the pass-boundary unstick changes in `reset_pass`. Action: a 30-minute bisect-style A/B on one healthy draw — `HFT_WORKER_BATCH` uncapped on the ring path; then watermark/pace knobs zeroed — is enough to find which one carries the regression. (a) in particular is a one-line env change and the batch cap's `t_eval` clock read per batch is exactly the "instrumentation inside the measured window" class that R10 lever 4 killed once before.

---

## 4. Priority 0 — Instruments Before Levers (your own law)

Every undecided branch in §5/§6 routes through one of these. All are cheap (days, not weeks); all follow patterns already in the codebase.

| # | Instrument | What it decides | Shape |
|---|---|---|---|
| I1 | **Null-mode CI arm** (`HFT_HYDRA_NULL=1` — already implemented in `hydra.rs`, never run in CI) | The non-CRC ceiling of the whole fabric: ring mechanics + sequencer + submission, with the kernel removed. Prices the supply/dispatch share of the 169–190 cyc/span directly instead of by subtraction | New ci.sh arm on the ring stack; report `null_span_cyc` next to `real_span_cyc` every draw |
| I2 | **Real-mix kbench row** (`fold512_rm`) | The *correct denominator* for fabric efficiency: the packed corpus flatters the kernel by ~2×. A kbench row replaying the actual span-length/pattern mix from the tape (precomputed at bench start) turns "48.9% of packed" into "X% of real-mix" — and X is the number Route S can actually move | kbench corpus = span offsets/lengths dumped once from a sustained pass (deterministic schedule ⇒ stable mix) |
| I3 | **Per-core memory probe row** (kbench or standalone: 2 threads streaming a 14.3 MB THP blob at fold-shaped stride, reporting GB/s) | The R9 supply co-wall (25–35 GB/s/core contended) has never been measured *on the draws*. 2B dies or lives on this number | One kbench row per draw; gate the record protocol on it (§7) |
| I4 | **8-chain fold probe** (`fold512_8ch`: 8 independent state pairs over one load stream) + constants-preloaded variant | The no-PMC latency-vs-ports discriminator (PROMPT.md §6.2, never built): if 8 chains scale, the step is latency-bound → dfold matters on good silicon; if flat, it's ports/supply → kernel program is done | kbench rows; run on every draw next to `fold512_rd` |
| I5 | **11z placement arm** (ring + siblings) + A2/A3 knob A/Bs | Anomaly pricing (§3.1) | One arm + two env sweeps |
| I6 | **Front A stage decomposition DIAG** (RX: poll vs entry-build vs publish; consumer: ladder vs emission vs mailbox-wait — all already partially in `rx_stats`) | Which share of the 0.6346 cyc/msg is entry-build (Lever B's target) vs ladder vs handshake — sizes Lever B's payoff before it's built | Extend the existing `HFT_EXP_DIAG` counters; run on one healthy draw at Front A rates |

---

## 5. Program V — The Road to 2B Sustained Verification

Staged so each stage's CI verdict gates the next. Budget math updated per stage. Kill criteria are explicit — this program has earned its refutation ledger.

### V0 — Restore the baseline (configuration, ~1 day)

- Default `HFT_RXDESC=0` (ring is the submission path; rxdesc becomes the armed soak). **Expected: healthy-draw headline 0.94–0.99B → 1.05–1.12B immediately.**
- Run 11z (ring+siblings) ×3 healthy draws; if it beats ring+distinct (expected per A1), flip `fabric_placement` back for the 2-worker/2-core/SMT shape *with the ring*, keeping distinct only where rxdesc-style paths ever return.
- Re-price 11e (prepatch) ×3 draws on the restored stack; uncap `HFT_WORKER_BATCH` on the ring path (A3a) and re-price.
- **Stage gate:** healthy median ≥ 1.15B, record-class ≥ 1.23B (i.e. R12-era territory restored + R13–R15 kernel dividends on top). If the kernel dividends don't stack on the restored baseline, A3 has more carrying and the same arms find it.

### V1 — Route S: the fold-offload thread (the promoted lever that was never built)

Draw 7's verdict promoted Route S to "primary 2B lever"; R16c scoped "serial/FNV absorption into the sibling assist path" and then it was superseded by the (now-refuted) rxdesc work. This is the version worth building, because the main-core budget is the one knife-edge no kernel lever touches:

- **Mechanism:** a 5th thread — the *fold servant* — owns the ordered FNV combine, pass-boundary bookkeeping (`complete_boundaries`), and harvest accounting. Main keeps: pacing, poll, framing, ladder, dup rejection, watermark, ring submission. Workers keep: span CRC + res rings. The servant consumes the *same* res-ring batches main drains today (it is literally the `fold_available` loop moved to its own core, fed by the existing cursors — no new protocol, one ownership flip on `res_tail`).
- **Placement:** in the siblings shape (post-V0), the servant shares worker-1's sibling with RX (RX is 72.6% busy at sustained rates — its park windows are the servant's room), or round-robins with main if the DIAG says so. The servant is scalar (FNV imul chains, integer bookkeeping) — exactly the "< 4.5% theft" class the R8 sibling data carved out, *provided* it touches no p5 work and no new cache lines (it consumes lines main already pulled).
- **Budget effect:** main 1.42 → ~1.20–1.27 cyc/msg. That alone does not close 1.15, but combined with V0's submission restoration (ring mechanics at their R12-era cost) it lands ~1.15–1.25 — i.e. **main stops being the binder at ~1.3B and becomes a non-binding supplier at 1.4–1.5B**.
- **Bonus:** at sustained rates main's freed ~0.15–0.2 cyc/msg *partially restores the assist channel* above the record's 34.15-vs-33.57 effect — the only "new" CRC capacity available on this machine shape.
- **Kill criterion:** if 11z-era main telemetry (post-V0) shows main < 90% busy at 1.2B (i.e. main is already not the binder), skip V1 entirely and go straight to V2/V3 — the worker span budget is the binder and Route S buys nothing.
- **CI arms:** 11serv (servant on) vs 11b, ≥3 healthy draws; kbench untouched (kernel unchanged); full parity battery (the fold must remain bit-exact and strictly ordered — the servant's claim order IS submission order, the same law the inline ring already proves).

### V2 — Supply engineering (gated on I3)

- If the I3 probe shows ≥ ~30 GB/s/core streaming on healthy draws: supply is not binding below ~1.5B — **stop here**, spend nothing.
- If it shows ≤ ~25 GB/s/core (contended-host reality): the 2B demand (27.7 GB/s/core) sits above the wall and *no* compute lever matters on median draws. Contingency, pre-specced in PROMPT.md R9: **L2-stripe render** — render the blob so each worker's chunk stream is a per-worker sequential stripe (workers already consume disjoint chunk grids: `chunk ≡ lane mod W`; a render-time permutation of span placement makes each worker's pass stream ~contiguous instead of stride-interleaved). This does not shrink the 14.3 MB working set, but it converts each worker's access pattern into a pure sequential stream the L2 streamer + THP track perfectly — worth 10–30 cyc/span on latency-exposed spans (the R9 spray experiment measured +36% from exactly this class of fix on the aliasing gap; fbench stage P vs K: 335 vs 517 cyc/span).
- Constraint check: span *values* are defined by `span_crc32c_8lane` over each span's own bytes — relocating spans inside the blob (render-time layout) does not touch the value definition (rule 2 intact; the desc offsets already de-reference through a base).

### V3 — The kernel endgame (two experiments, then done)

1. **dfold's record-class verdict.** The one open cell in the entire kernel matrix: 11v on a kbench ≥ 34 draw. dfold is shipped, armed, bit-exact, and priced neutral everywhere supply binds; record-class silicon is the only regime where the 9-cyc step is latency-limited and a 2× chain depth can show. Keep the 11v arm armed on every push until a record-class draw lands (expected ~2% of shards — budget 2–4 pushes of fishing).
2. **I4's verdict.** If `fold512_8ch` scales on target silicon (latency-bound) and dfold is still neutral on the *fabric*, the gap is span-schedule, not step — the kernel program ends and the residual lives in V1/V2. If 8ch is flat (ports/supply-bound), the kernel program ends immediately: **the census-6 floor is the wall and the fold is done.** Either way the kernel program terminates with a measurement, which is the only acceptable way it can end.
3. Endings/dispatch residuals (~20 + ~15 cyc floor vs ~25–35 + ~15–25 today): after vend+vtail, the remaining ending cost is the odd-block word chains (p1, parallel — proven cheaper left alone by the R15 lanes-1..7 refutation) and per-span dispatch. **Do not reopen the algebra.** The only remaining lever here is I1-guided: if null-mode shows dispatch ≥ 25 cyc/span, the submission/ring path (V0 restoration) is where the cycles are, not the kernel.

### V4 — The honest 2B claim math (post-V0..V3)

With V0 restored (~1.15–1.23 healthy baseline), V1 (+3–8% when main binds), V3 (0 to +8% on record-class only), V2 (0 to +10% on contended draws only):

- **Engineered frontier on median healthy draws: ~1.35–1.55B.** That is the number the current machine shape, corpus, and value definition support with every lever landed.
- **2.0B requires:** 114 cyc/span real-mix (step ≤7 + endings 20 + dispatch 15 + supply 12–15) **and** main ≤1.15 **and** ≥56% real-mix-to-packed conversion **and** a draw whose L3 delivers ≥27.7 GB/s/core. On record-class draws (kbench ≥ 34, uncontended L3) this is *conceivable* — the R12 protocol's median-of-3-healthy rule, however, deliberately excludes outlier fishing, and the median healthy draw's contended L3 is the binding knife-edge per I3's likely verdict.
- **Recommendation (write it into the claim doc *now*, before the fishing burns out the team):** the 2B claim has a structural dependency the 9 rules don't currently capture — **supply health**. Amend the claim protocol: a draw counts toward the 2B record only if its I3 probe ≥ 28 GB/s/core (the same honesty as the kbench ≥ 30.0 healthy gate and the `BLOB_BACKING verdict=thp-granted` log — a gate that depends on a machine property must log the property). If the fleet's healthy draws systematically fail the supply gate, the 2B claim on 4 vCPU is physically closed by the platform, and the honest options are: (a) re-open Route F with the task source (R12's terminated ruling — bring them the I3 data; it is exactly the evidence that ruling said it would take), or (b) re-scope the claim to "structural budget of the class" with 1.4–1.6B as the certified frontier (your own docs/24 §8.2 already mapped this honestly — this roadmap just gets you to it).

---

## 6. Program I — The Road to 5B Pure Ingest (the closer target)

This program has one dominant lever, and it is already designed in your own documents (PROMPT.md §6.4 "Lever B rxbuild"; docs/29 §5.4). It deserves a full spec and a build slot because it is the only route to 5B, and its budget closes.

### I1 — Lever B "rxbuild": publish-by-reference master entry array

**Mechanism.** The frames of every pass are byte-identical except the 10 session bytes (the schedule is deterministic; the blob is aliased). Today the RX re-builds ~64 B `FrameEntry` structs ~169M times/s, every pass, for data that never changes. Instead:

1. **Build once per render:** an event-indexed master `FrameEntry` array built at render time (RX-side, outside all measurement windows), covering the whole pass's frame stream. The array is the *master*; nothing per-pass rebuilds it.
2. **Per-turn slice publish:** the mailbox stops carrying entry *arrays* and carries (master, `event_range`, clock) tuples — a per-turn publish is two integers + a Release store. The consumer walks the master slice directly. Mailbox traffic collapses from ~64 KB/batch to ~32 B/batch.
3. **Session patching, prepatch-extended:** the session compare words live in the master; the per-pass bake patches 10 bytes × changed frames (the existing prepatch site list, applied to the master instead of the blob+entries). The `sess_lo/sess_hi` fields update in place; the elig byte's session component re-derives from the baked template compare the RX already runs.
4. **Tombstone-free contiguity:** the event-indexed master has no holes (tombstoned events advance the cursor without frames — the master records the *published* subsequence), so the consumer's walk needs no empty-slot checks. This kills the last per-frame branch in the scan.
5. **Consumer side:** the steady ladder keeps reading entries (now L1/L2-hot, written once) — **no ladder redesign** (the vector ladder stays refuted; don't relitigate). Optionally phase 2: replace entry-based block iteration with the **VPADDQ prefix-sum walk** over the blob's 2-byte length prefixes for the emit path (your own <0.15 cyc/msg estimate) — only if I6 shows the entry walk is still material after phase 1.

**Budget accounting (8573C, 2.3 GHz):**

| Path | Today | Post-Lever B |
|---|---|---|
| RX per-frame build | ~0.20–0.30 cyc/msg (30–40 µops × 1/28 msg/frame + slice/Option/elig) | ~0.02 (slice publish amortized) + patch amortized ~0.01 |
| RX poll + pacing | ~0.10–0.15 | unchanged |
| Consumer ladder + emission + sink | ~0.30–0.35 (R12c measured the ladder alone at ~0.32 model; record 0.6346 total incl. RX starvation) | unchanged |
| Mailbox/wait overhead | ~0.05–0.10 (RX at 72.6% ⇒ consumer occasionally waits) | ~0.02 (deeper effective buffering: 32 B/turn ⇒ the 4-buffer mailbox becomes effectively unbounded) |
| **Total** | **0.6346 measured** | **~0.42–0.48 → 4.8–5.5B on the same draws; record-class 5.5–6.0B** |

**Protocol hazards to design out up front (the house discipline):**

- The master is *shared read-mostly state* mutated per-pass by the RX's bake — the single-writer law stays intact (RX writes, consumer reads, Release/Acquire on a generation word, exactly the rxdesc records pattern you already proved).
- The elig byte's session component must re-verify against the consumer's *live* template per scan (the R12c session-exactness law — keep the 2-compare gate; the baked-vs-live hazard class is documented).
- `ALLOC_DELTA=0`: the master is a fixed-capacity preallocated array (events are bounded by the corpus; `EVENT_CAP` from the render walk at construction).
- Warm-start validation: the zero-fixes fast-path assert pattern from rxdesc transfers directly (pass 0 pays the build; passes 1..N find every field already correct except patched session words; assert the fix count).
- The parity matrix (steady/chaos × worker counts × session splits × multi-pass) must run against the *classic* RX path — the same 3-way suite shape as R16b/e.

### I2 — The reset path at ~9,900 passes/s

At 5B, a pass is ~101 µs. The reset/handshake/bake machinery runs 9,900×/s (3.4× the sustained arm's bake rate — the same duty class that made R10b's Front A a THP lottery). Prepatch + RX auto-advance already remove the synchronous bake; what remains to instrument (I6) and then shave:

- The futex handshake park (BUF_PARK_NS) must *never* fire in the steady window at 5B — if I6 shows park time, deepen the effective mailbox (Lever B's 32 B/turn makes this free).
- The per-pass golden-population assert and window bookkeeping on the consumer side (~91 µs/pass class cost at the sustained arm — Front A's version needs its own number from I6 before anyone optimizes it).
- Kill criterion: if post-Lever-B Front A stalls at ~4.5B with I6 showing reset-path ≥ 0.05 cyc/msg, the pass structure itself (505,849 msgs/pass is fixed by rule 5) is the wall — document it as the class's Front A budget and stop.

### I3 — What 5B does NOT need (save the effort)

- No RX sharding (the budget closes on one RX; a second RX thread adds ordered-merge cost the budget can't afford).
- No memory heroics (~15–20 GB/s total traffic, L1/L2-local).
- No consumer vectorization (the ladder is refuted as a lever; the elig byte already did the work).
- No transport changes (the blob is pre-rendered; poll batching at 256 is already amortized).

**Program I is worth 2–3 weeks including the parity matrix. It is the higher-confidence of the two programs and should start *now*, in parallel with V0's configuration work** — the two paths share no code (span submission vs frame entry build), so there is no merge hazard.

---

## 7. Claim Protocol & Fleet Economics (the statistics of finishing)

- **Healthy gate:** kbench `fold512_r` 1t ≥ 30.0 GB/s; discard < 29.0. Keep it. **Add the I3 supply gate for any 2B claim** (≥ 28 GB/s/core streaming) — with the same "a gate that depends on a machine grant must log the grant" reasoning as `BLOB_BACKING`.
- **Class rule:** 8573C-class only for both claims (8370C cannot aggregate — single clmul port, kbench 24–28). Confirmed correct by every 8370C draw in the ledger.
- **Median-of-3:** the 2B claim must hold on the *median* healthy draw. Given §5-V4's math, plan for the claim campaign to *demonstrate the frontier* (1.4–1.6B median) and treat 2.0B as the record-class contingency — or trigger the Route F ruling conversation with the I3 data in hand. Decide this **before** burning 20+ fishing pushes.
- **Fishing budget:** ~1-in-10–12 shards are healthy 8573C; a 50-shard push yields 3–5 candidates; each push ~14 min/shard wall. A 3-draw claim = 1–2 pushes *if* the stack is ready and every candidate draw runs the full ladder (11b, 11z, 11e, 11v, kbench, I3). Pre-stage the arm matrix so a candidate draw is never wasted on an incomplete sweep — draw 10 lost its 11v/11w cells to the 11u crash; that's 2% of silicon gone forever.
- **Noise floors to respect:** ±3.4% identical-config arm-position; ±10% noisy hosts. No default flip on a signal inside ±3.4% without ≥3 draws (the R9c→R9d law — it has saved this repo repeatedly; the diet's +1.15% median is the current live example of a correctly-handled marginal).

---

## 8. The Refutation Ledger (do not spend a minute here)

Carried forward from PROMPT.md §5 + PR #7's own additions, plus this roadmap's new entries:

1. Tri-stream / eval2 / eval_pair fold interleaves — port-issue-bound (docs/24 §7; arms 11g/11l/11j).
2. Lanes-1..7 zmm tail absorption (−2–3%, R15); vtail for r ≤ 8 (R15); negative-power GF(2) algebra (R13/R15).
3. `HFT_WORKER_BATCH` 64/256 as a *rate* lever on the deciding path (11u, no signal) — but see A3a: uncap it on the ring path as a *regression* check, not a lever.
4. vend/vtail defaults on 8370C (class gate stands — kbench rc > rv there).
5. Vectorized watermark ladder (consumer-side gather −3.6…−6.8% sustained; docs/25).
6. Per-span SPSC handoff (anti-ping-pong law, docs/20).
7. Single-core > ~617M verification (crc32 8 B/cyc ceiling); GFNI CRC hybrid; PMC instrumentation on hosted runners (perf_event_paranoid=2).
8. **Unpack-free census-4 kernel** (zero-divisor proof, docs/29 §2 — this one is *permanently* dead: GF(2)[y]/VM is a field).
9. **rxdesc as the default submission path** (−18…−21% vs ring, both classes, three protocol fixes deep — the arrays are now an instrument, not a product).
10. **L2-stripe render for Front A** (no memory wall there); **RX sharding for 5B** (budget closes on one RX).
11. Anything violating the 9 rules (memoization across passes, byte sampling, sub-32 checksums, counting unread bytes) — banned by name.

---

## 9. The Build Order (decision tree)

```
WEEK 1
├─ V0: flip default to ring (1 line) ─────────────► headline +10-18% on next push
├─ I5: arm 11z (ring+siblings) + 11e re-price + wbatch-uncap A/B ─► A1/A2/A3 priced
├─ I1: null-mode CI arm ─────────────────────────► supply/dispatch share priced
└─ I6: Front A stage DIAG ───────────────────────► Lever B payoff sized
WEEK 2-3
├─ Program I1: Lever B rxbuild (spec above) ─────► Front A 4.8-5.5B expected
│   └─ parity matrix + warm-start asserts + D-oracle legs
├─ I2: real-mix kbench row ──────────────────────► true fabric-efficiency denominator
└─ I3: memory probe row ─────────────────────────► 2B supply gate decided
WEEK 3-4
├─ V1: Route S fold-servant (IF V0 telemetry says main binds)
├─ V3a: keep 11v armed for record-class draw (zero work, just keep fishing)
└─ I4: 8-chain + no-load kbench probes ─────────► kernel program terminated with a measurement
WEEK 5+
├─ V2: L2-stripe render (ONLY if I3 says supply binds)
├─ Claim campaign: ≥3 healthy draws, full arm ladder every draw
└─ If I3 closes the 2B door on median draws: Route F ruling conversation with the data,
   or certify 1.4-1.6B as the class frontier (docs/24 §8.2 precedent).
```

**Success criteria, restated honestly:**

- **Front A 5.0B:** expected PASS with Lever B landed (median healthy 4.8–5.5B, record-class 5.5B+). This is the program's winnable barrier and it is winnable this month.
- **Sustained 2.0B:** expected outcome is a certified **1.4–1.6B median frontier** with the 2B verdict resting on the I3 supply gate — either the fleet's healthy draws clear it and the knife-edge stack is worth completing, or the ruling conversation (Route F) has the evidence it needs. What this roadmap refuses to do is let the 2B campaign consume another ten draws of fishing while the *default configuration* runs a refuted path 18% under the codebase's own demonstrated best.

---

## Appendix A — Proposed CI arm matrix (next push)

| Arm | Config | Purpose |
|---|---|---|
| 11b | **new default: ring (`HFT_RXDESC=0`), R13–R15 kernel stack** | headline |
| 11z | 11b + `HFT_FABRIC_PLACE=siblings` | A1 placement pricing on ring |
| 11e | 11b + prepatch off (both placements) | A2 re-price |
| 11w | rxdesc ON (the current default, inverted) | rxdesc soak — keeps the instrument warm |
| 11v | dfold ON | record-class case stays open |
| 11null | `HFT_HYDRA_NULL=1` (ring) | I1 — non-CRC ceiling (skip parity asserts per the mode's contract) |
| 11wb | 11b + `HFT_WORKER_BATCH` uncapped on ring path | A3a regression check |
| kbench rows | existing r/rv/rc/rd + **`fold512_rm`** (real-mix corpus) + **`fold512_8ch`** + **no-load variant** + **memprobe row** | I2/I3/I4 attribution |
| I6 | `HFT_EXP_DIAG` extended at Front A rates (one healthy draw) | Lever B sizing |

## Appendix B — The numbers this roadmap was computed from (traceability)

- Records: 1,234,801,472 sustained / 3,624,572,766 Front A (docs/24 §8, run 37108369001, kbench 34.90).
- Demand math: 27.66 B/msg CRC stream (docs/24 §2, DIAG-derived); 55.32 GB/s at 2B; ≤1.15 cyc/msg main budget at 2.3 GHz.
- Kernel: 14.28 B/cyc = 9 cyc/128 B step (docs/24 §2); p5 census 6 uops → 21.3 B/cyc floor (docs/29 §2); zero-divisor proof (docs/29 §2.1–2.3).
- Real-mix span 184 cyc vs packed ~91 (docs/27 §1); current ring 169–190 cyc/span (draws 13–15 worker DIAG); R9-era real-mix control 25.51 vs fabric 26.61 GB/s (docs/22 §7.10).
- Fabric efficiency 48.9% ± 0.3% (docs/29 §1, R7 harvest); main decomposition work_ms 3819/5000 (docs/25 §1); RX prod_ms 3628/5000 = 72.6% (docs/24 §1).
- rxdesc vs ring: −21.2% (draw 10), −20.6% (draw 11), −18.2% (draw 12), ~−10% strongest-draw (draw 15) — docs/29 §9.
- Placement pricings: +9.3%/+12.2% rxdesc-only (draws 10/11); R11 distinct refutation −0.9% (docs/24 §8.1); cross-era ring numbers §1.2/§3.1 above.
- dfold: neutral draws 6/7/11; record-class case open since draw 10's 11v never ran (docs/29 §9).
- Front A walls: R12c ladder recovery-to-parity (docs/25 §5.2/§7); Lever B design (PROMPT.md §6.4, docs/29 §5.4); reset-bake duty 3.4× sustained (docs/23 §9).
- Contended L3 band 25–35 GB/s/core (docs/29 §1 R7/R9 verdicts).
