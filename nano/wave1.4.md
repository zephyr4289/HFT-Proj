**T1's scientific mission is now genuinely closed — and its final report still contains the project's recurring signature move: a PASS table where a verdict question was asked.** Credit first, because it's earned: the instrument-tax result (3.25% sampled vs 52.3% full) is real measurement science, the doc-11 amendment is correct, un-instrumented 20.27M is a defensible number, and H5 closed as a clean null. Phase 2's crisis → quantified tax → law amendment is exactly how F-15 should have ended. But read your own summary table's Status column: *every* PR-2 row says "Measured per-message," "Sub-200 cycles," "(Overhead floor: 49)" — **not one row says PASS or FAIL.** The gate is `<60 cyc p50`. Your own data contains both a pass and a fail, and the table declined to choose. That's evasion by formatting. Findings:

## F-18 — The report contains two contradictory PR-2 results and reconciled neither

Headline table: raw p50 = **123**, p99 = 172. Sweep table: p50 = **49**, p99 = 74. Both are presented as truth, three paragraphs apart. The reconciliation writes itself: the sweep used sampled marks (3.25% tax), the headline used full per-message marks (~49+ cycles/message of proven overhead) — and indeed 123 − 49 ≈ 74 ≈ the per-message mark cost. **The sampled build is the only defensible basis for PR-2** — its tax is quantified, tiny, and documented; full-mark numbers are instrument-dominated artifacts that belong in the tax-quantification section, not the headline. Under sampled measurement: p50 = 49 < 60 ✓, p99 = 74 < 150 ✓ — **PR-2 passes, cleanly, and the linear-subtraction "adjusted" column becomes unnecessary.** Declare it. A gate table where the reader must do the arithmetic themselves is a gate table that has decided to be misunderstood. (Minor, same table: 123 cycles "≈50.3 ns" implies 2.45 GHz; your calibration says 2,300 MHz. The ns conversions are off ~6% — sloppy in exactly the way this project isn't supposed to be.)

## F-19 — The sweep's triple-identical numbers are a plumbing red flag

p50 = 49, p99 = 74, leader_p99 = 74 — *identical across Fixed(1), Fixed(16), and MtuBound(1400)*, three configurations differing 30× in batch size, on a noisy CI VM where every previous phase showed run-to-run variance. Either genuine invariance flattened by histogram bucket granularity, or — far more likely — **the config never reached the fabricator and all three arms ran the same packetization.** Decisive check, one line: print `packets_transmitted` per arm. Fixed(1) must show ~505,849 packets, MtuBound(1400) ~17k. Identical counts = plumbing bug, sweep invalid, H5 untested. Also: leader_p99 == p99 *exactly* in every run is odd for a 1/N subpopulation even under invariance. Run one config twice first — if back-to-back runs of the *same* config show variance, then triple-identity across *different* configs is impossible without a bug.

## F-20 — The prefetcher claim is unfalsified decoration

"Continuous streaming keeps L1 prefetching active" — no instrumentation measures the prefetcher. The lawful statement is the null: *no leader effect observed at these packet sizes; mechanism untested.* This is precisely the "probably page faults" move the critic slapped down at F-9. Null results don't need a mechanism story to be valuable — dressing them is how buzzwords get back in.

## F-21 — "Sustained 20.27M" violates doc 11's sustained definition — and the loophole is mine

505,849 msgs ÷ 20.27M/s = **25 ms of work.** Doc 11 §6 required ≥5 s at tier via loop mode — but I wrote an escape hatch ("or the full input duration when shorter") that a 25 ms single pass sails through. That's my drafting error: a quarter-second of cache-hot burst cannot support the word "sustained" — no turbo exhaustion, no timer tick accumulation, no preemption exposure. **Amend doc 11: throughput headline requires loop-mode ≥5 s (fresh session ids per repetition); single-pass rates are labeled "burst rate."** Loop mode was specced in §6 and there's no evidence it was built — so PR-1's current honest status is *"burst 20.27M; sustained pending loop-mode run."* The loop will almost certainly hold ~20M (same 15 MB, cache-resident) — which is exactly why running it is cheap and there's no excuse not to.

**Gate ruling:** T1 conditionally closed — scientific deliverables stand; F-18/F-19/F-21 are blocking carries into the terminal sweep, F-20 is a wording fix. T3 green-lights now because it's correctness-domain and touches none of this. Its spec:

````markdown
# 16 — Reference Arbitrator & Differential Oracle

```
Status:    DRAFT → FROZEN after G12-T3
Exit Gate: D1..D8 green (Termux + CI, URLs); oracle-validation runs
           (D3) prove the harness detects injected bugs; operator
           signature on the reference's source review; independence
           grep clean.
Evidence:  Per-config DIFF verdict lines; divergence dumps (expected
           empty); injected-bug detection logs; run URLs.
Authority: This doc owns the reference implementation's laws, the
           differential harness, and the oracle's epistemic scope.
           Config grammar: doc 10 §3. Sequencer semantics: docs 00/05.
Rule:      The oracle is deliberately stupid and structurally
           independent. If it shares one line of sequencing logic
           with production, it is worthless. Simplicity IS the design.
```

## 1. Epistemic Position (stated before code)

**Catches:** every implementation bug in arbitration — window/clamp
errors, clear-on-advance violations (zombie class), staging corruption,
EOS-PERSIST transition errors, session-boundary flush bugs, anchor
mishandling, duplicate-suppression errors. **Cannot catch:** shared
model errors (both implementations misreading the spec the same way) —
that risk was retired by the C9/C10/C11 primary-source read and remains
documented, not solved, here. The oracle is the machine-checked form of
L2: same multiset in, same stream out, sampled across config space.

## 2. Reference Laws

- **R-1 Independence:** zero imports from `nf-arbitrator` and
  `nf-protocol`. Hand-parses the 20-byte header and `[u16 len][bytes]`
  blocks (~25 lines of `from_be_bytes`). Grep-audited in CI.
- **R-2 Embarrassing simplicity:** per session — collect all delivered
  `(seq, bytes)`; sort by seq; on duplicate seq keep **first-received**
  bytes (FR-3's first-wins, order-matched to the sequencer's); emit the
  contiguous prefix from the anchor; final wm = anchor + prefix length.
  No window. No staging. No policy. No recovery logic — recovery
  responses are just packets in the stream; confluence makes them
  indistinguishable.
- **R-3 Unbounded:** may allocate freely. It is an oracle, not a
  product.
- **R-4 Authorship:** ~100 lines, reviewed personally by the operator,
  signature line in this doc. Non-delegable, same class as the PDF read.

## 3. Harness

Tap point: **the rendered packet stream, pre-transport** — every frame
the engine would ingest is fed, in identical delivery order, to both
the sequencer and the reference. Per config, assert **triple
equality**: `HashSink(sequencer) == HashSink(reference) ==
range_fold(gt[a_ref .. e_ref])` — the reference's own anchor and final
watermark supply the fold bounds, making L-FOLD self-checking. On
divergence: dump first differing `(seq, expected, actual)` and abort
the config loudly. Watchdog on everything (doc 10 §8).

Config space: all 17 matrix cells + **≥100 seeded-random configs**
(splitmix64 over the doc 10 §3 grammar: packetize, loss, delay, drops,
split, coverage, fault — all randomized). Deterministic per seed.

## 4. Test Matrix

| # | Test | Pass condition |
|---|---|---|
| D1 | All matrix cells through the differential | triple equality, every cell |
| D2 | 100 random configs | triple equality, every config |
| D3 | **Oracle validation (test-the-oracle):** inject three known bugs into a throwaway sequencer build — (a) disable clear-on-advance, (b) off-by-one clamp, (c) drop last staged message at EOS | **harness MUST flag divergence in all three.** An oracle never shown to fail proves nothing |
| D4 | Duplicate ordering: B's copy delivered before A's original | identical output (first-received wins in both) |
| D5 | Session splits + boundary-spanning drops | equality incl. per-session watermarks |
| D6 | Unclean-death cells (DropRequest×4) | equality against reference's own final state |
| D7 | Watchdog + no hangs | 60 s bound |
| D8 | Double-run determinism | same configs → same verdicts |

## Changelog
| Date | Version | Entry |
|---|---|---|
| __-__-__ | 1.0 | Initial: reference laws R-1..4, triple-equality harness, D1..D8 incl. oracle-validation-by-bug-injection, epistemic scope. |
````

**ED-16 build order:**

1. **Quick closures, one PR:** F-18 (PR-2 declared PASS on sampled build: p50=49, p99=74; full-mark numbers demoted to tax section; ns conversions fixed) · F-19 (packet-count print per sweep arm + same-config double-run; verdict on plumbing, then re-issue or confirm H5) · F-20 (null-result wording) · F-21 (loop-mode run ≥5 s, headline re-labeled burst-vs-sustained; doc 11 amendment noting the loophole was the architect's).
2. **T3 per doc 16** — reference, harness, D1–D8. D3 before D1/D2: **prove the oracle can fail before trusting it to pass.**
3. Artifacts + URLs; operator signature on the reference source.

Then the terminal sweep is all that remains: T2 window sweep, the matrix, fuzz, and the five-section final report. You're one oracle and one sweep away from a repo where the sequencer's correctness rests on three independent legs — primary source, confluence hash, and a deliberately stupid reference that shares nothing with the thing it judges. Bring me D3's divergence dumps first; I want to see the oracle kill all three injected bugs before it blesses anything.
