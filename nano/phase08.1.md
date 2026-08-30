Four real root causes, four real diagnoses — this is the first report in this project that reads like an incident review instead of a press release. Notice *how* these bugs were found: CI timeouts and hung tests caught liveness bugs that no narration would have. The pipeline is now doing senior-engineer work on its own. But I'm reviewing your fixes as a PR, and **two of them are incomplete in ways that will bite within a day** — one of them literally reintroduces the hang it's trying to fix. Issue by issue:

## Issue 1 — CmdChannel hammer: approved, with one law extracted

Diagnosis is exactly right: bounded producer + slower consumer = terminal starvation = reader spin-forever. Your fix (unbounded writer, reader signals stop) is correct. Extract the general law, because this is the first of many concurrency tests: **every concurrency test needs (a) a producer whose termination is reader-controlled, and (b) a wall-clock watchdog that fails the test — never hangs it.** A test that hangs wastes a CI runner for the step timeout; a test that *fails at 30s* produces a stack trace. Wrap the R-lane concurrency tests in the same watchdog pattern R14 already has. Also keep the assertion count meaningful — doc 08 §R1 said 10⁶ ops; either read 10⁶ times or amend the doc number, don't silently shrink it.

## Issue 2 — fake server: you re-derived the doc's own spec

Doc 08 §7 said *"Accept loop; one thread per connection"* — the implementation deviated, and the deviation produced exactly the hang the doc's sentence exists to prevent. Lesson worth internalizing: **when something hangs, diff the implementation against the normative pseudocode before debugging from scratch.** Two follow-ups your fix creates: (1) server counters (`requests_seen`, etc.) are now written from multiple connection threads — they must be atomics, or your test assertions are racy; (2) ADR-0007 stands — thread-per-connection is for the correctness lane; the **alloc lane still requires the server as a separate process**. Don't let the convenience of in-process leak into E2E-2a's alloc window.

## Issue 3 — your patch is a symptom fix, and it re-hangs on R6/R7

This is the important one. Your trailing loop:

```rust
while seq.watermark() < expected_wm { client.step(...); drain(...); sleep(1ms) }
```

**Walk it through R6 (`CloseOnRequest` ×4 → SessionDead):** the watermark *never reaches* `expected_wm` — that's the entire point of the test. Your drain loop spins forever on the failure path. Same for R7. You've replaced "hang before the response" with "hang after the failed recovery." The correct termination condition was never the watermark:

**Loop Termination Law (paste into doc 08, new §4a):** the engine loop — in the binary *and* in test harnesses — runs until `sequencer.state ∈ {ENDED, DEAD}`, plus (multi-session) schedule events remaining. ENDED-clean, ENDED-unclean (`final_wm < announced_next`), and DEAD are all *terminal outcomes the harness asserts against expectations* — not conditions to spin past. One termination rule everywhere; the binary and the R-lane tests use the same loop helper so harness semantics can't diverge from engine semantics. This is P-ORDER's missing twin: the poll *order* law and the loop *termination* law.

**And there's a real design bug underneath your symptom** — the tail-drop race. Worst case: scripted drop covers the last messages before EOS. Terminal heartbeat (N+1) and EOS are adjacent events in vt (doc 04 TH-1 makes EOS the next release). T-HB needs 250 µs of vt silence to fire; EOS arrives first → sequencer enters ENDED-unclean → E2E-2a fails — *with your patch or without it*. The fix is venue realism, and it goes in doc 04:

```markdown
| AM-6 | EOS was scheduled adjacent to the terminal heartbeat, giving
timeout triggers zero vt to fire before session end (tail-drop race) |
EOS per feed scheduled at terminal_heartbeat_vt + eos_lead_ns, default
50 ms (≥ 250µs trigger + 4×10ms grace + slack). Rationale: real venues
heartbeat for ~1s before ending a session; modeling that is honest AND
gives recovery its lawful vt budget. Rejected alternative: deferring
EOS delivery while an intent is pending — we don't reorder venue truth
to accommodate our plumbing. | Doc 04 §7 changelog; fabricator update |
```

With AM-6, the race disappears by construction: the grace law walks vt through the recovery, EOS arrives only after the budget existed.

## Issue 4 — approved as-is

`concurrency.cancel-in-progress` is exactly right. Add `timeout-minutes` to the test CI steps so a future hang costs minutes, not the job default — and that's belt-and-suspenders behind the Issue-1 watchdog law.

## Updated G8 ledger

The four fixes above are now **prerequisites**, not the gate. G8 acceptance stands as issued, plus: Loop Termination Law implemented (binary + shared harness helper, R6/R7 prove it) · AM-6 in doc 04 and the fabricator · server counters atomic · watchdog wrapper on all concurrency tests. And still open from last time, now four gates deep: **A-1** (watermark discriminator — your own stage-7 output shows N+1 under a split config), **A-2** (U-ZOMBIE verbatim trace), ASLR script committed.

Ship the corrected fixes, then show me: R1/R6/R7 green under the watchdog, and one E2E-2a run — with the drop range placed **at the tail of the day**, deliberately, to prove AM-6 under its worst case. That's the configuration that was silently broken before this message, and it's the one that proves the fix is real.
