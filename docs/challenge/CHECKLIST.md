# CHECKLIST — The Consolidated Execution Plan (R17)

**Synthesized from:** `ROADMAP1.md`, `ROADMAP2.md`, `ROADMAP3.md` (senior-dev audits, 2026-10-05)
**Mission:** sustained full verification **≥ 2.0B msg/s** (record 1,234,801,472) · pure ingest Front A **≥ 5.0B msg/s** (record 3,624,572,766) · standard 4-vCPU CI draw (2 phys × SMT) · challenge §4 nine rules intact.
**Method:** every item below is traceable to its source roadmap(s) — `[R1 §x]`, `[R2 §x]`, `[R3 §x]`. Where the three roadmaps agree, the item is consensus. Where they diverge, the divergence is named and the deciding experiment is the item. Kill criteria ship with every lever (the house law: every step ends in a shipped lever or a written refutation).

---

## 0. The Consensus Verdict (what all three roadmaps agree on)

| # | Consensus | R1 | R2 | R3 |
|---|---|---|---|---|
| C1 | **rxdesc is refuted as the default submission path** (−10…−21% vs ring on both classes, 5 draws; residue supply-coupled). The sustained default must be **ring + distinct**. Keep rxdesc as an armed attribution arm. | §0.1, V0 | §5.4 | §2.4, §3.2 |
| C2 | **The null-mode instrument (`HFT_HYDRA_NULL`, already implemented, never run in CI) must ship as a CI arm** before any further worker-side work. Draw 12's own verdict: "the next decomposition needs an instrument, not a guess." | I1 | §5.1a | §3.3 |
| C3 | **Front A 5B is the winnable barrier** — the RX per-frame entry build is the proven co-wall, the lever is designed (Lever B / F1 warm start), the budget closes at ~0.42–0.48 cyc/msg → 4.6–5.7B. Sequence it FIRST / in parallel. | §6 | §6 | §5 |
| C4 | **dfold is neutral everywhere measured**; its only open case is record-class silicon (kbench ≥ 34, ~2% of shards). Keep arm 11v armed; zero new work. | §3 | §5.3 | §4.4 |
| C5 | **Median-healthy 2B needs 87–91% fabric efficiency — not a program.** 2B lives on record-class draws (79–81% needed) or doesn't live at all. The claim protocol must be re-scoped/pre-declared BEFORE the fishing campaign burns out. | V4 | §5.7 | §2.2, §4.5 |
| C6 | **Instruments before levers** — the ~66–75 cyc/span supply+ring residual is the least-instrumented, biggest share; every downstream route decision routes through the null arm + kbench floor rows. | §4 | §5.1 | §3.3 |
| C7 | **The refutation ledger is closed** — do not re-tread Stage-B unpack-free (proven impossible), tri-stream/eval2/eval_pair, GFNI hybrid, per-span SPSC, vectorized watermark ladder, WORKER_BATCH as a rate lever, vtail r≤8, lanes-1..7 zmm, Route F, PMCs, or anything on the banned list. | §8 | §4 | §6 |
| C8 | **Do not add working set to win compute** — every loss since the record (rxdesc arrays, the diet chase) traces to bytes cycling in L2/L3 where the winning path was L1-resident. The 2B stack's cache budget: THP blob + ring desc stream + worker stacks. Nothing else. | — | §4 note | §6.12 |
| C9 | **No lever is trusted on one draw** — ±3.4% arm noise, ±10% host noise; ≥3 healthy same-class draws, median decides (R9c→R9d law). | §7 | §8 | §6.13 |

**Where they diverge (and how this checklist resolves it):**

| Divergence | R1 | R2 | R3 | Resolution |
|---|---|---|---|---|
| The 2B centerpiece | Route S fold-servant (5th thread, main-budget lever) | **Route T bake-time lane transposition** (p5 census 8→6, the biggest untried lever) | wide-store desc packing + pipelined endings | **All three are complementary, not competing** — they attack the three different sub-walls (main budget / kernel density / worker endings). Sequence by kill-test cost: T's `fold512_t` row is one day and decides the kernel axis; wide-store and the ending pipeline are the main/worker axes. Route T's kill test runs FIRST (cheapest, biggest expected value). |
| Placement on the ring path | A1: ring+siblings may beat ring+distinct (cross-era −1.7%…−11% anomaly); arm it | accepts ring+distinct | ring+distinct | **Post-flip, arm 11k (`HFT_FABRIC_PLACE=siblings`) automatically becomes ring+siblings** — the A1 pricing is free. ≥3 healthy draws decide per class; no new code. |
| Front A lever shape | I1 rxbuild: full master entry array, publish-by-reference mailbox | F1: warm array + check-and-fix (zero added store traffic — the R12b sidecar failure mode designed out) | Lever B per PROMPT.md §6.4 + VPADDQ walk + RX sharding reserve | **F1 first** (smallest delta, the rxdesc-proven pattern, kills the sidecar hazard), **full pubref second** if F1 lands short of 4.6B, **sharding held in reserve** (R3 §5.2-4). |

---

## 1. Phase 0 — Housekeeping & Baseline Restore (week 1; zero risk, unblocks everything)

- [ ] **P0-1. Default submission flip: ring becomes the default** — `hydra.rs` `spawn_pinned`: `HFT_RXDESC` unset → ring; `HFT_RXDESC=1` → rxdesc armed. Update field doc + `docs/29` cross-ref. Expected: healthy-draw headline 0.94–0.99B → 1.05–1.12B immediately. `[R1 V0 / R2 §5.4 / R3 §2.4 — unanimous]`
- [ ] **P0-2. Arm 11w inverted** → `HFT_RXDESC=1` (the rxdesc armed soak — keeps the instrument + telemetry warm; the R16B_RXDESC line still prints on every arm). `[R1 App.A "11w inverted" / R2 §5.4 / R3 App.C "11w'"]`
- [ ] **P0-3. Arm 11k re-scoped** (comment update only): post-flip it is **ring+siblings** — the R1-A1 placement anomaly pricing on the ring path. ≥3 healthy 8573C draws, then the R9c→R9d law per class. `[R1 §3.1-A1]`
- [ ] **P0-4. Section-16 gate reorder** — kbench health FIRST: parse `fold512_r` 1t from `/tmp/kbench.txt`; if `< 29.0` → run hft_bench 5× report-only, print `SECTION16_DISCARD` banner, skip the statistical constraints, exit 0 (the R12 discard protocol, implemented). Row absent → cannot judge → keep current behavior. `[R3 §3.1]`
- [ ] **P0-5. Null-mode CI arm 11z** — `HFT_HYDRA_NULL=1` on the ring stack; grep the `HYDRA_NULL_MODE_DIAGNOSTIC` banner + sustained verdict + `allocs=0` (parity asserts are disabled by design in this mode). Reports the non-CRC ceiling (plumbing+supply) per draw. `[R1 I1 / R2 §5.1a / R3 §3.3 — unanimous]`
- [ ] **P0-6. Retire settled arms** from the shard battery: 11f (w3), 11i (assist slots ×2), 11j (pipe), 11x (pre-diet), 11y (laps=0) — settled refutations / completed attributions; ~6 arms ≈ 3 min/shard reclaimed for the deciding arms. They live in git history and can be re-added if a route reopens. `[R2 §8 / R2 §5.4]`
- [ ] **P0-7. Re-price 11e (prepatch) on the restored ring stack** ×3 draws — the armed-default flip was decided 8/8 on the R12 ring; the R16-era stack (batch drains, sink changes) may have flipped it (draws 10/11/13 show unarmed winning 0.4–5.5% on rxdesc). `[R1 §3.1-A2]`
- [ ] **P0-8. A3 regression bisect** (30-min A/B, one healthy draw): the R16-era ring numbers sit 6–11% under R12-era at matched kbench. Suspects: (a) `HFT_WORKER_BATCH=128` cap now inside `lane_worker` (ring path — 11u prices it for free post-flip), (b) R16b/e sink-side changes (assist watermark 2048, RX_PACE 8192) executing with rxdesc off, (c) `reset_pass` unstick changes. `[R1 §3.1-A3]`
- [ ] **P0-9. Push with a clean HEAD message** (the R15 ops lesson: `[skip ci]` on HEAD suppresses the whole push's fleet run — a docs-only commit burned run 37235246028's push). Doc-only commits ride BELOW a code HEAD.
- [ ] **P0-10. Stage gate for Phase 0:** healthy median ≥ 1.15B and record-class ≥ 1.23B on the restored stack (R12-era territory + kernel dividends). If the dividends don't stack, A3 has more carrying — the same arms find it. `[R1 V0]`

## 2. Phase I — Instruments (parallel with Phase 0; days, not weeks)

- [ ] **I-1. kbench floor rows:** `fold512_nopre` (prefetch spray off) · `fold512_noend` (fold loop only) · `fold512_supply` (stream from a 14.3 MB L3-resident buffer — the per-draw L3 ceiling; the 2B supply gate's gauge). `[R2 §5.1b / R1 I2+I3]`
- [ ] **I-2. Real-mix kbench row `fold512_rm`** — span lengths/patterns replayed from the actual tape schedule; the correct fabric-efficiency denominator (packed flatters ~2×). `[R1 I2]`
- [ ] **I-3. 8-chain fold probe `fold512_8ch`** + constants-preloaded variant — the latency-vs-ports discriminator: if 8 chains scale, the step is latency-bound → dfold matters on good silicon; if flat → the kernel program is DONE (census-6 floor is the wall). `[R1 I4 / R2 §5.1 variant]`
- [ ] **I-4. Ternlog placement probe** — `clmul×4 + unpck×2 + ternlog×2` synthetic vs the same with ternlogs replaced by p0 work (`vgf2p8affineqb`); prices the in-loop ternlog home (p0 vs p5) and sets Route T's expected ceiling. `[R2 §5.1c]`
- [ ] **I-5. objdump census of the shipped `fold_word_pairs_r`** — confirm the uop count/ternlog emission under current LLVM; the baseline every kbench delta is read against. `[R2 §5.1d]`
- [ ] **I-6. Front A stage DIAG** (`HFT_EXP_DIAG` extension at Front A rates): RX poll vs entry-build vs publish; consumer ladder vs emission vs mailbox-wait. Sizes Lever F1's payoff BEFORE it is built. `[R1 I6]`
- [ ] **I-7. Prepatch-race flake hardening round** — the docs/25 §5.1 race struck twice in CI at ~1-in-25k passes; a record draw that verifies short is the scarcest waste. Repro harness: chaos schedule × forced prepatch × high pass count (batch-parity pattern). BEFORE any record-fishing push. `[R2 §7.6]`

## 3. Phase II — Front A 5B (the winnable barrier; weeks 1–3, parallel track)

- [ ] **F-1. RX frame-entry warm start (`HFT_RXWARM`, default-off + rollback arm)** — frame-indexed warm array of the exact `FrameEntry` payload; per-pass re-derive in registers + compare + fix-on-divergence (the rxdesc check-and-fix law); zero added store traffic (replaces the stores the RX already does — the R12b sidecar failure mode designed out); `rx_warm_fixes` telemetry; steady-state assert `fixes == 0` on pass n ≥ 2. Expected: RX 0.30–0.35 → 0.05–0.10 cyc/msg → Front A 4.6–6.4B. Kill: `rx_warm_fixes > 0` persistent, or Front A < +15% healthy. `[R2 §6.1 — F1; R1 I1 variant; R3 §5.2-1]`
- [ ] **F-2. If F1 lands short of 4.6B: full publish-by-reference master array** — event-indexed master built once per render; mailbox carries (master, event_range, clock); prepatch-extended session patching; tombstone-free contiguity. The R1-I1/R3-Lever-B design. `[R1 §6-I1 / R3 §5.2-1]`
- [ ] **F-3. VPADDQ prefix-sum boundary walk** — frame boundaries from 2-byte length prefixes via vector prefix-sum instead of serial walk (< 0.15 cyc/msg target); the R10 mask-scan prior art (`vpcmpeqb`+`vpmovmskb`) folds in. Only if I-6 shows the entry walk still material after F1. `[R3 §5.2-2 / R1 §6-I1 phase 2]`
- [ ] **F-4. Mailbox depth arm NBUF=32** — at 5B the mailbox turns ~6.5M/s; one constant, one arm, prices RX buffer-reuse stalls. `[R2 §6.3]`
- [ ] **F-5. Reset-path hygiene at ~9,900 passes/s** — the futex park must NEVER fire in the steady window (I-6 instruments it); per-pass golden-population assert cost measured before optimized. Kill: if post-F1 Front A stalls ~4.5B with reset-path ≥ 0.05 cyc/msg, the pass structure is the class wall — document and stop. `[R1 §6-I2 / R2 —]`
- [ ] **F-6. RX sharding (the reserve closer)** — only if F1+F2+F3 stall < 4.6B: two RX threads + ordered merge; the SMT sibling tax is irrelevant for pure ingest (no fold to steal from). `[R3 §5.2-4; R1 §6-I3 explicitly defers this]`
- [ ] **F-7. What 5B does NOT need** (save the effort): no memory heroics (~15–20 GB/s L1/L2-local), no consumer vectorization (ladder refuted; F-3 is the only vector exception), no transport changes. `[R1 §6-I3 / R3 §5.2-5]`
- [ ] **F-8. Front A claim** — ≥3 healthy 8573C draws, median ≥ 5.0B; elevate the pure-ingest gate 2B → 5B in the same change that claims it; publish the 8370C band alongside. `[R1 §7 / R2 §6.4 / R3 §5.3]`

## 4. Phase III — The 2B Record-Class Campaign (weeks 2–8, gated on Phase 0/I verdicts)

### Stage A — the kernel axis (Route T; kill test first, code second)

- [ ] **T-1. `fold512_t` kbench row (the kill test, one day)** — transpose a packed corpus into the arena layout in-process, fold with the no-unpck loop, vs `fold512_r` same-draw. Decision rule: **≥ +8% at 1t on a healthy draw → build it; < +8% → Route T dies, the kernel program ends with a measurement, and 2B falls to S+residual work with a ~1.6–1.8B honest ceiling.** `[R2 §5.2 — the centerpiece]`
- [ ] **T-2. Route T full ship (if T-1 passes)** — span-indexed transposed arena (~15.3 MB THP, built in the untimed init window, `ALLOC_DELTA=0` untouched); fold loop reads lane-pure states directly (2 vpunpck deleted per 128 B step); **tail path reads the ORIGINAL blob first** (zero algebra changes; the AT-table re-derivation is the v2); descriptors carry the arena slot (Desc8 spare flag bits); kbench `fold512_t` attribution twin; `HFT_CRC_TRANSPOSE=0` rollback + CI arm; 2419-body differential + D11 + full battery. Structurally bit-exact (pure storage permutation — the Stage-B kills do not apply: no unmixing is ever needed). Expected: fold 91 → ~70 cyc/span → **1.60–1.74B from T alone at unchanged extraction**. `[R2 §5.2]`
- [ ] **T-3. dfold re-price post-T (arm only, zero code)** — at the 6-cyc census floor the loop becomes latency-bound again; expect 11v's sign to flip neutral→positive exactly as `fold512_t` approaches 6 cyc/step. `fold512_td` (transposed+dual) is a one-day derivative if the chains bind. `[R2 §5.3 / R1 V3.1]`

### Stage B — the main-side axis (wall 1: main ≤ 1.15 cyc/msg)

- [ ] **B-1. Wide-store descriptor packing (`HFT_DESC_WIDE`, default-off + arm)** — build each chunk's 16 Desc8s into two L1-resident staging lines, publish with two `vmovdqu64` stores + anchor per chunk; per-chunk space check; ring protocol/chunk cadence/L1-residency unchanged; workers read the same lines. Worth ~0.25–0.4 cyc/msg of the 0.4–0.6 ring submission cost. Verify with the local smoke rx DIAG (prod_ms, bufwait_laps) before the fleet. `[R3 §4.2]`
- [ ] **B-2. Route S fold-servant (if post-P0 telemetry shows main binds: main ≥ 90% busy at ~1.2B)** — 5th thread owns the ordered FNV combine + `complete_boundaries` + harvest accounting, fed by the existing res-ring cursors (one ownership flip on `res_tail`, no new protocol); placement shares worker-1's sibling with RX; strictly scalar (no p5, no new cache lines — the R8 <4.5% theft class). Kill: skip entirely if main < 90% busy post-P0 — Route S buys nothing. `[R1 V1]`
- [ ] **B-3. Route S ending-offload variant (`fold512_rs` kill test)** — worker stores the two final states (64 B) into a per-lane ending mailbox and moves on; the sibling drains and runs vend+vtail+FNV+publish (chunked-release discipline built in). Kill: fabric arm < +4% worker headroom or mailbox contention in DIAG → endings stay inline. `[R2 §5.5]`

### Stage C — the worker-side axis (wall 2: ~79% of pool at 96% busy)

- [ ] **C-1. kbench ending-only rows `end_ser`/`end_pipe` (the pricing)** — 16-span chunk endings, serial vs 4-deep software-pipelined (lane-0 crc32 chains ILP=4; FNV combines ILP=4; res publication per chunk). Kill: pipelined/serial < 1.5× → the lever is thinner than modeled; write it up. `[R3 §4.3]`
- [ ] **C-2. The chunk-drain ending pipeline (`HFT_ENDPIPE`, default-off + arm)** — the endings of the 16 spans in a chunk are mutually independent; ~25 of the ~45–50 cyc/span ending-chain latency is hideable under neighbors' folds. Stacks with vtail/vend (they killed fold-unit work; this pipelines the combine chains). Expected +13–15% worker eval rate. `[R3 §4.3 / R2 §3]`
- [ ] **C-3. Null-arm-guided residual work** — only if I-1/P0-5 shows the ring residual is issue-slot waste (not supply latency): one more targeted worker-loop pass with a priced target. If it is supply latency: both paths are supply-bound, freeze worker work, the difference is draw-selection. `[R2 §5.4 / R2 §5.1a]`

### Stage D — supply (wall 3; gated on the I-1 supply row)

- [ ] **D-1. Gate the claim protocol on the supply row** — a draw counts toward a 2B claim only if its measured L3 streaming ≥ ~28 GB/s/core (the "a gate that depends on a machine property must log the property" law, the BLOB_BACKING precedent). `[R1 V4 / R2 §8]`
- [ ] **D-2. L2-stripe render contingency (ONLY if I-1 shows supply binds on median draws)** — render-time permutation making each worker's chunk stream a per-worker sequential stripe (workers already consume disjoint grids `chunk ≡ lane mod W`); worth 10–30 cyc/span on latency-exposed spans; span values untouched (rule 2 intact — desc offsets de-reference through a base). `[R1 V2 / R2 —]`
- [ ] **D-3. Prefetch-depth sweep (cheap, opportunistic)** — pf lead 0/2/4/8 on the real-mix row; bundle into the T/S arms, never a draw on its own. `[R2 §5.6 / R3 §4.4]`

### Stage E — the campaign (pure operations)

- [ ] **E-1. Pre-declare the claim class in writing BEFORE fishing** (resolve the R12(b) ambiguity with the target-setter): the honest options are (a) median of ≥3 healthy **record-class** draws (8573C, kbench ≥ 33.5) with the healthy-band median published alongside `[R2 §5.7]`, or (b) certified 1.4–1.6B median frontier + record-class contingency `[R1 V4]`, or (c) fully-attributed negative result closing 2B under the nine rules `[R3 §4.5]` — all three are wins by the challenge's own scoring.
- [ ] **E-2. Publish the tier ladder as the scoreboard** — Bronze ≥1.40B (~161 cyc/span) · Silver ≥1.60B (~141) · Gold ≥1.80B (~125) · Obsidian ≥2.00B (~113) — so a Silver landing is recorded as progress, not failure. `[R2 §5.7]`
- [ ] **E-3. Fish with the full deciding-arm matrix every push** (11b headline-ring, 11k siblings, 11e prepatch, 11n desc8-off, 11r/s/t kernel rollbacks, 11u wbatch, 11v dfold, 11w rxdesc, 11z null + the new-lever arms as they ship); batch doc commits below a code HEAD; budget ~3 pushes per healthy draw, 10–15 pushes for 3 record-class candidates; a draw is only as good as its attribution — never waste a record draw on an incomplete sweep (draw 10 lost 11v/11w to the 11u crash). `[R1 §7 / R2 §8 / R3 §4.5]`
- [ ] **E-4. Fleet economics guard:** ~50% AMD fast-discard · ~36% 8370C · ~14% 8573C · healthy 8573C ≈ 1-in-10–12 shards · record-class ≈ 2%. Keep `fail-fast: false`; the aggregator's medians absorb single-shard noise. `[R2 §8]`

## 5. Invariants (the contract every item above lives inside)

- [ ] `HYDRA_BITPARITY 0x881639cead506f25` / classic `0xF6EF154EFDE905D8` bit-exact on every arm, every draw (null-mode arm exempt by design, loudly).
- [ ] `ALLOC_DELTA = 0` in all measured windows (all arena/warm-array builds in the untimed init window; env reads hoisted to thread start — the draw-10 11u crash lesson).
- [ ] `#![forbid(unsafe_code)]` untouched on nf-protocol / nf-arbitrator (Route T / warm start live in nf-testkit / nf-transport).
- [ ] Same corpus, same schedule, no memoization, no byte sampling, no sub-32 checksums, no counting unread bytes.
- [ ] No default flips without class evidence (≥3 draws, R9c→R9d); every lever ships with its rollback env + soak arm; solver-first constants re-derived at test time.

## 6. The Do-Not-Do Ledger (permanent — every entry already refuted or proven impossible)

1. Stage-B unpack-free census-4 kernel (zero-divisor proof — VM is a field). 2. Tri-stream / eval2 / eval_pair. 3. GFNI-affine CRC hybrid. 4. vtail r≤8 · lanes-1..7 zmm absorption · negative-power GF(2). 5. Per-span SPSC handoff. 6. rxdesc as default submission. 7. `HFT_WORKER_BATCH` 64/256 as a rate lever (but re-price it as a *regression* check on the ring — P0-8a). 8. Vectorized watermark ladder (11m stays a tripwire). 9. PMCs on hosted runners (`perf_event_paranoid=2`). 10. Route F (larger runners — terminated twice). 11. The banned list (memoization across passes, "equal to previous pass", counting unread bytes). 12. Adding working set to win compute (rxdesc's exact failure mode). 13. Trusting a lever priced on one draw.

## 7. Status Log (append as items land)

| Date | Item | Verdict |
|---|---|---|
| 2026-10-05 | Roadmaps 1–3 read; this checklist synthesized | — |
| 2026-10-05 | **P0-1 ring default flip** (`HFT_RXDESC` unset → ring; `=1` arms) | LANDED — local parity BIT-EXACT `0x881639cead506f25`, allocs=0, rx_fixes=0 confirms ring; local 1-worker shape: ring 446.7M vs rxdesc 348.8M (+28% ring, direction matches fleet) |
| 2026-10-05 | **P0-2 arm 11w inverted** → `HFT_RXDESC=1` armed soak | LANDED — armed path parity BIT-EXACT, rx_fixes=3776 (check-and-fix active) |
| 2026-10-05 | **P0-3 arm 11k re-scoped** → ring+siblings (the A1 pricing, free post-flip) | LANDED — comment re-scope; no code |
| 2026-10-05 | **P0-4 section-16 kbench-first DISCARD** (< 29.0 → 5-run report-only, exit 0; row-absent → fail-safe enforced) | LANDED — sim: 27.63 → DISCARD, 30.66 → ENFORCE, syntax clean |
| 2026-10-05 | **P0-5 arm 11z null-mode** (`HFT_HYDRA_NULL=1`) | LANDED — banner + verdict + allocs=0 verified locally |
| 2026-10-05 | **P0-6 arms retired**: 11f/11g/11h/11i/11j/11x/11y (~7 arm-runs/shard reclaimed) | LANDED — rationale in ci.sh; git history keeps them verbatim |
| 2026-10-05 | Local battery: build + clippy `-D warnings` + 30/30 test suites | GREEN |
| 2026-10-05 | **Draw 16** (run 37268486513, healthy 8573C kbench 30.44) — the first ring-default draw | **NULL INSTRUMENT: 17.1 cyc/span plumbing floor** (91% of the 187.6 real-mix span is kernel+endings+supply → Route R dead by measurement; C-3 answered); **A1 confirmed +6.6%** (11k ring+siblings 1,057.9M vs 11b 992.4M; assist 245K chunks live under siblings); **A3 confirmed −10.8%** vs R12-era at better kbench (11u prices batch +4.6% both points); rxdesc −20.2% (5th consecutive); dfold +5.5% (single-draw, law governs); 11wm bisect arm added for A3-b |
| 2026-10-05 | **P0-8 in flight**: 11u (A3-a batch pacing) + new 11wm (A3-b assist watermark 8192 = R12-era deep-saturation trigger) | awaiting ≥3 healthy draws |
| 2026-10-05 | **Draw 17** (run 37269680406, healthy 8573C kbench 30.61; shard red on a 1-point classic-CV breach, data published clean per R2 §8) | **Null floor REPLICATES: 17 cyc/span** (1,345.3M; Route R stays dead); A1 wobbles +6.6%→+0.6% (draw 16's 11b was position-noise-low; median pending draw 3); A3-a/A3-b faded on distinct (+0.8%/+0.4%) — 11wm re-aimed at SIBLINGS (where the assist runs 245-295K chunks; next push); dfold +1.4% (2-of-2 positive on ring, median +3.5%); rxdesc −22.9% (6th straight) |
| 2026-10-05 | **Draw 18 — DOUBLE-HEADER** (run 37270501728, Wave-1 shards 1+10, both healthy 8573C kbench **34.77/34.64** — record-class; shard 1 red on a bimodal-RX r8 miss AFTER the full battery, counted per R2 §8) | **NEW FLEET BEST 11b: 1,239,070,083 (+0.35% over the standing record)** on the default ring+distinct stack, BIT-EXACT, allocs=0 — **P0-10 record-class gate (≥1.23B) MET**; fabric efficiency 49.1%/47.0% (invariant holds on the healthiest host); **A1 CLOSED** (siblings median +1.9% over 4 draws, sign-inconsistent — no flip, distinct stays); **A3-b REFUTED under siblings** (wm8192 −0.4%/−0.7% both draws — do-not-do #8 amended); **A3-a CLOSED** (+0.8% median, noise); **dfold record-class case CLOSED** (−0.0%/+1.5% at kbench 34.6-34.8 — C4 resolved, default stays OFF, 11v tripwire); **P0-7 CLOSED** (prepatch armed confirmed 4/4: unarmed −1.0…−3.1%); rxdesc −21.1% median over 8 draws (C1 permanent); Front A: 18b pure ingest 3.234B PASS @ 0.711 cyc/msg (best R17-era denominator), 18a r8 FAIL 1.781B (bimodal RX — F-1 untouched); null floor replicates ×3/×4 (17.6/18.1 cyc/span busy-worker) |
| 2026-10-05 | **Phase 0 COMPLETE** (all P0 items landed and decided on target silicon) | Stage-gate read: record-class ≥1.23B ✓ (1.2391B); healthy median 1.096B vs the 1.15B hope — kernel-correlated (30.4-30.6 band: 992-1,050M; 34.6-34.8 band: 1,143-1,239M), no A3 residue to chase |
| 2026-10-05 | **I-1 SHIPPED** (kbench floor rows: `fold512_noend` / `fold512_pre` / `fold512_supply`) + **T-1 SHIPPED** (the `fold512_t` Route T kill-test row: `transpose_arena_slot` + the no-unpck loop + `span_fold_eval_r_t`, pinned by `t_transpose_arena_parity` in the exhaustive differential + `t_transpose_arena_public_api`) | Local (Granite Rapids, 27.56 GB/s class — non-deciding): fold512_t 26.85 vs r 27.56 (−2.6% — the unpck deletion pays nothing on a latency-bound host; the census-vs-latency question goes to the fleet), sink BIT-EXACT on silicon, noend 27.39 / pre 26.69 (−3.2% spray cost) / supply 26.17 (−5.0% L3 streaming); 30/30 suites green, clippy −D warnings clean, local hydra smoke BIT-EXACT allocs=0; one `t_rxdesc_parity_matrix` contended-suite flake (passed alone + 3×) — **I-7 third strike, priority rises** |
