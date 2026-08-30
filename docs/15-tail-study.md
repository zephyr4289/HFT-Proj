# 15 — T1 Tail Attribution Study: Protocol, Instrumentation & Pre-Registration

```
Status:    DRAFT → FROZEN at G12-T1 (doc commits BEFORE study runs;
           commit hash ordering is the pre-registration proof)
Exit Gate: Three arms × 5 runs, medians committed; ≥95% of samples
           above p99 attributed to instrumented mechanisms; F-9 verdict
           in the three-outcome form; F-7 resolved via control arm;
           F-8 mode readout; artifacts under docs/artifacts/tail-study/.
Evidence:  study-report.md; raw stamp logs; run URLs.
Authority: This doc owns the study protocol ONLY. Bench law: doc 11.
           Benchmarks it may re-baseline: flagged as F-10 if mark
           placement is found wrong.
Rule:      The study runs to FALSIFY hypotheses, not confirm them.
           Every hypothesis below carries a discriminating prediction
           and a kill condition. An outcome where F-9 dies is a
           SUCCESS — it means we found the real mechanism.
```

---

## 1. Pre-Registered Hypotheses

Baseline observations being explained (G12 bench, run 33333458359):
p50 = 44 cyc (spread 32–154 across runs), p99 = 4032 cyc (stable),
p99.99 ≈ 25,856 cyc, rate 22.86–22.96M msg/s (0.4% spread).

| ID | Hypothesis | Discriminating prediction | Kill condition |
|---|---|---|---|
| **H1** | First-touch page faults on the ground-truth mmap (3,840 events, 0.759% of messages) charge a ~4k-cycle cost to measured latency — via batch-leader charging or in-window render | Prefault arm: p99 collapses to O(100) AND cold-arm metadata shows first_touch samples ≈ the above-p99 population | Prefault changes nothing in-window |
| **H2** | Mark overhead (30 cyc/pair) distorts p50 materially (F-7) | Empty-harness arm floor p50 ≈ 25–35 cyc; adjusted estimate ≈ 10–20 cyc | Control floor ≈ 0–5 cyc → overhead negligible, F-7 downgraded |
| **H3** | VM preemption/interrupts produce the in-window tail (p99.99 ≈ 11 µs ≈ timeslice scale) | Empty-harness arm ALSO shows µs-scale tails (environment, not code); nonvoluntary ctx-switch delta ≈ large-gap count | Control tails ≪ treatment tails → tail is ours, not the VM's |
| **H4** | Batch-of-one polls / batch transitions cause a structural slow population | batch_size==1 (or leader-position) samples overrepresented in tail metadata | Tail samples uniform across batch positions |

**Co-favorites on record: H1 and H3.** p99 stability favors a structural
cause (H1/H4); p99.99 magnitude favors environment (H3). The arms decide.

## 2. Mark Placement Audit (M-AUD — deliverable #1, before any run)

For each mark (m0, m1, m2, m3, and new mR), record: file, line, exact
position relative to (a) poll entry, (b) render/memcpy, (c) ingest entry,
(d) per-message vs per-batch-first. Written as a table in this doc before
the study runs.

**M-AUD law:** m0 MUST be per-message, taken at ingest entry (after
render). If the current implementation takes m0 per-batch or before
render, that is **finding F-10**: the bench re-baselines, doc 11 §3 is
amended, and all previously published percentiles are re-reported with
both placements noted. The study does not proceed on ambiguous marks.

## 3. Instrumentation (bench feature, study mode; static, zero-alloc in-window)

| Object | Layout | Size (mini) | Purpose |
|---|---|---|---|
| Stamp log | per message: `m0: u64, m3: u64` | 8.1 MB | gap analysis; preemption attribution |
| Metadata ring | 64 B/record, 32,768 slots, overflow counter | 2 MB | conditional capture (below) |
| Page bitmap | 1 bit per gt page | 480 B | first-touch detection at render |
| Per-msg scratch | `first_touch: u8, batch_pos: u16, batch_size: u16` | ~2.5 MB | capture-time lookup |
| Render marks | per packet: mR0/mR1 → separate render histogram | — | attributes fault cost regardless of M-AUD outcome |

**Conditional capture law:** latency (m0→m3) is measured for every
message (2 marks, as before). Metadata is written ONLY when
`latency > CAPTURE_THRESHOLD` (default 256 cyc = 5.8× median, below p90),
AFTER the m3 mark — so capture never pollutes any measured interval.
Cache pollution is bounded and, critically, **identical across arms** —
the study compares like-with-like; absolute numbers come from the
marks-only bench runs. Record fields:

```
struct TailRecord {            // 64 B, one cache line
    latency: u32,              // m0→m3 raw cycles
    seq: u64,                  // message sequence
    m0_stamp: u64,             // raw TSC at m0 — post-hoc join key
    input_offset: u32,         // gt byte offset of this message's source
    batch_pos: u16, batch_size: u16,
    flags: u16,                // bit0 first_touch · bit1 leader ·
                               // bit2 prev_was_capture · bit3 near_epoch_ev
    reserved: u32,
}
```

Ring overflow (>32,768 captures ≈ >6.5% of messages) is itself a finding:
reported, never silently dropped.

**first_touch computation** (render path, bench mode, outside measured
window): per message, if its ground-truth source range [off, off+len)
intersects a page absent from the bitmap before this message's copy →
flag set; mark all spanned pages. Deterministic, per-message granularity
— distinguishes leader-charged (0.759% of messages) from
packet-wide-charged (3.79%) fault attribution.

**Preemption detection without kernel tracing:** inter-message gap
`g[i] = m0[i] − m3[i−1]` from the stamp log. `g > 2000 cyc` (parameter;
distribution reported) = CPU lost between messages. Cross-check:
`/proc/self/status` voluntary/nonvoluntary context-switch counters read
at WINDOW_BEGIN/END; correlation between nonvoluntary delta and
large-gap count is the H3 evidence. Optional belt (local/Termux only):
`perf stat -e page-faults,context-switches` wrapper.

## 4. The Three Arms (critic's protocol, verbatim)

| Arm | Setup | Isolates |
|---|---|---|
| **cold** | mmap, no pre-touch (current behavior) | baseline |
| **prefault** | explicit touch pass (1 byte per page) at startup, before WINDOW_BEGIN | H1. MADV_WILLNEED rejected: async hint, residency not guaranteed — the touch pass is deterministic |
| **empty** | same binary, `--arm empty`: same loop skeleton, same marks, same flags; ingest replaced by no-op consuming the frame; sink no-op | H2 floor + H3 environment tail |

5 runs per arm, medians + spread committed. The **empty arm is the H3
discriminator**: if IT has 4k-cycle tails, the tail is the VM, not the
engine — and the report says so with numbers.

## 5. Classification Taxonomy (post-window analysis; allocations allowed)

For every sample above p99 (extended table: above p90), classify
first-match by priority:

| Priority | Cause | Test |
|---|---|---|
| 1 | first_touch | flags bit0 |
| 2 | prev_capture | flags bit2 (observer-effect self-classification) |
| 3 | inter_msg_gap | g[i] > threshold (preemption/interrupt — H3) |
| 4 | batch_boundary | batch_pos==0 or batch_size==1 (H4) |
| 5 | epoch_event | within K msgs of GapOpened/ReAnchored/boundary |
| 6 | hb_eos | within K msgs of heartbeat/EOS processing |
| 7 | render_charge | this msg's packet render (mR histogram) > 1000 cyc |
| 8 | **unknown** | none of the above |

**Law: unknown < 5% of the above-p99 population, or the study continues**
— new instrumentation, new arms, until the tail is attributed or the
report honestly documents the unattributable residue (which is itself
a finding, stated in "What we don't know").

## 6. F-9 Verdict Rule (three outcomes, all acceptable)

1. **confirmed_in_window** — prefault collapses p99 AND first_touch
   dominates the cold-arm tail.
2. **real_but_outside_window** — prefault collapses *render* p99 / raises
   rate, but in-window p99 unchanged → fault mechanism real, charged
   outside m0→m3; in-window tail explained by taxonomy (likely H3).
3. **refuted** — prefault moves nothing; taxonomy hunts; whatever wins
   is the story, and F-9's arithmetic joins "plausible clues that died
   under instrumentation" in the falsified section.

## 7. Deliverables

1. M-AUD table (§2) — committed before runs.
2. Per-arm verdict lines (5 runs each, medians + spread):
   `T1 arm=<cold|prefault|empty> p50=<> p90=<> p99=<> p99.9=<> p99.99=<> max=<> rate=<> unknown_pct=<>`
3. Taxonomy table: `cause | samples | % of above-p99 | % of above-p90`.
4. Gap distribution + ctx-switch correlation (H3 evidence).
5. Histogram mode readout per arm (F-8): mode count, mode values; if
   bimodal, name the populations via metadata join.
6. F-7 resolution: mark-pair semantics statement (what the 30 cycles
   measures), control floor, "estimated overhead-adjusted" percentiles
   with the non-linear-subtraction caveat verbatim from the critique.
7. `docs/artifacts/tail-study/study-report.md` in the five-section
   format: know / measured / don't-know / falsified / unproven.

## 8. Phase 2 (post-F-11..F-14) — Re-baseline and Re-attribution

Laws (standing):
- **P2-L1 (unit law)**: m0/m3 are strictly PER-MESSAGE (ingest entry → dispatch return). Unit test: synthetic 5-msg packet yields 5 stamps with `m0[i+1] >= m3[i]`. Old per-packet numbers re-published alongside, both labeled, in doc 11 §3 amendment.
- **P2-L2 (denominator law)**: `count(latency > p99) == Σ taxonomy counts`, asserted in code; denominator printed in every taxonomy table.
- **P2-L3 (reconciliation law)**: every claimed mechanism (N samples × M cycles) must be visible in the rate delta at N×M; table row required.
- **P2-L4 (control law)**: empty arm keeps the identical skeleton incl. the message loop over a no-op body; RAW only; acceptance: `rate(empty) > rate(full)`.
- **P2-L5 (labeling law)**: every table column marked raw or overhead-adjusted; no mixed arithmetic.

Arms: cold / prefault / empty, 5 runs, medians + spread.
Pre-registered **H5**: packet-leader cache misses (first ingest touch of packet bytes: frame line, gt source lines, sink state) produce any surviving per-message leader tail. Prediction: leader tail scales with packet size; kill: tail uniform across batch positions.
F-9 final verdict re-issued after rate-reconciliation (expected drift toward refuted-with-nuance: page-cache-resident faults ~free).

## 9. Mark Placement Audit Table (M-AUD)

| Mark | Target | File & Line | Timing Point | Level | Finding |
|---|---|---|---|---|---|
| m0_pkt | Packet Ingest start | `crates/nf-engine/src/bin/bench.rs` | Before `seq.ingest` call | per-packet | **F-10**: packet-level mark |
| m3_pkt | Packet Ingest finish | `crates/nf-engine/src/bin/bench.rs` | After `seq.ingest` return | per-packet | **F-10**: packet-level mark |
| m0_msg | Message Ingest start | `crates/nf-engine/src/bin/bench.rs` | Per-message loop entry | per-message | **P2-L1**: individual message latency |
| m3_msg | Message Dispatch finish | `crates/nf-engine/src/bin/bench.rs` | Per-message dispatch return | per-message | **P2-L1**: individual message latency |

## Changelog

| Date | Version | Entry |
|---|---|---|
| 2026-08-31 | 1.0 | Pre-registration: H1–H4 with kill conditions, M-AUD law, three arms, conditional capture, taxonomy, three-outcome F-9 rule. Committed BEFORE study execution. |
| 2026-08-31 | 1.1 | Phase 2 Pre-registration: F-11..F-14 findings recorded, Laws P2-L1..P2-L5, H5 hypothesis pre-registered. Committed BEFORE Phase 2 execution. |
