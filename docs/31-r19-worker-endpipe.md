# R19 — The Worker Ending Pipeline ("endpipe")

**Program:** Engineer 2's directive (docs/directives/ENGINEER_2_DIRECTIVE.md
§4.1 — the primary lever) + ROADMAP3 §4.3 ("worker-side: software-pipelined
span endings — the biggest unbuilt lever").

**Baseline to beat:** sustained full verification 1,331,355,913 msg/s
(36.82 GB/s delivered CRC, 0 allocs, Shard 44) — the workers' serial
ending chain is the directive's 26% latency blob: ~45-50 cyc/span of the
~179-195 cyc/span real-mix worker budget (docs/23 §5).

**Target:** +15-20% worker evaluation throughput → sustained toward
≥ 2.0B msg/s on the hunted 8573C-class draws.

---

## 1. The lever (why the ending serializes)

The production drain (the ring worker, post-R17 default) evaluates one
span at a time: `kernel.eval(body)` = vector fold → lane-0 tail → lane
endings → FNV combine, strictly begin-after-end per span. Three serial
latency chains sit at the end of every span:

* the lane-0 tail (the vtail composed field + `vend_xmm`, or the R14
  `fold_extra` chain + crc32 tail — ~15-25 cyc of chained xmm clmuls /
  crc32 links on p1);
* the lane endings' store/reload + odd-word `crc32` chains (p1);
* the FNV-1a-64 combine — 9 dependent `imul` links, ~27 cyc of pure
  p1 latency, frozen by rule 2 (the value definition).

Nothing in the value definition couples one span's ending to the next
span's fold (rule 2 freezes per-span semantics, not the schedule). The
ending blob is therefore *hideable* latency: the R10 pair (eval_pair's
deferred ending) proved the mechanism compiles and holds bit-parity; it
never became the default because the CI arms priced it neutral through
the pre-R13 stack (the fold then ran the mirror kernel at 8 p5 uops/step
— a different bottleneck regime).

## 2. The design (the quad)

`span_fold_eval_quad_r` (crcfold.rs) — the R10 pair shape generalized
to ILP=4, straight-line per quad:

```
Stage 1: four vector folds          (p5 — the clmul streams)
Stage 2: four lane-0 tails          (p1 — four INDEPENDENT chains)
Stage 3: four lane endings          (p5 vend_zmm + p1 odd words)
Stage 4: four FNV combines           (p1 — four independent imul chains)
```

The ending stack is split into stage functions (`ep_lane0_vtail`,
`ep_lane0_cont`, `ep_lane0_dispatch`, `ep_lanes`, `ep_fnv`) extracted
verbatim from `finish_span_r_vtail` / `finish_span_r_inner` — the serial
path is UNTOUCHED (zero regression risk to the default arms; the
differential pins the composition on every dispatch axis). The quad
dispatch mirrors `finish_span_r_inner3` exactly (vend/vtail/dfold env
gates, the r≥16 vtail economy gate included).

Bit-parity: `t_endpipe_quad_differential` — 12 length tuples × 4
dispatch axes × four DIFFERENT bodies per quad (cross-span independence
proof) — the register-path quad, the checkpoint serial (`fold_span` →
`end_span`), and the checkpoint pipelined schedule (`end_quad4`) all
== the scalar kernel, per slot.

## 3. The worker integration (HFT_ENDPIPE=1)

`lane_worker`'s drain gains a flat pipelined form that runs BEFORE the
legacy loop and consumes the whole batch (`i → n`); the legacy loop then
no-ops on entry — the non-ENDPIPE arms stay byte-identical (the emit
closure is hoisted and shared; the diff to the default path is the
closure's address, not its behavior). Group formation:

* a QUAD when four descs are in range with no anchor among them (three
  `desc8_anchor_ahead` checks — 63/64 predicted not-taken at the 64-span
  chunk grid);
* a PAIR (the R10 shape) where two are in range anchor-free — the tail
  fallback;
* a single span otherwise (null-mode keeps singles: the diagnostic stub
  must not pipeline).

Knob precedence: ENDPIPE > PIPE > EVAL2 > TRI (the CI arms never combine
them). `CrcKernel::eval_quad`: Scalar = 4 serial evals, Fold512 = 2
pairs, Reflect = the quad.

## 4. The R12 spray demotion (found and fixed inside the arm)

Reading the drain loop's brace nesting against its git history exposed
a latent regression: R12's anchor handling (commit 3f4b26c) wrapped the
eval branches in an inner `while i < n` loop that drains the batch —
leaving the OUTER loop (which holds the prefetch spray block) running
ONCE PER BATCH. Since R12 the ring worker's spray issued ≤ `burst`
(24) lines per batch (~1 span) instead of per span; the PfCfg doc still
describes the per-span design, and the R9-era +36% spray evidence
predates the restructure. The flat drain runs the spray PER GROUP with
the budget scaled for the group plus the pipeline's fold reach:

* **lead** = `HFT_PF_AHEAD + 4` spans (EP_LEAD = the quad's reach — the
  next quad's bodies must be in flight while this quad's endings retire);
* **budget** = `burst × (g + 4)` lines per group (the R8/R9 smoothing
  cadence restored: ~burst lines per evaluated span);
* `HFT_PF_LINES=0` disarms the spray inside the arm (the attribution
  control); `HFT_PF_AHEAD` sweeps the directive's 8-12 span window.

## 5. The instruments (kbench)

* `fold512_end16s` / `fold512_end16p` — endings of 16-span groups, serial
  vs pipelined, fold states precomputed untimed (the OnceLock pattern;
  the ROADMAP §4.3 measure-first row). Decision rule: end16p/end16s ≥ 1.5
  on a healthy target-class draw certifies the modeled win.
* `fold512_r_quad` — the register-path quad vs `fold512_r` (the
  `fold512_r_pair` precedent at 4).

## 6. Local evidence (the honest weak draw)

Local sandbox: 1-vCPU Granite Rapids VM (family 6 model 0xAD), ~19-22
GB/s fold512_r (healthy fleet draws: 30-34.9 — a weak, contended,
draw-adjusted-report-only environment), ±5-8% inter-run spread, and
must-be-ordered rows (r vs noend) inverting between runs.

* `t_endpipe_quad_differential`: green, all axes.
* Full nf-testkit suite with `HFT_ENDPIPE=1`: 66/66 green (chaos
  bitparity `0x881639cead506f25`, allocs=0).
* Fabric (1 worker, degenerate main-bound topology — worker 73% busy):
  burst verdict +1.2%, per-span eval at parity, sustained bit-exact.
* kbench (3 runs, stable ±2%): `r` 21.9-22.5, `r_pair` 18.4 (the R10
  pair), `r_quad` 19.9-20.3, `end16s` 132-136, `end16p` 120-122.

**Readings:**

1. The quad BEATS the existing R10 pair on the packed corpus locally
   (19.9-20.3 vs 18.4) but sits below serial — consistent with the
   refutation ledger's fold-interleave verdict (port-issue-bound) NOT
   applying here (no fold interleave; ending deferral only), and with
   the packed corpus hiding the blob (L1/L2-hot streaming; the real mix
   is L3-resident with supply stalls — fbench P vs K: 335 vs 517
   cyc/span).
2. The end16 twins' serial row self-overlaps through the OoO engine
   (independent iterations) — the naked-ending ratio underprices the
   pipeline's real effect; the FABRIC arm is the decisive instrument
   (the R15 vtail precedent: kbench-neutral, fabric-decided).
3. The fleet decides: CI arm `11ep` (bit-exact + allocs=0 gated per
   draw) + the pf sweep sub-runs (HFT_PF_AHEAD=8/12).

## 7. Round 1 fleet verdict (run 37371810788 — 63 certified shards)

**The lever is REFUTED on the fabric.** Draw-adjusted sustained (11ep vs
11b, same shard, back-to-back arms):

| Shard | class (clock) | 11b default | ENDPIPE | Δ |
|---|---|---|---|---|
| 16 | 8370C (2.79 GHz) | 1049M | 835M | -20.4% |
| 42 | 8370C (2.79 GHz) | 1024M | 853M | -16.7% |
| 64 | 8370C | 1003M | 858M | -14.5% |
| 67 | 8370C | 1018M | 840M | -17.5% |
| 70 | 8370C | 989M | 779M | -21.2% |
| 65 | 8573C | 1239M | 1043M | -15.8% |
| 80 | 8573C | 1254M | 1061M | -15.4% |

Median **-16.7%** (range -14.5..-21.2) across 2 silicon classes — 5x
beyond the ±3.4% arm-position noise floor. Both frozen contracts held on
every draw: `HYDRA_BITPARITY == 0x881639cead506f25` bit-exact,
`ALLOC_DELTA == 0`. The kbench rows on the healthy draw: `end16p/end16s
= 127.14/133.07 = 0.955` (the ROADMAP's own ≥ 1.5 build rule fires the
kill), `r_quad/r = 29.41/31.07 = 0.947`. The pf sweep: deeper lead is
monotonically worse (853 → 798 → 792 on shard 42; res_waits appear at
lead 8+ — 1.4K-21K waits vs 0).

### 7.1 The mechanism (why the modeled win inverted)

The workers are ~99% busy in BOTH arms (eval_ms ≈ 4.95s of 5.0s), but
per-span cost rose 91.9 → 118 ns (+28%) on shard 42. The modeled
~45-50 cyc/span serial ending blob does NOT exist as *hideable* latency
in the serial drain: the OoO engine already overlaps span k+1's fold
with span k's ending (the end16s row proves it — the "serial" ending
loop runs at ~28 cyc/span throughput, 2.3x below its ~50-cyc critical
path, i.e. the machine pipelines it on its own). The drain's per-span
structure (small register footprint, one fold loop + one ending in
flight, one sequential load stream) is exactly what the OoO window
wants; the quad's 4-wide state (8 zmm fold states + 4 ending sets +
grouped emission) adds pressure the wide core pays for without latency
to hide — a -5.3% kernel-level penalty amplified to -17% by the
supply-coupled real mix. This is the R10 pair's historical neutrality,
re-priced in the R17-era worker-bound regime where kernel-level losses
now pass through to the fabric.

### 7.2 The R12 spray finding, re-priced

The restored per-group spray cannot be net-positive on this corpus: the
default arm (quad + spray, lead 6) already loses 16.7%, and every
deepening (8, 12) loses more. With the R9-era aliasing + the hardware
streamer covering the sequential real-mix layout, the spray hints now
cost issue slots and L1/L2 pressure without covering exposed latency —
i.e. the R12 "demotion" was accidentally the right economics on the
current stack (the PfCfg knobs are effectively vestigial on the ring
path). Round 2's `HFT_PF_LINES=0` control prices the spray's exact
share of the -16.7%.

## 8. Round 2 (the decomposition) + the standing verdict

Round 2 (run 37378348020, 81/81 shards green, 7 fresh 8573C-class
draws) completed the attribution. Sustained per shard — 11b default,
ENDPIPE (quad + spray), NOSPRAY (quad alone, `HFT_PF_LINES=0`):

| Shard | 11b | quad+spray | quad alone | quad Δ | spray share |
|---|---|---|---|---|---|
| 10 | 992M | 869M | 986M | -0.6% | -11.8% |
| 11 | 1136M | 948M | 1075M | -5.4% | -11.1% |
| 21 | 1171M | 955M | 1012M | -13.6% | -11.9% |
| 35 | 1164M | 900M | 1027M | -11.8% | -10.9% |
| 5 | 1298M | 1071M | 1230M | -5.2% | -12.2% |
| 69 | 1202M | 977M | 1106M | -8.0% | -10.7% |
| 70 | 1175M | 962M | 1100M | -6.4% | -11.7% |

**The decomposition:**

* **The quad alone: median -6.4%** (range -0.6..-13.6) — a net loss on
  every draw but one (shard 10's -0.6% is inside the noise floor, and it
  is the best case). The modeled +13-16% is inverted: the ending chains
  the quad was built to hide are already hidden by the OoO engine in
  the serial drain; the 4-wide register footprint only costs.
* **The spray restore: an additional consistent -11..-12%** on every
  draw. The R12 spray demotion was accidentally LOAD-BEARING for
  performance: on the R9-aliased blob with the hardware streamer
  covering the sequential layout, per-span spray hints cost issue slots
  and cache pressure while covering no exposed latency. The PfCfg
  spray is confirmed vestigial (and net-harmful if resurrected) on the
  ring path — the directive's Priority 2 (MLP prefetch deepening) is
  refuted with it: every lead ≥ the restored baseline measured worse
  (round 1's monotone 853 → 798 → 792).

**The standing verdict (both rounds, 14 draws, 2 silicon classes, 144
certified shard runs):**

* **The ENDPIPE quad does not ship as a default.** The lever's premise
  (a hideable 26% serial ending blob) is measured false on the
  production drain. The machinery stays merged as the armed attribution
  arm (the eval_pair/eval2/tri precedent: `HFT_ENDPIPE=1`, rollback
  default-off, 11ep priced per draw) — negative machinery with a clean
  differential is cheap to keep and the knob documents itself.
* **The kbench rows stay** (they killed the lever honestly and will
  price any future ending-side idea: end16s IS the ending's true
  throughput floor, ~127-133 GB/s on healthy 8573C draws — the ending
  is a ~28 cyc/span THROUGHPUT cost the machine already pipelines, not
  a latency blob).
* **The R12 spray demotion is documented, not "fixed"**: the finding
  stands (the spray has been dead since R12), and both rounds say
  restoring it is net-negative on the current stack.
* **The 2B program impact** (the honest arithmetic): with the ending
  lever refuted, the worker side has no remaining hideable-latency
  headroom by software pipelining; the sustained path to 2B runs
  through Route K (kernel density — the R16a/R17t programs) and the
  main-side levers (Engineer 1's ingest scan, Engineer 3's descriptor
  packing), not the ending. A negative result with full attribution —
  exactly what the challenge's scoring section says counts.
