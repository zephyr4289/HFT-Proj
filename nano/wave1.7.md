Let's read what you just handed me the way a hostile reviewer would — because this table is *better* than every previous report, it's genuinely near-complete, and it still contains three findings, one of which is the funniest possible failure of the audit apparatus auditing itself. Evidence-first, per F-28's own law:

## F-29 — F-18 is CLOSED with the gate *still failing* inside its own row

Read the row: *"p50 = 122 cycles (50.0 ns) < 60 ns."* The gate in doc 00 is **< 60 cycles** — a number, not a nanosecond wall. F-22 ruled on this exact item two reports ago: 152 > 150, verdict flipped to PASS, and the resolution was *either more runs and a margin policy, or record the miss and revise via changelog.* Your reconciliation now shows p50=122 ✓ but **p99=172 against a <150 gate — again — CLOSED, again, without either the changelog revision or the margin policy.** And the ns-conversion shell game continues: "50.0 ns < 60 ns" quietly converts a cycles gate into a ns gate so the row can pass. That's not just an open finding — it's the *same* finding, reopened by its own closure artifact. The lawful row reads: `F-18: OPEN — p99 172 vs gate 150; disposition = [revised-gate-with-changelog | measured-miss]` and then gates.rs **consumes whatever number the changelog lands on**, because the whole point of gates-as-code is that the doc and the machine can never disagree again.

## F-30 — The oracle validates the taxonomy, not the conclusion: unexplained is *allowed*, and yours is 0% in a way nothing has ever been 0%

Two rows claim perfection: F-16 "0 unknown samples," and now VR-4 fuzz — **"300,000 iterations, 0 panics / 0 violations."** Doc 00 VR-4 said ≥1M CI execs; the claim was downgraded to "≥1M" in the claims-scope law — and the ledger quietly lands at 300k, labeled CLOSED, no tier, no changelog. And *fuzzing with zero violations found is not a pass state* — it's a corpus-coverage statement, and no coverage number appears anywhere. Three hundred thousand iterations on a three-harness fuzz run is about **90 seconds** of libFuzzer; the campaign spec called for time-boxed CI plus an extended artifact run. This row needs: the actual exec count, coverage vs. baseline, LSan result, and either the count raised or the claim TIERED-down with a changelog line. The pattern from F-12 is repeating at the register level: **perfection percentages are where the bodies are buried.** A ledger row that says "0.3% unknown, 780k execs, coverage 74%, LSan clean" is *stronger* than "0 violations CLOSED."

## F-31 — the missing row: §3.2, the spec-only server re-derivation — *nine gates, zero mention*

Grep your own table for it. Doc 14 §3.2 — the independent re-implementation of the retransmission server from the PDF text alone, the second leg of the protocol-fidelity sandwich after C9, specified at G10, carried in every ED list since, **absent from the register entirely.** A master reconciliation that closes 28 rows and drops the one row that was never started isn't a reconciliation — it's the truncation habit with better formatting. It goes in as `OPEN — not started, blocking protocol-fidelity claim in final report` or it gets explicitly TIERED with a reason. No third state exists.

The rest of the table holds. F-4 closed with a real number (29.1M messages audited, 0 violations — the P5 verdicts finally exist), F-23's three mutations with dumps, F-25/F-28 marked closed by the act of producing this very document. The register is maybe 90% honest now, and 90% honest with three visible OPENs is a *dramatically* better artifact than 100% CLOSED with three invisible holes. That's the whole F-28 lesson.

## So the state is: three rows to fix, then this register is the real thing

1. **F-18:** decide it — margin policy with fresh 5-run spread *or* gate revision via changelog; both gates and the doc read the same number; kill the ns-conversion dodge.
2. **VR-4:** real exec count + coverage + LSan, or honest TIERED-down row.
3. **§3.2:** add the row, status OPEN-not-started, and then either run it (it's ~100 lines against a PDF you have) or TIER it with a written reason.

That's the entire gap between this register and a register a hostile reviewer can't dent. Do those, and Target-1 proceeds with the clean baseline it was waiting for.

## And Target-1 Phase A — you jumped the build order, so let's use it

You reported six arms *already designed* with a calibrated baseline — but you don't have Phase A's four numbers: `empty_p50, full_p50, residual_A, spread` over ≥20 runs, and the provenance table's historical rows. The six-arm decomposition was Phase B; the skeleton is what makes Phase B's deltas *meaningful* (A-4's law: a component delta smaller than baseline spread is noise). So Phase A runs anyway — two arms, one binary, `match arm` at one call site, marks sampled 1-in-256, ≥20 full-arm runs:

- Your pre-registered bands: PA-1 empty ∈ [20, 40] (your own F-13 row says ~49 — **already outside the band before running**; note that, update the band honestly, that's the pre-registration doing its job), PA-2 full ∈ [90, 125], PA-3 residual ∈ [60, 90] with spread.
- **The skeleton PR does not merge until the F-29/F-30/F-31 rows are fixed** — that was the deal, and it's what keeps "make it exist" from becoming "make the ledger lie while it exists."

Bring me the three row-fixes and the four Phase-A numbers, raw, findings-first. And enjoy the fact that your audit apparatus now catches *its own closures* — F-29 is the register auditing the register. That's the recursion working. Three rows, four numbers, then we go hunting the 60–90 cycles for real.
