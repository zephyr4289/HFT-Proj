That message deserves a direct answer, not a doc — and the direct answer is: **you're right on every technical point, and I'm adopting every correction.** F-9 becomes a hypothesis with an instrumented trial, never a conclusion. F-7's arithmetic subtraction is dead — the empty-harness control replaces it, and every adjusted number gets the label "estimated overhead-adjusted." The 95%-explained criterion is adopted verbatim. And the sharpest thing you said — "the strongest evidence isn't that the architecture anticipated the critique; it's that an attack revealed something unmeasured" — is now the project's operating principle, because F-7/F-8/F-9 *are* the proof of it: three findings that existed before the critique and were only found when the numbers got attacked. More of that.

Here's the work list, one build order, no filler:

## ED-14 — The Final Work List (converges at G12)

**Wave 1 — instrument first, measure second (this weekend's work):**

1. **T1 Tail Attribution Study** — with the critic's full instrumentation spec, adopted verbatim:
   - Every sample above p90 carries metadata: `sample_id, seq, latency_cycles, input_offset/page, batch_size, poll_iteration, first_touch flag (offset % 4096 tracked at render time), epoch, heartbeat/EOS boundary proximity`.
   - **Control/treatment protocol:** cold-mapping run → prefaulted run → empty-harness run (F-7's control: same marks, same loop shape, same flags, minus the work) — three arms, 5 runs each, medians compared.
   - **Trial rule, verbatim:** run it *trying to prove F-9 wrong*. Prefault leaves p99 at 4,032 → we found a more interesting problem; that outcome is a success, not a failure.
   - Deliverable: the classification table (`Cause | Samples | % of tail`), **≥95% of samples above p99 explained by instrumented mechanisms or the study continues**; raw, empty-harness-baseline, and "estimated overhead-adjusted" columns; mark-pair semantics (one full start→end empty pair vs per-mark) stated in the report — F-7's ambiguity is itself a finding to close.
   - F-8 rides along: histogram mode readout in every run; p50 spread reported as a named metric. If bimodality confirms, identify the two modes; if not, record that too.

2. **T3 Reference Arbitrator** — spec is the critic's version, now law: collect → sort by seq → emit contiguous prefix. Embarrassingly simple. No shared helpers, no production logic, no optimization, written from doc 00 FR text. Differential-diffed against the sequencer across **all matrix cells plus ≥100 randomized configs**. Module header documents the epistemic limit (catches implementation bugs, not shared model bugs; model risk retired by the C9 primary-source read). **You personally review its ~100 lines** — same non-delegable rule as the PDF read. This is the strongest single item on the list because it's the only second oracle the sequencing logic will ever have.

3. **T2 Window Sweep** — {256, 512, 1024, 2048, 4096} × {M1, M11}, identical seeds. Table columns: correctness (golden hash holds at every size — the S-2 prediction), beyond_window_dropped, recovery intents, p99, VmHWM. Deliverable: the knee table + **ADR-0002 rewritten with data**, closing the "arbitrary constant" debt the critic found. If correctness wobbles anywhere, we found a W1 violation the property test missed — also a success.

**Wave 2 — the matrix and the rest (already specced, execute):**

4. G11 matrix cells M1–M17 × their laws (L-DET, L-FOLD, L-DIFF), Termux + CI URLs; M-BURST drop-onset sweep table; M-STARVE with W3 histograms on.
5. Fuzz campaign: three harnesses, real-bytes corpus + the 9-byte tail + C10 dictionary, ASan/UBSan/**LSan**, coverage baseline committed.
6. F-3/F-4/F-5/F-6 closures (XDP verdict stage, full-day audit artifact + P1–P5, R1/R2 retarget + deletion grep, L-DIFF implemented).
7. Doc 14 §3.2 evidence: the spec-only server re-derivation interop run (C9's UDP protocol, ~100 lines, fresh context, no doc 02/08 access) — divergences as corrections, convergences as ADR-0009.
8. PR-5 laws R-A/R-B wired and asserted; spec-silence register; claims-scope + VR-4 downgrade pasted into doc 00; §3.1 signature; `EOS_TRAIN_LEN` pinned.
9. **T4:** XDP-FULL kernel-path verdict lines — Table 3 exists or 22.9M stays replay-scoped, permanently.

**Wave 3 — the G12 terminal deliverable, in the critic's exact format:**

10. **bench-report.md** restructured to: *What we know / What we measured / What we don't know / What we falsified / What remains unproven.* That last section is mandatory and non-empty — candidates already on record: real-NIC zero-copy (TIERED), T-NIC NUMA/IRQ behavior (TIERED), T3's shared-model limit, the spec-silence rows, recovery-server behavior beyond the published spec. A final report with an empty "unproven" section is how the buzzword stack would end this project; we don't.
11. README rewrite under the claims-scope law + banned-phrase grep; `12-gates.md` — every row CLOSED with a URL or explicitly TIERED; all docs 00–11 FROZEN; corrections register C1–C11 with lineage; ADRs 0001–0009.
12. Then the only remaining move: **publish and invite the attack** — explicitly, in the README: *"Sequence semantics, recovery semantics, benchmark methodology, and protocol fidelity are the attack surfaces we consider weakest; corrections register shows three architecture-level errors caught before deployment. Break it."* The repo is structurally built for that review — evidence ledger, run URLs, lineage. That's the difference between surviving review and merely surviving.

**Execution order is load-bearing:** 1→2→3 first (they instrument and oracle the system the matrix will hammer), then 4–9 in parallel, then 10–12. And the reporting standard from here on is the one you set: no more "everything green 🔥" — every report leads with findings and the numbers that produced them, and a run with zero findings is a run you haven't looked at hard enough, which is itself a finding about the report.

One last thing, because it's the through-line of this whole thread: three corrections originated with the architect, three with your own CI output, three with external critics, and one with a 15-year-old NASDAQ PDF. **Every layer of this system — the bytes, the gates, the critics, and especially the primary source — has now overruled the person who designed it.** That's not an embarrassment to manage. That's the product. When T1 runs, report both arms raw — I'm genuinely curious whether the hypothesis survives contact with the prefault flag, and either answer improves the report.
