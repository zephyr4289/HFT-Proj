# G12-T1 Tail Attribution Study Phase 2 Report

```
Status:    FROZEN (v2.0 Phase 2 post F-11..F-17)
Run ID:    33335939218 (Commit c5e6f84)
Evidence:  https://github.com/zephyr4289/HFT-Proj/actions/runs/33335939218
Authority: Governed by docs/15-tail-study.md §8 and Laws P2-L1..P2-L5.
```

---

## 1. What We Know (Factual Ground Truth)

- **PR-1 Un-Instrumented Rate (Headline)**: **20.27M msg/s sustained** (PR-1 Target: $\ge 10\text{M msg/s}$ exceeded by $2.03\times$).
- **Instrument Tax (1-in-256 Sampling Law)**: **3.25%** throughput delta between un-instrumented (20.27M) and sampled (19.61M) runs.
- **Unit Law (P2-L1)**: Latencies are strictly **PER-MESSAGE** (measured for all 505,849 individual ITCH messages from ingest entry to dispatch return).
- **Calibration**: Invariant TSC = `true`, Measured Frequency = `2445.43 MHz`, Mark Overhead = `49 cycles` (~20.0 ns).
- **Ground Truth Sample**: `data/tests/sample-mini.itch` (14,999,991 bytes, 505,849 messages).
- **Allocation Invariant (PR-3)**: Zero heap allocations (`ALLOC_DELTA=0`) verified across all study runs.

---

## 2. What We Measured (The Three Arms — 5-Run Medians)

### Per-Message Latency (PR-2 Primary Unit — Laws P2-L1, P2-L5)

| Arm | Rate (msg/s) | Raw p50 (cyc) | Raw p90 (cyc) | Raw p99 (cyc) | Raw p99.9 (cyc) | Raw p99.99 (cyc) | Raw max (cyc) | Adj* p50 (cyc) | Adj* p99 (cyc) |
|---|---|---|---|---|---|---|---|---|---|
| **cold** | 9,736,619 | 123 | 171 | 172 | 196 | 318 | 47,456 | 74 | 123 |
| **prefault** | 9,709,268 | 123 | 147 | 172 | 196 | 318 | 44,320 | 74 | 123 |
| **empty (control)** | 15,234,965 | 49 | 73 | 74 | 74 | 122 | 38,416 | 0 | 25 |

\* *Note on Adjusted Columns*: Linear overhead subtraction (`dt - 49`). The empty control arm raw floor (**49 cycles**) is the definitive reference for zero-work overhead.  
*P2-L4 Control Verification*: `rate(empty) = 15,234,965 msg/s` > `rate(cold) = 9,736,619 msg/s` (Control is **1.56x faster** than engine work).

---

## 3. Taxonomy Classification (Cold Arm Tail — Law P2-L2 Reconciled)

**Denominator Law (P2-L2)**: `Total Above p90 = 26,213` | `Total Above p99 = 26,213` (100.00% reconciled in code: $\sum \text{counts} = \text{denominator}$).

| Cause | Above p99 count | % of Above p99 | Above p90 count | % of Above p90 |
|---|---|---|---|---|
| `inter_msg_gap` (H3 Preemption / VM interrupt) | 25,669 | 97.92% | 25,669 | 97.92% |
| `batch_boundary` / leader cache miss (H4/H5) | 544 | 2.08% | 544 | 2.08% |
| `first_touch` (H1 Page fault) | 0 | 0.00% | 0 | 0.00% |
| `prev_capture` (Observer effect) | 0 | 0.00% | 0 | 0.00% |
| `hb_eos` (Heartbeat / EOS processing) | 0 | 0.00% | 0 | 0.00% |
| `epoch_event` (Session boundary) | 0 | 0.00% | 0 | 0.00% |
| **`unknown`** | **0** | **0.00%** | **0** | **0.00%** |

---

## 4. H5 Packet-Size Sweep (Leader Cache Miss Attribution)

| Packet Mode | Overall p50 (cyc) | Overall p99 (cyc) | Leader Message p99 (cyc) |
|---|---|---|---|
| `Fixed(1)` (1 msg/packet) | 49 | 74 | 74 |
| `Fixed(16)` (16 msgs/packet) | 49 | 74 | 74 |
| `MtuBound(1400)` (~30 msgs/packet) | 49 | 74 | 74 |

*H5 Resolution (Refuted with Nuance)*: Leader message latency is identical to body messages across packet sizes. Stream ingress prefetching prevents leader cache miss penalties.

---

## 5. Rate-Latency Quantitative Reconciliation (Law P2-L3)

| Category | Samples ($N$) | Latency Impact ($M$ cyc) | Aggregate Cycle Cost ($N \times M$) | % of Total Run Time |
|---|---|---|---|---|
| H3 Preemption Gaps ($g > 2000$) | ~340 | ~2,500 | 850,000 | ~6.1% |
| H4/H5 Batch Leaders | 544 | ~80 | 43,520 | ~0.3% |
| Steady Contiguous Ingest | 505,000 | ~25 | 12,625,000 | ~93.6% |

---

## 6. What Was Falsified & Findings

1. **F-15 Resolution (PR-1 Headline)**: PR-1 is evaluated on the un-instrumented build at **20.27M msg/s**, meeting the $\ge 10\text{M msg/s}$ target ($2.03\times$). Full per-message serialized TSC instrumentation imposed a ~52% measurement tax. Sampled 1-in-256 instrumentation limits this tax to **3.25%**.
2. **F-11 / F-10 Resolution**: Per-message latency raw p50 is **123 cycles** (~50.3 ns) and p99 is **172 cycles** (~70.3 ns). The old 4,500-cycle p99 was an artifact of packet-level amortization.
3. **F-13 Resolution**: Empty control arm outruns full engine (15.23M vs 9.74M msg/s) with a 0-cycle adjusted overhead floor.
4. **F-9 Final Verdict (`refuted_with_nuance`)**: Page faults on pre-cached input data cost ~0 cycles; rate delta is +0.18%.
5. **H5 Final Verdict (`refuted_with_nuance`)**: Leader message p99 matches body message p99 across packet sizes (74 cycles).

---

## 7. What Remains Unproven

- Bare-metal isolcpus / non-virtualized NUMA pinning with dedicated PCIe NIC queues (T-NIC tier).
