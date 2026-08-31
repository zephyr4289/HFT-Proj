This is the report where the apparatus catches its own FAIL — and that's the milestone. **R1 = 17.60% with the verdict honestly printed as FAIL.** The gate that was born from F-35 (a fake 0.01% residual) has now survived its first real stress and refused to lie. Two failure demos with raw output. Nesting clean, guards on, runner identity printed on every line. After a month of verdict columns disagreeing with their numbers, a verdict column that *fails loudly at 17.6%* is the single most important output this project has produced. Read that twice before the findings, because the findings are about the number the gate just handed us — and the gate being *right* is what makes them actionable.

## F-39 — R1 = 17.6% is not "TSC perturbation." It's an overdetermined system of one equation, and I've been sloppy about it all phase

The report waves R1 off as "bracket mean incurs serialization perturbation." Look harder. Bracket-sampled mean = **129.40**, rate-space total = **110.04**, and the count-sink variant's *un-sunk* engine runs at c₁ = 26.50 cyc — while the bracket's own work_residual on this runner was 84. The instruments don't disagree because one is noisy; they disagree because **they're measuring different populations and I never forced the accounting to close.** Three specific holes, all mine:

1. **Mean vs median, but also: sampled-mean ≠ population-mean.** 1-in-256 sampling is unbiased only if sample position is independent of latency. Message latencies in a batch are *correlated* (leaders slow, tails fast — your own F-19 showed packet-position structure). A stratified-by-position sample of a correlated population has systematic bias, and 19 cycles is exactly the size such bias produces. My B-3 law said "report MEAN" — it never said "make the sample unbiased." Spec hole.
2. **The bracket includes the sink (129.40 ≈ 110.04 total-ish). But rate-space c₀ *also* includes the sink.** So where does 19 cycles of *difference* live? It's the difference between "what the marks see between m0 and m3" and "what the wall clock sees per message" — i.e., **the inter-mark gap, the loop overhead outside the bracket, and the sampling bias, all summed into one number we can't decompose.** One equation, three unknowns.
3. **The dose-response proved the bracket is *linear* (slope 0.54) — linear ≠ calibrated.** It measures work *proportionally*, but we never established the *offset* behavior when real (non-synthetic) work with memory traffic, branch patterns, and cache state sits in the bracket. Real work may distort the measurement *distribution* (not the floor) in ways synthetic ALU loops can't test.

**The fix is not "tolerate R1" and not "subtract a fudge."** It's closing the system with a third measurement that separates the unknowns — and it's cheap:

```
B-3b OVERDETERMINATION LAW:
  (i) Bias probe: run the bracket in DENSE mode (1-in-4) and SPARSE
      (1-in-256). If dense-mean ≈ sparse-mean within 2%, sampling
      bias is bounded and dead as a hypothesis. If not — measured
      bias, named, subtracted with a provenance row.
  (ii) Gap probe: stamp m3[i] and m0[i+1] on sampled messages only;
      accumulate Σ(gap). Then the closure identity is:
        rate_c0 = bracket_mean + mean_gap + non_sampled_error
      Each term measured, each term in the table, residual must
      close ≤2% across the *sum*, not across one fabricated
      comparison.
  (iii) R1 is reported as the COMPOSITE closure residual of (ii),
       never as a raw two-number subtraction again.
```

That converts R1 from "an instrument fails" into "an instrument got decomposed" — and it's the last piece standing between this decomposition table and citability.

## F-40 — The hash-sink number moved by 28% and the table doesn't mention it

Phase B (unguarded, wrong runner, DCE-contaminated): Δ_sink = 83.33. Phase B-Redo (guarded, 8573C): **Δ_sink = 83.54.** Nearly identical... except Phase B's 83.33 was measured at *2445 MHz on different silicon* and included DCE collateral damage. The new 83.54 is clean — **and doc 05's model said ~40.** The hash-sink defendant is now off by 2×, and nobody subpoenaed it. The B-2 split exists precisely to isolate this: **83.54 cycles for FNV-1a over ~29-byte messages is ~3 cycles/byte — an order above what one FNV round should cost.** This is either (a) real: per-message sink-call overhead (trait dispatch, proof pass, function boundaries) dominates and the *hash itself* is small — findable by one more ectomy level (count-sink vs fnv-sink vs black-hole-sink, three-way split), or (b) the guards themselves are costing (black_box per message) — findable the same way. **New pre-registered hypothesis H10: per-message sink *invocation* overhead (dispatch + proof pass) exceeds the hash computation itself.** Discriminator: the three-way sink split. And note what this does to the *narrative*: if H10 confirms, the honest final-report sentence becomes "engine core 26.5 cyc; harness sink 83.5 of which hash ~X and invocation ~Y" — and every latency figure in the README inherits the split. This is exactly the class of thing TARGET-1 existed to find.

## F-41 — Two adjacent deltas of 0.25 and 0.07 cycles are *below your noise floor* and being reported as component costs

You have full_p50 spread of ~24 cycles at the runner level, 20-run medians with run-to-run variance — and the table reports "Proof minting: 0.25 cyc" and "ITCH validation: 0.07 cyc" as isolated components. **Those aren't measurements; they're differences of noisy quantities.** A±1.5 and B±1.5 do not resolve a 0.07 difference. The dose-response arm showed *10 injected iterations* move the needle by 6 cycles — a 0.07-cycle "component" is two orders below what this instrument can resolve, and presenting it as an isolated cost is the sub-cycle claim (F-33) reincarnated with a decimal point. Law: **any delta below the arm-pair noise floor (computed from the two arms' run spreads) is reported as "unresolvable at current instrument precision — upper bound <X cyc," never as a point value.** The interesting content is the *upper bounds*: proof minting < ~1 cyc, ITCH validation < ~1 cyc — both *consistent with* the analytical model, which is a real reconciliation result and should be stated as such.

## F-42 — R2 says "≈3.2%" with no lane, no counter list, no run URL

"Tracked on bare-metal PMU lanes" — *there is no bare metal.* You're on GitHub runners. That number has no provenance row, no counters enumerated (cycles? instructions? IPC?), no instruction on reproduction. It cannot be cited; per the claims-scope law it shouldn't have been printed. Either R2 runs via `perf stat` in the CI container (available on GH runners with `sudo perf` if enabled — verify, don't assume) with counters and URL in the provenance table, **or R2 is TIERED to the bare-metal appendix until the box exists** and B-3's gate stands on R1-closure alone. No in-between state.

## State of Target-1 — one closing move, then this table is citable

The decomposition now has a defensible skeleton: total 110.04 (±R1-closure), hash 83.54, engine 26.50, seq 11.61, walk 2.85, poll 11.62, proof/parse upper-bounded <1, transport 11.62 baseline. Every number carries a runner line. The remaining work is a *closure protocol*, not new machinery:

1. **B-3b probes** (dense/sparse bias + gap stamps) → R1 becomes a composite closure with each term named. Expect the 19.4 to dissolve into gap + bias + a small true residual — and each piece lands in the report as a *finding*.
2. **H10 three-way sink split** → the biggest single number in the table gets explained instead of merely measured. This is the headline hunt: what's actually inside 83.5?
3. **F-41 precision law** → upper-bound wording for sub-noise deltas, applied retroactively to the two existing rows.
4. **R2 disposition** — perf-in-CI with counters or TIERED. Pick one, provenance it.
5. **Doc 05 §12's budget table gets its verdict as defendant:** per-component, model-vs-measured, with the 2× hash divergence and the confirmed sub-1-cycle components written into the reconciliation. That's the model-competing-against-measurement the critic demanded at the very beginning of TARGET-1 — and it's now possible because the measurements finally exist.

Bring me the dense/sparse pair and the gap-stamp closure first — those two numbers either kill the 17.6% or decompose it into named terms, and either way R1's FAIL turns into the most explanatory single line in the study. Then the sink split. You're one probe and one split away from a cycle table where every term is measured, bounded, or named — which is the entire mandate of TARGET-1, finally within reach.
