# 18. Target-1: Component Decomposition, Dose-Response & Rate-Space Accounting

```
Document:  18-target1.md
Status:    FROZEN (v2.0 governed by wave1.9.md)
Authority: Governed by docs/00-spec.md §PR-2, docs/11-bench.md, and Laws A-1..A-5.
```

---

## 1. Governance & Laws of Decomposition

### Law A-1b: Mark-Placement Law
Marks $m_0$ and $m_3$ bracket the **SAME code region in every arm** — from ingest entry to sink return. Per-message loop scaffolding (batch iteration, transport dispatch, frame lifetime) sits **OUTSIDE** the bracket in all arms.

### Law A-1d: Dose-Response (Instrument-Validity Law)
An arm or bracket cannot cite zero cost until it has demonstrated sensitivity to injected work.
The instrument is validated by injecting $K \in \{0, 10, 20, 50, 100\}$ units of calibrated work inside the measurement window and verifying a linear slope:
$$\frac{d(\text{p50})}{dK} \approx \text{unit\_cost} \quad (\text{Linear Sensitivity Verified})$$

### Law A-2: Rate-Based Stage-Ectomy Protocol
Per-packet transport polling and framing costs sit outside the per-message bracket by construction. To measure both per-packet and per-message components without bracket distortion, the engine executes un-instrumented builds (zero RDTSCP marks) across 5 systematic stage-ectomy variants over $\ge 20$ runs each:
- **$S_0$ (Full Production Replay)**: Ingress + MoldUDP64 parse + Sequencer + Token pass + Sink callback.
- **$S_1$ (Minus Sink)**: Ingress + Parse + Sequencer, sink `on_msg` is no-op `black_box`.
- **$S_2$ (Minus Sequencer Core)**: Ingress + Parse, watermark check only, skipping ring buffer slot write & clear-on-advance.
- **$S_3$ (Minus MoldUDP64 Block Parse)**: Ingress polling + 20B Header parse, skipping message block iteration.
- **$S_4$ (Transport Polling Baseline)**: Raw UMEM buffer poll only.

$$\text{Stage Cost } \Delta_i = \text{cyc/msg}(S_i) - \text{cyc/msg}(S_{i+1}) \quad \text{where } \text{cyc/msg} = \frac{\text{freq}}{\text{rate}}$$

### Law A-3: Cross-Check Reconciliation Law
The sum of stage-ectomy deltas plus per-packet amortized overhead must equal total measured wall clock cycles per message within a 2.0% residual:
$$\left| \sum \Delta_i - \text{Wall cyc/msg} \right| \le 2.0\%$$

### Law A-4: Spread Discipline
A component delta is meaningful if and only if $\Delta > \text{spread}(\text{baseline})$. Spread is reported as a first-class metric across $\ge 20$ statistical runs.

---

## 2. Band Provenance & Pre-Registration Table

| Band ID | Metric | Original Pre-Reg Band | Revised Pre-Reg Band | Revision Rationale & Commit |
|---|---|---|---|---|
| **PA-1** | `empty_p50` | $[20, 40]\text{ cyc}$ | $[40, 55]\text{ cyc}$ | Revised pre-run; F-13 empirical floor of 49 cyc (VM serialized RDTSCP overhead). Commit `f537940`. |
| **PA-2** | `full_p50` | $[90, 125]\text{ cyc}$ | $[110, 130]\text{ cyc}$ | Revised pre-run to match sampled 1-in-256 baseline (122 cyc). Commit `f537940`. |
| **PA-3** | `dose_response_slope` | N/A | $[0.90, 1.10]$ | Law A-1d instrument validity validation across injected work loop. |
| **PA-4** | `stage_ectomy_residual` | N/A | $\le 2.0\%$ | Law A-3 reconciliation gate between stage deltas and wall clock. |

---

## 3. Findings & Defect Register (F-32..F-34)

| Finding ID | Title | Status | Resolution / Evidence |
|---|---|---|---|
| **F-32** | A-1b vs A-1c Spec Contradiction | **CLOSED** | Struck conflicting A-1c loop arm; replaced with Law A-1d (Dose-Response) and Phase B Rate-Based Stage-Ectomy. |
| **F-33** | False "Sub-Cycle" Physics Claim | **CLOSED** | Struck "sub-cycle L1 execution" claim; parse and transport costs documented as per-packet work outside the per-message bracket. |
| **F-34** | Arm 1 Description Misattribution | **CLOSED** | Corrected description column to delineate per-packet scaffolding from per-message work. |
