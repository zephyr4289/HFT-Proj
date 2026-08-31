The provenance table with commit hashes, the fuzz row sitting honestly OPEN, the falsified exec-count law recorded — that's the apparatus fully alive. Now, evidence-first: **your loop-arm result is the exposure of my second consecutive spec bug, and the report converted my bug into a false physics claim. Both go on the ledger before anything gets built.**

## F-32 — A-1b and A-1c contradict each other, and I wrote both

Read my own laws back-to-back: **A-1b** says loop scaffolding (transport poll, batch walk, frame handling) sits *outside* the m0→m3 bracket in every arm. **A-1c** says the loop arm measures that same scaffolding *inside* its bracket. Mutually impossible — the loop arm as I specced it cannot exist. Your agent implemented the only self-consistent reading (scaffolding outside → bracket contains a no-op → `loop_p50 = empty_p50 = 49`), and the numbers exposed the contradiction perfectly: **a measurement arm that was designed to be blind came back blind.** My failure, second doc-18 hole in a row.

## F-33 — "loop_cost = 0, ITCH lookup executes in sub-cycle L1 time" is physically false, and the report must strike it

There is no sub-cycle execution. A load is ≥1 cycle; an L1 hit is ~4–5; parse of a 20-byte header plus a table lookup plus bounds checks is dozens of instructions minimum. **Zero *measured* ≠ zero *cost* — it means outside the measurement window.** The report took an instrument blind spot and wrote it up as a performance achievement. That's F-20's prefetcher-story disease in numeric form, and it's exactly the claim a hostile reviewer would detonate on sight. The lawful sentence: *"parse and transport costs are per-packet and structurally invisible to the per-message bracket; they are measured in rate space (below)."*

## F-34 — the Arm 1 description column describes work no arm measures

"Full transport poll + MoldUDP64 block slice + ITCH validation" — all per-packet costs, all outside every per-message bracket by construction. The column is wrong even if the code is right.

## And the arithmetic your own numbers were already screaming — the second instrument

Compute `cycles/message = freq / rate` from numbers already in your reports:

| Build | Rate | cyc/msg (2445 MHz) |
|---|---|---|
| Un-instrumented burst | 24.05M | **101.7** |
| Un-instrumented sustained (5s loop) | 20.12M | **121.5** |
| Sampled build (wall) | ~19.8M | **~123.5** — and bracket p50 = 122 |

Three observations that redefine Phase B: **(1)** the bracket's median (122) ≈ the sampled build's total wall cyc/msg (~123.5) — so the bracket, sampled, spans nearly the entire per-message period; the inter-message gap is tiny *for the median*, meaning per-message loop overhead is near-zero — the real costs are inside the bracket plus per-packet work outside it. **(2)** burst (101.7) vs sustained (121.5) is an **unexplained ~20 cyc/msg** — new pre-registered hypothesis **H9: repetition overhead** (session re-anchor per loop, cache warmth decay, or batch-shape changes across repetitions; discriminated by a sweep of loop-repetition length). **(3)** the bracket-vs-wall reconciliation must use *means*, not medians — p50 122 with a right-skewed tail means mean-bracket is higher; the wall number is a mean. Two instruments, one truth, reconciled — that's the real Phase B.

## The corrected protocol — commit before building

**Law A-1d (dose-response, the instrument-validity law):** an arm may not cite a zero until it has *registered injected work*. Build a test arm that runs the ITCH validation **10× per message**; predict the delta (~10× the per-validation cost); if p50 doesn't move by the predicted amount, the bracket doesn't measure what you think it measures. This is D3's oracle-validation logic applied to the bench itself: **an instrument that cannot be made to move is not an instrument.** The empty arm's 49 passed this implicitly (it measures the marks, exactly, with zero spread). The loop arm never got the chance — my spec made it impossible.

**Phase B — rate-based stage-ectomy (per-packet costs become visible):** un-instrumented build variants, each with one stage removed — `minus-sink`, `minus-emit`, `minus-sequencer-apply`, `minus-parse` — 20 runs each, `cyc/msg = freq/rate`, spread recorded. **Stage cost = delta between adjacent variants.** No marks, no bracket ambiguity, per-packet work fully visible. Then the cross-check law:

```
Σ(stage-ectomy deltas) + per-packet parse ÷ msgs_per_packet
    ≈ mean(bracket components) + H9 overhead
    reconciled against total wall cyc/msg, residual ≤ 2%
```

The 73-cycle work_residual *stands* — it's real per-message engine work — but it's now one component in a ledger that must sum, not a headline that stands alone.

**Build order:** dose-response arm (prove the instrument moves) → stage-ectomy variants (the table) → mean-bracket readout → reconciliation table with the residual → H9 discrimination sweep. Bring me the dose-response curve first: injected work on the x-axis, measured delta on the y-axis, one line, and if the slope isn't ~1, nothing else you build this phase is citable.

You were right about the division of labor, so here it is in practice: the loop arm was my hole, the "sub-cycle" sentence was the report's hole, and the fix for both is the same law — *nothing gets cited until the instrument proves it can see*. Now the agent has a spec with no contradictions in it. Go build.
