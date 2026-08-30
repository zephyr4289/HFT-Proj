**Stop — before celebrating "100% Green": this study is a *triumph* whose report fails its own exit gate, and the distinction is the whole point.** The triumph first: pre-registration ordering held (`aad4644` before `7babed4` — the commit history now proves no retrofit), the three-outcome F-9 rule was applied honestly (outcome 2, the subtlest one), and M-AUD did what it existed to do: **it falsified our own benchmark's unit of measurement.** That's the single most valuable outcome available — a hostile reviewer would have found it in minutes; we found it ourselves, with proof of when we knew.

But now the audit, because your report contains four findings, and the first one is the headline you *reported as a verdict* instead of acting on:

## F-11 — The M-AUD law was violated: the study proceeded on the marks it declared wrong

Doc 15 §2, verbatim: *"The study does not proceed on ambiguous marks… the bench re-baselines… all previously published percentiles are re-reported."* Your own H4 finding says marks bracket `ingest` **per packet** (~5 messages each). That's F-10 fired — and instead of re-baselining per-message marks and re-running, the study classified the *misplaced* distribution and called the artifact a finding. Walk the consequences:

- Every number in your arms table — p50=26, p90=4,544, p99=4,672, the entire taxonomy — is a **packet-level statistic**. The "4,500-cycle cluster" is the batch charged to its leader message. That's a property of the *instrumentation*, not the engine. H4's verdict describes the ruler, not the measured object.
- Worse: the original bench (p50=44, p99=4,032) ran the same code path — so **PR-2's published percentiles were also packet-amortized, mislabeled as per-message.** And note the unexplained shift: p50 moved 44→26 between "identical" configurations, *while adding instrumentation* — added work made the median faster by 40%? Something changed between runs (placement? render inclusion?) and the report doesn't say what. Two published latency tables now exist with unproven sampling units.

The action is unchanged and non-negotiable: **per-message marks, re-run all three arms, re-publish both placements' numbers.** Then — and only then — the taxonomy runs on real data. The genuinely interesting question is still unanswered: *what is the actual per-message latency distribution?* Maybe it's flat and the entire old p99 was instrumentation. Maybe a real leader-tail survives (see H5 below). Either answer is a finding; right now we have neither.

## F-12 — The taxonomy's denominator doesn't reconcile with its own histogram

p99 of 505,849 messages means ~**5,058** samples above p99, by definition. Your taxonomy classifies 480 + 12 = **492** — 9.7% of the population the law covers. Even if the sample base is packets (101,194 → ~1,012 above p99), 492 is half of *that*. So "100% attribution, unknown 0.00%" is computed over an unexplained denominator, and the classifier's top priority (batch_boundary) is precisely the artifact category F-11 identified — a cascade whose first-match rule assigns nearly everything to the measurement bug will trivially show 0% unknown. **The taxonomy law is not met.** Acceptance in phase 2: `count(latency > p99) == Σ taxonomy counts`, asserted *in code*, with the denominator printed in the table header.

## F-13 — The empty arm is broken as a control, two independent ways

1. **A no-op control cannot be 3.6× slower than the work it removes.** Empty: 6.06M msg/s. Full engine: 22.1M msg/s. That's only possible if the loop skeleton differs — most likely the empty arm skips the per-message batch loop, violating §4's "same skeleton" requirement. Whatever the cause, a control that visibly measures something else cannot resolve F-7.
2. **p50..p99.99 = 0 is physically suspect for raw marks.** Two serialized TSC reads around a no-op floor at ~20–30 cycles (your own mark-pair measurement says ~30 — and max=26 is *consistent* with that floor, which is the one good thing here). A median of exactly zero means the recorded value is post-processed (overhead-subtracted and clamped?) or the stamps collapsed in this arm. Doc 15 §7 required *raw and adjusted, both labeled* — the report gives neither label, for any arm. And note: a floor of 26–30 cycles validates the *existence* of overhead; it does **not** validate subtracting it from a 44-cycle distribution — that was the critic's exact non-linearity point. The control's correct use: report its raw distribution alongside, never subtract-and-claim.

**Phase-2 acceptance for the control: empty rate > full rate.** If a no-op doesn't outrun the real work, the skeletons differ, full stop.

## F-14 — The prefault arm contradicts the H1 verdict's "real" half, quantitatively

The verdict says faults are real (they fire in render) and outside the window (in-window percentiles identical). But check the rate integral: 3,840 faults × even a *modest* 500 cycles each = 0.83 ms on a ~22.9 ms run ≈ 3.6% expected rate gain. Observed: **+0.06%.** For that delta, faults must cost ~10–30 cycles each — essentially free, because the `.itch` file was just written by `zcat` and its pages sit in a hot page cache, likely prefetched — meaning F-9 is closer to outcome 3 (*refuted, with nuance: the pages exist but the cost evaporates in the page cache*) than outcome 2. And simultaneously, the prefault arm's upper tail got **worse** (p99.9: 5,632 → 10,496) — unexplained, and incompatible with "identical between arms." **New standing law:** *any mechanism claimed to own N samples at M cycles must be visible in the rate at N×M — histogram and rate must reconcile.* You have both numbers in every run; the check is free.

## Gate verdict

**G12-T1: OPEN — phase 2 required.** Not because the study failed — because it *succeeded at falsification* and then declared victory one step early. Its own exit gate (valid instrumentation, reconciled denominator, raw/adjusted labeling) isn't met. One more observation worth sitting with: the two most suspicious numbers in your report — "100% Green" and "unknown 0.00%" — were the first two things to break under audit. Perfection claims are where the bodies are buried. A taxonomy with 4% unknown honestly reported beats one with 0% unknown and a broken denominator.

## Phase-2 protocol (paste into doc 15 as §8; commit before running)

```markdown
## §8 Phase 2 (post-F-11..F-14) — Re-baseline and Re-attribution

Laws (standing):
  P2-L1 (unit law): m0/m3 are PER-MESSAGE (ingest entry → dispatch
  return). Unit test: synthetic 5-msg packet yields 5 stamps with
  m0[i+1] ≥ m3[i]. Old per-packet numbers re-published alongside,
  both labeled, in doc 11 §3 amendment.
  P2-L2 (denominator law): count(latency > p99) == Σ taxonomy counts,
  asserted in code; denominator printed in every taxonomy table.
  P2-L3 (reconciliation law): every claimed mechanism (N samples × M
  cycles) must be visible in the rate delta at N×M; table row required.
  P2-L4 (control law): empty arm keeps the identical skeleton incl.
  the message loop over a no-op body; RAW only; acceptance:
  rate(empty) > rate(full).
  P2-L5 (labeling law): every table column marked raw or
  overhead-adjusted; no mixed arithmetic.

Arms: cold / prefault / empty, 5 runs, medians + spread.
Pre-registered H5: packet-leader cache misses (first ingest touch of
packet bytes: frame line, gt source lines, sink state) produce any
surviving per-message leader tail. Prediction: leader tail scales with
packet size; kill: tail uniform across batch positions.
F-9 final verdict re-issued after rate-reconciliation (expected drift
toward refuted-with-nuance: page-cache-resident faults ~free).
```

Execution order: **marks fix + unit test → empty-arm fix (verify it outruns full) → re-run arms → re-publish PR-2 with both units → re-attributed taxonomy under P2-L2 → reconciliation table under P2-L3 → updated study-report.md.** Report back with the per-message distribution — I want to see whether the tail survives honest measurement, and both answers are wins: flat tail means the engine is even better than the broken bench suggested and every prior p99 was instrumentation; surviving tail means H5 is live and we instrument cache behavior next.

T2 (window sweep) and T3 (reference arbitrator) remain queued behind this — T3 especially, since a per-message re-baseline will renumber every latency claim the reference work will be judged against. One thing at a time: **fix the ruler, then measure.**
