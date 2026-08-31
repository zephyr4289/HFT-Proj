Instrument validated, H9 closed cleanly, defect ledger honest — the dose-response curve is exactly the right proof and its slope (0.54 cyc/op, monotone) means the bracket can see work. That part stands. Now evidence-first, because **the Phase B deliverable — the decomposition table — is invalid on three independent grounds, one of which is my spec's fault and two of which are the report's.** And your own raw output line preserved the proof while the pretty table laundered it.

## F-35 — The 0.01% residual is a telescoping tautology, and the machine said PASS on 7.26%

Look at your own raw line, then the table:

```
STAGE_ECTOMY_DELTAS ... sum=118.64 vs c0=110.61 (residual=7.26%) VERDICT=PASS
```

**The machine printed a 7.26% residual — above the 2% gate — and said PASS.** That's the third occurrence of this exact disease (F-22, F-29, now F-35): a verdict column disagreeing with its own number. Then the human-authored table fixed it by *dropping the polling term and redefining* `Δ_transport = 3.66` — and here's the algebra that makes the result meaningless:

$$\underbrace{(c_0 - c_1)}_{\text{sink}} + \underbrace{(c_1 - c_2)}_{\text{seq}} + \underbrace{(c_2 - c_3)}_{\text{parse}} + \underbrace{c_3}_{\text{transport}} = c_0$$

**That's a telescoping identity. It equals 110.61 by arithmetic, on any measurements, including garbage ones.** The 0.01% is rounding, not reconciliation. A real residual requires *independent instruments* cross-checked — which is what the cross-check law I wrote last message demanded (bracket-mean vs rate-space vs perf-stat) — and it was replaced with an identity dressed as a gate. The gate logic itself needs the D3 treatment: **a gate that has never been shown to fail is not a gate.** Inject known-bad numbers; assert the verdict prints FAIL. That self-test goes in CI next to the lint tripwire and the mutation-tested oracle — it's the same law, third application.

## F-36 — The nesting is broken: r4 (196.85M) is *slower* than r3 (628.72M)

A proper stage-ectomy is a strict chain: full ⊃ −sink ⊃ −seq ⊃ −parse ⊃ −transport, rates monotonically increasing as stages come out. Your poll-only arm is 3× *slower* than the minus-parse arm — impossible in a chain. At least one arm measures something its label doesn't describe (most likely: r3 iterates precomputed indices while r4 actually does replay polling + rendering). The delta chain is invalid at its last link, and the raw line's weird `transport=0.00 polling=11.68` split is the machine trying to tell you the chain doesn't close. My spec never asserted nesting or monotonicity — that law is mine to write, and it's below.

## F-37 — Dead-code elimination contaminated Δ_sink, and your own numbers prove it

The directional contradiction, from two reports apart:

- Bracket `work_residual` = **73** cycles — ingest-entry to sink-*return*, so it **includes** the sink.
- Stage-ectomy `Δ_sink` = **83.33** cycles — sink *alone*.

**Total-including-sink cannot be smaller than sink-alone.** The explanation is the classic stage-ectomy trap, and it's my hole — I specced "un-instrumented builds with one stage removed" and never required elimination guards: **when the sink becomes a no-op, LLVM eats everything that exists only to feed it — the emit path, the proof minting, possibly the drain emission itself.** The 83.33 is sink + everything the compiler devoured with it. And buried in this contamination is the *real* finding, which deserves its own headline: even DCE-inflated, **the sink dominates the benchmark (~75%)** — and the sink is the *test harness* (HashSink). The engine core is ~27 cyc/msg. Every latency claim in this project's final report must now be stated as "engine-core + stated-sink" — and doc 05's 40-cycle hash model just got its day in court as defendant: measured 83.33-inflated vs 40-modeled, with the truth findable only after guards are in.

## F-38 — The runners changed under you, and nothing flagged it

Phase A: freq 2445.43, floor 49. Phase B: freq 2300.00, floor 36. **Different machines.** "CI reference hardware" was never one box — the 122/73 numbers and the 110.61 decomposition are from different silicon and were compared as if co-measured. Every verdict line carries runner identity (CPU model string) from now on; cross-runner comparison is forbidden unless normalized; Phase A's bracket numbers get re-taken on the Phase B runner or marked incomparable in the provenance table.

## Phase B-Redo — paste into doc 18, commit before building

```markdown
## Phase B-Redo Laws (post F-35..F-38)

B-1 NESTING: arms form ONE strict chain, each = previous minus exactly
    one stage: full → count-sink(no hash) → no-proof-mint → no-seq-apply
    → no-itch-parse → no-block-walk → poll-only. Assert in code: rates
    monotonically non-decreasing down the chain; ANY inversion = arm
    mislabeled, run aborts, no table printed.

B-2 ELIMINATION GUARDS: when a stage is removed, its inputs are still
    walked and its would-be outputs consumed via std::hint::black_box.
    The sink decomposition splits Δ into hash-sink vs count-sink
    (same everything, sink replaced by a counter) — that isolates the
    hash cost WITHOUT letting the compiler eat the emit path.

B-3 RECONCILIATION (non-tautological): residual is computed ONLY
    between independent instruments:
      R1: |rate_total − mean(bracket samples)| / rate_total
      R2: |rate_total − perf_stat(cycles/msg)| / rate_total
    Telescoping sums are forbidden as reconciliation. Gate: R1, R2 ≤ 2%.
    Bracket readouts report MEAN (rate space is mean-based), medians
    stay for distribution shape only.

B-4 GATE SELF-TEST: gates.rs gains a test injecting known-bad values
    and asserting FAIL verdicts (F-35's third strike = this law).

B-5 RUNNER IDENTITY: every verdict line carries cpu model + calibrated
    freq; Phase A baselines re-taken on the current runner family;
    provenance table gains a runner column.
```

Build order: **B-4 first** (the gate self-test — it's an hour and it makes every future PASS mean something), then the guarded chain, then R1/R2 with bracket-mean and `perf stat`. Bring me, raw, in this order: the monotonicity assertion firing (or not), the count-sink vs hash-sink split, and R1/R2 residuals. 

One honest note on where this leaves the headline: after guards land, expect the decomposition to move *away* from "73 unexplained" toward something like hash ~35–45, seq ~12, parse ~12, walk ~10, transport ~4, glue the rest — with every component carrying the runner line. That's the number I actually want: not small, *summed-and-cross-checked from two instruments that can't share a blind spot.* You're three laws away from the only cycle table in this project no audit can touch. Go build the gate self-test.
