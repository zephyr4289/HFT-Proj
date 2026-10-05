# R18 "Vector Ingest" — The 6.0B Pure-Ingest Program

> Mission: Engineer 1 (Vector Ingest & AVX-512 Parser Architect), branch
> `feat/ingest-6b-simd`. Target: **≥ 6.0B msg/s pure ingest** (≤ 0.383
> cycles/message @ 2.30 GHz · ≤ 0.466 @ 2.80 GHz), nine rules intact,
> bit-exact golden parity (`0xF6EF154EFDE905D8` replay conformance /
> `0x881639cead506f25` hydra fold), `ALLOC_DELTA = 0`.
>
> Standing baselines at branch start (record draws #37338823281 /
> #37348948183): **5,455,603,369 msg/s** (0.51 cyc/msg @ 2.79 GHz, Shard 24,
> 8370C) / **5,286,441,351 msg/s** (0.44 cyc/msg @ 2.30 GHz, Shard 15,
> 8573C). Median healthy draw: ~4.9B (kbench 32.67-33.31 band).

## 0. Executive Verdict (read this first)

1. **The directive's Priority-1 premise is architecturally dissolved.**
   The AVX-512 "masked vector prefix-sum frame stride scan" targets a walk
   that no longer runs at runtime: since R8 the span path's message
   boundaries are construction-baked into the `(first_seq, offset, len)`
   triples (render time, amortized out-of-window) and the R8 span emission
   is O(1)/frame. docs/29 §5.4 said this verbatim ("an architecture that no
   longer exists at runtime"); the live VPADDQ lever (the R12c 8-entry
   ladder) was fleet-priced by the F-3 round and **killed 0/3 healthy at
   ≥ +15%** (draws 27/28/29: −1.2 / −12.5 / +0.85%). Re-arming it is not a
   lever; it is a refuted experiment with an instrument left aboard.
2. **The wire-format walk is serial by construction.** A
   `[u16 BE len][payload]` stream's block positions are a serial prefix
   chain (`p_{k+1} = p_k + 2 + load16(p_k)`); "extract all packet length
   delimiters simultaneously" is not achievable for genuinely variable
   framing, and the ITCH type-length table is payload-level validation —
   not a framing oracle. The only legal SIMD form of this walk would
   re-derive what render-time baking already gives for free.
3. **The Front A wall at the current stack is the consumer's per-frame
   scalar ladder** (nf-arbitrator `steady_step`: ~25-30 µops/frame over a
   64-byte `FrameEntry`, 7 field loads, 6-7 well-predicted branches) —
   **outside Engineer 1's file fence** — sitting on a **consumer↔RX
   handoff equilibrium** whose failure mode was measured for the first
   time in this program (§2: unbalanced consumer speedups flip the
   pipeline into futex park-storms and *regress* the pass 1.8x).
4. **What shipped from this branch is the honest subset inside the
   fence**: the R18 6B gate (gates-as-code, verdict line per draw, JSON
   fields, tripwires — §4) and this attribution. The 6.0B median-draw
   claim is **OPEN** and its lever list is §5, handed to the engineers
   whose files own the in-window cycles (Engineer 3: arbitrator consumer
   walk; Engineer 2: transport RX/publication).

## 1. Reconnaissance: where the 0.44-0.51 cycles/message actually live

The Front A span arm's measured window (`hft_bench::wall_pass_pipelined`)
is, in full:

```
t0 = read_monotonic_raw_ns()
while transport.next_batch()                                  // nf-transport mailbox
    seq.ingest_entries_ladder(transport.entries(),             // nf-arbitrator
        transport.now_ns(), &mut sink, ladder)                 // sink = hft_bench (mine)
dt = read_monotonic_raw_ns() - t0
```

Per frame (23.0 msgs/frame canonical: 505,849 msgs / 21,996 frames), the
consumer's `steady_step` executes (disassembly, fat-LTO `target-cpu=native`):

| µop class | instructions | notes |
|---|---|---|
| entry field loads | 7 × `mov` from one 64B line | `blocks.len`, `sess_lo/hi`, `bytes.len`, `first_seq`, `memo` (discr + `valid_count`), `feed` |
| compare/branch | 6-7 | all cold-exit, well-predicted |
| arithmetic | ~6 | `last = first + n - 1`, `w` advance, counter indices |
| SpanRec buffering | 5-6 stores @ 48B stride | the O(1)/frame R8 emission |
| counters | 2 RMW | `pk[fi]`, `byt[fi]` (amortized via register locals) |
| sink flush | ~32 loads + tree-add per 32 frames | LLVM already emits a **tree reduction** (depth ~5) for the count fold — there is no serial-chain win to take |

Attribution on the record shape: ~90% arbitrator walk, ~2-5% sink fold
(tree-reduced; priced locally ≈ 1.3 µs of a 215 µs pass ≈ 0.02 cyc/msg),
~2% mailbox/`next_batch`. **nf-protocol is not on the span-arm hot path at
all** — its `itch5::validate`/`FrameMemo` verdicts are baked at construction
(out-of-window), and `gates.rs` is cold verdict plumbing.

The FrameEntry is already exactly 64 bytes (one line) after the R12c/R16e
diets; the eligible compaction levers (field merging to shrink loads) all
cross into nf-transport construction sites — outside the fence.

## 2. The park-storm equilibrium (the new measurement of this round)

A null-fold A/B on the local sandbox (guards-only sink, count kept
population-proportional, golden assert disabled, two separate binaries to
avoid in-binary branch artifacts) produced a **1.8x regression from
removing work**:

| binary | span median (cyc/msg) | rate | consumer parks/pass-arm |
|---|---|---|---|
| baseline (fold live) | 1.37-1.43 | 2.28-2.37B | 4 |
| null-fold (fold removed) | 2.43-2.54 | 1.26-1.31B | **53** (+0.6 ms park time) |

Mechanism: the faster consumer outruns the RX publication cadence,
exhausts `next_batch`'s 512-spin budget, and eats futex park/wake
latencies (~10-15 µs each) per handoff. The same consumer is *restored to
parity* merely by adding two per-batch vDSO clock reads (HFT_EXP_DIAG=1:
both binaries then report identical 0.2 ms passes) — the pipeline sits on
a knife edge where ~1% of consumer-side time gates the park discipline.

Readings this explains retroactively:

* The F-3 fleet kill's "gather+call overhead nets slightly negative" also
  carried this shape: any consumer win below the RX ceiling does not
  convert to wall rate once handoffs dominate.
* F-4 (NBUF=32) pricing −1.6/−1.9%: deeper runahead did not move the
  equilibrium on fleet silicon.
* The kernel-correlation law (Front A tracking kbench `fold512_r`): the RX
  side's production rate is memory-bound on the 15 MB corpus — on
  record-class hosts the consumer is the wall (0.44-0.51 cyc/msg), on
  weaker hosts the RX is.

**Law for the 6B program: consumer-side and RX-side levers must be priced
TOGETHER on the same draw; a consumer-only lever below the RX ceiling is
priced by the handoff, not by the µops it removes.** The parks guard
already surfaces per draw (`DIAG cons span: parks= park_ms= slow_waits=`
from `diag_summary("span")`, stderr → the draw log): any future arm whose
premium comes with `parks >> baseline` is an equilibrium flip, not a win.

## 3. What was NOT shipped, and why (the honest refusals)

* **Re-arming the R12c vector ladder as the Front A default** — killed by
  the fleet 0/3 healthy ≥ +15% (draws 27-29); the house law writes kills
  from ≥ 3 readings, and they are written.
* **An AVX-512 count-fold in the sink** — the disassembly shows LLVM
  already tree-reduces it; the remaining cost is ~32 L1 loads whose
  wide-load/vpermb replacement saves < 0.01 cyc/msg (below local pricing
  resolution, §2's equilibrium makes local pricing of it meaningless).
* **The O(1) count identity** (`Σ count = last.first_seq + last.count −
  first.first_seq`) — the `on_span_batch` contract explicitly does not
  guarantee inter-rec consecutiveness ("whenever the source frames were");
  shipping it would price an unproven invariant against the golden assert
  as its only safety net. Refused.
* **A 64-bit session-token compare** replacing `sess_lo`/`sess_hi` —
  collision-unsound for arbitration exactness. Refused.
* **Coalesce/NBUF default flips** — swept locally (HFT_COALESCE ∈
  {64…2048}: 1.39-1.74 cyc/msg, noise-dominated on the 2-core sandbox);
  the R8 law (coalesce 128, NBUF 16) stands until a fleet arm prices
  otherwise. Never price an arm on faith.

## 4. What shipped

* `crates/nf-protocol/src/gates.rs`:
  `PR1_R18_PURE_INGEST_MIN_MSG_PER_SEC = 6_000_000_000` +
  `evaluate_pr1_r18_pure_ingest` + tripwires (FAIL on 0 / the standing
  records / 5,999,999,999; PASS at 6.0B and above). Gates-as-code law
  (F-22): the threshold lives once, consumed by the bench verdict and CI.
* `crates/nf-engine/src/bin/hft_bench.rs`: the per-draw
  `PR1_R18_PURE_INGEST_VERDICT rate=… target=… -> …` line (stderr) and
  `r18_pure_ingest_target` / `r18_pure_ingest_verdict` JSON fields — the
  R16 non-asserting pattern verbatim: reported every draw, asserted only
  at submission-time elevation (median healthy draw crossing).
* All existing CI grep contracts preserved (`PR1_R8_…`, `PR1_R16_…`,
  `LADDER_DIAGNOSTIC`, `RXBUILD_DIAGNOSTIC`, `RXWARM_DIAGNOSTIC`,
  `span_rate_msg_per_sec`, sink `"count+span"`); the JSON change is
  strictly additive.

## 5. The 6B lever list (the handoff)

Budget math at 2.30 GHz: 6.0B needs ≤ 0.383 cyc/msg; the 8573C band
stands at 0.44 (gap 0.057) and the 8370C at 0.51 @ 2.79 GHz (gap 0.044).
In fence order:

1. **(E3, arbitrator) Shrink the steady walk's per-frame µop count.** The
   scalar `steady_step` spends ~25-30 µops/frame; candidates measured
   legal by this round's disassembly: fold the memo gate into the elig
   byte already carried (publisher computes `valid_count == n`; the
   consumer then reads ONE byte instead of discriminant + u16), and the
   R12c group path's remaining ~90-µop elig verification that the elig
   byte already carries. Both are construction-exact (the publisher's
   baked values) — the same law the R12c elig byte itself shipped under.
2. **(E2, transport) Raise the RX publication ceiling in lockstep** —
   the §2 law: every consumer-side win below the RX ceiling converts to
   parks, not rate. The RX's per-publication work (poll walk + slice
   bounds + Release stores) prices against kbench `fold512_r` on the
   corpus.
3. **(E2+E3) Park-free handoff for the 110 µs pass shape** — the 512-spin
   + futex design pays a syscall-scale latency per park; a Front-A-scoped
   spin-only mode (bounded by the pass period, never sleeping inside the
   measured window) removes the failure mode §2 measured at 1.8x.
4. **(E3) STEADY_RECS depth / recs layout** — 32 × 48B recs per flush;
   the fold is tree-optimal but the flush frequency is the arbitrator's
   constant. Doubling depth halves fixed flush costs (~0.005 cyc/msg —
   priced honestly as minor).

Any of these landing must carry the draw-log evidence: `parks==baseline`,
`LADDER_DIAGNOSTIC` flip-validation, 17× BIT-EXACT parity, `allocs=0`,
and the R12 class protocol (≥ 3 healthy same-class draws, median decides,
kbench ≥ 30.0 each).

## 6. Local sanity evidence (this branch)

* `cargo test --release -p nf-protocol`: 19/19 pass (R18 tripwires included).
* Full workspace suite + clippy: see the CI run on this push (the 80-shard
  fleet runs `scripts/ci.sh` verbatim on target silicon).
* Local smoke (sandbox Xeon, AVX-512 present, 2 cores — directional only):
  R8 verdict PASS, R16/R18 FAIL verdict lines print; JSON parses with the
  new fields; golden population assert (505,849) live on every pass.

## 7. Ledger

* **Draw 30 (run 37366527072, commit fc9b6e8 — the gate lands unarmed;
  the default stack is byte-identical to main).** The fleet priced the
  R18 verdict line's presence on the standing default across 4 target
  draws before the singleton queue was displaced by the next engineers'
  pushes (34/80 shards ran, 4 landed target silicon):

  | shard | host | kbench fold512_r 1t | default Front A | cyc/msg | R18 |
  |---|---|---|---|---|---|
  | 40 | 8573C @ 2.30 GHz | 33.53 | 4,883,985,208 | 0.47 | FAIL (reported) |
  | 61 | 8573C @ 2.30 GHz | 32.81 | 5,007,662,228 | 0.46 | FAIL (reported) |
  | 76 | 8573C @ 2.30 GHz | 32.21 | 5,119,929,149 | 0.45 | FAIL (reported) |
  | 44 | 8370C @ 2.79 GHz | 28.97 | (cancelled mid-CI by queue displacement — 5-run arms only) | — | — |

  The readings sit exactly on the standing kernel-correlation curve
  (4.44B @ 30.89 / 4.70B @ 32.67 / 4.90B @ 33.31, draws 27-29): no
  regression and no premium — the expected signature of a
  measurement-infrastructure-only push. Invariants per healthy shard:
  35× BIT-EXACT, replay conformance
  `hash=0xF6EF154EFDE905D8 count=505849 watermark=255850 violations=0`,
  `ALLOC_DELTA=0`, golden population asserted every pass.

  **The 6B claim remains OPEN**: median healthy draw ~5.0B vs the 6.0B
  gate; the gap (0.45 → 0.383 cyc/msg) lives in §5's lever list, in
  files owned by Engineers 2/3. This branch's contribution stands as
  the gate the claim will be reported against, the equilibrium law
  that prices any lever that touches it, and the attribution that
  says where the cycles are.
