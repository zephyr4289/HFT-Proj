# 11 — Benchmark Methodology & Performance Report

```
Status:    DRAFT → FROZEN after G12 (TERMINAL GATE)
Exit Gate: Bench artifacts committed (5-run medians, histograms, rate
           sweep, environment tables, bench-report.md); PR-1/PR-2
           evaluated on stated reference hardware with URLs;
           instrumentation-free conformance build proven; PROJECT LEDGER
           fully closed or TIERED (see §9). No doc follows this one.
Evidence:  docs/artifacts/bench-report.md; per-run verdict lines; CI
           run URLs; nm proof of un-instrumented release build.
Authority: This doc owns the measurement law, clock discipline, mark
           definitions, histogram spec, workload definitions, sustained
           definition, environment runbook, and report format. Targets:
           doc 00 PR-1..PR-5. Cycle budgets: doc 05 §12. Matrix cells:
           doc 10.
Rule:      Timing artifacts are STATISTICAL — sanity-gated, never
           diff-gated. Every number is tied to the box and run that
           produced it (NG-10, applied to ourselves). A gate that
           cannot fail is not a gate: if PR-2 fails on reference
           hardware, that outcome is recorded and budgets are revised
           via changelog with numbers attached — never quietly.
```

---

## 1. The Honesty Split (what performance claims mean here)

| Claim | Evidence required | Basis |
|---|---|---|
| "≥ 10M msg/s sustained, single core" | §6 sustained definition, 5-run median, reference hardware, URL | PR-1 |
| "p50 < 60 cyc, p99 < 150 cyc" | replay-core histogram (§3), 5-run median, URL | PR-2 |
| "zero allocations" | already machine law (doc 07 lanes) — bench changes nothing | PR-3 |
| NOT CLAIMED, ever | FPGA-tier latency; real-NIC zero-copy; vendor comparisons | NG-10 |

**Vendor comparison law:** we compare against our own PR-4 budgets and
nothing else. Comparing our VM-measured numbers to a vendor's marketing
PDF would be the buzzword stack reborn with decimal points.

**Two classes of CI output, never mixed:** deterministic artifacts
(golden hashes, audits — diff-gated) and statistical artifacts
(latency, rate — sanity-gated: monotone rates, alloc=0, count correct).
A pipeline that diffs timing output rots; one that sanity-gates it
survives.

## 2. Clock Discipline

- **Invariant-TSC detection:** `CPUID.80000007H:EDX[8]`. Absent →
  `CYCLES_UNAVAILABLE`: ns-mode (`CLOCK_MONOTONIC_RAW`) only, PR-2's
  cycles gate explicitly waived and marked in the report. We never
  publish cycles from a non-invariant TSC.
- **Calibration:** 256 `(tsc, mono_raw)` sample pairs over ≥ 200 ms;
  slope via Theil-Sen (median-of-slopes — outlier-immune); measured
  frequency printed in **every** bench verdict line; recalibrated per
  run. Reported frequency drift between runs > 0.5% = environment
  instability, noted in the report.
- **Serialization:** start mark = `lfence; rdtsc`, end mark = `rdtscp`.
  Mark overhead measured via 10⁴ back-to-back mark pairs (median),
  printed. Numbers reported **raw and overhead-adjusted, both labeled**
  — we never silently subtract.
- Marks land in a fixed per-message scratch ring (4 × u64); one clock
  read per mark. No per-message allocation, ever.

## 3. Measurement Points & Modes

Bench build only (`feature = "bench"`):

```
m0 receipt      replay: event delivered to ingest · XDP: rx-ring pop in poll()
m1 framed       header + full validation done (S0–S4)
m2 arbitrated   contiguous-emit or stage decision (S5)
m3 dispatched   sink call returned
```

- **REPLAY-CORE (primary, PR-2's domain):** m0→m3 without the kernel
  path. This is what doc 05 §12's budgets were written against. The
  sequencer is judged here.
- **XDP-FULL (informational):** m0→m3 through the kernel socket path.
  Separate table, separate tier (CI/veth; T-NIC when a box exists).
  Never averaged into, compared against, or footnoted into
  replay-core numbers. Mixing modes in one table is a report bug.

**Sampling law:** total (m0→m3) recorded for every message; per-stage
deltas sampled deterministically — every 256th message (`seq & 255 ==
0`). Deterministic sampling keeps runs comparable and costs 4 marks per
256 messages instead of per message.

## 4. Histogram (static, zero-alloc by construction)

Bounded log-linear buckets: range `[0, 2²⁶)` cycles (~16M — anything
higher is an anomaly bucket, counted separately and reported), ~4 KiB
fixed array. `record()` = bucket-index arithmetic + one `u32`
increment. Readout (post-window only) may allocate. Four instances —
total, parse, arbitrate, dispatch — ~16 KiB static. The histogram lives
inside the measured window and therefore inside PR-3: **the bench
binary itself asserts `ALLOC_DELTA=0`.** If instrumentation ever
allocates, the bench lane fails on its own law.

## 5. Build Separation Law

Instrumentation exists **only** under `bench`. The conformance and
release binaries compile it out — and this is *proven*, not asserted:

```
cargo build --release                      # no features
nm target/release/replay | grep -c rdtscp  # MUST be 0
```

CI runs that grep as a lane. If instrumentation leaks into the
un-instrumented build, the zero-alloc lanes' numbers stop meaning
anything — this check is what keeps doc 07's laws valid after doc 11
exists. Alloc + strace lanes re-run green on the un-instrumented build
at G12.

## 6. Workloads & The Sustained Definition

| ID | Workload | Measures |
|---|---|---|
| W1 | M12 max-rate (single feed, in-order, no loss), mini + dev | throughput ceiling |
| W2 | Rate sweep, paced tiers {1, 2, 5, 10}M msg/s | achieved rate, drops, p50/p99 per tier |
| W3 | M-STARVE overlay with histogram on | latency inflation under contention (critique item 13's numbers) |
| W4 | Per-stage sampled marks | where the cycles actually go |

**Sustained (normative):** ≥ 5 s at target tier with kernel drops
< 0.1% and stable tails (last-second p99 ≤ 2× first-second p99) — or
the full input duration when shorter, **with the actual duration
printed**. Input sizes: mini (0.05 s at 10M), dev (~0.7 s), full day
(~7 s). Hence **loop mode:** the fabricator repeats the day with fresh
session ids (bench mode only). Loop mode does NOT assert golden —
correctness is doc 10's jurisdiction; bench asserts structural sanity
only (strict monotonicity, ALLOC_DELTA=0, counter coherence). The
decoupling is deliberate: exact tests stay exact, statistical tests
stay statistical.

## 7. Environment Runbook (tiered, like everything else)

| Tier | Setup | Claims permitted |
|---|---|---|
| CI (reference hardware for PR-1/PR-2) | `taskset` single vCPU; governor state **recorded** (runners won't let us set it — record, don't pretend); 5 runs, median + spread committed | PR-1, PR-2 with VM-noise caveat in report |
| Termux | big-core pinning; max freq read from sysfs and printed | informational artifact |
| Real box (T-NIC appendix) | `isolcpus nohz_full rcu_nocbs`, IRQ steering, NUMA | the only place kernel-tuning claims may appear |

Every verdict line carries: mode, arch, measured TSC freq, governor
state if readable, run id. A number without its environment line is
not an artifact.

## 8. Report Format (`docs/artifacts/bench-report.md`)

```
1. Environment tables (per tier, per run)
2. Methodology (calibration freq, mark overhead, sampling law)
3. Table 1 — replay-core percentiles: 5 runs, median + spread
4. Table 2 — rate sweep (per tier: achieved/dropped/p50/p99)
5. Table 3 — xdp-full (informational, tier-labeled)
6. Table 4 — starvation inflation (W3)
7. Budget reconciliation vs PR-4 (per-branch cycles vs budget)
8. Caveats block (VM noise, no real NIC, sampled stages)
```

Verdict line (one per run, archived raw):

```
BENCH mode=replay-core msgs=N rate=<msg/s> p50=<cyc> p99=<cyc>
      p99.99=<cyc> max=<cyc> allocs=0 freq=<MHz> run=<id>
```

## 9. G12 — The Terminal Sweep (nothing rolls past this gate)

1. **All ED-11 items:** 17 matrix cells × their laws (Termux + CI
   URLs) · fuzz campaign (3 harnesses, real-bytes corpus, sanitizers,
   coverage baseline) · PR-5 laws R-A/R-B wired and asserted ·
   drop-onset sweep table · **F-3** XDP verdict stage · **F-4**
   full-day audit artifact + P1–P5 verdicts + stage rename ·
   **F-5** R1/R2 retarget story + deletion grep · **F-6** L-DIFF
   implemented · spec-silence register (≥ 4 rows) · claims-scope law
   and VR-4 downgrade pasted into doc 00 · doc 14 §3.1 signature ·
   `EOS_TRAIN_LEN=5` pinned.
2. **Bench:** §5 nm-lane green · PR-1/PR-2 evaluated, 5-run medians,
   URLs — **failures recorded, not hidden** · bench-report.md
   assembled per §8.
3. **Project close-out:** `12-gates.md` — every row CLOSED with a URL
   or explicitly TIERED · corrections register C1–C11 complete with
   lineage (C1's authorship noted) · ADRs 0001–0009 present · docs
   00–11 all FROZEN · README rewritten under the claims-scope law
   (banned-phrase grep run against it: `production`, `enterprise-
   grade`, `beats` must appear only in the "what this is NOT" block).
4. Say **freeze 11** and **project-complete**.

## Changelog

| Date | Version | Entry |
|---|---|---|
| 2026-08-31 | 1.0 | Initial: honesty split, clock discipline, marks/modes, static histogram, build separation + nm proof, sustained definition + loop mode, tiered runbook, report format, terminal gate sweep. |
