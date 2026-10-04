# PROMPT.md — R16 "DOUBLE HELIX" DEEP-RESEARCH BRIEF
### Sustained Full Verification ≥ 2.0B msg/s · Pure Ingest (Front A) ≥ 5.0B msg/s · Zero Cheating
**Repo:** `zephyr4289/HFT-Proj` (branch `r15-frontier`, PR #7 open) · **Prepared:** 2026-10-04 · **Author:** Lead Principal Systems Architect · **Researcher:** you.

You asked what research I need. This file is the complete brief. Read §1 (context), §2 (why the targets are hard — the budget math), then execute the workstreams in §3 in the priority order of §4. §5 lists everything already refuted — do not spend a minute there. §6 is what I build while you research. Bring back data in the format of §7; the command recipes are in §8.

---

## 0. THE MISSION

Cross two walls on the CI-hunted dedicated silicon (the 8370C Ice Lake / 8573C SPR-EMR class draws we fish for in the 50-shard fleet), with **no cheating and no bypassing** (the 9-rule law of `docs/challenge/CHALLENGE-R13-p5-wall.md` §4, restated in §1.5):

1. **Sustained full verification ≥ 2,000,000,000 msg/s** — every emitted byte read and CRC32C-verified in-window, every pass, `HYDRA_BITPARITY == 0x881639cead506f25` bit-exact, `ALLOC_DELTA == 0`, same corpus/schedule, ≥ 3 independent healthy draws of the same class.
2. **Pure ingest (Front A) ≥ 5,000,000,000 msg/s** — the RX-pipelined transport path at ≤ ~0.46 cyc/msg on the 8573C clock, without regressing a single existing gate.

Standing records to beat: **1,234,801,472 msg/s sustained** (34.15 GB/s delivered CRC, 8573C record draw, kbench `fold512 1t` = 34.9 GB/s) and **3,624,572,766 msg/s Front A** (0.6346 cyc/msg, RX-co-bound). The 2B target is **+62%** over the record; the 5B target is **+38%**. Nothing in the current pipeline's physics gets us there by tuning — we need new structural levers, and five unknowns must be resolved before I commit silicon to them. That is your research.

---

## 1. WHERE THE MACHINE STANDS (context you must carry)

### 1.1 The fifteen-generation ledger (what was tried, what won)

| Gen | Program | Mechanism | Result |
|---|---|---|---|
| R0 | Baseline | FNV-1a-64 hash-trap diagnosis | 24.4M msg/s |
| R1–R4 | TITAN | Page warming, `FrameMemo` verdicts (4.41 cyc/msg), O(1) span dispatch, 8-lane hw CRC32C + prefetch | 259M single-core |
| R5–R7 | HYDRA | Pure-vs-serial split, 16-span chunked SPSC rings (anti-ping-pong law), worker lookahead prefetch | 603M fabric |
| R8–R10 | GIGAHFT / TeraPhase / Assist | VPCLMULQDQ mirror-domain folding, zero-copy 128-bit ring descriptors, dedicated RX transport thread, 64-slot assist ring recycling surplus submitting-core cycles into SIMD CRC | 1.109B sustained, 3.625B ingest |
| R11–R12 | Desc8 / Ladder | 8-byte compact descriptors, prepatch engine, THP 2MB grants, placement topology resolution | **1.2348B sustained (record)**, 3.624B ingest |
| R13 | Reflect | Natural-domain reflected fold kernel — killed the GFNI bit-reverse + bswap shufb (p5 census 8→6 per 128B step) | record draw 1,234,210,590; reflect beats mirror +4.4% |
| R14 | vend | Vector Barrett ending (CPUID-gated SPR+; refuted on 8370C's single clmul port) | +4.45% on record-class draw |
| R15 | vtail (PR #7, this branch) | Lane-0 tail absorbed into the vend field via 216 length-indexed GF(2) constants, r≥16-gated; full rollback knobs (`HFT_CRC_VEND`, `HFT_CRC_VTAIL`) + attribution arms 11s/11t + kbench `fold512_r/rv/rc` triple | bit-exact 5/5 draws; **neutral (+0.37%, within the ±3.4% arm-position noise floor)** on the first deciding-class 8573C draw |

Key structural facts: corpus = `sample-mini.itch` (15,000,000 bytes, **505,849 msgs/pass**, sha256-pinned, avg 29.65 B/msg raw, ~27.66 B/msg CRC'd stream), dual-feed `MtuBound(1400)` schedule, virtual-clock pacing, transport is **pre-rendered memory frames** (no kernel networking in the bench paths). The verification fabric: main core (pacing → poll → framing → session dispatch → dup rejection → watermark sequencing → span emission) + dedicated RX thread + HYDRA workers (8-lane strided VPCLMULQDQ fold + assist ring). Worker pool sized from `available_parallelism`.

### 1.2 The draw history (the honest bar — from docs/28 §6)

| # | Silicon | Health | 11b sustained | Front A | kbench 1t (r/rv/rc) |
|---|---|---|---|---|---|
| 1 | 8370C | noisy host (uniformly −10%) | 865.4M | 1.67B | 27.6 / 27.7 / 28.8 |
| 2 | 8370C | healthy | 945.9M | — | — |
| 3 | 8370C | marginal (R8 gate −0.9%) | 889.2M | 1.982B | — |
| 4 | **8573C** | **healthy mid-band** | **1,061.8M** | **3.234B** | 30.00 / 29.99 / 29.39 |
| 5 | 8370C | healthy tight | 964.8M | 2.781B | — |
| — | 8573C | **record draw (R12-era)** | **1,234.8M** | **3.625B** | **34.9** (fold512 1t) |

Fleet noise measured: ±3.4% between byte-identical arm positions; ±10% for noisy hosts. The R8 pure-ingest 2B gate has flaked twice on noisy/marginal draws (1.67B, 1.982B) — the **median healthy draw is the real bar**, not the record.

### 1.3 The current fold kernel (the thing we must make 1.5–2× faster)

Per 128-byte step of the R13 reflect kernel (`fold_step_r`, crcfold.rs): **4× `vpclmulqdq` zmm + 2× `vpunpckqdq` + 2× `vpternlogq`** — p5 census 6 uops, no GFNI, no bswap. Measured density on the record draw: **14.3 B/cyc ≈ 9 cyc/step**. The clmul-only floor is 4 cyc/step = 32 B/cyc (~2.2× today). The unpack-free 64-bit-granularity formulation would drop the p5 census to 4 → ~6 cyc/step = 21.3 B/cyc if port-bound. Endings are already optimized (R14 vend, R15 vtail — both pinned by 2419-body exhaustive differentials and 216-constant test-time re-derivation). The kernel is where sustained throughput lives: at the record, delivered CRC = 34.15 GB/s = kbench 1t (34.9) × 98% — **the sustained number IS the kernel number times fabric efficiency.**

### 1.4 The two targets' physics in one paragraph each

**Sustained 2B:** delivered CRC stream at 2.0e9 msg/s × 27.66 B/msg = **55.3 GB/s**. The hunted box gives each draw **2 physical cores × SMT (4 vCPU)**; fold gains nothing from SMT (2cpu_smt = 1t: 32.83 = 32.84 GB/s), so the fold ceiling is 2 × kbench1t = 60–70 GB/s on healthy draws. The record draw ran at 49% of that ceiling (34.15/69.8) because the rest of the budget went to the serial path, RX, endings, and ring/assist traffic. **2B demands 79–92% of the 2-core fold ceiling while also feeding a ~0.5-core serial/ingest path — arithmetic-impossible at today's 14.3 B/cyc density.** Either density rises to ≥ ~21 B/cyc (kernel lever), or the hunted machine gets bigger (fleet lever), or the serial path moves onto cycles the fold can't use (SMT/assist lever). All three need your data.

**Front A 5B:** record is 3.625B at **0.6346 cyc/msg, RX-co-bound** — the RX thread's per-frame entry build is the wall (docs/25's ladder refutation proved it: consumer-side cost dropped to ~2.5 µops/frame and Front A recovered only to RX parity). 5B needs ≤ ~0.46 cyc/msg at 2.3 GHz (≤ 0.56 on the 8370C's 2.79 GHz). The designed fix is **Lever B "rxbuild"**: publish-by-reference master entry array — event-indexed master built once per render, per-turn slice publish, prepatch-extended session patching, tombstone-free contiguity — designed in the R15 worklog, not yet implemented. It removes the per-frame entry build entirely. Your research (R10, R8) de-risks and extends it.

### 1.5 THE LAW (every researched technique must pass all 9 — no cheating, no bypassing)

1. Bit-exact: every pass reproduces `0x881639cead506f25` (and `0xF6EF154EFDE905D8` classic); D11 differential vs scalar reference + table-driven CRC32C.
2. Same value definition: `span_crc32c_8lane` semantics (8 strided lanes + tail + FNV-1a-64 combine + length) are frozen.
3. Every emitted byte is read and CRC-verified in-window, every pass, on a worker or assist core.
4. No memoization across passes; no blob-generation hashing; no diff-vs-previous-pass.
5. Same corpus (`sample-mini.itch`), same schedule, 505,849 msgs/pass, same sha256.
6. `ALLOC_DELTA == 0`; `#![forbid(unsafe_code)]` stays on `nf-protocol` / `nf-arbitrator` (SIMD lives in `nf-testkit`, untouched by this rule).
7. No shrinking the verified byte count (the existing fair alias-dedup accounting is the floor).
8. No silicon shopping — report the draw's kbench next to every result; draw-adjusted comparisons only.
9. Reproducibility: raw CI logs published; **≥ 3 independent healthy draws of the same class** for any record claim.

Banned by name: sub-32 checksums, byte sampling, "equal to previous pass", counting bytes verified without reading them.

---

## 2. THE BUDGET MATH (why I need exactly this research)

### 2.1 Sustained 2B — the resource ledger on the hunted box

The hunted draw (8573C class): ~2.3–2.6 GHz, 2 physical cores × SMT = 4 vCPU. Per 5.0s pass at 2.0B msg/s:

| Resource | Demand at 2B | Supply (healthy draw) | Verdict |
|---|---|---|---|
| CRC fold throughput | 55.3 GB/s delivered (= 2e9 × 27.66 B) | 2 × 30.0–34.9 GB/s (fold ignores SMT) | **79–92% of ceiling — no room for the rest; needs density ≥ ~21 B/cyc** |
| Main-core serial path | pacing + poll + framing + dispatch + emission + FNV + ring: today ≈ 1.97 cyc/msg-equivalent at the 1.2348B record → at 2B the whole core budget is ~1.2 cyc/msg | 1 core (+ its SMT sibling) | needs ~40% serial-path cut |
| Ingest (main) | 0.63 cyc/msg today → 0.52 core at 2B/2.44 GHz | shares main core | must coexist or move |
| RX thread | per-frame entry build (0.63 cyc/msg at Front A rate; sustained-rate share smaller but nonzero) | 1 sibling slot | Lever B kills the entry build |
| Worker supply bandwidth | 55.3 GB/s reads of a ~14–16 MB working set (L2=2MB/core, LLC shared+noisy) | per-core L2/L3 read BW: **UNKNOWN — R9** | the sleeper risk |
| Endings + FNV | R14/R15 already cut; ~0.16 cyc/msg FNV is frozen by rule 2 | p1 cycles, assist ring | assist redesign — R8 |

Three independent routes to 2B, all research-gated:
- **Route K (kernel):** fold density 14.3 → ≥21 B/cyc (9 → ≤6 cyc/128B step). Unlocks 2× fold ceiling → 55.3 GB/s = ~53% of a 2-core 104 GB/s ceiling — schedulable. Gated by R1 (uops tables), R3 (GFNI hybrid), R6 (PMCs), R11 (unpack-free algebra).
- **Route F (fleet):** hunt a machine with ≥4 physical cores of the same class (larger runner / different pool). 2B becomes a fabric-scaling problem I know how to solve. Gated by R5, R12, R4.
- **Route S (scheduling):** keep 14.3 B/cyc but move ALL non-fold work off the fold cores' critical ports (assist on siblings using p1 scalar crc32 + p0; fold at 95%+ utilization on both physical cores). Ceiling ~66–78 GB/s × fabric efficiency ~85% → 56–66 GB/s delivered — *just* covers 55.3 if everything is perfect. Gated by R8, R9, R6.

My read: 2B needs Route K or Route F; Route S alone is knife-edge. Your data decides which.

### 2.2 Front A 5B — the RX ledger

- Record 3.625B = 0.6346 cyc/msg, and the RX thread is the proven co-wall (docs/25: consumer-side dropped to ~2.5 µops/frame, Front A only recovered to RX parity; "the next Front A lever is the RX itself").
- 5B = 0.46 cyc/msg @ 2.3 GHz / 0.56 @ 2.79 GHz. The per-frame entry build (frame header parse → per-message entry construction → mailbox publish) must nearly vanish → Lever B publishes frame descriptors + offsets by reference and consumers walk frames directly.
- Secondary walls at 5B nobody has measured: (a) the **frame-walk itself** (2-byte length prefixes, ~28 msg/frame at 1400B MTU — the walk is ~1 branch + 1 load per message even in the best case: is <0.2 cyc/msg walk possible with mask-based skip scanning? R10 prior art); (b) **session/dup/watermark** per-frame costs; (c) the **virtual-clock pacing + render** side (is the generator ever the wall on a healthy draw? R7 logs answer this); (d) whether one RX core can be split into 2 sharded RX threads with ordered merge (R8 SMT data).
- Also: 5B×~2 B/msg actually read by the walk ≈ 10 GB/s + frame headers ≈ trivial — memory supply is NOT the Front A wall; pure uop/branch economics.

### 2.3 The five unknowns (each maps to workstreams)

| # | Unknown | Workstreams |
|---|---|---|
| U1 | What truly binds the fold step on ICL vs SPR/EMR — p5 issue, dependency latency, or load supply? (never measured; no PMC data exists) | R1, R2, R6, R7 |
| U2 | Which kernel family hits the per-class floor: zmm reflect, ymm dual-chain, unpack-free 64-bit-granularity, or GFNI-affine hybrid? | R1, R2, R3, R11 |
| U3 | How many physical cores + uncontended cache bandwidth does the hunted VM actually deliver — and can we hunt bigger? | R4, R5, R9, R12 |
| U4 | What do SMT siblings steal from a saturated fold — how much serial/assist can live there? | R4, R8 |
| U5 | What are the honest healthy-draw medians and draw frequencies (record claim needs ≥3 healthy draws ≥ target)? | R7, R12 |

---

## 3. THE RESEARCH WORKSTREAMS

Format per workstream: **Q** (the question) · **Why** (what decision it unlocks) · **Data** (exactly what to bring back) · **Sources** (where to look) · **Unlocks** (the build decision it gates). Everything marked TO-VERIFY is a hypothesis I currently cannot check from inside the sandbox — your job is to convert it to VERIFIED with a source.

---

### R1 — THE UOPS TABLES (the kernel floor question) — *top priority*

**Q:** For the exact cores we hunt — Ice Lake server (Sunny Cove, 8370C), Sapphire Rapids (Golden Cove, 8573C), Emerald Rapids (Raptor Cove), Granite Rapids — what are the **port assignment, latency, and reciprocal throughput** of the instructions my kernels are made of?

**Why:** The entire Route K decision tree hangs on these numbers. Examples of what flips on a single table cell: if `vpclmulqdq zmm` is a single uop at 1/cycle on p5 on Golden Cove, the 4-clmul step floor is 4 cycles and the census-4 unpack-free kernel floors at ~4–6 cyc/step (21–32 B/cyc — Route K lives). If zmm clmul is cracked into 2 uops on Ice Lake (TO-VERIFY — I suspect it is on Sunny Cove server), the 8370C class wants ymm dual-chain instead. If `vgf2p8affineqb zmm` issues on p0 (not p5), the GFNI hybrid (R3) becomes the only path to >2× density. If `vpternlogq`/`vpxorq` zmm run on 3 ports (p0/p1/p5), the XOR plumbing is free and the census math simplifies.

**Data (one markdown table per µarch, every cell with a source link):**

| Instruction | µarch | Ports | Latency | TP (uops/cyc) | uops (cracked?) | Source URL |
|---|---|---|---|---|---|---|

Instructions (all at xmm/ymm/zmm widths where they exist): `VPCLMULQDQ`, `PCLMULQDQ`, `VGF2P8AFFINEQB`, `VGF2P8AFFINEINVQB`, `VPUNPCK{L,H}QDQ`, `VPTERNLOGQ`, `VPXORQ`, `VPSHUFB (zmm)`, `VPERMB/VPERMD`, `VPSRLQ/VPSLLQ (zmm)`, `VMOVDQU64` (load & store), `VPGATHERQQ` (long shot), `CRC32` (GPR, r32/r64 forms), `KORQ/KANDQ/KMOVQ`, `VPMOVMSKB`, `RDTSC/RDTSCP`, and `PREFETCHT0/T2` (does it cost a uop on these cores?).

**Sources:** uops.info (primary — it has measured Ice Lake server (e.g. Xeon 8360Y/8380) and Sapphire Rapids (Xeon 8480/8470-class) entries; check whether Emerald Rapids is listed separately or whether SPR numbers carry); Intel Optimization Reference Manual (port tables per µarch, the "SMT" and "512-bit" sections); Agner Fog's tables (older but cross-check); wikichip.org per-µarch pipeline pages (Sunny Cove, Golden Cove, Raptor Cove, Redwood Cove).

**Unlocks:** U1, U2 — the per-class kernel family choice and the theoretical density ceiling; whether the challenge doc's "clmul floor = 4 cyc/step, 32 B/cyc, ~65 GB/s per physical core" claim is real on 8573C.

---

### R2 — PUBLISHED CRC32C SPEED RECORDS (the competitive landscape)

**Q:** What is the fastest **publicly documented single-core CRC32C throughput** on Ice Lake / Sapphire Rapids / Emerald Rapids / Granite Rapids, and what technique achieves it?

**Why:** Our fold does 30.0–34.9 GB/s 1t on 8573C draws. If any public implementation beats that on the same class, it is a portable kernel lead and Route K's shortcut. If nothing public beats ~35 GB/s, we are at the known frontier and Route K must come from R3/R11 research instead of porting.

**Data:** a table {implementation, version, µarch (exact CPU if stated), GB/s single core, buffer size used, technique (xmm/ymm/zmm fold, GFNI, slices), source link}. Include negative/contradictory results too (they bound the frontier).

**Sources (checklist — verify each, don't assume):**
- **Intel ISA-L** (`github.com/intel/isa-l`) — `crc32_iscsi_*` benchmarks; **TO-VERIFY: does ISA-L ship a `crc32_gfni` family, and what are its numbers on SPR/EMR?** This is the single most important row: if ISA-L's GFNI path materially beats the clmul fold on SPR, Route K's design changes completely.
- zlib-ng (`crc32c` via `crc32` backends), Cloudflare's fork + their blog engineering posts on CRC32C/copy speed, Google `crc32c` (crc32c-c-hw etc.), Linux kernel `crc32c-intel` benchmarks, Stephan Brumme's CRC32C page (fastest CRC32C writeups), the original folding paper (Gopal, Gulley, et al., *"Fast CRC Computation for Generic Polynomials Using PCLMULQDQ"*, Intel) and any 2023–2026 follow-ups covering GFNI/AVX-512.
- Academic search: "CRC32C AVX-512", "carry-less multiplication folding throughput", "GFNI CRC", "billion messages per second feed handler", "market data parser SIMD".

**Unlocks:** U2 — either a direct kernel lead (port + re-pin with our 2419-body differential) or proof we're at the frontier.

---

### R3 — THE GFNI BIT-MATRIX CRC HYPOTHESIS (the only >2× density candidate I see)

**Q:** Can CRC32C be reformulated as byte-wise GF(2) affine transforms (`vgf2p8affineqb` / `vgf2p8affineinvqb`) that issue on **p0**, co-issuing with the p5 clmul fold — or replacing it — to break the 4-clmul-per-step p5 monopoly?

**Why:** This is the mathematical heart of Route K. The 2B target needs fold density ≥ ~21 B/cyc; clmul-only floors at 32 B/cyc *if* p5 is the only binding port and TP=1; a p0+p5 hybrid could in principle halve the step time again. The algebra is plausible (CRC is a GF(2)-linear map of the message for fixed length; per-byte affine = precomputed matrix powers of the bit-reversal/companion matrix; the state-dependence folds exactly like clmul constants do — same mathematics, different primitive), and it is exactly the kind of thing the R13 lesson warns must be derived, probed, and exhaustively pinned before any silicon claim.

**Data:**
1. Every public instance of CRC-via-GFNI: implementations, papers, blog posts, ISA-L internals, zlib experiments (search: "crc32 gfni", "vgf2p8affineqb crc", "bit matrix crc32", "crc32c affine"). Include failed attempts and why they failed.
2. The port/TP cells for `vgf2p8affineqb` ymm/zmm on ICL vs SPR vs EMR vs GNR from R1 (p0 vs p5 vs dual — this decides everything).
3. Any measured GB/s for GFNI-based CRC or hash kernels on these µarchs (even for other polynomials/algorithms — I only need the primitive's throughput evidence).
4. If you find NOTHING public: say so explicitly — that's a positive result (novel territory; I'll derive it locally with the solver discipline, but then R1's port data becomes the only feasibility evidence).

**Unlocks:** U2 / Route K's stretch case. If both the algebra evidence and p0 issue rate check out, the step floor could reach ~2–3 cyc/128B (43–64 B/cyc) and 2B on the current box becomes comfortably schedulable. If either fails, Route K = R11's unpack-free census-4 kernel (~21 B/cyc ceiling) and 2B leans on Route F.

---

### R4 — THE HUNTED SILICON'S EXACT FACTS (SKU datasheets)

**Q:** What exactly *are* the machines behind "8573C" and "8370C" draws — and what does a 4-vCPU CI VM on them physically get?

**Why:** Route F and the supply question U3. Concrete sub-questions: 8573C is an Azure-exclusive Xeon (TO-VERIFY: Emerald Rapids generation, which VM series runs on it — Dsv5/Esv5/M-series?), host core count (64C? 56C?), LLC size (TO-VERIFY ~300MB class), base vs all-core turbo (our draws clock ~2.3–2.6 GHz — which ends of the curve are we seeing?), and whether Azure partitions LLC per VM (I believe it does NOT — noisy-neighbor LLC explains our ±10% draw variance, but VERIFY). Same for 8370C (Ice Lake, Dv4-class?). Also: are GitHub `ubuntu-latest` runners 2-phys×SMT VMs (matching our topology capture), or full 4-vCPU pinned differently?

**Data:** a spec sheet per SKU {generation, core layout, L2/L3 sizes, mesh/cha topology, clocks (base/all-core/single), TDP, host core count, Azure series mapping, LLC partitioning policy, SMT port-sharing notes} + a "what our 4-vCPU VM actually gets" summary. Every claim sourced (wikichip, Azure docs, Intel ARK-ish datasheets for the C-SKUs, community CPU-inventory sites that list which Azure series run which Xeons).

**Unlocks:** U3, U4 — whether 55.3 GB/s of fold supply is schedulable on the current hunt, what draw variance to expect (and therefore how many pushes a ≥3-draw record claim costs — R7 cross-ref), and whether the SMT-sibling slots are worth farming (Route S).

---

### R5 — GITHUB FLEET MECHANICS (the hunt distribution & the bigger boats)

**Q:** (a) What CPUs do GitHub-hosted `ubuntu-latest` / `ubuntu-22.04` / `ubuntu-24.04` runners actually run in late 2026, and how do our draws distribute across them? (b) Can we get **bigger or dedicated machines** — larger runners (8/16/64-core), labels, public-repo entitlements, self-hosted — on the same Xeon class? (c) What are the hard fan-out limits (jobs per workflow, matrix size, concurrency) and artifact retention/API quotas we'd hit when harvesting logs at scale (R7)?

**Why:** "The dedicated CPU we hunt in CI" — today we fish 50 shards per push on shared `ubuntu-latest` and keep whatever the pool gives (the AMD-heavy pool problem: run 37189060243 was 50/50 non-target). If an 8-core+ runner of the 8573C class is reachable (larger runners for public repos — TO-VERIFY current 2026 policy and pricing; TO-VERIFY whether larger runners are even on Xeon Platinum Azure SKUs vs EPYC), the entire 2B program restructures from kernel-heroics to fabric-scaling, which is the kind of problem this codebase has solved five times. Also: if draws can be pre-filtered (runner labels by CPU family), we stop burning pushes on AMD shards.

**Data:** (a) GitHub's runner-images repo/docs + changelog: current CPU inventory per linux label (and any announced migrations — e.g., if `ubuntu-latest` moved generations in 2025–2026, our draw bands shift and old baselines need draw-adjustment). (b) Larger runners: availability for **public** repos, core-size menu, whether CPU family is selectable/labelable, per-minute cost, region options; also self-hosted/Arc policy for this repo. (c) Limits: max jobs per workflow run, max concurrent jobs (free tier public repo), matrix ceiling, artifact retention default (90d?) and API rate limits for the R7 harvest. (d) Any community data on runner CPU consistency (do "the same" labels hit heterogeneous SKUs?).

**Sources:** docs.github.com (Actions docs: runner types, larger runners, usage limits), github.com/actions/runner-images (issues + readme list actual hardware), GitHub community/forum threads on runner CPU models, billing docs.

**Unlocks:** Route F feasibility (with R12's ruling), the R7 harvest plan, and draw-quality calibration for every future claim.

---

### R6 — PMC / perf AVAILABILITY ON THE RUNNERS (the Step-0 unlock)

**Q:** Can a GitHub-hosted ubuntu runner execute hardware performance counters — `perf stat -e cycles,instructions,UOPS_DISPATCHED_PORT.PORT_0..PORT_7` (or the topdown slots group) — inside a workflow step? What is `/proc/sys/kernel/perf_event_paranoid`? Are hardware PMCs exposed through the Azure hypervisor at all (vPMC passthrough)? Is `turbostat`/MSR access available? Are there NMI/watchdog conflicts?

**Why:** The challenge doc's "Step 0 — measure first" (§2) has NEVER been executed because nobody has confirmed PMCs exist on the fleet. U1 (what binds the fold step) is answerable in one 20-second `perf stat` around a `kbench fold512 1t` run if counters work. If they don't, I discriminate port-bound vs latency-bound vs supply-bound by microbench design (§6.2) — slower and noisier but doable; knowing early which world we're in saves me a week.

**Data:** (a) exact `perf_event_paranoid` value on `ubuntu-latest` (any community thread, any repo that ran perf on GHA and printed it, or your own test repo — one 5-line workflow answers it, recipe in §8); (b) whether `perf stat -e cycles` (hardware) returns numbers or 0/<not supported>; (c) whether `uops_dispatched.port*` events exist (they're ICL+ uncore/core events — on SPR they exist as `UOPS_DISPATCHED.PORT_5` etc. — but only measurable if vPMCs are passed through); (d) if hardware counters are dead: does `perf stat` software mode (task-clock, page-faults, context-switches) still work (it always does — I'll use it for scheduling noise detection); (e) `turbostat` / `/dev/cpu/*/msr` availability.

**Sources:** GitHub community threads ("perf counters GitHub Actions", "perf_event_paranoid runner"), Azure docs on vPMCs in Dv5/Ev5 series VMs, any OSS CI that publishes perf data from GHA, your own probe workflow.

**Unlocks:** U1 by direct measurement — the fold-step decomposition on target silicon (p5 utilization %, IPC, port saturation), which settles whether R11's census-4 kernel gets its theoretical win or whether latency/supply dominates and the design pivots.

---

### R7 — THE FULL CI LOG HARVEST (the honest statistics) — *top priority, pure grunt work*

**Q:** What does the COMPLETE draw×arm×kbench matrix over every workflow run we ever fired say — including the ~15+ fleet runs since run 37189060243 and every PR #7 re-roll?

**Why:** Every strategic decision is currently made from 6 hand-collected draws. The full matrix gives: (a) the **healthy-draw median per class** (the true bar — the record draw was a top-decile instance at kbench 34.9 vs the 30.0 mid-band); (b) the **kbench↔sustained correlation** (how much of sustained is kernel-limited vs fabric-limited — if 11b/kbench1t is roughly constant, fabric efficiency is flat and only Route K/F moves the needle; if it varies, Route S has room); (c) the **draw frequency** of healthy 8573C instances (how many pushes per usable draw → how long a ≥3-draw record claim takes); (d) refined **noise floors** (arm-position, host-class, time-of-day); (e) whether the R8 ingest gate failures correlate with kbench (draw-quality flag for the gate, so we can tighten it honestly instead of luck-fishing); (f) topology captures in artifacts (`lscpu`, cache sizes, sibling maps) — free U3/U4 data nobody has tabulated.

**Data:** one CSV (plus a rendered markdown pivot): columns = {run_id, branch, sha, date, shard, cpu_name (from `cpu_name.txt`), topology (from topology captures if present), section (11b/11e/11n/11r/11s/11t/11u/kbench rows…), metric, value, gate_outcome, notes}. All rows, including failures. Artifacts are named `shard-result-N` with `draw-shardN.log`, `kbench.txt`, `bench_hydra.txt`, `bench_results.json`, `cpu_name.txt`.

**Sources:** our own repo's Actions history — recipes in §8. If artifacts older than the retention window are gone, salvage what the run *logs* still show (gh run view --log) and mark gaps.

**Unlocks:** U5 and the calibration of every other decision; this is also the dataset the ≥3-draw record claim will be defended with.

---

### R8 — SMT SIBLING INTERFERENCE PHYSICS (the assist budget)

**Q:** On Golden Cove / Raptor Cove (and Sunny Cove), what do published measurements say about co-running two hyperthreads when one saturates p5 (zmm clmul+shuffle) and the other runs scalar integer/multiply work (`crc32` on p1, `imul`, loads/stores, branches)? How much of the fold thread's throughput does the sibling steal, and how much scalar work fits "for free"?

**Why:** Route S and the assist ring's 2B budget. Today's assist recycles surplus main-core cycles into scalar CRC on p1. At 2B the serial path (~1.2 cyc/msg) + FNV + RX need a home; the only place is the fold cores' siblings. Known datapoint: fold-only on both siblings adds nothing (32.83=32.84) — the 512-bit datapath is core-shared. TO-VERIFY: the interference function for *mixed* colocation — Intel's optimization manual has SMT port-sharing guidance per µarch; academic SMT-interference studies and cloud-perf blogs have measured curves. If a sibling running p1/branch work costs the fold <5%, Route S is alive; if it costs 20%+, Route S is dead and 2B must come from Route K/F.

**Data:** (a) per-µarch SMT architecture notes (which execution resources are shared vs replicated: ports, 512-datapath, LSD, uop cache partitioning); (b) any measured throughput curves for vector+sibling-scalar mixes (Intel opt manual SMT section, academic studies 2020–2026, perf engineering blogs); (c) specifically for Ice Lake server and SPR if anything exists; (d) context-switch/cacheline-sharing costs for our chunked SPSC rings across siblings (16-span chunk cadence vs coherency traffic — any lock-free queue interference measurements on these µarchs).

**Sources:** Intel Optimization Reference Manual (SMT + resource-sharing sections per µarch), uops.info footnotes, wikichip per-core SMT diagrams, academic SMT interference literature, real-world profiling writeups (Brendan Gregg, Cloudflare/Netflix engineering blogs).

**Unlocks:** U4 — sizes the assist redesign, decides whether RX-sharding across siblings is viable for Front A 5B, and prices Route S honestly.

---

### R9 — MEMORY HIERARCHY CEILINGS (the supply question)

**Q:** On SPR/EMR (and ICL for the 8370C class), what are the realistic **per-core** sustained read bandwidths from L1D / L2 / L3 / DRAM, and how much does an unprivileged 4-vCPU VM lose to LLC contention from neighbors?

**Why:** The sleeper risk of 2B: workers must pull 55.3 GB/s combined from a ~14–16 MB per-pass working set with 2 MB private L2 per core. If per-core L3 read BW is ~25–35 GB/s (TO-VERIFY for SPR mesh), two fold cores can just barely feed 55–70 GB/s *from L3* — but only if the set stays L3-resident against noisy neighbors (R4). Alternative build-side lever if L3 is marginal: restructure blob placement so each worker's chunk stream is an L2-resident 2 MB stripe (stride-interleaved per worker at render time — needs a render.rs change; cheap to build, but only worth it if L3 BW is the binding wall). Nobody has ever measured the split on our draws.

**Data:** (a) published per-core L1/L2/L3/DRAM read GB/s for ICL-SP / SPR / EMR (STREAM variants, Intel MLC results on cloud VMs — `intel-memory-latency-checker` published runs on Dv5/Ev5-class Azure instances are gold if you find them); (b) LLC bandwidth vs #active-cores curves (mesh saturation ~ where?); (c) any Azure-specific LLC-contention/noisy-neighbor measurements for D/E-series 4-vCPU VMs; (d) if you find nothing VM-specific, bare-metal numbers + a note — I'll add an in-CI memory-probe arm (my side, §6.3).

**Sources:** Intel MLC published results, cloud benchmarking blogs (azureprice-type CPU inventories, per-series benchmark sites), academic Xeon-mesh bandwidth papers (SPR mesh studies 2022–2025), wikichip, any HPC procurement reports with SPR per-core STREAM numbers.

**Unlocks:** U3 — decides whether the 2B fabric needs the L2-stripe render restructure (a build lever I'll spec) or whether L3 supply suffices on healthy draws.

---

### R10 — RX / FEED-HANDLER PRIOR ART (the 5B references)

**Q:** What is the fastest publicly documented parsing of MoldUDP64/ITCH-class binary market data (msgs/s, per core and per box), and what techniques do the fastest implementations use for (a) frame-walking below 0.5 cyc/msg, (b) sharded/multi-queue RX with ordered handoff, (c) publish-by-reference zero-copy fanout?

**Why:** Front A 5B needs the RX at ≤0.46 cyc/msg. Lever B (publish-by-reference master entry array) is designed; I want external validation and any trick I haven't considered before freezing it. Specific questions: do any published parsers use mask-based batch length-scanning (AVX-512 `vpcmpeqb`+`vpmovmskb` to find message boundaries / sentinel bytes) instead of serial 2-byte-prefix walks? Any MoldUDP64-specific literature (academic or vendor whitepapers)? Any DPDK rss/ordered-queue or io_uring multishot patterns whose ordered-merge cost per item is documented <0.1 cyc? Vendor marketing numbers (Exegy, Vela, Fixnetix/Itiviti, Nasdaq's own handlers) — treat as upper bounds, note methodology if given.

**Data:** table {system/paper, year, msgs/s, hardware, msgs/core, technique, verifiability} + a shortlist of techniques absent from our design (frame-walk tricks, ordered sharding, lock-free publish patterns beyond SPSC chunks).

**Sources:** academic (IEEE/ACM: "market data feed handler", "ITCH parser", "MoldUDP64", "ULTRA-low latency feed processing"), vendor whitepapers, DPDK docs (ordered queues), io_uring docs, engineering blogs of HFT firms (rare but they exist — e.g., talks at QCon/StrangeLoop on feed handlers, "parsing market data at line rate" writeups).

**Unlocks:** Lever B freeze + possibly a cheaper frame-walk primitive for the 5B path; also sanity-checks whether 5B msgs/s is even a thing anyone has claimed (if not, we're first — good to know for the claim's framing).

---

### R11 — YMM DUAL-CHAIN & THE UNPACK-FREE FOLD (challenge §3B/§3C revival)

**Q:** (a) On each hunted class, does `vpclmulqdq ymm` have better aggregate throughput than `zmm` (2 chains of 256-bit vs 1 chain of 512-bit — port cracking question, needs R1's cells)? (b) Is there any published fold implementation at **64-bit lane granularity with hi32/lo32 constant splits** — eliminating `vpunpckqdq` entirely at the same clmul count (the challenge doc's §3B design: "transpose the constants, not the data")? (c) What does the original Gopal et al. folding paper (and any AVX-512-era revisions) say about optimal interleave/step shapes for 8+ parallel streams?

**Why:** The reflect kernel's remaining p5 census is 4 clmul + 2 unpack = 6 uops/step. The unpack-free variant floors at 4 clmul (+ free p0/p1 XORs) → if U1 says port-bound, that's +33–50% density (14.3 → 19–21+ B/cyc) — the single highest-confidence Route K lever, and I can derive and pin its algebra locally with the existing solver discipline regardless of your findings. Your data (published implementations + the paper's step-shape analysis + R1's ymm cells) decides per-class shape: ICL might want ymm×2 (if zmm clmul cracks — TO-VERIFY), SPR/EMR want zmm unpack-free.

**Data:** (a) R1's VPCLMULQDQ ymm-vs-zmm rows per µarch (explicitly); (b) every public fold implementation's step shape (interleave width, #streams, unpack strategy — annotate their asm if available): ISA-L, Intel's paper appendix kernels, Brumme's, zlib-ng's, any AVX-512 CRC repos; (c) any analysis of clmul **dependency-chain latency** per µarch (fold steps are serial per stream: clmul lat 4–7 TO-VERIFY — if latency-bound, more interleaved streams win, if port-bound, fewer uops win — R6/R2 data cross-references this).

**Unlocks:** U1/U2 — the R16 kernel family per class; the design I build the week your data lands.

---

### R12 — TARGET DEFINITION & THE ASSIGNMENT'S RULES (ask the task source — highest leverage, lowest effort)

**Q:** Get written answers from whoever assigned the 2B/5B task: (a) Is the machine class **fixed to what the current CI fleet gives** (4 vCPU, 2 phys × SMT, Xeon 8573C/8370C draws), or is **any CI-reachable machine of the hunted class fair game** — specifically larger runners (8/16+ cores) if reachable, and self-hosted dedicated boxes? ("for the dedicated cpu we hunt in ci" — I read this as "the fleet's dedicated CPUs"; confirm it doesn't mean a specific dedicated runner you know about that I don't.) (b) Does the ≥3-draw reproducibility rule require same-SKU draws, or same-class (8573C-family) suffices? (c) Are there spend limits (larger runners bill per-minute) or wall-clock limits per record attempt? (d) Is the 2B gate defined on the median healthy draw, best healthy draw, or ≥3-draw minimum (the honest readings differ by ±10%)? (e) Front A 5B: same silicon family requirement, and does the R8 gate's 2B floor stay as-is (we'd raise it to 5B when claiming)?

**Why:** If (a) allows bigger machines, Route F dominates: an 8-phys-core 8573C-class draw makes 2B sustained a fabric problem (workers scale; kbench says 30–35 GB/s/core × 8 cores = 240–280 GB/s ceiling vs 55.3 needed — comfortable), and 5B Front A becomes RX-sharding. If (a) pins us to 4 vCPU, everything rides on Route K (R1/R3/R11) + Route S (R8) — a much harder, much cooler program, and I need to know that on day one, not week three.

**Data:** the assignment's exact text + the ruling on each sub-question, in writing, pasted into your findings file verbatim.

**Unlocks:** the entire program's shape.

---

## 4. PRIORITY ORDER & THE DECISION TABLE

**Execute in this order** (R12 and R7 first — they're cheap and shape everything; R1 next — it gates the kernel program; R5/R6 in parallel are quick fact-finds; then depth work R3→R2→R4→R9→R8→R11→R10):

```
R12 (ruling)  ──► shapes the program
R7  (harvest) ──► the honest statistics (mechanical, start immediately, runs in background)
R1  (uops)    ──► kernel floor          ┐
R5  (fleet)   ──► bigger boats?         ├─ these three settle Route K vs Route F vs Route S
R6  (PMCs)    ──► Step-0 measurement    ┘
R3  (GFNI)    ──► the >2x hypothesis    ┐
R2  (records) ──► competitive frontier  ├─ depth work, can run in parallel
R11 (fold)    ──► unpack-free shape     ┘
R4  (SKUs)    ──► silicon facts         ┐
R9  (memory)  ──► supply ceilings       ├─ Route S + supply
R8  (SMT)     ──► assist budget         ┘
R10 (RX art)  ──► Lever B freeze        ── last, refinement only
```

| Your finding | My build decision |
|---|---|
| R12 allows bigger machines | Route F: fabric restructure for 8+ workers (desc8 rings already scale; the placement/supply code from R11/R12 eras generalizes), record claim on the bigger class |
| R12 pins 4 vCPU + R1 says p5-bound & TP=1 | R16a kernel: unpack-free census-4 fold (I derive/pin locally) → expect 19–21 B/cyc; then GFNI hybrid if R3 alive |
| R3 algebra + p0 issue both check out | R16b kernel: GFNI-affine hybrid fold — the 32–64 B/cyc moonshot, full solver discipline before silicon |
| R1 says zmm cracks on ICL | class-gate ymm dual-chain for 8370C (the single-clmul-port class gets its own shape) |
| R6 says PMCs work | CI arm: port-decomposition around kbench — every future kernel claim ships with measured p5% |
| R8 says sibling costs <5% | Route S: assist ring absorbs serial+FNV+RX on fold siblings; 2B attempt = K/S hybrid |
| R8 says sibling costs >20% | Route S dead; serial path must shrink itself (Lever B extensions to the sustained path) |
| R9 says L3 per-core < 28 GB/s | render.rs L2-stripe restructure: per-worker 2 MB stripes at render time (build lever, I spec it) |
| R7 correlation kbench↔11b flat | fabric efficiency is maxed; only K/F move the needle — tune nothing else |
| R7 shows draw-frequency of healthy 8573C < 1/10 pushes | raise shard count per push (R5 limits) or accept multi-day record campaigns — plan the claim calendar |
| R10 surfaces sub-0.2 cyc/msg frame-walk | fold it into Lever B before freeze |

## 5. THE REFUTATION LEDGER — DO NOT RESEARCH THESE (already killed, with evidence)

1. **Tri-stream / eval2 / eval_pair fold interleave** — port-issue-bound, not chain-bound (docs/24 §7 parity evidence; arms 11g/11l). More streams ≠ more throughput.
2. **Lanes-1..7 zmm tail absorption** — moves p1 uops onto saturated p5, −2..3% measured (R15 local refutation).
3. **vtail for r ≤ 8 spans** — old path wins there (no fold_extra chain to kill); gated off (R15).
4. **Negative-power GF(2) ring algebra** — y^(−1) construction fails its own consistency probe; only positive powers + empirical G[r] tables (R13 §4.2 + R15 solver kills).
5. **`HFT_WORKER_BATCH` 64/256** — no signal on any draw (arm 11u, R15).
6. **vend/vtail on the 8370C class** — single clmul port; class-conditional default is correct (kbench 27.6/27.7/28.8 r/rv with rc 28.8 — the crc-chain wins there).
7. **Vectorized watermark ladder** — refuted; arm 11m keeps the armed soak as a tripwire (docs/25).
8. **Per-span SPSC handoff** — the anti-ping-pong law: 16-span chunks won by >10× bus-traffic reduction (docs/20).
9. **Single-core > ~617M msg/s** — the crc32 8 B/cyc hardware ceiling with ~31.65 B bodies; only the fold breaks past it (docs/20 physics).
10. **Anything from the banned list** — memoization across passes, byte sampling, sub-32 checksums, "equal to previous pass", counting unread bytes (§1.5). If a "technique" verifies fewer bytes or carries state across passes, it is not research, it is cheating — bring me none of it, including "optimizations" that would pass a naive benchmark but violate rules 3/4/7.

## 6. WHAT I BUILD WHILE YOU RESEARCH (division of labor)

1. **Corpus anatomy** (local, no CI needed): per-span length histogram, r≥16 gate coverage, per-arm span mix, frame/message ratio at MtuBound(1400) — feeds the R16 kernel's length-indexed tables and Lever B's walk design.
2. **kbench probe battery** (no-PMC discrimination for U1 — ships as opt-in rows, [skip ci] local first): (a) 8-independent-chain fold (scales ⇒ latency-bound; flat ⇒ port-bound); (b) constants-preloaded/no-load variant (isolates load supply); (c) unpack-free 64-bit-granularity prototype — I derive its algebra with the R15 solver discipline and pin it against the 2419-body differential BEFORE any perf claim; (d) GFNI-affine CRC prototype sketch (R3's algebra, pre-staged so your port data lands in a running experiment).
3. **CI arms**: memory-probe row (per-core L2/L3 read GB/s in-fleet — complements R9's literature with OUR draws' truth) and, if R6 says counters exist, the perf-stat port decomposition around kbench.
4. **Lever B "rxbuild" freeze**: event-indexed master entry array, per-turn slice publish, prepatch-extended session patching, tombstone-free contiguity — full spec + code on `r15-frontier` (or a fresh `r16-double-helix` branch off it), gated behind `HFT_RX_PUBREF`, rollback default-off, D-oracle parity + rollback soak arm, exactly the house pattern. R10's findings fold in before the freeze.
5. **docs/29-r16-double-helix.md skeleton** with a pre-built decision slot for every workstream of §3 — your data lands into named sections, no re-litigating.

## 7. DELIVERY FORMAT (what to bring back)

- One markdown file per workstream: `R1-uops.md` … `R12-ruling.md`, plus an `INDEX.md` summary with a one-line verdict per workstream (e.g. "R1: zmm clmul 1 uop p5 TP1 on SPR ✓; cracked on ICL ✗ → ymm dual-chain for 8370C").
- Every number carries a **source URL + access date**; copy the raw table/text into the file (links rot). PDF/HTML snapshots welcome.
- Mark each claim **VERIFIED (measured/documented)** vs **TO-VERIFY (still open)** vs **REFUTED (source contradicts the hypothesis)**. I treat unlabeled numbers as rumors.
- R7: the CSV + the markdown pivot table + a "healthy-draw median per class" verdict line.
- R12: verbatim assignment text + the ruling. If a ruling is ambiguous, bring the ambiguity — do not guess.
- If you find something important I didn't ask for (a technique, a number, a contradiction with our physics), file it under `INDEX.md → SURPRISES` — that section has historically been the most valuable one.

## 8. COMMAND RECIPES

**R7 harvest (gh CLI, authenticated):**
```bash
# list every workflow run we ever fired
gh run list --repo zephyr4289/HFT-Proj --workflow ci.yml --limit 500 \
  --json databaseId,displayTitle,conclusion,createdAt,headBranch,event --out runs.csv

# per run: download all shard artifacts (repeat per run_id; mind rate limits — batch + sleep)
gh api repos/zephyr4289/HFT-Proj/actions/runs/<run_id>/artifacts --paginate > artifacts-<run_id>.json
gh run download <run_id> --repo zephyr4289/HFT-Proj -D harvest/<run_id>

# each harvest/<run_id>/shard-result-N/ contains:
#   draw-shardN.log (full battery), kbench.txt, bench_hydra.txt, bench_results.json, cpu_name.txt, shard_num.txt
# extract: grep -E "11b\.|11t\.|11s\.|11r\.|11u\.|Front A|fold512" draw-shard*.log | head -100
```

**R6 probe (your own throwaway repo, 5 lines):**
```yaml
# .github/workflows/perf.yml — one job, ubuntu-latest
- run: cat /proc/sys/kernel/perf_event_paranoid
- run: perf stat -e cycles,instructions -- true            # hardware events: numbers or <not supported>?
- run: perf stat -e task-clock,context-switches -- true    # software baseline (always works)
- run: perf list | grep -i "uops_dispatched\|port_5" | head # event existence
- run: sudo turbostat --quiet --interval 1 --num_iterations 2 true || true
```

**Local sanity (if you get shell time on any SPR/EMR/ICL box, e.g. a cloud trial):**
```bash
lscpu | grep -E "Model name|Socket|Core|Thread|L2|L3"      # topology truth
perf stat -e cycles,instructions,uops_dispatched.port_port5 <./kbench fold512 1t>   # the Step-0 measurement
```

---

## APPENDIX A — CI battery arm inventory (what the numbers in R7 refer to)

| Arm | Meaning |
|---|---|
| 11 | Benchmark + G12-T1 tail attribution study (defaults) |
| 11b | HYDRA bit-exact multi-core span conformance, UNPINNED (the sustained headline row) |
| 11c | R8 kernel-ceiling microbenchmark (fabric physics telemetry) |
| 11d | R9 fabric-shape kernel ablation (layout attribution) |
| 11e | R11 unarmed prepatch soak (rollback evidence) |
| 11f | R9 third-worker placement sweep (RX-hyperthread scavenging) |
| 11g | R9 worker eval2 interleave sweep (load-MLP) |
| 11h | R9 worker prefetch shape sweep |
| 11i | R10 assist-ring depth sweep |
| 11j | R10 worker pipelined-tail sweep |
| 11k | R11 distinct-core worker placement (SMT ceiling unlock) |
| 11l | R11 tri-stream fold sweep |
| 11m | R12 vectorized watermark ladder ARMED soak (refuted; tripwire) |
| 11n | R12 Desc8 OFF (rollback attribution) |
| 11r | R13 mirror-domain (reflect OFF = mirror ON) soak |
| 11s | R14 vend OFF soak |
| 11t | R15 vtail OFF soak |
| 11u | R15 worker drain granularity sweep (HFT_WORKER_BATCH 64/256) |
| 12–16 | oracle D1..D12, window sweep + 17-cell matrix, fuzz campaign, spec-server validation, statistical gate (30 runs + warmup 5) |

## APPENDIX B — the invariants, verbatim (bind every build and every claim)

- `HYDRA_BITPARITY == 0x881639cead506f25` (span path), classic replay `0xF6EF154EFDE905D8` — every pass, every arm, every draw.
- `ALLOC_DELTA == 0` on the hot paths, asserted per-pass.
- Pure streaming: no cross-pass caching/memoization/diffing; the 10-byte session prefix is the only per-pass mutation.
- Corpus/schedule frozen: `sample-mini.itch`, 505,849 msgs/pass, `MtuBound(1400)` dual-feed, sha256-pinned.
- `#![forbid(unsafe_code)]` stays on `nf-protocol` and `nf-arbitrator`.
- Every record claim: ≥ 3 independent healthy draws of the same class, raw logs published, the draw's kbench printed beside the number. `[skip ci]` for docs/iteration commits; silicon-verification commits ride clean HEADs (the R15 ops lesson).

*End of brief. Hunt well. — the Architect*
