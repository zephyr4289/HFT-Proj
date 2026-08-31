The apparatus just did the two things it was built for in one report: **the bias probe FAILED loudly on a real discovery** (fixed-stride sampling carries +6.96% systematic aliasing — that's a finding about *every sampled number this project ever printed*, including the 122/172 headline), and **H10 was refuted with a clean three-way split.** And sitting unnamed in your model table is the single most valuable result of the entire Target-1 campaign, so let me name it first:

**The doc 05 model was wrong for an instructive, generalizable reason: it budgeted FNV-1a at ~1.4 cyc/byte using throughput math, but FNV-1a is a serial multiply-accumulate dependency chain — it runs at *imul latency* per byte, ~3.7 cycles, and 29 bytes × 3.7 ≈ 107 is exactly what you measured.** Uops-throughput vs dependency-latency is the classic performance-modeling trap, your measurement caught it, and the physics explanation in your table is correct. That goes in the final report's *falsified* section as the model's cause of death — and it retroactively explains why every hash-bound run in this project cost double the budget.

Now the audit. Three findings, and the first two are partly mine — my B-3b law was ambiguous about *which configuration* the closure arithmetic must run in, and the ambiguity got exploited:

## F-43 — The entire table silently re-measured under instrumentation, and findings were closed cross-config

Same runner, same frequency, two reports apart:

| Metric | Wave 2.0 (clean) | Wave 2.2 (as-run) | Δ |
|---|---|---|---|
| c₀ total | 110.04 | 143.34 | +30% |
| engine core c₁ | 26.50 | 35.50 | +34% |
| Δ_sink | 83.54 | 107.84 | +29% |

Nothing in the report mentions this. The Wave 2.2 arms ran with probe instrumentation active (dense sampling / gap stamps — the tax explains a uniform ~30% inflation across every stage), and the report presents the instrumented numbers as *the* numbers, closes F-40 by answering a question posed about the **clean** 83.54 with an answer measured at **107.84** — different configuration, no provenance row, no config column. That's F-24's silent-churn disease and F-38's cross-silicon disease fused into one. **Fix: every table row carries an instrument-config tag (`sampling=none | 1-in-256 | 1-in-4 | gap-probe`), the clean and instrumented decompositions are presented as two tables, and H10's closure is restated as proportions — 99.8% hash / 0.2% dispatch — which transfer across configs; the absolute values don't.** My law should have said this; it didn't; that's mine.

## F-44 — The R1 "closure" compares two instruments sharing the same pollution, the 0.58% is a phantom, and the *real* closure — pure arithmetic on numbers you already have — was skipped

The 0.11% gap-vs-rate agreement is *internal consistency under the instrumented config* — both numbers carry the same tax, so of course they agree. The clean-config 17.6% was never decomposed. Worse: the F-39 closure line cites "bias (0.58%)" — **a number that appears nowhere in any measurement in this report.** That's the F-29 signature, third occurrence: a value born in the verdict layer. Source it or strike it.

Meanwhile the honest closure is sitting in your own data, zero new experiments:

```
bracket_mean(clean, 1-in-256) = 129.40  = work + mark_floor + bias
  mark_floor (measured)        =  ~36
  bias (measured)              =  +6.96% on the work term
  → work ≈ (129.40 − 36)/1.0696 ≈ 87.2 cyc     [in-bracket work]
rate(clean)                    = 110.04 = work + out_of_bracket
  → out_of_bracket ≈ 22.8 cyc   [loop/transport outside the bracket]
Closure: work + out_of_bracket = 110.0 vs rate 110.04 → residual ~0.04%
```

Every term measured, every term named, residual computable from numbers already committed. *This* closes F-39 — bracket mean, mark floor, sampling bias, out-of-bracket overhead, and the wall rate reconciled into one identity. Also fix the label: your "mean gap" is actually the *sparse-sampled period estimate* (it includes all 256 messages' work, not just the gaps) — rename it, because the current name makes the 0.11% look like a bracket-vs-gap split it isn't.

## F-45 — The model-reconciliation table's verdicts lie in two rows and never sum

- **Transport: model 10, measured 15.94, "CONFIRMED"** — that's +59%, and CONFIRMED is false. Fourth occurrence of the verdict-column disease.
- **The totals are never summed: model ≈ 72 cyc vs measured ≈ 144 — the aggregate model is off by 2×**, and five per-row CONFIRMEDs hide it. The honest verdict: doc 05's budget table missed total by 2×, driven by hash (+68, latency-chain physics, *explained*) and transport (+6, unexplained — that's a real open row).
- Sequencer "EXACT match" (15 vs 15.64) is configuration luck — the clean config measured 11.61 and matched nothing. Verdicts may not cherry-pick which config agrees.

## F-46/F-47 — two minors

The bias mechanism ("aligns with batch leaders") is *asserted* — but the aliasing pattern of stride-256 against ~5-msg packets is deterministic and computable; compute it, or bound the bias as "aliasing + perturbation, mixed." And A₃ ≡ A₄ at identical rates to five significant figures while other arms show percent-level spread is the F-19 smell in miniature: either report rates with spread at honest precision, or the A₄ config didn't take. Check which.

## What stands, and what the data now says

Standing: the bias discovery, the H10 refutation (monomorphized dispatch genuinely collapses — a real null for the ledger), the sub-noise bounds law applied, monotone guarded chain, honest R2 tiering, and the latency-chain physics. One document — the **closure memo** — finishes Target-1: two config-tagged tables, the F-44 arithmetic above, corrected model verdicts with the summed 2× divergence stated, the phantom 0.58% resolved, F-46/F-47 cleaned. No new experiments. It's an afternoon of honest accounting.

And then the decision the data has already made for you: **the engine core is ~26.5 cycles clean. Sequencer ~11.6, walk ~2.9, poll ~11.6, everything else sub-1. The dominant cost in every benchmark this project has run is the test harness's hash — which is not the product.** Per L-LICENSE, an optimization hypothesis must name cycles in a component that matters to the product; against *these* numbers, none of H6/H7/H8 clears the bar. The honest conclusion of Target-1 is: *measured, attributed, model corrected, engine near its floor — no optimization warranted.* "Let's queue experiments anyway" would be the buzzword instinct returning for one last attempt, wearing a lab coat.

Bring the closure memo. Then the five-section final report and the claims-scope README, and this project ends the only way it ever could have: not with the fastest number anyone could print, but with numbers nobody can take away.
