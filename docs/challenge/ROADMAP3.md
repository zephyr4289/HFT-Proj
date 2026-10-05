# ROADMAP — Breaking the 2B Sustained / 5B Ingest Walls

**Repo:** `zephyr4289/HFT-Proj` · **Branch:** `r15-frontier` (PR #7, 21 commits, +5241/−242) · **Prepared:** 2026-10-05
**Standing records to beat:** `1,234,801,472 msg/s` sustained full verification (34.15 GB/s delivered CRC, record-class draw, kbench `fold512` 1t = 34.9) and `3,624,572,766 msg/s` Front A (0.6346 cyc/msg, RX-co-bound).
**Targets:** ≥ 2.0B msg/s sustained (55.32 GB/s delivered) · ≥ 5.0B msg/s Front A (≤ 0.46 cyc/msg @ 2.3 GHz) — nine rules intact, ≥ 3 healthy same-class draws, median decides.

---

## 0. Executive Verdict (read this first)

After reading every doc (00–29, the challenge, the tail-study artifacts), the full PR #7 diff inventory, the fold kernel (`crcfold.rs` `fold_step_r`/`fold_word_pairs_r2`), the rxdesc protocol (`rxdesc.rs`), and the complete draw ledger (draws 1–15), the situation is:

1. **The Double Helix premise is half-dead on silicon.** The R16d placement flip is *vindicated* (+9.3% to +12.2% within-draw). The R16b rxdesc array protocol is *refuted as shipped*: −21.2% vs the ring on record-class silicon (draw 10), −20.6% healthy (draw 11), and the R16e diet only bought back ~2–10 points. **Ring + distinct is the real ceiling stack (1.055B healthy / 1.214B record-class).** Your headline 11b arm is currently running a stack that loses to your own rollback arm on every deciding draw.
2. **dfold is dead as a primary lever** — neutral on contended (draw 6), healthy (draws 7, 11) silicon. Its only open case is record-class hosts (~2% of shards), where it remains armed as a contingency, not a plan.
3. **The 2B budget only closes on record-class draws** (pool 68–70 GB/s → 79–81% fabric efficiency needed) — and even that requires two levers you have not yet built: a **main-side submission fix that doesn't tax the workers** (§4.2 — wide-store desc packing, *not* the array protocol) and a **worker ending-offload** (§4.3 — the FNV/crc32 ending chain is ~26% of span cycles and has never been attacked; docs/23 §5 named it, R13 §3-D queued it, it's still open). On median healthy draws (pool 60–62), 2B needs ~91% efficiency — that is not a realistic program; the record-class campaign is.
4. **Front A 5B is the more winnable target** (Lever B "rxbuild" is designed but unimplemented; the RX entry build is the proven co-wall). Sequence it *first* — it needs no silicon lottery.
5. **Your CI is burning pushes on fixable flakes**: the run at HEAD (37235246028, shard 19) failed on the statistical gate's stddev/CV constraints (4.76 > 2.5 cyc, 106% > 25%) on a noisy 8370C that the kbench gate should have discarded before the 30-run gate ever fired. Fix the gate ordering (§3.1).

The roadmap below is ordered by expected value per unit of effort, not by intellectual appeal.

---

## 1. How You Got Here (the compressed history — what each era actually proved)

This matters because every remaining lever must be judged against the *mechanisms that already worked* versus the *class of lever that has repeatedly failed*.

| Era | Mechanism that won | Number | The lesson it paid for |
|---|---|---|---|
| R0 | Diagnosis: FNV-1a serial `imul` chain was 89% of "engine" latency | 24.4M | Measure first; harness artifacts masquerade as physics |
| TITAN (R1–R4) | Page warming, `FrameMemo` (4.41 cyc/msg), O(1) span dispatch, 8-lane hw CRC32C + prefetch | 259M 1-core | Single-core CRC ceiling ≈ 8 B/cyc is absolute |
| HYDRA (R5–R7) | Pure-vs-serial split, **16-span chunked SPSC** (anti-ping-pong law, >10× bus traffic), worker lookahead | 603M | Per-item cross-core handoff is a trap; batch or die |
| GIGAHFT (R8–R10) | VPCLMULQDQ mirror-domain fold, zero-copy 128-bit desc stores, dedicated RX thread, 64-slot assist ring | 1.109B | The RX thread decouples supply; assist recycles surplus main cycles |
| R11/R12 | Desc8 (4/line → 8/line), consumed-event prepatch, THP 2MB, placement resolution | **1.2348B** | Cache-line density and placement are levers; the record is main-thread-shaped |
| R13 | Reflect kernel: killed GFNI bit-reverse + `vpshufb` bswap (p5 census 8 → 6 per 128 B) | +4.4% kernel | The mirror-domain plumbing was the wall, not clmul |
| R14 | vend — vector Barrett ending, SPR+-gated | +4.45% record class | Endings are worth real points; class-conditional defaults are correct |
| R15 (PR #7) | vtail — lane-0 tail absorbed via 216 length-indexed GF(2) constants, r≥16-gated | neutral (±3.4% noise) | Correct derivation discipline; the serial chain it killed wasn't the binding one |
| R16 (PR #7) | dfold (T=2 block-parity, K² step) / rxdesc (array submission) / placement flip / worker diet | neutral / **−18..21% vs ring** / **+9..12%** / +1.15% | Two hits, two misses — and the misses were *predicted by the scaffold*, which is why the attribution arms exist |

**The meta-lesson of the last three programs:** every lever that moved the number did so by (a) removing work from a core that was the proven bottleneck (RX thread, Desc8 on main, prepatch), or (b) increasing density of an execution resource that was measured under-utilized (p5 census cuts). Every lever that *moved work onto a busier resource or a colder cache* lost (vtail-forced endings on r<16, rxdesc's L2/L3-resident arrays, lanes-1..7 zmm absorption). Hold that filter up against every remaining idea in this document.

---

## 2. The Physics (where the 2B budget actually stands)

### 2.1 The demand side is fixed

```
2.0e9 msg/s × 27.66 B/msg          = 55.32 GB/s delivered CRC
2.0e9 msg/s × 29.65 B/msg raw      = 59.30 GB/s raw blob streaming
                                    (the 15 MB corpus re-read ~12,200×/pass-equivalent)
```

### 2.2 The supply side, per draw class

| Draw class | kbench 1t | 2-core pool | Efficiency 2B needs | Frequency in fleet |
|---|---|---|---|---|
| Record-class 8573C | 34.05–34.9 | 68–70 GB/s | **79–81%** | ~2% of shards |
| Strong healthy | 32.8–33.5 | 64–67 GB/s | 83–86% | occasional |
| Median healthy | 30.0–30.7 | 60–61.4 GB/s | **~91%** | ~1-in-10..12 shards |
| Marginal (29.0–30.0) | 29.6 | ~59 | 94% | more common |
| 8370C healthy | ~28–29 | ~57 | 97% | common — not claim-eligible |

Your own R7 fabric-efficiency invariance says the ring plumbing delivers **48.9% ± 0.3% of pool** on every healthy draw ever measured. The best single-core real-mix ever demonstrated is **~86% of packed kbench** (docs/23). So:

- **Median-healthy 2B = 91% fabric efficiency = 6% above the best real-mix number you have ever seen, sustained, multi-core, with main+RX co-resident.** Not a program. A miracle.
- **Record-class 2B = 79–81% = 5–7 points below your single-core real-mix precedent.** Hard, bounded, and *architecturally precedented*. This is the only honest road to 2.0B.

### 2.3 The three sub-walls inside the 79–81%

**Wall 1 — main must fit in 1.15 cyc/msg.** At the record the main budget was ~1.86 (ladder 0.63 + ordered fold 0.15 + ring submission 0.4–0.6 + poll). rxdesc attacked this and pushed the cost onto the workers at a 1.5–2× markup (§2.4). The correct attack is §4.2.

**Wall 2 — workers must run at ~79% of kbench while 96% busy.** Today the ring's workers run 179–195 cyc/span at 96.2% busy and deliver 48% of pool. The gap between "busy" and "folding" is: the per-span **ending chain** (16 chained `crc32` + FNV-1a-64 8-lane `imul` chain + store/reload ≈ 26% of span cycles — docs/23 §5), the chunk-grid walk, res publication, and prefetch exposure. The ending chain is a *serial-latency* blob inside an otherwise throughput-shaped loop: `crc32` (3 cyc lat, 1/cyc TP, p1) and `imul` (3 cyc lat, p1) chain ~24–48 cyc of pure latency per span that ILP across spans can hide — nobody has pipelined it (§4.3).

**Wall 3 — memory supply.** At 2B, body reads alone are 55.3 GB/s (27.7/core) against a contended L3 band of 25–35 GB/s/core (R9), plus 59.3 GB/s raw blob streaming from a 15 MB working set that must stay LLC-resident against noisy neighbors. **Every extra byte of side-data cycling through L2/L3 directly steals fold supply.** This is *why* the rxdesc arrays lose: 8 slots × 1 MB, ~276 KB/pass/lane of L2/L3-cycling descriptor state where the ring's desc stream is L1-resident (~16 KB/lane). The diet's residue shrank from +24–29 to +8 cyc/span exactly as supply improved (draw 15, kbench 33.46) — supply-coupling confirmed by your own telemetry. Corollary: the 2B stack must have a **minimal total cache footprint**: ring desc stream, THP blob, *nothing else*.

### 2.4 Why rxdesc lost (the full causal chain, so it stays dead)

1. The premise (main-bound at 1.86 cyc/msg, workers +15% idle headroom) was measured on the R11-era *siblings-stacked* ring. R16d's flip already moved the workers to distinct cores — the system flipped to consumption-bound, so main-side submission savings bought nothing while the arrays' worker-side cost (record resolution, array reads, re-anchoring, 8-slot L2 cycling) became the marginal cost.
2. The warm start is elegant and correct — the check-and-fix fast path works (rx_fixes 5,184 total vs 8,640/pass). **The protocol is not the problem; the working set is.** Eight 1 MB arrays cannot be L1-resident; the ring's 16 KB/lane desc stream can.
3. Nine coherence laws and a diet later, the best case is −10% on the strongest draw and −18% on the median. The remaining residue is supply-coupled (draw 15's own verdict). There is no diet installment that makes 8 MB of arrays L1-resident.

**Ruling: keep `HFT_RXDESC` as an armed attribution arm, but the sustained default for the 2-worker/2-physical-core/SMT shape must be ring + distinct.** This is the single highest-value one-line change available (worth ~+100–150M msg/s on the headline arm at current draw quality, for free).

---

## 3. Phase 0 — Housekeeping (this week; zero risk, unblocks everything)

### 3.1 Fix the CI flake that burned the HEAD run

Run 37235246028 (7b8c70c, shard 19) failed in section 16: `stddev: 4.7642 > 2.5 cycles`, `cv_percent: 106.6476 > 25.0%` on a 2793 MHz 8370C that was running the uniformly-low band (657–758M sustained, kbench would have flagged it). The statistical gate fired *after* ~9 minutes of benchmarking on a host the kbench health line had already disqualified.

- Reorder: run the kbench health probe **first**; if `fold512_r` 1t < 29.0, skip the 30-run statistical gate (or run it 5x and report-only), emit `DISCARD per R12`, and exit 0. Your R12 protocol already defines discards — the gate script just doesn't implement them.
- The armed `r16_pure_ingest_target: 5000000000` verdict FAILs on every draw until Front A ships (it printed FAIL even on the 2.597B span-rate host). Mark it `INFO-until-R17` so it can't fail a shard; re-arm it in the same change that submits Lever B.
- Keep the constraint check for healthy draws only — that's when it's signal (it caught real variance on record attempts).

### 3.2 Land PR #7 in slices

21 commits, +5241/−242, six programs deep. Even solo, that's unreviewable and un-bisectable. Land in this order, each with its rollback knob already shipped:

1. **R15 vtail** — bit-exact 5/5 draws, neutral-positive median, rollback knobs, derivation scripts. Safe.
2. **R16d placement flip** — vindicated twice within-draw (+9.3%, +12.2%). Safe.
3. **The default flip to ring + distinct for the 2-worker SMT shape** (§2.4 ruling) with `11w` repurposed as the rxdesc arm.
4. **R16e diet** — +1.15% median over 4 pricings, direction positive 3-of-4. Keep.
5. **rxdesc stays merged but non-default** (armed arm) — the code is good engineering and the parity suites are valuable; it just doesn't drive the headline.
6. **dfold stays default-OFF, arm 11v armed** — its record-class case (draw 10's 11v never ran) closes opportunistically in the §5 campaign.

### 3.3 Ship the null-mode instrument (your own draw-12 conclusion)

`HFT_HYDRA_NULL` — the fold kernel stubbed to a sum-of-bytes while the full protocol runs — prices protocol cost without the kernel on every future draw. It converts the remaining attribution debate ("which ~cyc/span is the ring/rxdesc/chunk-walk?") from inference into measurement. One CI arm. Do it before any further worker-side work so the §4.3 effort is aimed by data, not by the docs' educated guesses.

---

## 4. Phase 1 — The 2B Sustained Program (the record-class campaign)

Honest shape: **three levers, each independently sized, all required.** Expected stack on a record-class draw: 1.21B (ring+distinct baseline) × 1.10–1.15 (main submission fix) × 1.10–1.18 (ending offload) → **1.55–1.65B**, then dfold + draw-class tailwind on the kbench ≥ 34.5 hosts → the 2B attempt window. This is a 4–8 week program with a real chance, not a guarantee. The alternatives are worse: median-draw 2B is arithmetically closed (§2.2), and Route F is terminated twice over (R12 + R5).

### 4.1 Baseline restack (already scoped above)

Ring + distinct default. Expected headline on record-class: **~1.21B** (draw 10's 11n already demonstrated it). This is the number the rest of the program builds on.

### 4.2 Main-side submission: wide-store descriptor packing (the rxdesc win without the rxdesc cost)

**The target:** ring submission 0.4–0.6 cyc/msg → ~0.15–0.25, bringing main to ≤ 1.0–1.1 cyc/msg — the number §5.3 of your doc assumed rxdesc would deliver.

**The mechanism:** Desc8 is 8 bytes; a 16-span chunk of descriptors is exactly 128 bytes = two 64-B lines. Main currently pays per-span 8-byte stores + the chunk-open anchor + space checks. Instead:

- Build the chunk's 16 Desc8s into two L1-resident staging lines (register-resident zmm pairs or a 128-B stack block — *not* scattered stores), then publish with **two `vmovdqu64` stores per 16-span chunk** plus the anchor store. Per-span store count drops 8×; the worker-visible ring protocol, chunk cadence, L1-resident desc stream, and every one of the anti-ping-pong properties are *unchanged* — the workers read the same lines they read today.
- The space check becomes per-chunk (one ring-slot boundary compare per 16 spans), which the ring's chunk cadence already supports.
- docs/21's store-forwarding warning applies to *consuming* stack copies on the hot path; here the staging block is produced once and store-once, no load-hit-store forwarding on the critical path. Verify with the local smoke's `rx` DIAG (`prod_ms`, `bufwait_laps`) before the fleet.

**Why this is the right shape:** it's the only remaining main-side lever that (a) doesn't add worker-side cost (unlike rxdesc), (b) doesn't change the coherence protocol (unlike rxdesc's 9 laws), and (c) is rollable per-draw (`HFT_DESC_WIDE=0`).

**Sizing evidence:** at the record, main is ~1.86 cyc/msg and the system is main-bound; §5.3's arithmetic shows 2.7B main ceiling at 0.85 cyc/msg. Wide-store packing is worth ~0.25–0.4 cyc/msg of that gap. Combined with the flip, main stops binding at ~2.0–2.2B and the consumption side (workers) becomes the sole ceiling — which is where §4.3 lives.

### 4.3 Worker-side: software-pipelined span endings (the biggest unbuilt lever)

**The target:** workers run 179–195 cyc/span on the ring at 96% busy. The ending chain — 16 chained `crc32` (lane-0 continuation through the tail), the FNV-1a-64 combine over 8 lane values (serial `imul` chain, ~3 cyc/link ⇒ ~24+ cyc of pure latency), plus store/reload — is **~26% of span cycles ≈ 45–50 cyc/span** (docs/23 §5). That is a *latency* blob: the loop is throughput-shaped everywhere else, so ~30–40 of those cyc/span are hideable.

**The mechanism:** the fold drains in 16-span chunks; the endings of the 16 spans in a chunk are *mutually independent* (each span's value definition is frozen per rule 2 — but nothing says one span's ending must retire before the next span's fold begins). Restructure the chunk drain into a software pipeline:

```
stage 1: fold steps for spans k..k+3          (p5 — the vector loop)
stage 2: lane-0 crc32 tail chains, 4 spans in flight (p1, ILP=4)
stage 3: FNV combines, 4 chains in flight      (p1, ILP=4)
stage 4: res publication                       (per chunk, not per span)
```

- With 4 spans' endings in flight, the 24–48 cyc serial latency per span overlaps the neighbors' fold work; the p1 throughput bound is 16 spans × 24 uops / 1 per cyc ≈ 24 cyc/span worst case — **~25 cyc/span saved ≈ +13–15% worker eval rate**. This stacks with the R15 vtail (which killed the tail *fold* units, not the combine chains) and with vend (which the pipeline feeds normally).
- Rule-2 compliance: bit-exact per span, verified by the existing 2419-body differential + `HYDRA_BITPARITY` per arm. No value definition changes; only *when* the work retires changes.
- **Measure first** (the house law): a kbench ending-only row (endings of a 16-span chunk, serial vs pipelined, no fold) prices the exact win before you touch the worker. If the pipelined/serial ratio isn't ≥ 1.5 on target silicon, the lever is thinner than modeled — write it up and move on.
- The null-mode instrument (§3.3) tells you how much of the 179–195 is endings vs walk vs res — aim the pipeline where the instrument says.

### 4.4 Kernel contingency: dfold's record-class case + the honest density ledger

- dfold is neutral everywhere measured because the real corpus is **span-supply-bound, not step-latency-bound** (draw 7's own reframe: at 30.66 GB/s ≈ 10 cyc/step, the kbench-realistic span corpus binds on span-level supply/endings). Its remaining open case is kbench ≥ 34 hosts where 9 cyc/step ≈ the pure latency bound. Keep 11v armed; it costs nothing.
- The p5 census floor (6 uops → 21.3 B/cyc) is real but **the unpack-free Stage B that would reach it is mathematically dead** (your zero-divisor proof is correct: VM is a field; the mixed state is unrecoverable). The GFNI hybrid is refuted (intra-byte affine). ymm dual-chain is refuted on the single-clmul-port class. **There is no known path to >21.3 B/cyc census that survives your own refutation ledger.** Budget accordingly: the kernel is a fixed resource; the program's variance lives in efficiency (§4.2, §4.3), not density.
- One honest exception worth one kbench row: **prefetch-depth sweep** (0/2/4/8 spans lead) on the real-mix row. docs/20's 4-span lead was tuned pre-R13 kernel and pre-flip. At 30+ GB/s/core the MLP window wants ~2.4 KB in flight; if the 8-span row shows +5%+ it's a free config change.

### 4.5 The claim campaign (the part that is pure operations)

The record 1.2348B *was* a record-class draw — the protocol already blesses this shape. The campaign:

1. Batch every push with the full arm ladder (you already do this) and fish until ≥ 3 healthy 8573C draws **with kbench ≥ 30.0** for the *stack's* baseline median, then continue fishing for record-class (kbench ≥ 34) draws where the 2B attempt arms run.
2. Expected cost: healthy 8573C at ~1-in-10..12 shards, record-class ~1-in-50. At 50 shards/push: **~3 pushes per healthy draw, ~10–15 pushes for 3 record-class candidates.** Plan the calendar; batch doc commits with `[skip ci]` (the R15 ops lesson — the HEAD run burned a push on a docs commit).
3. Every attempt publishes kbench beside the number (rule 8) and the raw logs (rule 9). The claim is the *median* healthy draw ≥ 2.0B — which, per §2.2, means the claim realistically settles as: record-class draws demonstrate 2B, and the ≥3-draw median claim must be over the **claim class you pre-declare**. Declare the class in the challenge doc *before* the campaign (this is exactly R12(b)'s ambiguity — resolve it in writing with whoever assigned the target, per PROMPT.md §R12, before spending the pushes).

**Probability assessment (honest):** §4.2 + §4.3 + record-class selection gets the stack to ~1.55–1.65B demonstrated, with 2B requiring the *top* of both lever ranges + a kbench ≥ 34.5 draw + dfold showing its only-positive case. I'd price it at 25–40%. The 86%-of-kbench single-core precedent is the reason it's not 10%; the multi-core efficiency invariance (49% ± 0.3% across every draw) is the reason it's not 60%. If §4.3's instrument shows the ending chain is *not* the dominant residue, stop, publish the refutation, and the honest conclusion is: 2.0B on this fleet class is closed under the nine rules — a result the challenge doc's own scoring section explicitly values ("a negative result with full attribution counts").

---

## 5. Phase 2 — The Front A 5B Program (higher win probability; sequence its build *before* the 2B campaign)

### 5.1 Why this one first

- The co-wall is *proven and localized*: docs/25's ladder refutation dropped the consumer to ~2.5 µops/frame and Front A recovered only to RX parity — the RX per-frame entry build *is* the wall, and **Lever B "rxbuild" is already designed** (event-indexed master entry array, per-turn slice publish, prepatch-extended session patching, tombstone-free contiguity — R15 worklog / PROMPT.md §1.4).
- The demand is bounded: 5B = 0.46 cyc/msg @ 2.3 GHz vs 0.6346 today — RX sheds ~27%.
- No silicon lottery: the R8 2B ingest gate already passes on healthy draws (2.6B span-rate observed even on the noisy HEAD host), and the 5B gate is armed and waiting.
- Every gate constraint is already encoded in CI (`r16_pure_ingest_target: 5000000000`).

### 5.2 The build order

1. **Lever B freeze** (per PROMPT.md §6.4): publish-by-reference frame descriptors; consumers walk frames directly. Gate behind `HFT_RX_PUBREF`, default-off, D-oracle parity + rollback soak arm — the house pattern, verbatim.
2. **The VPADDQ prefix-sum boundary walk** (< 0.15 cyc/msg target): frame-level descriptors + 2-byte length prefixes resolved by a vectorized prefix-sum scan instead of a serial walk. R10's prior-art question (mask-based batch length scanning: `vpcmpeqb` + `vpmovmskb`) folds in here — at 28 msg/frame, even a 0.1 cyc/msg walk improvement is 20% of the remaining RX budget.
3. **Re-measure, then attack whatever RX is now** (the honest law): if the entry build was half the RX budget, Lever B lands at ~0.5 cyc/msg ≈ 4.2–4.6B. The last 10–15% is either (a) the frame slice/poll itself, or (b) the ladder + dup-rejection on main re-binding. Do not guess; the Front A DIAG lines will say.
4. **RX sharding as the closer** (only if needed): two RX threads with ordered merge. R8's SMT data says a sibling slot costs the fold < 4.5% — and for *pure ingest* runs the fold isn't the constraint at all, so the sibling tax is irrelevant here. This is the cheapest remaining 2× in the whole program and it's held in reserve.
5. **Memory supply is not the wall** (your own arithmetic: 5B × ~2 B/msg actually read ≈ 10 GB/s) — the wall is pure uop/branch economics. Keep it that way: no per-message allocation, no per-message branches that the fused header decode doesn't already own.

### 5.3 The claim

Same class protocol as §4.5; formally elevate the pure-ingest gate 2.0B → 5.0B in the submitting change (your R12 ruling already encodes this). Probability: **50–60%** — Lever B + walk + sharding is a complete, sized program against a proven, localized wall, with no draw-quality dependency anywhere near the 2B program's.

---

## 6. The Do-Not-Do List (every item is already refuted by your own ledger — restated so future-you doesn't re-derive them)

1. **Stage-B unpack-free census-4 kernel** — zero-divisor proof (VM is a field). Dead mathematically, not empirically.
2. **Tri-stream / eval2 / eval_pair interleave** — port-issue-bound, not chain-bound (docs/24 §7, arms 11g/11l).
3. **GFNI-affine CRC hybrid** — affine is intra-byte; p0 anyway (Task 7 verdicts).
4. **vtail for r ≤ 8; lanes-1..7 zmm absorption** — refuted locally (R15).
5. **Per-span SPSC handoff** — anti-ping-pong law, >10× (docs/20).
6. **rxdesc as the default sustained path** — −18..21% across two classes, residue supply-coupled (draws 10–15). Armed arm only.
7. **`HFT_WORKER_BATCH` 64/256** — no signal on any draw (arm 11u).
8. **Vectorized watermark ladder** — refuted; 11m stays a tripwire.
9. **PMCs as an instrument on hosted runners** — `perf_event_paranoid=2`; kbench differential probes are the only instrument. (R6 remains open as a *cheap probe*, but don't build a program that depends on port counters.)
10. **Route F (bigger runners)** — terminated twice (R12 ruling + R5 policy: no µarch pinning, per-minute billing, ARM64 has no VPCLMULQDQ).
11. **Anything on the banned list** — memoization across passes, byte sampling, sub-32 checksums, "equal to previous pass", counting unread bytes. Not research.

And the two *renewed* warnings from this analysis:

12. **Don't add working set to win compute.** Every loss since the record (rxdesc arrays, the diet's chase) traces to bytes cycling in L2/L3 where the winning path had L1-resident state. The 2B stack's cache budget is: THP blob + ring desc stream + worker stacks. Nothing else.
13. **Don't trust a lever priced on one draw.** ±3.4% arm-position noise, ±10% host noise; the diet's 4 pricings (+1.4/+0.9/+0.0/+2.6%) is what a real small win looks like in this fleet — most "wins" bigger than that on a single draw have been noise or regression. The ≥3-draw median is not bureaucracy; it's the only signal discriminator you have.

---

## 7. Consolidated Timeline

| Week | Work | Exit criterion |
|---|---|---|
| 1 | §3.1 gate reorder + 5B gate to INFO; §3.2 PR slices 1–3 (vtail, flip, ring+distinct default); §3.3 null-mode instrument shipped | HEAD run green on a marginal host; headline 11b = ring+distinct on every draw |
| 2 | §4.2 wide-store desc packing (local + one fleet ladder: 11b vs 11b-wide) | main DIAG shows submission ≤ 0.25 cyc/msg; no worker regression on 2 draws |
| 2–3 | §4.3 kbench ending-only row (serial vs pipelined) → if ≥ 1.5×, the chunk-drain pipeline; parity matrix + soak in both worker shapes | +10%+ worker eval rate on the DIAG; bit-exact everywhere; 2 healthy draws agree |
| 3–4 | §4.4 prefetch-depth kbench row; dfold record-class case closes opportunistically | one row; no program risk |
| 4–8 | §4.5 record-class fishing campaign (full ladder every push, `[skip ci]` for docs) | ≥ 3 healthy stack draws + record-class attempts logged; claim class pre-declared in writing |
| **parallel, weeks 1–3** | §5.2 Lever B freeze + VPADDQ walk (Front A) | R8 gate margin grows; Front A ≥ 4.2B on a healthy draw |
| week 4+ | §5.2 step 4 RX sharding *only if* Lever B stalls < 4.6B | 5B gate PASS on 3 healthy draws → claim |

**The two deliverables at the end:** a 2B record-class claim (25–40%) or a fully-attributed negative result that closes the 2B question on this fleet class under the nine rules — and a 5B Front A claim (50–60%) that needs no luck at all. Both are wins by the challenge doc's own scoring.

---

## Appendix A — Draw ledger (consolidated from docs/28 §6 + docs/29 §9; the numbers every claim in this document rests on)

| Draw | Class | kbench 1t | Stack | Sustained | Key verdict |
|---|---|---|---|---|---|
| 1 | 8370C noisy | 27.6 | R14 | 865.4M | uniformly-low band |
| 2 | 8370C healthy | — | R14 | 945.9M | — |
| 3 | 8370C marginal | — | R14 | 889.2M | R8 gate −0.9% |
| 4 | 8573C healthy | 30.00 | R15 | 1,061.8M | vtail deciding draw (neutral) |
| 5 | 8370C healthy | — | R15 | 964.8M | — |
| 6 | 8573C contended | 29.61 | R16 | 1,015.3M | dfold +0.01% (supply-bound) |
| 7 | 8573C healthy | 30.66 | R16 | 1,096.1M | dfold −0.23%; Route S promoted |
| 8 | 8573C noisy | 28.02 | R16 pre-rxdesc | 1,177.9M | best pre-R16b; distinct pool 55.3 |
| 9 | 8370C healthy | — | R16 pre-rxdesc | 943.2M | class band confirmed |
| 10 | 8573C **record-class** | 34.05–34.9 | R16b+R16d | **11n ring 1,214.2M** / 11b 957.3M / 11k siblings 876.4M | flip +9.3%; rxdesc −21.2%; dfold case open |
| 11 | 8573C healthy | 30.16 | R16b+R16d | **11w ring 1,060.6M** / 11b 842.1M | rxdesc −20.6%; gap 50/50 wake/dilution; flip +12.2% |
| 12 | 8573C healthy | 30.23 | R16e diet | **11w 1,054.5M** / 11b 862.3M | diet +1.4%; wake-cadence fixed; residue supply-coupled |
| 13 | 8573C healthy | 32.79 | R16e diet | **11w 1,124.2M** / 11b 945.5M | diet +0.9%; ring residue ~+20 cyc/span |
| 14 | 8370C noisy | 27.59 | R16e diet | 11w 991.7M / 11b 668.8M | discard |
| 15 s5 | 8573C healthy (strongest) | **33.46** | R16e diet | **11w 1,098.1M** / 11b 986.4M | diet +2.6%; residue +8 cyc/span; array working set = the suspect |
| 15 s7 | 8573C healthy | 30.54 | R16e diet | 11w 1,084.8M / 11b 840.2M | diet +0.0% |
| — | 8573C **record** (R12 era) | 34.9 | R12 | **1,234.8M** / 3.625B Front A | the record; 49% of pool |

## Appendix B — Gate arithmetic (one screen)

```
Demand @2B:   55.32 GB/s delivered  (2e9 × 27.66 B)
Pool:         2 × kbench1t = 60–70 GB/s (healthy → record class)
Efficiency:   49% today (ring, invariant) → 79% needed (record) → 91% (median)
Worker view:  96% busy at 179–195 cyc/span; ending chain ≈ 45–50 cyc/span (unpipelined);
              kernel+ending floor ≈ 2.8–3 cyc/msg; the rest is walk/res/prefetch/supply
Main view:    ≤ 1.15 cyc/msg allowed at 2B; ring stack = 1.18–1.38 → wide-store packing (§4.2)
Supply view:  55.3 GB/s body reads vs 25–35 GB/s/core contended L3 → L1-resident everything else
Front A @5B:  0.46 cyc/msg vs 0.6346; RX entry build = proven co-wall; Lever B designed, unbuilt
```

## Appendix C — CI arm inventory additions proposed by this roadmap

| Arm | Meaning |
|---|---|
| 11w' | **ring default inverted** (rxdesc armed) — the §2.4 ruling, headline becomes ring+distinct |
| 11x-wide | wide-store desc packing (§4.2), `HFT_DESC_WIDE` |
| 11y-pipe | chunk-drain pipelined endings (§4.3), `HFT_ENDPIPE` |
| kbench `end_ser` / `end_pipe` | ending-only rows, serial vs 4-deep pipelined (§4.3 pricing) |
| kbench `pf{0,2,4,8}` | prefetch-lead sweep on the real-mix row (§4.4) |
| 11z-null | `HFT_HYDRA_NULL` protocol-cost instrument (§3.3) |
| section 16 | gate reordered: kbench health first; statistical constraints healthy-draws-only; 5B ingest verdict INFO until R17 (§3.1) |
