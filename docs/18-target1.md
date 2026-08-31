# 18. Target-1: Six-Arm Component Decomposition & Residual Accounting

```
Document:  18-target1.md
Status:    FROZEN (v1.0 governed by wave1.8.md)
Authority: Governed by docs/00-spec.md §PR-2, docs/11-bench.md, and Laws A-1..A-5.
```

---

## 1. Governance & Laws of Decomposition

### Law A-1b: Mark-Placement Law
Marks $m_0$ and $m_3$ bracket the **SAME code region in every arm** — from ingest entry to sink return. Per-message loop scaffolding (batch iteration, transport dispatch, frame lifetime) must sit **OUTSIDE** the bracket in all arms. Arms differ **ONLY** in the work function executed inside the bracket.

### Law A-1c: Loop-Cost Arm Law
A dedicated third configuration — `Arm::Loop` — executes the full scaffolding (real transport poll, real batch walk, real frame handling, MoldUDP64 block slicing, and ITCH length parsing) with the stateful arbitrator work function no-op'd.
The measured difference:
$$\text{loop\_cost} = \text{loop\_p50} - \text{empty\_p50}$$
The core headline metric of engine work becomes:
$$\text{work\_residual} = \text{full\_p50} - \text{loop\_p50}$$
$$\text{full\_p50} = \text{mark\_floor} + \text{loop\_cost} + \text{work\_residual}$$

### Law A-4: Spread Discipline
A component delta is meaningful if and only if $\Delta > \text{spread}(\text{baseline})$. Spread is reported as a first-class metric across $\ge 20$ statistical runs.

---

## 2. Band Provenance & Pre-Registration Table

| Band ID | Metric | Original Pre-Reg Band | Revised Pre-Reg Band | Revision Rationale & Commit |
|---|---|---|---|---|
| **PA-1** | `empty_p50` | $[20, 40]\text{ cyc}$ | $[40, 55]\text{ cyc}$ | Revised pre-run; F-13 empirical floor of 49 cyc (VM serialized RDTSCP overhead). Commit `f537940`. |
| **PA-2** | `full_p50` | $[90, 125]\text{ cyc}$ | $[110, 130]\text{ cyc}$ | Revised pre-run to match sampled 1-in-256 baseline (122 cyc). Commit `f537940`. |
| **PA-3** | `loop_p50` | N/A (New Arm) | $[60, 100]\text{ cyc}$ | Pre-registered under Law A-1c to isolate scaffolding and ITCH parser cost. |
| **PA-4** | `work_residual` | $[60, 90]\text{ cyc}$ (dirty) | $[15, 45]\text{ cyc}$ (clean) | Pre-registered clean headline metric: $\text{full\_p50} - \text{loop\_p50}$. |

---

## 3. The Decomposition Arms

1. **Arm 0 (`Arm::Empty`)**: Measures `mark_floor` (pure instrument overhead $m_0 \to m_3$ around empty `black_box`).
2. **Arm 1 (`Arm::Loop`)**: Measures `mark_floor + loop_cost` (MoldUDP64 parsing, message block slicing, ITCH length validation).
3. **Arm 2 (`Arm::TransportOnly`)**: Ingress polling + MoldUDP64 header validation (`m0 → m1`).
4. **Arm 3 (`Arm::ItchParse`)**: ITCH 5.0 full message dispatch (`m1 → m2`).
5. **Arm 4 (`Arm::SequencerCore`)**: Sequencer watermark check, contiguous slot update, Clear-on-Advance (`m2 → m3`).
6. **Arm 5 (`Arm::Full`)**: Full end-to-end production replay pipeline with LiveFeedProof typestate pass to downstream Sink.
