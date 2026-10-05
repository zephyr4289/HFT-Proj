# ROADMAP — Breaking the 2.0B Verification / 5.0B Ingest Barriers

**For:** the author of HFT-Proj (`docs/00`–`docs/29`), from a full independent audit of
`main @ 78a7587`, the live `r15-frontier` branch (PR #7 @ `e497e3d`, CI draws 1–15),
and the complete CI run history through run 418.

**The mission being analyzed:** sustained full verification **≥ 2.0B msg/s** (record
1,234,801,472) and pure ingest Front A **≥ 5.0B msg/s** (record 3,624,572,766), on the
standard 4-vCPU CI fleet draw (2 physical cores × SMT), challenge §4 rules intact.

**Format:** every number below is either quoted from your own docs/telemetry or derived
from them with the arithmetic shown. Every proposed lever carries a **kill test** — the
cheapest experiment that decides go/no-go before kernel code is written. Sections 4 and 9
are the most important ones: what has already been refuted (so you never re-tread it), and
what will kill each remaining lever.

---

## 1. Where you actually are (the state of the evidence, draw 15)

The record stands at **1,234,801,472 msg/s sustained full verification** (34.15 GB/s of
verified body bytes, run 37108369001, R11) and **3,624,572,766 msg/s Front A**
(0.6346 cyc/msg), both on a record-class 8573C draw. Everything shipped since — the R13
reflect kernel (+2.7–5.8% sustained), the R14 vend ending (+4.45% on the strong
instances, class-gated to SPR+), the R15 vtail (+0.37%, neutral-within-noise), the R16
dfold (neutral on two draw classes, record-class case still open), the R16b rxdesc
array-submission protocol (−21% vs the ring, diet-narrowed to −10–18%), and the R16d
distinct-placement flip (+9.3–12.2% within-draw, vindicated) — has moved the
**draw-adjusted** stack to roughly record territory on strong instances but has not
moved the all-time number. The best post-R11 fabric draw is the ring on distinct
placement: **1,214,220,732 msg/s** (draw 10, arm 11n, kbench 34.86 record-class
silicon).

The honest reading of your own draw ledger:

| Quantity | Value | Source |
|---|---|---|
| All-time sustained record | 1,234,801,472 msg/s (34.15 GB/s) | R11, run 37108369001 |
| Best R16-stack draw | 1,214,220,732 (ring+distinct, 11n) | draw 10, run 37219883871 |
| Healthy-draw median (8573C) | ~1.062B | docs/29 §9 draws 4,5,7,11–13,15 |
| Front A record | 3,624,572,766 (0.6346 cyc/msg) | R11 |
| Front A healthy band | 2.25–3.23B | docs/26 §6, docs/28 §6 |
| kbench `fold512_r` 1t, healthy→record | 30.0–34.9 GB/s (13.0–15.2 B/cyc) | draw ledger |
| kbench `2cpu_distinct` pool | 60.6–69.9 GB/s | draws 10,11,13,15 |
| Fabric efficiency (delivered/pool) | **48.9% ± 0.3%** on ring, every healthy draw | R7 harvest |
| rxdesc fabric efficiency | 37.9–39.3% of pool | draws 10–15 DIAGs |
| Ring worker real-mix cost | 181–189 cyc/span at 96.2% busy | draws 12–15 DIAGs |
| Packed-kernel equivalent | ~91 cyc/span for the same bodies | kbench at same density |

The distance to the targets: **2.0B requires 55.32 GB/s delivered CRC** (2.0B × 27.66
B/msg), which is **79–91% extraction of the measured pool** (69.9 GB/s record draw →
79.1%; 60.6–63.7 GB/s median healthy → 87–91%). You are extracting 48.9%. Front A 5.0B
requires **0.46 cyc/msg** at 2.30 GHz — a 27.5% cut of the record's 0.6346, on a path
whose co-wall (the RX per-frame entry build) you have already correctly identified and
priced. These are the two numbers the whole roadmap is organized around.

---

## 2. The physics of the targets (the budget, honestly)

### 2.1 The 2.0B verification budget

```text
Demand:      2.0B msg/s × 27.66 B/msg           = 55.32 GB/s delivered CRC
Span rate:   ~50 msg/span (MtuBound(1400) bodies) = ~40M spans/s fabric-wide
Per worker:  20M spans/s at 2.3 GHz × ~0.98 busy = 2.254 Gcyc/s
⇒ Per-span budget at 2.0B:                        ~113 cyc/span
Current ring worker:                              181–189 cyc/span
⇒ Required cut:                                   ~68–75 cyc/span (−38%)
```

That 113 cyc/span budget is the single most important line in this document. Everything
in Section 5 exists to close a ~70 cyc/span gap, and the decomposition in Section 3 shows
exactly where those cycles are today.

### 2.2 The pool side: why raising the pool beats raising extraction

Your R7 harvest found the fabric-efficiency invariance: delivered bandwidth is 48.9%
± 0.3% of the 2-core kbench pool on *every* healthy draw, with the ring protocol. That
invariance is a statement about the *real-mix overhead stack* (endings + supply + ring
mechanics) being a stable multiplier on the packed kernel cost. There are only two ways
to a higher delivered number:

1. **Raise the pool** (more delivered GB/s at the same extraction), or
2. **Raise extraction** (shrink the real-mix overhead stack).

Option 2 is what R16b/R16e attacked, and the fleet's verdict is sobering: the array
protocol *lowered* extraction (39% vs 48%) and the diet recovered only part of it. The
overhead stack is stubborn because most of it is structural (per-span endings, per-span
ring discipline, supply latency at ~1.4 KB bodies). Option 1 — raising the packed kernel
density itself — has an identified, quantified, *untried* reserve: the p5 census floor.
This is the strategic core of this roadmap:

```text
Measured kernel:        9 cyc/128B step  = 14.2–15.2 B/cyc = 32.8–34.9 GB/s (1t)
p5 census (current):    4 clmul + 2 unpck + 2 ternlog(flex) → 6–8 p5-bound uops/step
p5 floor (best case):   6 cyc/step       = 21.3 B/cyc      = ~49 GB/s (1t)
Post-Route-T pool:      2cpu_distinct    ≈ 90–98 GB/s      (vs 60.6–69.9 measured)
Extraction needed at 2.0B vs post-T pool:  55.32/94 ≈ 56–59%
```

That last line is the entire strategic argument: **Route T converts an extraction
problem you cannot solve (79–91%) into one you have already demonstrated (49%)**. The
fabric does not have to get more efficient if the kernel gets materially denser; the
overhead stack can stay exactly as stubborn as it is.

### 2.3 The supply co-wall (the one you cannot engineer away, only around)

Your R9 finding stands: contended L3 supplies 25–35 GB/s/core, so 55.32 GB/s aggregate
sits at 79–92% of the 2-core contended band. Two facts keep this survivable. First, the
verification working set is the ~14.3 MB THP-backed blob (plus, post-Route-T, a ~15.3 MB
transposed arena) — L3-resident on the 57 MB 8573C, and the *streaming* access pattern
of ~1.4 KB contiguous bodies is the pattern L3 handles best. Second, your record draws
(kbench ≥ 34) empirically sit on hosts with uncontended L3, which is exactly why the
record protocol fishes for them. The roadmap consequence: never price a kernel lever on
a contended draw (you already learned this with dfold draw 6 — supply-bound hosts show
nothing), and treat sustained L3 read bandwidth from the blob as a **per-draw measured
constant** (a kbench row, Section 5.1), not a number to optimize.

### 2.4 The Front A budget

```text
5.0B msg/s at 2.30 GHz = 0.460 cyc/msg end-to-end pure ingest
Record:                 0.6346 (RX co-wall ~0.30–0.35 of it; main scan+submit ~0.30)
Required cut:           0.175 cyc/msg, on a path where the RX is 58–86% busy
```

The R12b sidecar refutation and the R12 ladder refutation together prove that *adding*
work to either side fails; the only remaining shape is *removing* work, and there is
exactly one big removable left: the RX rebuilds a pass-invariant frame stream every
pass. Section 6 makes that the program.

---

## 3. Where the cycles actually go (the per-span ledger)

At the R13/R14 record draw, the real-mix worker span cost was ~184 cyc/span against the
packed kernel's ~91; the current ring DIAGs show 181–189. The decomposition your own
docs establish:

| Component | cyc/span (est.) | Status after R16e |
|---|---|---|
| Fold loop (packed-kernel equivalent) | ~91 | reflect + dfold axis shipped; density 14.2–15.2 B/cyc |
| Ending stack (vend 2×zmm + vtail lane-0 + FNV entry) | ~20–24 | vend+vtail shipped; lanes 1–7 odd-block words are p1-parallel (free) |
| Supply + ring mechanics (desc loads, chunk walk, prefetch spray, res publish, waits) | ~66–75 | **the residual** — least instrumented, biggest single share |
| **Total real-mix** | **181–189** | 96.2% worker busy (ring) |

Three consequences follow directly from this table, and they organize everything else:

1. **The fold loop is the only component with a proven, quantified reserve.** The census
   says 9 cyc/step against a 6–8 cyc port floor; closing even part of that is worth
   15–25 cyc/span. No other component has a lever of that size.
2. **The residual (~70 cyc/span) is bigger than the ending stack you spent R14/R15 on**,
   and it is currently *unpriced* — you said it yourself in draw 12's verdict: "the next
   decomposition needs an instrument, not a guess." Section 5.1 builds that instrument
   as Stage 0 for a reason: every downstream decision changes if the residual turns out
   to be supply-latency (unfixable, draw-selection territory) versus issue-slot waste
   (fixable in the worker loop).
3. **The ending stack is nearly exhausted.** vend + vtail took it from ~30 to ~20–24;
   the remaining serial chain is ~4 dependent clmuls plus the FNV entry. Route S
   (Section 5.5) can move it off the vector core entirely, but its ceiling is ~10
   cyc/span of worker time — real, but second-order next to the fold.

---

## 4. The refuted-lever ledger (do not re-tread any of this)

These are dead by your own measurements or by structural proof. They are listed so that
no future round spends a draw rediscovering them, and because three of them *constrain*
the designs proposed later.

| Lever | Verdict | The evidence that killed it |
|---|---|---|
| Tri-stream fold (T=3) | DEAD | kbench parity on every placement; kernel is port-issue-bound, not chain-bound (docs/24 §7) |
| eval2 (two spans, two load streams) | DEAD | −14.7% (8573C), front-end bound |
| eval_pair (software-pipelined two spans) | DEAD | −11.0% Intel, −21% local |
| 8-way vector ladder (R12 gather) | DEAD as sustained lever | −5.3% / −6.8% on 8573C; +1.0% on 8370C only |
| RX-published SoA sidecar | DEAD | Front A −41% on 8370C; +25–30 µops and +38% store traffic on the RX |
| rxdesc frame-index prefill (first design) | DEAD | dual-feed dup divergence — frame index ≠ span index |
| Unpack-free Stage B kernel (transpose constants on raw loads) | **PROVEN IMPOSSIBLE** | lane-mixing kill + clmul lane-granularity kill + zero-divisor kill (docs/29 §2) |
| GFNI CRC hybrid | DEAD | affine is intra-byte, p0 anyway |
| ymm dual-chain | DEAD | zmm=ymm=xmm clmul share ports (port probe) |
| Route F (larger runners) | TERMINATED | org-only, no silicon pinning, ARM64 lacks VPCLMULQDQ |
| PMC instrumentation on runners | DEAD | `perf_event_paranoid=2`; kbench differential probes are the only instrument |
| distinct placement *without* supply fix (R11 world) | DEAD-then-REVIVED | refuted at −0.9% pre-rxdesc; vindicated at +9.3–12.2% with the flip under R16d |
| Slots=4 / slots=256, w3 workers, deep-prefetch | DEAD | −2.5% to −23% across draws |

One subtle constraint from this table matters for Section 5.2: the Stage B proof kills
*recovering lane purity algebraically from raw loads*, but it does **not** kill *storing
the data lane-pure in the first place*. That distinction is the entire content of
Route T.

---

## 5. The 2.0B sustained verification program

### 5.1 Stage 0 — the instruments (build these first, they are cheap)

You cannot price any of the routes below without three additions to kbench and one CI
arm. Each is a few dozen lines; together they replace guessing with attribution.

**(a) The null-kernel arm (`HFT_HYDRA_NULL`, CI arm 11z).** The fabric runs the exact
ring schedule, chunk grid, descriptors, prefetch spray, and res publishing, but `eval`
is replaced by a one-instruction consume (`_mm512_loadu_si512` of the body's first line
xored into an accumulator). The DIAG's `eval_ms` then measures the *plumbing + supply
floor* of the worker loop, and `full − null` is the true kernel-extraction share. This
is the instrument draw 12 said was missing. Expected reading if the residual theory is
right: null ≈ 60–80 cyc/span. If null comes back at ~30, the fold loop itself is
secretly ~150 and Route T's budget doubles; if it comes back at ~100, the ring protocol
is the wall and Route R changes verdict.

**(b) kbench floor rows.** Add three 1t rows to the packed corpus: `fold512_nopre`
(prefetch spray off), `fold512_noend` (fold loop only, ending stubbed to a state sum),
and `fold512_supply` (stream from a 14.3 MB L3-resident buffer instead of the L2-sized
kbench buffer). `noend` prices the ending stack in isolation; `nopre` prices the spray's
cost; `supply` measures each draw's actual L3 streaming ceiling for 1.4 KB bodies — the
number that tells you whether a given host can even theoretically deliver 27.7 GB/s per
core. These rows are the per-draw health gauge beyond kbench 1t.

**(c) The ternlog placement probe.** The census floor is 6 cyc/step if the two
`vpternlogq` uops route to p0, and 8 if they land on p5 behind the clmuls. Your R13
probe measured ternlog at 0.445 cyc/op standalone (2/cyc — it is a p0+p5 flex op), but
the *in-loop* placement under clmul pressure is unmeasured. Two probes decide it: a
`clmul×4 + unpck×2 + ternlog×2` synthetic mix (measures the assumed current-census
floor) and the same mix with the ternlogs replaced by independent p0 work (`vgf2p8affineqb`).
The delta between the two is the ternlog's true in-loop home. This is 20 lines in
`r13_port_probe.rs`'s harness and it sets Route T's expected ceiling precisely.

**(d) objdump census of the shipped loop.** You did this for the mirror kernel in R13;
do it once for `fold_word_pairs_r` as shipped (reflect + the current LLVM). Confirm the
uop count per step is still 8 p5-capable + loads, and check whether the two ternlogs
emitted are `vpternlogq` or whether LLVM picked `vpxorq` (same port class, different
schedulings). One hour of work, and it is the baseline against which every kbench delta
below is interpreted.

### 5.2 Route T — bake-time lane transposition (the centerpiece)

**The idea.** The Stage B refutation proved you cannot *recover* lane purity from raw
loads algebraically. But the fold's `vpunpck` exists only because the blob stores bytes
in wire order. The bodies are **pass-invariant and immutable** — your own transport
double-buffer doc says so ("only the 10-byte headers change"). So transpose them *once,
at startup*, into a span-indexed arena where every 128-byte fold unit is stored
**pre-interleaved in the exact lane order the fold consumes**. The fold loop then loads
the even and odd states *directly*:

```text
Raw layout (today), per 128B unit:      Transposed arena (Route T):
  n0 = loadu(p)                            even = loadu(t + 128*j)
  n1 = loadu(p + 64)                       odd  = loadu(t + 128*j + 64)
  even = unpacklo(n0, n1)   ← 2 p5 uops    (nothing — the lanes are already pure)
  odd  = unpackhi(n0, n1)   ← 2 p5 uops
```

Per 128 B step the p5-capable census drops from 8 to 6 uops (4 clmul + 2 ternlog-flex);
per span the fold loop drops ~15–25 cycles. This is the single largest identified lever
anywhere in the program, and it is *layout*, not algebra — the Stage B kills do not
apply because no unmixing is ever needed: each load lane *is* a reference-lane qword by
construction.

**The arena design.** For each of the ~10.9k spans per pass, allocate a padded slot of
`ceil(len/128) × 128` bytes (≤ 1,408 B for MTU-bound bodies; ~15.3 MB total, THP-backed,
built once at startup alongside the existing 14.3 MB grant — inside the untimed init
window, `ALLOC_DELTA` untouched). Within a span's slot, block *j* stores the span's
bytes `128j..128j+128` as: the 8 even-state lane qwords in lane order (bytes 8·0–7,
8·16–23, … wait — concretely: `even_lane[k] = qword at wire offset 64j + 8k`, `odd_lane[k]
= qword at wire offset 64j + 64 + 8k`), followed by the 8 odd-state lane qwords in lane
order. The fold loop for a span is then *shape-identical* to `fold_word_pairs_r` with
the two unpcks deleted and the prologue reading the arena directly.

**Why this is bit-exact by construction, not by testing.** The 8-lane value definition
(`span_crc32c_8lane`: lane *k* folds the sub-stream `{64b + 8k}`) is untouched — every
qword still reaches the same lane with the same fold constants, in the same order. The
transpose is a pure permutation of *storage*, not of the algebra. The ending stack
(vend, vtail, lane-0 continuation) consumes *states*, which are identical. The differential
suite (2419-body exhaustive + D11) still runs as the tripwire, but the correctness
argument is structural. Two real hazards to engineer, both mechanical:

1. **The lane-0 tail data terms.** vtail's composed-field ending reads the tail's data
   qwords in *wire* positions via the `AT[t]` constants. In the arena those bytes sit in
   transposed positions. The fix is a solver re-derivation (`r15_tail_derive.py`
   parameterized by arena coordinates — the AT table is regenerated for transposed
   offsets; the algebra framework already exists and re-derives all constants at test
   time). Alternatively, keep the tail path reading the *original blob* (the descriptor
   carries both offsets; the tail is ≤ 71 bytes, one or two cache lines) — zero algebra
   changes, one extra L1-hot read per span. Ship the second variant first.
2. **Unaligned span starts.** Spans start at arbitrary bytes in the blob, but the arena
   is *span-local* — slot byte 0 is the span's first body byte, so pair 0 is always
   exactly `arena[0..128]`. All alignment hazards vanish by construction. This is why
   the arena is span-indexed rather than a whole-blob transform: a blob-grid transpose
   would reintroduce mid-unit span boundaries, which is precisely the complexity the
   Stage B designs died of.

**Integration points** (all already exist): `submit_span`/rxdesc descriptors carry the
arena slot index instead of (or alongside) the blob offset — the Desc8 format has 16
spare flag bits and the warm-start arrays make either mapping free. The worker's
`prefetch_body` points at the arena slot (a *contiguous, 128-aligned* ~1.4 KB region —
better prefetcher behavior than today's blob offsets). The assist path folds arena slots
identically. kbench gains `fold512_t` (transposed corpus, no-unpck loop) as the
attribution twin; CI gains arm `11x2` (`HFT_CRC_TRANSPOSE=0` rollback).

**The kill test (before any kernel code).** Add the `fold512_t` row to kbench *first*,
as a standalone microbench: transpose a packed corpus into the arena layout in-process,
fold with a no-unpck copy of the loop, compare GB/s vs `fold512_r` on the same draw.
**Decision rule: if `fold512_t` does not beat `fold512_r` by ≥ 8% at 1t on a healthy
draw (30+ GB/s class), the census theory of the 9-cycle step is wrong or supply-bound —
kill Route T, write the refutation, and the 2B program falls entirely to Route S +
residual work (Section 5.5), with the honest expectation of ~1.6–1.8B ceiling.** If it
gains ≥ 15%, the post-T pool math (§2.2) goes live and the 2B path re-opens fully.

**Expected value.** Fold loop 91 → ~70 cyc/span (at 7.0–7.5 cyc/step, realistic between
the 6-cyc census floor and today's 9). Delivered at unchanged extraction: pool 90–98
GB/s × 48.9% ≈ 44–48 GB/s ≈ **1.60–1.74B sustained** — Silver/Gold territory from Route
T alone, before Routes S and P spend the remaining ~45 cyc/span of gap down to 113.

### 5.3 Route D — re-price dfold *after* Route T

dfold measured neutral on contended *and* healthy draws because the loop at 9–10
cyc/step is census-bound, not latency-bound. But post-Route-T the step approaches the
6-cyc census floor — which is *also* the `clmul→xor→clmul→xor` chain latency. At that
point the loop becomes latency-bound again, and dfold's 2× chain budget (4 chains, 12
cycles per pair-step) is exactly the medicine. The scaffold is shipped (arm 11v, kbench
`fold512_rd`, constants proven); the re-pricing costs one arm on the draws you are
already fishing. Sequence it *after* `fold512_t` lands, on the same draws, and expect
the sign to flip from neutral to positive exactly when `fold512_t` approaches 6 cyc/step.
If `fold512_t` itself shows no supply headroom (its own latency chain binding below the
census floor), `fold512_td` (transposed + dual-stream) is a one-day derivative of two
proven code paths — the merge algebra is unchanged (dfold's congruence is layout-blind:
it multiplies states by K², and states are layout-invariant).

### 5.4 Route R — the ring vs rxdesc decision (settle it, then stop paying for it)

The evidence across five draws is now consistent: the array protocol runs 190–227
cyc/span where the ring runs 181–189, its residue is supply-coupled (+8 cyc/span on
supply-rich draws, +24–29 on contended), and its fabric extraction (39%) is ten points
below the ring's invariance band. The diet is a consistent but small neutral-positive
(+1.15% healthy median). Against that, the ring on distinct placement is the highest
fabric number ever drawn (1.214B). The roadmap position: **the 2B record claim should be
attempted on the ring path.** Keep rxdesc as the documented refutation-with-a-diet
(the 11w/11x/11y arms already do this), and do not spend further diet installments
until the null instrument (5.1a) prices the *ring's* residual — because if the ring's
residual is itself ~70 cyc/span of supply latency, both paths are supply-bound and the
difference between them is second-order. Concretely: one more diet installment *only if*
the null arm shows the ring residual is issue-slot waste rather than latency; otherwise
freeze rxdesc work and reallocate the effort to Route T and Route S. The one rxdesc
capability worth salvaging regardless: its warm-start pattern is the exact template the
Front A program needs (Section 6).

### 5.5 Route S — sibling absorption of the serial stack

The distinct flip put main and RX on the workers' SMT siblings; the R8 class bound says
scalar sibling work costs the vector loop < 4.5% *if it stays off p5*. The assist path
currently spends main's surplus on *in-window CRC chunks* — which is p5 work on a shared
core, i.e. the one kind of help that competes with the worker it helps (draw 12's DIAG:
assist_chunks 1,317–30,552 under the array protocol, mostly wasted). Route S redirects
that surplus to the *serial* stack instead:

- **The ending offload.** The worker, at fold-loop exit, stores its two final states
  (64 B) into a per-lane ending mailbox and proceeds to the next span's fold loop; the
  sibling (main's core, which has the cycle surplus at ≥ 1.5B rates) drains the mailbox,
  runs vend + vtail + the FNV entry, and publishes results. The worker sheds ~15–20
  cyc/span of latency-bound serial work that currently overlaps its fold loop poorly
  (it is *dependent* on the loop's last state). The handoff costs the worker ~2 stores
  and the sibling ~30 cyc of scalar+vector-light work per span — the sibling has them.
- **The FNV serial fold stays on main** (it already does — 0.16 cyc/msg).
- **Kill test.** Prototype in kbench first as `fold512_rs`: fold loop + state-store,
  with a *separate thread* running endings, measuring the fold loop's step time change
  at 1t and 2cpu_distinct. If the fold loop's packed rate does not improve ≥ 4%
  (the ending's share of the *packed* loop is small, but the real-mix overlap is the
  prize — so the decisive test is the fabric arm on a healthy draw: expect +8–12
  cyc/span of worker headroom), kill it. The risk is coherence traffic on the ending
  mailbox; the chunked-release discipline from HYDRA (16-span chunks) is the known
  cure and should be built in from the start.

### 5.6 Route P — supply and prefetch tuning (small, cheap, do it opportunistically)

The worker prefetches body lines at per-span cadence; bodies are ~1.4 KB = ~22 lines.
Three cheap experiments, each one kbench row or one env knob: prefetch *distance* (T0
on span n+2's first 4 lines instead of n+1's 22 — let the HW L2 streamer own the middle
of a contiguous 1.4 KB region, it is designed exactly for this); per-size-class cadence
(MTU-bound bodies are uniform enough to hard-tune); and a `2cpu_distinct` kbench row
with the arena (post-Route-T) to re-tune distance for the arena's tighter layout. Each
is worth 0–8 cyc/span; none is worth a draw on its own — bundle them into the Route T
and Route S arms.

### 5.7 The tier ladder and the claim protocol (the honest part)

With the per-span budget arithmetic fixed (113 cyc/span at 2.0B; rate ≈ 2.254Gcyc/s ÷
cyc/span ÷ 2 workers × 50 msg/span), the tier ladder in required per-span cost is:

| Tier | Sustained | Required cyc/span | From (cumulative) | Plausibility |
|---|---|---|---|---|
| Bronze | ≥ 1.40B | ~161 | Route T alone (70 + 22 + residual ~65 = 157) | **likely** — T's kill test decides |
| Silver | ≥ 1.60B | ~141 | T + residual → ~50 (null-instrument-guided) | plausible |
| Gold | ≥ 1.80B | ~125 | T + residual → ~35 + Route S (+10) | possible, record-leaning draws |
| Obsidian | ≥ 2.00B | ~113 | T + residual → ~25 + Route S + record-class pool (69.9) | knife-edge, record draws only |

Two honesty commitments follow, and they are the roadmap's own recommendation to you:

1. **Re-scope the claim protocol before Obsidian, not after.** The R12 protocol requires
   the median *healthy* draw (kbench ≥ 30.0, pool ~60.6–63.7) to clear 2.0B — that
   demands 87–91% extraction, which no fabric has ever shown on any stack. The physics
   says 2.0B lives on **record-class draws** (kbench ≥ ~34, pool ≥ ~69.9, ~2% of
   shards, ~3–5 candidates per 50-shard push are healthy 8573C, of which record-class
   is roughly 1-in-5). The R11 record itself was set on exactly such a draw, and the
   challenge's standing rule (≥ 3 independent draws of the deciding class) is satisfiable
   at record class: ~3 record draws ≈ 150 shards ≈ 3–4 pushes at your current cascade
   economics. Recommend restating the protocol now: *"the 2.0B claim holds on the median
   of ≥ 3 independent healthy record-class draws (8573C, kbench fold512_r 1t ≥ 33.5),
   with the healthy-band median published alongside."* This is not moving the goalposts;
   it is aligning the goalposts with the silicon distribution you empirically have.
2. **Publish the per-tier ladder as the program's scoreboard** (like docs/26 §5), so a
   1.6B Silver landing is recorded as the progress it is rather than a 2.0B failure.
   Your draw ledger already has the discipline; give it the tiers.

---

## 6. The 5.0B Front A program

### 6.1 Lever F1 — the frame-entry warm start (the rxdesc pattern, applied where it wins)

The RX thread rebuilds a **pass-invariant** frame stream every pass: same blob, same
MoldUDP64 framing, same block layout, same first_seq values, same memo verdicts — only
the session template (and the virtual-clock release cadence) rotates per pass. R16b
proved the warm-start pattern on the span side (5,184 fixes total across thousands of
passes vs 8,640 per pass without it). Front A is the place that pattern pays its real
dividend, because the RX's per-frame entry build is the measured co-wall (prod_ms
58–86%).

The design, with the R12b lessons baked in:

- The RX keeps a **frame-indexed warm array** of the exact `FrameEntry` payload it
  currently builds (bytes ptr, blocks slice, first_seq, sess words, feed, memo, elig
  byte). Pass 1 (the untimed reference pass) builds it the classic way and leaves it
  resident. Every measured pass, the RX re-derives each entry's fields *in registers*
  during its normal slice walk and **compares** against the warm entry, fixing in place
  on divergence (the check *is* the correctness — the rxdesc law). Steady-state cost:
  one 64-byte compare per frame ≈ 2–4 µops, vs ~27–30 µops of build.
- **Zero added store traffic** — this is the sidecar refutation's exact failure mode,
   and the warm array avoids it because it *replaces* the stores the RX already does
   rather than adding parallel SoA stores. The entry array *is* the mailbox payload
   (the existing NBUF=16 `EntryBuf` structure); nothing new crosses a cache line.
- Session rotation per pass: the warm entries' sess words are rewritten per pass from
  the pass's baked template (2 stores per frame, already register-hot) — or, if the
  schedule keeps one session per run, nothing changes at all. Either way the consumer's
  `sess_lo/hi` compare semantics are untouched.
- The dup half of the dual-feed stream: unchanged (the `last < w` check in
  `steady_step` is already ~6 cycles; warm start does not touch the consumer).
- Telemetry: `rx_warm_fixes` per run (the rxdesc analogue; the steady-state assert is
  `fixes == 0` on pass n ≥ 2 for the steady schedule), plus `prod_ms` before/after.
- Rollback: `HFT_RXWARM=0` reverts to the classic build verbatim (CI arm).

**Expected value.** RX per-frame cost collapses from ~0.30–0.35 to ~0.05–0.10 cyc/msg;
Front A becomes main-bound at the scan+submit floor (~0.30–0.40 cyc/msg) → **4.6–6.4B
on 8573C-class draws**. The R8 2B gate stops being marginal on noisy draws as a side
effect. The 8370C class must be priced separately (its Front A band is −26%; the warm
start removes *more* work there, so the direction should hold, but the class gate law
stands: no default flip without that class's draws).

### 6.2 Lever F2 — the consumer's prefix-sum walk (build only if F1 lands short of 5B)

With warm entries in place, the consumer's 8-frame group check (`steady_scan_ref`'s
seven loads + six compares per entry) becomes vectorizable *without any RX-side cost*,
because the entries are already in a fixed stride the consumer can gather from L1. This
is the R12 ladder's original design, minus everything that refuted it: the eligibility
checks stay scalar per-group (they were the −3.6%), and only the *relations* (anchor,
pair-eq, dup-le, chain) go through the existing `ladder8` AVX-512 kernel over the warm
array. `HFT_VEC_LADDER` is already built, differential-tested, and refuted-on-sustained
— on the warm array its economics change (the gather it was missing is now L1-hot).
Arms decide. Expected: 0.30 → ~0.22 cyc/msg consumer-side; worth ~0.5–0.8B of Front A.

### 6.3 Lever F3 — mailbox depth and reset hygiene (small, known)

NBUF=16 with the freed-counter reuse protocol is sound, but at 5B the mailbox turns
~104M frames/s → 6.5M buffer turns/s; a `NBUF=32` arm (one constant, one CI arm)
prices whether the RX ever stalls on buffer reuse at the target rate. The prepatch
armed default stays (8/8 positive). The R16b pass-boundary unstick law (§5.2 law 8) is
the correctness frame for any change here — do not touch `reset_pass` without its
at_eos discrimination tests.

### 6.4 The Front A claim protocol

Same class discipline as verification: healthy 8573C draws (kbench ≥ 30.0), ≥ 3 draws,
median claim; publish the 8370C band alongside. Elevate the pure-ingest gate 2.0B →
5.0B in the same change that claims it (the R16 doc's own rule). One honesty note:
Front A's historical draw band (2.25–3.6B) is *wider* than sustained's, and the RX
warm start's value depends on host scheduling latency (the RX is a spin thread) —
expect the median-healthy claim to land ~4.6–5.2B, with record draws above.

---

## 7. Correctness guardrails (the invariants that make all of this claimable)

Every lever above lives inside the existing proof frame; none of it is allowed to
weaken it. Restating the frame as the engineering contract for the roadmap:

1. **Bit-exact goldens on every arm and every draw**: `HYDRA_BITPARITY`
   0x881639cead506f25, matrix 0xF6EF154EFDE905D8 — the Route T arena in particular is
   *structurally* value-identical (pure storage permutation), and the differential
   suite exists to catch the implementation, not the math.
2. **Solver-first constants**: every new constant table (Route T's AT-variant if the
   tail path is transposed; dfold's K² pair was the template) is re-derived at test
   time from the table reference (`t_*_constants_derivation`). No transcription can
   survive the suite.
3. **`ALLOC_DELTA = 0`** in all measured windows — the draw-10 11u crash (env parsing
   inside the worker loop) is the standing reminder that allocation tripwires fire on
   *any* per-iteration cost; hoist all env reads to worker start.
4. **`#![forbid(unsafe_code)]` untouched on nf-protocol/nf-arbitrator** — Route T and
   the warm start live entirely in nf-testkit/nf-transport where the unsafe contract
   already governs.
5. **No default flips without class evidence** (the R9c→R9d law) — vend's 8370C
   refutation is the template: class-conditional defaults with env rollbacks and a
   soak arm per axis.
6. **The prepatch-race flake (docs/25 §5.1) must be hardened before any record
   attempt.** It has now struck twice in CI (R12's 11j count divergence, R14 draw 4's
   11n failure) at ~1-in-25k passes, and a record draw that verifies short is a wasted
   record-class draw — the scarcest resource in the program. The R16b law-8 unstick
   fix addressed the pass-boundary class; the prepatch/auto-advance race itself remains
   open. Budget one hardening round (repro harness: chaos schedule × forced prepatch ×
   high pass count, the batch-parity pattern) before the record-fishing pushes.

---

## 8. Fleet economics and CI process

The 5-wave cascading 10-shard matrix is the right machine for this program, and it has
known economics worth engineering around deliberately:

- **Draw distribution** (R7 harvest, still current): ~50% AMD (fast-discard), Intel
  half ≈ 36% 8370C / 14% 8573C; healthy 8573C ≈ 1-in-10–12 shards; record-class
  (kbench ≥ 34) ≈ 2% overall. A record-class claim needs ~3 record draws ≈ 150–200
  shards ≈ 4–6 pushes. Budget pushes, don't fish ad hoc: each push should carry the
  full deciding-arm set so *every* draw prices something.
- **Arm taxonomy per push** (the current set is close to right): 11b default, 11n ring
  legacy-desc, 11w rxdesc-off, 11x pre-diet, 11y laps=0, 11v dfold, 11t vtail-off,
  11r kernel-rollback, 11k placement — **plus, once built: 11z (null), and the
  Route T arm** (`HFT_CRC_TRANSPOSE` default-off soak). Arms that have not produced a
  decision in 5+ draws (11f w3, 11i slots, 11j pipe) should be retired from the default
  matrix to buy shards for the deciding arms — they are settled refutations now.
- **A draw is only as good as its attribution**: keep the kbench 1t + 2cpu_distinct +
  2cpu_smt trio printed beside every sustained verdict (you already do — this is the
  discipline that made draws 10–15 decidable), and add the `fold512_supply` row (§5.1b)
  so every draw's L3 ceiling is on record. Record-class claims should be required to
  show supply headroom (delivered ≤ 85% of the draw's measured supply ceiling), or the
  draw is contaminated no matter what kbench says.
- **The run-418 class of failures** (single-shard Wave-2 failures) should never block
  a fishing push: the cascade's short-circuit exists for exactly this; keep
  `fail-fast: false` and let the aggregator's ≥3-draw medians absorb single-shard
  noise.

---

## 9. Risk register (what kills each lever, decided in advance)

| Lever | Kill test | If killed |
|---|---|---|
| Route T (transposition) | `fold512_t` ≥ +8% vs `fold512_r` at 1t on a healthy draw | The census theory is wrong/supply-bound; ceiling ~1.6–1.8B via S+P; re-scope claim to record draws or Silver tier |
| Route T tail handling | Solver fails to close the transposed AT table | Ship with the original-blob tail read (≤ 71 B extra L1 traffic); at most −2 cyc of the win |
| dfold post-T | 11v neutral on post-T healthy draws | The loop is issue-bound to the floor; dfold retires as documented refutation #14 |
| Route S (ending offload) | `fold512_rs` fabric arm < +4% worker headroom or mailbox contention visible in DIAG | Endings stay inline; Obsidian likely unreachable; Gold becomes the summit |
| Route R (ring freeze) | Null arm shows ring residual is issue-slot waste, not latency | Reopen the diet with a priced target (the ~24 cyc/span decomposition draw 12 asked for) |
| Frame warm start (F1) | `rx_warm_fixes > 0` persistently on the steady schedule, or Front A < +15% on healthy draws | The frame stream is less pass-invariant than the span stream; fall back to F3 + ladder-on-warm (F2) only |
| Ladder-on-warm (F2) | 11m-equivalent arm ≤ 0 on the warm array | Keep the scalar scan; Front A rides F1 alone |
| Record draw scarcity | < 3 record draws in 8 pushes | Re-scope per §5.7 (median-of-record-class → best-of-class with supply attribution, or Silver as the shipped claim) |
| Prepatch flake strikes a record draw | — (it will, at ~1-in-25k passes) | Hardening round §7.6 *before* fishing; per-pass tuple checks already catch it (no false record) |

---

## 10. Recommended build order (each step priced, with its decision gate)

1. **Stage 0 instruments** (§5.1): null arm, `noend`/`nopre`/`supply` kbench rows,
   ternlog placement probe, objdump census. *No performance claim — pure attribution.*
   Gate: the null arm's residual reading sets Route R's verdict and re-ranks 2–4.
2. **Route T kill test** (§5.2): `fold512_t` kbench row only. One day of work.
   Gate: ≥ +8% at 1t healthy → build the arena + kernel + arm; < +8% → write the
   refutation, skip to 4–6.
3. **Route T full ship** (if gated in): arena in nf-transport, no-unpck kernel variant,
   tail-on-original-blob first, `HFT_CRC_TRANSPOSE` knob, CI arm, D11 + exhaustive
   differential + full battery. Gate: sustained tier ladder moves (expect 1.4–1.7B
   healthy-band, Silver shipped).
4. **dfold re-price** on post-T draws (§5.3): arm only, zero new code.
5. **Route S prototype** (§5.5): `fold512_rs` kbench row → fabric arm on healthy draws.
   Gate: +8–12 cyc/span worker headroom → ship class-conditional.
6. **Route R verdict** from the null arm (§5.4): freeze or continue rxdesc; retire dead
   arms from the matrix (§8).
7. **Front A F1 warm start** (§6.1): independent track, can ship in parallel with 2–5.
   Gate: `rx_warm_fixes == 0` steady-state + Front A ≥ 4.5B healthy-class median →
   elevate the gate and claim.
8. **Flake hardening round** (§7.6) before the record-fishing pushes.
9. **Record-fishing campaign** with the full deciding-arm matrix (§8), claim per the
   re-scoped protocol (§5.7), ledger in the docs/28 §6 format.

Steps 1–3 are where the 2.0B program is won or lost; step 7 is where 5.0B is won. The
whole plan fits in roughly 2–4 weeks of your current cadence, and — this is the part
worth internalizing — **every step ends in either a shipped lever or a written
refutation, which has been the actual engine of this project from the FNV trap to the
Double Helix.**
