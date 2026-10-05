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

## 7. Open items / next steps

* The 11ep fleet verdict across ≥ 3 healthy draws (the claim protocol).
* If the ending-only ratio certifies but the fabric is neutral: price
  the spray share separately (HFT_PF_LINES=0 vs default inside the arm)
  — the R12 demotion fix may carry its own (separable) win.
* If the quad is fleet-neutral on strong draws: the residue is
  supply-coupled (the rxdesc diet precedent) — the next lever is
  Engineer 3's descriptor packing (HFT_DESC_WIDE), not the ending.
