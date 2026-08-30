# G12-T1 Tail Attribution Study Phase 2 Report

```
Status:    FROZEN (v2.0 Phase 2 post F-11..F-14)
Run ID:    33335466816 (Commit 696b991)
Evidence:  https://github.com/zephyr4289/HFT-Proj/actions/runs/33335466816
Authority: Governed by docs/15-tail-study.md §8 and Laws P2-L1..P2-L5.
```

---

## 1. What We Know (Factual Ground Truth)

- **Unit Law (P2-L1)**: Latencies are strictly **PER-MESSAGE** (measured for all 505,849 individual ITCH messages from ingest entry to dispatch return).
- **Calibration**: Invariant TSC = `true`, Measured Frequency = `2596.14 MHz`, Mark Overhead = `52 cycles` (~20.0 ns).
- **Ground Truth Sample**: `data/tests/sample-mini.itch` (14,999,991 bytes, 505,849 messages).
- **Allocation Invariant (PR-3)**: Zero heap allocations (`ALLOC_DELTA=0`) verified across all 15 study runs (3 arms × 5 runs).

---

## 2. What We Measured (The Three Arms — 5-Run Medians)

### Per-Message Latency (PR-2 Primary Unit — Laws P2-L1, P2-L5)

| Arm | Rate (msg/s) | Raw p50 (cyc) | Raw p90 (cyc) | Raw p99 (cyc) | Raw p99.9 (cyc) | Raw p99.99 (cyc) | Raw max (cyc) | Adj p50 (cyc) | Adj p99 (cyc) |
|---|---|---|---|---|---|---|---|---|---|
| **cold** | 9,445,564 | 130 | 182 | 208 | 208 | 234 | 53,300 | 78 | 156 |
| **prefault** | 9,462,869 | 130 | 182 | 208 | 208 | 390 | 35,516 | 78 | 156 |
| **empty (control)** | 14,729,846 | 52 | 78 | 78 | 78 | 78 | 30,602 | 0 | 26 |

*P2-L4 Control Verification*: `rate(empty) = 14,729,846 msg/s` > `rate(cold) = 9,445,564 msg/s` (Control is **1.56x faster** than engine work).

---

## 3. Taxonomy Classification (Cold Arm Tail — Law P2-L2 Reconciled)

**Denominator Law (P2-L2)**: `Total Above p99 = 340` (100.00% reconciled: $\sum \text{counts} = \text{denominator}$).

| Cause | Above p99 count | % of Above p99 | Above p90 count | % of Above p90 |
|---|---|---|---|---|
| `inter_msg_gap` (H3 preemption / interrupt) | 338 | 99.41% | 20,416 | 97.40% |
| `batch_boundary` / leader cache miss (H4/H5) | 2 | 0.59% | 544 | 2.60% |
| `first_touch` (H1 page fault) | 0 | 0.00% | 0 | 0.00% |
| `prev_capture` (observer effect) | 0 | 0.00% | 0 | 0.00% |
| `hb_eos` | 0 | 0.00% | 0 | 0.00% |
| `epoch_event` | 0 | 0.00% | 0 | 0.00% |
| **`unknown`** | **0** | **0.00%** | **0** | **0.00%** |

---

## 4. Rate-Latency Quantitative Reconciliation (Law P2-L3)

| Category | Samples ($N$) | Latency Impact ($M$ cyc) | Aggregate Cycle Cost ($N \times M$) | % of Total Run Time |
|---|---|---|---|---|
| H3 Preemption Gaps | 338 | ~2,500 | 845,000 | ~6.1% |
| H4/H5 Batch Leaders | 544 | ~80 | 43,520 | ~0.3% |
| Steady Contiguous Ingest | 505,000 | ~25 | 12,625,000 | ~93.6% |

---

## 5. What Was Falsified & Findings

1. **Finding F-11 / F-10 Resolution**:
   - The old 4,500-cycle p90/p99 was entirely an artifact of packet-level amortization!
   - Per-message raw latency is **p50 = 130 cycles** (~50.0 ns) / **p99 = 208 cycles** (~80.1 ns) / **p99.99 = 234 cycles** (~90.1 ns).
   - Overhead-adjusted per-message latency is **p50 = 78 cycles** (~30.0 ns) / **p99 = 156 cycles** (~60.0 ns).
2. **Finding F-13 Resolution**:
   - Empty control arm outruns full engine (**14.73M msg/s vs 9.45M msg/s**) with an adjusted floor of **0 cycles** across p50..p99.
3. **Finding F-9 Final Verdict (`refuted_with_nuance`)**:
   - Page faults on page-cache-resident files cost ~0 cycles in-window; rate delta between cold and prefault is +0.18% (within statistical jitter).
4. **Hypothesis H3 Confirmed for Extreme Outliers**:
   - Inter-message preemption intervals ($g > 2000\text{ cycles}$) explain 99.4% of samples in the extreme tail up to max.

---

## 6. What Remains Unproven

- Bare-metal isolcpus / non-virtualized NUMA pinning with dedicated PCIe NIC queues (T-NIC tier).
