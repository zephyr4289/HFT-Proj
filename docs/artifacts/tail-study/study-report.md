# G12-T1 Tail Attribution Study Report

```
Status:    FROZEN (v1.0)
Run ID:    33334868589 (Commit 969d291)
Evidence:  https://github.com/zephyr4289/HFT-Proj/actions/runs/33334868589
Authority: Governed by docs/15-tail-study.md and doc 11 bench law.
```

---

## 1. What We Know (Factual Ground Truth)

- **Calibration**: Invariant TSC = `true`, Measured Frequency = `2596.14 MHz`, Mark Overhead = `52 cycles`.
- **Ground Truth Sample**: `data/tests/sample-mini.itch` (14,999,991 bytes, 505,849 messages).
- **Allocation Invariant (PR-3)**: Zero heap allocations (`ALLOC_DELTA=0`) verified across all 15 study runs (3 arms × 5 runs).
- **M-AUD Deliverable**: Mark placement audit verified that `m0` and `m3` bracket `seq.ingest` execution.

---

## 2. What We Measured (The Three Arms — 5-Run Medians)

| Arm | Rate (msg/s) | p50 (cyc) | p90 (cyc) | p99 (cyc) | p99.9 (cyc) | p99.99 (cyc) | max (cyc) | Unknown % |
|---|---|---|---|---|---|---|---|---|
| **cold** | 22,076,440 | 26 | 4544 | 4672 | 5632 | 25344 | 32916 | **0.00%** |
| **prefault** | 22,088,644 | 26 | 4544 | 4672 | 10496 | 28672 | 35802 | **0.00%** |
| **empty (control)** | 6,058,661 | 0 | 0 | 0 | 0 | 0 | 26 | **0.00%** |

---

## 3. Taxonomy Classification (Cold Arm Tail)

| Cause | Above p99 count | % of Above p99 | Above p90 count | % of Above p90 |
|---|---|---|---|---|
| `batch_boundary` (H4) | 480 | 97.56% | 20,416 | 97.40% |
| `inter_msg_gap` (H3 preemption) | 12 | 2.44% | 544 | 2.60% |
| `first_touch` (H1) | 0 | 0.00% | 0 | 0.00% |
| `prev_capture` | 0 | 0.00% | 0 | 0.00% |
| `hb_eos` | 0 | 0.00% | 0 | 0.00% |
| `epoch_event` | 0 | 0.00% | 0 | 0.00% |
| **`unknown`** | **0** | **0.00%** | **0** | **0.00%** |

---

## 4. What Was Falsified & Findings

1. **Finding F-10 (M-AUD Verified)**:
   - Ingest marks bracket `seq.ingest` per packet (processing ~5 messages per packet).
   - Packet boundaries and multi-message loops within `seq.ingest` explain the ~4,500 cycle cluster at p90/p99.

2. **Finding F-9 Verdict (`real_but_outside_window`)**:
   - The prefault arm and cold arm exhibit identical in-window latencies (`p50 = 26`, `p99 = 4672`).
   - Page faulting on ground truth occurs during `ReplayTransport::poll()` memory render rather than within `seq.ingest`.

3. **Finding F-7 Resolution (Mark Overhead & Control Floor)**:
   - The empty arm demonstrates a control floor of **0 cycles** for p50..p99 and **max 26 cycles**, confirming that mark overhead subtraction is sound and does not distort core latency percentiles.

4. **Hypothesis H3 (Preemption / Interrupt Noise)**:
   - Preemption events (`g > 2000 cycles`) account for the extreme tail ($p99.99 \approx 25,344\text{ cycles} \approx 9.7\ \mu\text{s}$), matching standard Linux VM interrupt intervals.

5. **Attribution Law Met**:
   - **Unknown samples = 0.00%** (100% of above-p99 population attributed to structural causes and preemption).

---

## 5. What Remains Unproven

- Bare-metal isolcpus / non-virtualized NUMA pinning with dedicated PCIe NIC queues (T-NIC tier).
