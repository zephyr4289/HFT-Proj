**Yes — one phase remains. But first the audit, because this report contains the exact failure doc 11 predicted, plus a substitution that hollows out the oracle you just built.** The genuine wins first: the 5.01-second loop run (122.4M messages, fresh sessions, zero allocs) is the first time "sustained" means what it says in this project — F-21 closed for real. D2's 100/100 random configs is real breadth. F-19's plumbing proof (1,011,710 vs 21,996 packets) is exactly the right way to close a red flag. T1 is now closed, all four findings discharged.

Now the findings — and notice where they live. Not in the engineering. In the verdict layer on top of it:

## F-22 — A PASS verdict sitting next to a failing number

Your own table: **PR-2 p99 = 152 cycles. Gate: < 150. Verdict: PASS.** Read doc 11's rule, written for precisely this moment: *"if PR-2 fails on reference hardware, that outcome is recorded and budgets are revised via changelog with numbers attached — never quietly."* The clause fired and was ignored **in the same report that cites its own benchmark basis.** And since CI ran green, the actual machine assertion must be looser than the documented gate — which means the "Machine Verdict" column is not machine-checked at the threshold it claims. A verdict column that disagrees with its own number is narration wearing a machine's costume. The 1.3% overage is almost certainly noise — and the correct response to noise is a *documented margin policy* (re-measure with more runs, or record the miss and revise the gate via changelog with the noise argument explicit), not a flipped verdict. This is the diskCleaner prime directive in miniature: "SUCCESS" printed where the evidence says otherwise.

## F-23 — D3 was substituted, and the oracle's target class was never tested

Doc 16 §4 D3 specified three **sequencer-logic mutations**: (a) disable clear-on-advance, (b) off-by-one window clamp, (c) drop-last-staged-at-EOS. What you injected: payload corruption, uncovered drops, truncated frames — **input mutations**. Walk why the substitution guts the test:

- Bug A (payload corruption) is caught by *any* hash comparison — the golden hash catches it too. It demonstrates nothing about the differential harness's unique value.
- Bug C ("safely dropped without crash") isn't a divergence test at all — that's VR-4's job description.
- And the zombie class — the one bug family your own doc 05 §4.1 says *"passes most tests and corrupts bytes late"* — **was never injected.** That family is the oracle's entire reason to exist, because it's the class E2E golden runs can miss on lucky schedules.

Here's the statistical point that makes this blocking: **D2's 100/100 is only meaningful if the harness's false-negative rate is known to be low. D3 is what measures the false-negative rate.** As executed, that rate is unmeasured for sequencer-logic bugs — so 100/100 could equally mean "no bugs" or "harness blind to them." An oracle never shown to catch its target class is a rubber stamp with good posture.

**G12-T3: OPEN.** The infrastructure is built and real — but closure requires D3-redo: the three specced mutations (reuse the tagged naive build from G5's U-ZOMBIE evidence for (a)!), run against the D2 config set, three divergence dumps pasted. This is hours of work, and it doubles as the strongest validation the clear-on-advance architecture will ever get.

## F-24 — Scope fine-print and silent number churn

- **D1 says all cells, ran M1..M5.** The executive table implies full D1; the fine print reveals 5 of 17. Legitimate blocker (the matrix isn't built yet) — but the honest cell is "5/17, remainder gated on matrix construction," not an implied complete pass. Fold the rest into the sweep when M6–M17 exist.
- **The numbers changed again without provenance.** Burst: 20.27M → 24.05M (+19%). Sampled p50/p99: last report's sweep said 49/74; this report says 106/152 — and the earlier family (78/156 adjusted, 130/208 raw) was never reconciled against either. Three mutually inconsistent number families have now crossed three reports, each presented as final. Some churn is legitimate (plumbing fixes, loop mode) — but a number that changes silently between reports is a number no one can cite. **Required: a provenance table** — `metric | value | build | mark mode | workload | run URL | supersedes` — with one line naming what each family actually measured. If 49/74 was dispatch-only while 106/152 is ingest-to-dispatch total, say so; if it was a different build, say that.

## ED-17 — The terminal sweep (this is the last build list of the project)

Order matters; the debt pays first:

1. **D3-redo** — three specced mutations, three divergence dumps. T3 closes only then.
2. **Gates-as-code (F-22's systemic fix):** extract PR-1/PR-2/sustained/tax thresholds into one constants file consumed by *both* doc 00's tables and the CI asserts — the doc 03 LENGTH-table lesson applied to gates: single source of truth, doc and machine reading the same number. Then a 152-vs-150 can never be green, and verdict columns become *computed output*, not written prose.
3. **F-22 closure:** 5+ runs on PR-2 p99, median + spread; if ≥ 150 — record, revise via changelog with the noise argument explicit, or let the FAIL stand. All three options are honest; silence isn't.
4. **F-24 provenance table** reconciling all three latency families and both throughput numbers.
5. **T2 window sweep** (specced ED-14): {256…4096} × {M1, M11}, golden invariant at every size, knee table, **ADR-0002 rewritten with data**.
6. **Matrix M6–M17** + D1-all-cells + M-BURST drop-onset sweep + M-STARVE with histograms.
7. **Fuzz campaign** (doc 10 §5): three harnesses, real-bytes corpus + the 9-byte tail + C10 dictionary, ASan/UBSan/LSan, coverage baseline.
8. **The carry ledger, every item:** F-3 (XDP verdict stage) · F-4 (full-day audit artifact + P1–P5) · F-5 (R1/R2 retarget + deletion grep) · F-6 (L-DIFF) · doc 14 §3.2 (spec-only server — now a *UDP* server post-C9, still zero evidence) · §3.1 signature · spec-silence register · claims-scope + VR-4 downgrade pastes · EOS_TRAIN pin.
9. **Terminal deliverables:** the five-section report — and note how rich your *falsified* section has become: H1 refuted-with-nuance, H5 null, F-9's page-cache collapse, the batch-artifact p99, the instrument tax. That section is the project's character now. README under the claims-scope law, banned-phrase grep, `12-gates.md` every row CLOSED-with-URL or TIERED, docs 00–16 frozen, corrections lineage C1–C11 complete.

One closing observation, because it's the pattern to break before the last report: **three consecutive reports have each contained a green verdict that didn't survive five minutes of reading its own numbers** — 17-tests-vs-deleted-mailboxes, 100%-attribution-on-a-broken-denominator, and now PASS-on-152. The apparatus caught every one, which is exactly why it exists. But notice the failures are never in the engineering — always in the narration layered on top. So the terminal sweep's deepest job is making that layer mechanical: gates computed from constants, numbers carrying provenance, oracles proven by killing them. When your final report writes itself from machine output — that's the day "100% Green" finally means what it says.

D3-redo first. Bring me three divergence dumps and a zombie dying inside the differential harness.
