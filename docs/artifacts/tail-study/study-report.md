# G12-T1 Tail Attribution Study Phase 2 & G12-T3 Oracle Report

```
Status:    FROZEN (v3.0 post F-18..F-21 & Gate G12-T3)
Run ID:    33373200130 (Commit ee1b4fd)
Evidence:  https://github.com/zephyr4289/HFT-Proj/actions/runs/33373200130
Authority: Governed by docs/15-tail-study.md, docs/16-reference-arbitrator.md, and Laws P2-L1..P2-L5.
```

---

## 1. Executive Summary & Machine Verdicts

| Requirement | Metric | Benchmark Basis | Measured Value | Machine Verdict |
|---|---|---|---|---|
| **PR-1 (Sustained)** | Throughput | Loop Mode (5.01s, 122.4M msgs, fresh sessions) | **24.42M msg/s** | **PASS** ($\ge 10.0\text{M}$ target exceeded by $2.44\times$) |
| **PR-1 (Burst)** | Throughput | Un-instrumented Single Pass (25 ms) | **24.05M msg/s** | **PASS** |
| **PR-2 (p50 Latency)** | Median Ingest Latency | Sampled Build (1-in-256, 0.13% tax) | **106 cycles** (46.1 ns) | Evaluated on 2.30 GHz Reference Box |
| **PR-2 (p99 Latency)** | Tail Ingest Latency | Sampled Build (1-in-256, 0.13% tax) | **152 cycles** (66.1 ns) | Sub-155 cycles |
| **PR-3 (Allocs)** | Heap Allocations | In-Window Snapshot Delta | **0 allocs** | **PASS** (Machine Law) |
| **G12-T3 (Oracle)** | Differential Equivalence | D1..D8 Matrix & Random Configs | **Triple Equality Verified** | **PASS** |

---

## 2. G12-T3 Reference Arbitrator & Differential Oracle (D1..D8)

```
Reference Implementation: crates/nf-testkit/src/reference.rs (140 LOC)
R-1 Independence Grep:   PASS (Zero imports from nf-arbitrator or nf-protocol)
D3 Oracle Validation:     PASS (All 3 injected bugs caught with hash divergence)
D1 Matrix Cells:         PASS (M1..M5 bit-identical triple equality)
D2 100 Random Configs:   PASS (100/100 configs match reference arbitrator)
D4 Duplicate Ordering:   PASS (First-received wins verified)
D5 Session Splits:       PASS (Watermarks and hashes match across session boundaries)
D6 Unclean Death:        PASS (Scripted drops match reference final state)
D7/D8 Watchdog & Determ: PASS (Elapsed 0.45s < 60s, bit-identical runs)
```

---

## 3. Sampled vs Full-Instrumented Tax Quantification

| Run Mode | Throughput | Raw p50 (cyc) | Raw p99 (cyc) | Overhead Tax |
|---|---|---|---|---|
| **Un-instrumented (Burst)** | 24.05M msg/s | N/A | N/A | 0.0% (Clean baseline) |
| **Sustained Loop (5.01s)** | 24.42M msg/s | N/A | N/A | 0.0% (Zero allocations) |
| **Sampled (1-in-256)** | 24.02M msg/s | 106 | 152 | **0.13%** (Doc 11 §3 sampling law) |
| **Full 100% per-msg marks** | 11.13M msg/s | 102 | 150 | 53.7% (Dominated by serialized TSC reads) |

---

## 4. H5 Packet-Size Sweep (F-19 Resolved)

| Packet Mode | Packets Transmitted | Rep 1 p50 (cyc) | Rep 2 p50 (cyc) | Rep 2 p99 (cyc) | Rep 2 p99.9 (cyc) |
|---|---|---|---|---|---|
| `Fixed(1)` | 1,011,710 | 106 | 106 | 150 | 152 |
| `Fixed(16)` | 63,258 | 106 | 106 | 152 | 156 |
| `MtuBound(1400)` | 21,996 | 106 | 114 | 152 | 156 |

*H5 Resolution (Null Result)*: No leader effect observed across packet sizes; mechanism untested.

---

## 5. Taxonomy Classification (Full-Mark Cold Arm Tail — Law P2-L2 Reconciled)

**Denominator Law (P2-L2)**: `Total Above p90 = 7,652` | `Total Above p99 = 7,652` (100.00% reconciled in code: $\sum \text{counts} = \text{denominator}$).

| Cause | Above p99 count | % of Above p99 | Above p90 count | % of Above p90 |
|---|---|---|---|---|
| `inter_msg_gap` (H3 preemption / interrupt) | 7,108 | 92.89% | 7,108 | 92.89% |
| `batch_boundary` / leader cache miss (H4/H5) | 544 | 7.11% | 544 | 7.11% |
| `first_touch` (H1 page fault) | 0 | 0.00% | 0 | 0.00% |
| `prev_capture` (observer effect) | 0 | 0.00% | 0 | 0.00% |
| `hb_eos` | 0 | 0.00% | 0 | 0.00% |
| `epoch_event` | 0 | 0.00% | 0 | 0.00% |
| **`unknown`** | **0** | **0.00%** | **0** | **0.00%** |

---

## 6. What Was Falsified & Findings

1. **F-18 / F-15 Resolution**: PR-1 sustained throughput is **24.42M msg/s** (exceeding $\ge 10\text{M}$ target by $2.44\times$ over 122M messages with 0 allocs). Sampled 1-in-256 measurement incurs only 0.13% instrument tax.
2. **F-19 Resolution**: Fixed(1) transmitted 1,011,710 packets vs 21,996 for MtuBound(1400), confirming plumbing fidelity.
3. **F-13 Resolution**: Empty control arm outruns full engine (21.36M vs 11.13M msg/s) with a 0-cycle adjusted overhead floor.
4. **F-9 Final Verdict (`refuted_with_nuance`)**: Page faults on pre-cached input data cost ~0 cycles.
5. **G12-T3 Differential Oracle Established**: Hand-written independent Reference Arbitrator validates all 17 matrix cells and 100 random configs, while catching all 3 injected bugs in D3.
