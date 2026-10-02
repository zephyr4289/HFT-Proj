# 23 — R10 ASSIST: the spin→CRC equilibrium, the codegen landmine, and the pipelined tail

## 1. The program

R10 opens on the handoff's two fronts with one observation drawn from the
record's own telemetry: **the submitting core is the pipeline's largest
idle CRC engine**. On the Zen3 draw (CI 37005861779, 718.1M sustained),
`work_ms=86%` against a pure-ingest duty of ~20% at that rate — the
difference was backpressure spin behind saturated workers, with
`assist_chunks` at 5.4% and RX starved 3x on its shared core by the spin
traffic. The fabric did not lack CRC capacity; it lacked the machinery to
spend main's cycles on it.

The levers, one commit each:

| # | Lever | Commit | Mechanism |
|---|---|---|---|
| 1 | The deep assist ring | d9bf14d | INLINE_SLOTS 4→64 (`HFT_ASSIST_SLOTS`), O(1) counter-indexed claim/fold, ids array eliminated (span id = first_span + i) |
| 2 | The outlining landmine | f3b7e37 | `fold_word_pairs` gains `#[inline(always)]` — a 10x codegen cliff documented and pinned |
| 3 | The pipelined-tail pair | 6939a86 | `eval_pair` (`HFT_WORKER_PIPE=1`, default OFF): A's vector fold, B's vector fold, A's endings, B's endings — one sequential load stream |
| 4 | The window's instrumentation tax | 8b7e04e | the span arm's per-batch DIAG clock reads and per-pass env::var leave the measured window |

## 2. The deep assist ring (the spin→CRC conversion)

The R8 assist (ec9b6fe) buffered a mere 4 inline chunks. Inline chunks sit
at the SUBMIT point, far ahead of the fold cursor, and can only fold once
every worker-owned chunk before them has returned — so the shallow ring
clogs after ~256 assisted spans and the submitting core falls back to the
backpressure spin it was built to avoid. The equilibrium this produced on
Zen3: workers at 99.2% busy delivering 20.3 GB/s of their 23.94 pair
ceiling, main spinning ~50% of its wall, RX at 48% "busy" (3x its solo
rate's duty — the spin traffic starved its SMT sibling), the whole
pipeline backing up behind the worker pair.

R10's ring goes 64-deep (4096 assisted spans of lead). Claim order ==
submission order == fold order, so two monotone counters index the ring
and the fold's oldest-unfolded lookup is O(1) (the R8 4-slot linear scan
left with the deep ring — it would have been O(64)). The span-id array
left the chunk: ids are `first_span + i` by construction, one store/load
per assisted span of traffic saved. Forced-inline mode (the parity
configuration) now WAITS for a free slot by folding instead of silently
falling back to a lane — the all-inline guarantee is structural.

Local sandbox (single worker, degenerate placement — main alone on cpu0,
worker+RX on cpu1), sustained 5s arm, fold512:

| HFT_ASSIST_SLOTS | sustained | assist_chunks |
|---|---|---|
| 4 (= R8 equilibrium) | 265.9M | ~48k (10.6%) |
| 16 | 343.2M | — |
| 64 (default) | 421.7M–464.3M | 377k (~48%) |
| 256 | 442.3M | — |

The 4-slot arm reproduces the pre-R10 baseline within noise — the
mechanism is isolated. Default 64: +73% end-to-end on the sandbox, with
`work_ms` 4525→3700 (spin converted to CRC), bit-exact per-pass tuples,
ALLOC_DELTA=0, D1..D12 green, force-inline parity green.

The sandbox number is a mechanism proof, not a prediction: its degenerate
placement (worker sharing a core with RX) amplifies the assist's value.
The runner classes (workers on a dedicated SMT pair) will find their own
equilibrium — ci.sh 11i sweeps 4 and 256 next to the 11b default on
every draw.

## 3. The outlining landmine (a codegen law)

Adding `span_fold_eval_pair` — a second caller of `fold_word_pairs` —
detonated a landmine that had sat under the GIGAHFT kernel since its
first commit: the block-pair loop carried neither `#[target_feature]`
nor `#[inline(always)]`, and had only ever inlined by single-caller luck
into `span_fold_eval`'s feature-enabled body. Two callers made LLVM
outline it. A standalone copy WITHOUT the feature attribute compiles
every AVX-512 intrinsic into an out-of-line call to core's wrapper
functions, with 512-bit values passed through stack memory and
`vzeroupper` at every boundary.

Measured on the local SPR sandbox, kbench `fold512`: ~20 GB/s inlined
vs **1.9 GB/s outlined — a 10x cliff**, stable across runs and target
directories. The sink stays bit-identical (the VALUES are correct either
way; only the schedule collapses) — which is exactly why this class of
regression is invisible to every parity oracle the project has. Only a
throughput instrument catches it. This is the strongest argument yet for
kbench's per-run presence in CI: the fold kernel is one inlining
decision away from a silent 10x, on every future contributor's machine.

`#[inline(always)]` now pins it; the law is documented on the function.

## 4. The pipelined-tail pair (kernel schedule)

The 962.5M Intel draw's fbench decomposition priced the fold512 kernel's
per-span overhead at ~14% below the uniform kbench ceiling (real-mix
packed control 25.51 vs kbench 29.79 GB/s on the pair): the store/reload
round-trip of the fold states, the 16 chained crc32 endings, the lane-0
scalar continuation, and the serial FNV imul chain all execute AFTER each
span's vector fold instead of under the NEXT span's.

R10's `eval_pair` reorders the schedule — A's vector fold, B's vector
fold, A's endings, B's endings — so the out-of-order engine overlaps A's
ending latency with B's in-flight clmul chains. Crucially this is NOT
eval2's interleave (measured -28% dead on the post-aliasing fabric: two
interleaved load streams thrash the sequential streamer): the load order
is unchanged, one sequential stream, the pair a single ~2.7KB read on the
aliased blob.

- `HFT_WORKER_PIPE=1` arms it in the worker loop; default OFF until
  per-class CI verdicts land (the R9c/R9d discipline).
- kbench grows a `fold512_pair` row: the kernel-level attribution rides
  every draw next to `fold512` and `fold512_eval2`.
- D11 pins `eval_pair == (eval, eval)` on the mismatched-pair corpus.
- ci.sh 11j runs the armed fabric on every push.

Local sandbox: kbench `fold512` 25.50 vs `fold512_pair` 25.60 GB/s —
parity on the packed corpus (the endings there were already
well-overlapped; the sandbox is too noisy to decide the fabric effect).
The runner classes decide.

## 5. The window's instrumentation tax (Front A)

The R8 pipelined span arm — the Front A gate — carried its per-batch DIAG
instrumentation inside the measured window unconditionally: two
`read_monotonic_raw_ns()` vDSO calls per ingested batch (22
publications/pass = 44 calls) and one `env::var` (a heap allocation) per
pass. ~1% of the 119us Zen3 pass at the 4.24B record, spent measuring
instead of ingesting. The flag is now read once at startup; the tax runs
only under `HFT_EXP_DIAG`. The window still spans reset-handshake to last
drain and the golden population assert still fires every pass.

## 6. Open fronts (the CI decides)

* **Front B, runner-class equilibria:** the deep ring's conversion rate on
  the real 4-vCPU topology — where the workers own a dedicated SMT pair —
  is the open question 11i/11b answer per class. Zen3 arithmetic if main
  converts even half its spin budget: workers ~20.3 + assist ~10+ GB/s
  against the 27.65 GB/s demand.
* **Front B, the THP dividend:** R9e (8b3192d) has never drawn an Intel
  runner. Zen3 gained +6% from THP; the 962.5M record plus any Intel THP
  dividend plus the assist conversion is the gate-breaking stack.
* **The pair verdict:** 11j + the kbench rows, per class.
* **Front A:** post-8b7e04e the arm needs fresh CI DIAG draws to find the
  next binder (RX render vs main ingest at the new equilibrium).

## 7. Commit trail

| Commit | Lever |
|---|---|
| d9bf14d | the deep assist ring (HFT_ASSIST_SLOTS, O(1) indexed fold, ids elimination) + ci.sh 11i |
| f3b7e37 | fold_word_pairs inline(always) — the outlining landmine documented and pinned |
| 6939a86 | eval_pair + HFT_WORKER_PIPE + kbench fold512_pair + D11 extension + ci.sh 11j |
| 8b7e04e | the span arm's DIAG timing leaves the measured window |
| (docs) | §8: the first CI verdict — Zen3 718.1→898.0M default / 917.0M armed, +25% on the worst silicon |

## 8. The first CI verdict (Zen3 7763, run 37030556327)

c3cea3e, the pool's worst silicon (scalar8lane, 2445 MHz). Same runner
class as the R9e draw (37005861779, 718.1M) — a direct before/after:

| Arm | Config | sustained |
|---|---|---|
| 11b | **slots=64 default** | **898.0M** (+25.0% vs 718.1M) |
| 11i | slots=4 (the R8 equilibrium) | 717.7M — the pre-R10 number reproduced within 0.1% |
| 11i | slots=256 | 908.8M |
| 11e | prepatch armed | 917.0M — armed now BEATS unarmed on Zen3 |
| 11h | deep prefetch (6,32,32) | 905.3M — within arm variance of default |
| 11j | worker pipe | 895.4M — neutral on scalar, as expected (a fold512 lever) |
| 11f | third worker | 785.0M — dead on Zen3 (-13%) |
| 11g | eval2 interleave | 883.9M — dead, as R9 measured |

The mechanism, confirmed by the runner's own DIAG: `assist_chunks`
65k→364k (5.6x — the submitting core's spin budget converting to CRC),
`crc_demand_gb_s` 19.73→24.83 (the fabric now DELIVERS 4.5 GB/s more
verified bytes), `work_ms` 86%→81% at a 25% higher rate (the same wall
buys more output), workers pinned at 98.4% busy throughout. The 898M
default sits at 108% of the fbench F-stage replica (22.88 GB/s) because
the assist adds main-core CRC on top of the worker pair's ceiling —
exactly the designed arithmetic. Front A on the same draw: **3.07B
PASS** (the class historically drew 2.04–2.48B; the DIAG-tax removal
and the ring both feed the span arm).

Verification on the draw: D1..D12 green (2109 bodies incl. eval_pair),
17/17 matrix cells golden, ALLOC_DELTA=0, per-pass bit-exact tuples on
every arm, ALL CONSTRAINTS PASSED.

Open after this draw:

* **the prepatch ordering flipped on Zen3** (917.0 armed vs 898.0
  unarmed, outside the ~1.5% arm-variance band). R9d's default-OFF
  verdict was measured on Intel/Zen3/9V74 WITHOUT the deep ring; the
  equilibrium changed. An Intel draw with the R10 stack decides the
  default flip — a scalar-class win alone doesn't flip it.
* **slots saturation**: 64→256 buys +1.2% on Zen3; the curve is
  saturating near the default. No default change pending more draws.
* **Intel fold512 + Zen5**: untested with the R10 stack. The 962.5M
  record (slots=4, no pipe) + the assist conversion + `eval_pair` +
  the never-measured Intel THP dividend is the gate-breaking stack —
  11j and the kbench `fold512_pair` row attribute it per draw.
