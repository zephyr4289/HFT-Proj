**Hold on — before "engineer the next thing," this report contains the most consequential finding in the entire project, and it's hiding in plain sight in your D3 dumps. Read them again, slowly:**

```
Mutation A: seq_wm=1231  ref_wm=1224
Mutation B: seq_wm=600   ref_wm=601
```

**Mutation A: the oracle caught the zombie *after the wraparound* — wm 1231 vs 1224 is exactly the W=1224 arithmetic from doc 05 §4.1's failure trace.** The production sequencer emitted *seven messages too many* and a corrupted hash — which is the precise, predicted, catastrophic class. The oracle killed it. That's D3 done *properly*: three sequencer-logic mutations, all caught, dumps as artifacts. F-23 closed. **G12-T3 is now genuinely, provably closed — this time by divergence, not by narration.** The oracle has a demonstrated false-negative rate of zero-for-three on its target class, which is what makes the 17/17 matrix cells and D2's 100/100 actually mean something. That's the milestone: the sequencing correctness now stands on three legs — primary source, golden hash, and a proven-killing oracle.

Now the audit — because "G0 through G12 fully PASSED, 100% Green, all gates closed" is doing that thing again, and this time it's the biggest version of it yet:

## The report closes the project while ~15 items in the ledger are still open — and some were never mentioned

**F-25 — the carry ledger is silently truncated.** This terminal report claims full closure and lists five deliverables. But from this thread alone, still owed and unmentioned:

- **F-4** (three gates deep, then some): the full-day audit artifact + P1–P5 verdicts — the one thing that grades your length table against ~70M real messages. Never pasted. Ever.
- **F-3**: XDP verdict line as its own stage — was F-9's whole point.
- **F-5** (five gates deep): the R1/R2 retarget story + the deletion grep proving the mailboxes are gone.
- **F-24** (one gate): the provenance table reconciling three inconsistent latency families. The same numbers that flipped silently between reports.
- **Doc 14 §3.2** — the spec-only server re-derivation, zero evidence since it was specified.
- **§3.1 signature** (six gates), spec-silence register, claims-scope + VR-4 pastes, EOS_TRAIN pin.
- **The fuzz campaign — VR-4 — the item with "zero coverage" flagged at doc 10, re-flagged in ED-11 and ED-17.** A terminal closure report that omits the fuzz campaign isn't a closure report; it's a table of contents with the missing chapters removed. Also unverifiable from here: LSan on the alloc-lane, the coverage baseline, the M-BURST/M-STARVE artifacts, the M6–M17 cell verdict lines.
- **12-gates.md says every gate PASSED** — but by this thread's count, F-findings are open carry items attached to those same gates. A ledger that says PASSED while its own findings are open is F-22's "verdict column disagreeing with its numbers," scaled to the whole project. Per the standing pattern: **every gate's closure line needs a URL or a TIERED marker — and the ones with open F-items say so.**

## Three smaller ones

**F-26 — "Proven optimal knee at 1024" overclaims its own data.** The sweep showed 1024 wins on throughput with 0 drops *under M1/M11's reorder regime*. "Proven optimal" is only lawful if the sweep included the drop-onset dimension (window vs burst depth vs drop rate — M-BURST's actual subject, unmentioned here) or an analytical argument tying window size to the fabricator's disorder scale. Otherwise the honest sentence is: "1024 optimal *within the tested disorder envelope*." ADR-0002 rewritten-with-data is great — but ADRs get to say what the data says, not more.

**F-27 — LOC table vs. reality.** nf-arbitrator at 17 files / 1,533 LOC for "sequencer, window, state machine, LiveFeedProof, session/gap" — plausible. "All gates G0–G12 formally PASSED and CLOSED" over a 9.4k-LOC codebase whose testkit (2.4k) exceeds the arbitrator itself — that ratio is right for this project's philosophy, but the *claim* of total closure over a codebase this size, rendered in one line, is exactly the compressed-claim format this thread has spent a month teaching you to distrust. The volume table isn't the problem. The verdict table is.

**F-28 — the shape of the whole report.** Notice what a *terminal closure* report should lead with per the standard set four messages ago: findings first. This one leads with the volume table and "100% Green." The D3 dumps — the actual proof that the project's core novel claim (proven oracle) works — are *second*. When the strongest artifact is buried under the celebratory table, that's the tell.

## What actually happens now

The project is **functionally complete.** D3-redo, gates-as-code, T2-with-data, 17/17 matrix, D2's 100/100, sustained-throughput law, the falsified-hypothesis ledger — that's a real, finished body of work, and the LOC ratio (core ~3.7k, testkit+engine proof machinery ~5.7k) is the *healthy* shape. But there is a difference between "functionally complete" and "closed," and this thread's entire identity is that difference. Closure is a claim about *evidence completeness*, not code completeness.

So — two moves, in order:

1. **The Final Ledger Reconciliation (this closes the project for real):** every F-item and every carry from this thread, enumerated in one table — `item | status | artifact URL | or TIERED, and why`. The F-25 list above is the seed. Fuzz campaign executed or explicitly TIERED-with-reason. 12-gates.md rewritten so **no gate says PASSED unless every attached finding has a URL** — and the report leads with open items, not with volume. This is an afternoon of accounting, not engineering. It's also the difference between a repo that survives a hostile reviewer and one that gets taken apart in the first five minutes because the ledger says "all closed" and a `grep` says otherwise.

2. **Then TARGET-1 starts clean** — and it starts *small*, because doc 17 demands it: build the **six-arm skeleton first with the empty harness and full path only.** Before any decomposition: close F-24 (provenance table — Target-1 reuses it), fix the ns-conversion factor (F-18's residual), pin the sampled-marks config, and get one full-path p50 with a spread. Then add the component arms one at a time. Six arms, residual gate at ≤2%, the analytical model enters as defendant. Doc 15's discipline applies in full — pre-registered, committed before execution, and the residual is the headline number, reported before any component table.

Bring me the F-25 reconciliation table first — the full one, ugly, with OPENs visible. Then the two-arm Target-1 numbers. The pattern of the last five gates has been the same every time: the green summary breaks under its own details within one message. Close the ledger honestly once, and for the first time in this project's history, "closed" will actually mean closed.
