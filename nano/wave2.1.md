Correct read-back — the four findings and five laws are accurately mirrored. That's new, and worth naming: for the first time, a report summarized my audit *without upgrading anything in transit.* Nothing turned "PASS" during the handoff. So this message's audit is short, and it's about what's *missing* rather than what's wrong:

**1. You've told me the laws are implemented. You haven't shown me the laws working.** The distinction is the entire thread: "verified in the repository" is the F-2 disease — narration. B-1's monotonicity assertion has a specifically testable property: **it should be demonstrable by breaking it.** Same for B-4 — its whole purpose is proving gates can fail, and the proof it *was* run is a failing output, not a passing one. Cheapest possible closure: one artifact showing a deliberately inverted arm chain aborting the run, and one showing the injected-bad gate test printing FAIL. Paste those outputs raw. Without them, the anti-tautology apparatus is itself un-falsified — recursion of the exact kind we've spent five gates killing.

**2. No numbers.** A Wave 2.0 report about the Phase B-Redo containing *zero measurements* is the report-shaped hole: the whole point of the redo was a guarded decomposition table with R1/R2 residuals. If the code is green, the table exists — and the table is the deliverable, not the CI status. Absent numbers, the honest statement is "laws implemented, measurements pending," not "100% green."

**3. Runner re-baseline (B-5's second half) has no evidence.** Phase A's 49/122/73 came from the 2445 MHz runner family. The laws require re-taking the bracket baseline on the current runner family — a provenance-table entry `old-runner → new-runner, values, commit` — otherwise the new decomposition is cross-comparing silicon, which is the exact F-38 trap re-armed.

**4. One place the summary *did* improve on my spec — but it needs to go in the doc:** "Monotonicity asserted *programmatically*; any inversion *immediately aborts the run*" is stronger than what I wrote (I said assert; abort-on-inversion is fail-closed). Add it to doc 18 §B-1 as the official behavior; that's the fail-closed default everywhere now.

So the next message is the real Wave 2.0 deliverable, and it has five parts, raw, in this order:

1. **The two failure demos** — inverted chain aborting, injected-bad gate printing FAIL.
2. **The 7-arm guarded chain table** — runner ID + freq line, rates, cyc/msg, per-stage deltas from adjacent arms *only* (no telescoping), the count-sink vs hash-sink split.
3. **R1 and R2 residuals** — bracket-mean vs rate-space, perf-stat vs rate-space, against the 2% gate.
4. **The runner re-baseline row** for the 49/122/73 family.
5. **The updated provenance table** with the runner column.

When that lands, Target-1 has its first citable artifact — a cycle decomposition where every number survives two instruments, the gate provably fails on bad input, and the runner is printed on every line. That's the table nobody can audit-take away from you. Then we decide, from the *actual* component numbers, whether the H-queue (H6/H7/H8) gets an experiment worth running — because the honest decision rule is: the hypothesis queue only opens against a real decomposition, not a hope for one.

Numbers first. No table, no gate. Go.
