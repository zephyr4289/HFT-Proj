**G12-T1 Phase 2: the study finally earned its verdict — and its own numbers just produced the biggest finding yet, which the report once again walks past while announcing "100% Green."** The discipline held where it counted: pre-registration ordering, the unit fix with both units published, a reconciled denominator asserted in code, a control that finally behaves like a control (empty now outruns full, 1.56×), and F-9's final verdict — *refuted with nuance* — is exactly what the page-cache arithmetic predicted. The per-message reality is genuinely beautiful: **sub-250-cycle dispatch for 99.99% of messages, tail included.** The old 4,032-cycle p99 was 94% instrumentation, and now we know it to the sample. That's the study doing its job.

Now the finding your report buried at the bottom of a table without noticing it:

## F-15 — The re-baselined bench falsified the published throughput number: PR-1 is not met

Look at your own rate column: **cold = 9.45M msg/s — down from 22.86M.** And the empty arm — the *no-op skeleton* — runs at 14.7M. Read that ratio: **the measurement scaffold alone, doing literally no engine work, costs more than half of the 10M target.** Two candidate stories, and the report doesn't distinguish them:

**(a) Per-message marks are the tax.** Four serialized TSC reads per message at ~52 cycles/pair… two pairs ≈ 100+ cycles of measurement per message on a ~105-cycle/message budget (3.5 GHz-class core at 9.4M/s). That's not distortion; that's the instrument consuming the patient. Doc 11's own sampling law (§3: total every message, *stages sampled every 256th*) exists precisely to prevent this — the phase-2 re-baseline may have put all four marks on every message.

**(b) The honest unit shift.** The old 22.86M was measured on packet-amortized marks — the same broken ruler that produced p99=4,032. If the old rate was *also* per-packet-inflated, then 9.4M is closer to truth, and PR-1's "≥10M" is unmet by the bench build on CI hardware.

The reconciliation table says (a) is at least half the story: ~93.6% of runtime in steady ingest at ~25 cycles/message ⇒ **the engine's own un-instrumented budget implies ~5.9M–15M msg/s depending on what the loop costs outside dispatch — and the empty arm says the loop+scaffold is 68 ns/message.** The un-instrumented release binary (the one the nm lane proves clean) is the only legitimate place to measure PR-1, and *that number has never been taken.* 

**Resolution protocol, non-negotiable:** PR-1 moves to the un-instrumented bench — rate measured by wall-clock × message count over the full run (no marks at all), plus a marks-every-256th variant to quantify the instrument tax. Then one of two headlines: *"PR-1 met: X M msg/s un-instrumented; instrument tax Y%"* — or *"PR-1 unmet on CI reference hardware: X M msg/s; shortfall attributed: measurement Z%, loop N%"* with the loop's cost profiled (perf stat or the sampled marks) before any optimization is permitted. If optimization does become necessary, it starts at the top of the reconciliation table — but the analysis phase comes first, always. And doc 11 §1's claim table gets a one-line amendment: PR-1 numbers are wall-clock from the un-instrumented build; instrumented builds report throughput only for reconciliation.

## F-16 — The taxonomy is over-fit to one cause, and H5's test hasn't actually run

99.41% of the above-p99 tail = inter_msg_gap, 2 samples = batch_boundary. But H5's pre-registered prediction was *"leader tail scales with packet size"* — a **packet-size sweep**, which hasn't run. Meanwhile your own reconciliation table assigns 544 batch-leader samples × 80 cycles ≈ 0.3% — the prediction *could* still be true at exactly the magnitude where it's invisible next to preemption noise. Also unresolved from F-12: above-p90 = 21,010 counts against ~25,292 expected samples — still unexplained, still unasserted. Phase-2 closure requires: the packet-size sweep for H5 (Fixed(1) vs Fixed(16) vs MtuBound — trivial with the fabricator), the p90 denominator reconciled the same way p99 now is, and one honesty line in the report if the answer is "H5's effect exists at 0.3% and is not resolvable at this scale" — a null result, pre-registered, cleanly closed. **That last one is exactly the "what we falsified" material the final report needs more of.**

## F-17 — Small, but it's the same smell: "adjusted = raw − 52" everywhere

Empty arm: raw median 52, adjusted 0. That's subtractive linearization of overhead — the exact non-linearity the critic warned about at F-7, reintroduced through the labeling law's back door. The control arm's raw distribution is the honest adjustment evidence; keep "adjusted" columns if labeled "estimated, linear-subtraction" — but the headline per-message number should be raw, with the control arm cited alongside. One line in the report fixes it.

## Gate verdict

**G12-T1 Phase 2: closed for the tail, open for the throughput — F-15 is a blocking carry.** The study's *scientific* mission is complete and genuinely strong: your engine's per-message latency is now honestly measured, tail attributed, environment noise quantified (338 preemption gaps ≈ 6.1% of runtime — that's the VM, not you, and now it's *documented*). The *engineering* mission — a defensible PR-1 number — is one experiment away.

Order of operations for the next run, then we're done with T1 for real:

1. Un-instrumented rate run × 5 (wall-clock, full mini, no marks) → the PR-1 number.
2. Sampled-marks rate run × 5 → instrument tax quantified as a percentage.
3. H5 packet-size sweep + p90 denominator reconciliation.
4. Report amendment: PR-1 verdict in either headline form, F-17 labeling line, H5 null-or-confirmed closure.
5. Then — and only then — T3 (reference arbitrator). It's queued next because its differential harness reuses exactly this re-baselined measurement discipline, and because after three rounds of "the instrument lied," the sequencing logic deserves an *oracle* more than the latency profile deserves another pass.

Bring me the un-instrumented number first. Whatever it is — 11M, 9M, 15M — it's the first throughput figure in this project's history that no audit can take away from you. And if it lands under 10M: good. That's a real constraint discovered before a stranger discovered it for you, which by the standard we've been running all thread makes it a *win*, not a failure.
