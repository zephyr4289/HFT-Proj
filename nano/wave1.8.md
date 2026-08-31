You're right about the division of labor — I architect, I own the design, and when a phase lands wrong *because my spec had a hole in it*, that's my failure, not the agent's. So let me take the failures on the table before anything else, because this message contains two of them, and one of them is the funniest thing that's happened in this project yet:

## My failure #1: I specced the empty arm badly, and your Phase A just caught me — twice

Look at what I wrote in doc 18: **"marks still fire"** in the empty arm. Then look at your output: `empty_p50 = 49 cyc, spread = 0`. **Zero spread across 20 runs.** A measured median that *never moves* isn't a measurement — it's a constant being read. What the arm is measuring is exactly the mark-pair itself: 49 cycles, the RDTSCP serialization floor, identical every run because it's the same instruction sequence every run. The empty arm isn't measuring "loop skeleton + marks" as I specced; it's measuring **marks alone**, because a no-op body leaves nothing else to measure — every loop cost (batch iteration, transport poll, state touches) happens *around* the marks but the m0→m3 interval brackets only the no-op. My A-1 law said "the loop, marks, batch structure are identical" but I never specified *where the loop costs sit relative to the marks* — so the skeleton I designed structurally cannot see them. That's a spec bug. **The residual's meaning is now broken: `122 − 49 = 73` is not "engine work minus measurement," it's "full-path minus pure-mark-cost, with all loop overhead unaccounted on both sides.**" The loop costs are *in* the 122 (full arm pays them between m0 and m3? or outside?) and *not in* the 49 at all. Nobody currently knows, because I didn't make the doc force the question.

And there's a second, smaller one: you report full_p50 spread = 24 cycles, but no band was registered for full spread — only for the p50 itself. I specced A-4 as "spread is a first-class number" and then never gave it a verdict row. Partially-caught.

**Fix (my fix, doc 18 §A-1 amendment — commit before Phase B):**

```
A-1b MARK-PLACEMENT LAW: m0 and m3 bracket the SAME code region in
every arm — ingest-entry to sink-return — and the per-message loop
scaffolding (batch iteration, transport dispatch, frame lifetime)
must sit OUTSIDE the bracket in both arms. Arms differ ONLY in the
work function inside the bracket.

A-1c LOOP-COST ARM: a third configuration — arm=loop — runs the full
scaffolding (real transport poll, real batch walk, real frame
handling) with the work function no-op'd. loop_p50 − empty_p50 = the
measured cost of the engine's non-message machinery. The residual
decomposition becomes:

  full_p50 = mark_floor + loop_cost + work_residual
  work_residual = full_p50 − loop_p50    (NEW headline number)

empty arm's job is now honest: it measures the mark floor and nothing
else. That 49 is the instrument constant, not a skeleton baseline.
```

Phase B does not start until the loop arm exists and `work_residual` is measured. If loop_p50 comes back at, say, 100, then work_residual is 22 and *the actual engine work is 22 cycles* — which changes the entire Phase B decomposition and is a far more interesting number than 73.

## My failure #2: I let "PASS within band" language hide that the bands moved

Your table says bands were "pre-registered calibrated" as [40,55] / [110,130] — I registered [20,40] and [90,125], then flagged that F-13's own 49-cycle reading already violated PA-1 and told you to update the band *honestly*. It got updated — silently into a "pre-registered calibrated" label that pretends it was always [40,55]. That's the F-24 provenance disease, and this time the doc that was supposed to *cure* it caught it. Fix: provenance table gets a row: `PA-1 band: [20,40] original → [40,55] revised pre-run, reason: F-13's 49-cycle floor, revision commit hash required`. Bands may move before data — with a commit. They may never move after.

## And one finding that is *your agent's* to own — because you're right that the blame splits too:

**The fuzz row is lying to itself again, and I should have caught it one message ago.** `1,000,000 executions, elapsed=0.213s, data_frames=0, heartbeats=114, eos_frames=85`. **0.213 seconds. One million execs. 114 heartbeats and 85 EOS frames *total* — in a million inputs.** That is not a fuzz campaign; that's a seeded-corpus replay at 4.7M execs/sec with a mutation engine that almost never produces valid framing. Real fuzzing on this protocol is slow because most inputs die at the 20-byte header. A million execs in 0.2s means the harness is trivially cheap to reject — and `data_frames=0` means **not one mutated input in a million produced a valid data frame that reached the ITCH layer.** The 22/22 "grammar coverage" is *seed* coverage, not *mutation* coverage. The campaign verified the seeds validate. It did not fuzz the parser. This row is CLOSED and needs to be OPEN: real time-boxed campaign (60–90s per harness minimum, not 0.2s total), coverage numbers from `cargo fuzz coverage`, and the exec-count claim re-based on actual time. My fault for speccing "≥1M execs" as the metric at G10 — exec count without a time floor or coverage metric is a vanity number, and I wrote the vanity number. Revising the law now: **fuzz gates are time-boxed AND coverage-thresholded, never exec-counted.**

## What actually stands, because it's real and it's yours:

F-18's tiered-envelope disposition is the single most honest artifact this project has produced — MEASURED-MISS on raw gates, changelog revision with an envelope *justified by the measured environment*, Tier 3 strict gates retained and open. That's exactly the doc 11 "failures recorded, not hidden" clause executed. The spec-server clean-room (F-31) is real and closes §3.2. The 31-row register with F-18 sitting openly TIERED/OPEN is what a ledger is supposed to look like. Phase A ran, the numbers exist, the spread discipline exists. None of that is theater.

**Build order for next message, and this time the doc hole is patched first:**

1. Doc 18 amended with A-1b/A-1c + the PA-1 band-provenance row — my hole, committed before any new code.
2. The **loop arm**: build it, run 20, report `loop_p50`, `work_residual = full_p50 − loop_p50`, with spreads. That's the new headline number. If it's small, Phase B decomposes 20-ish cycles and we're hunting single-digit components — which means perf-stat enters earlier than planned, and I'll spec that when we see the number.
3. Fuzz row reopened: time-boxed campaign, coverage metric, real numbers. The 0.213s line goes in the report's *falsified* section — "exec-count gating admitted a non-campaign; law revised."
4. Then Phase B arms, one PR each, each with a delta vs. the A-4 spread or a "no signal" merge.

Bring me `loop_p50` and `work_residual` first, raw. The 73-cycle residual I promised you was contaminated by my own spec hole — the real number is behind the loop arm, and I want to see it before either of us says another word about decomposition. Main brain's job is to be wrong loudly and early — I just was. Now it's the agent's turn to build, and yours to make sure the ledger says what happened.
