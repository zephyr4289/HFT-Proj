# 18. Target-1: Component Decomposition, Dose-Response & Non-Tautological Accounting

```
Document:  18-target1.md
Status:    FROZEN (v3.0 governed by wave2.0.md)
Authority: Governed by docs/00-spec.md §PR-2, docs/11-bench.md, and Laws B-1..B-5.
```

---

## 1. Phase B-Redo Laws (post F-35..F-38)

### Law B-1: Strict Nesting Chain & Inversion Assertion
Decomposition arms form **ONE strict chain**, each equal to the previous minus exactly one stage:
$$\text{Full (HashSink)} \to \text{CountSink (No Hash)} \to \text{No-Proof-Mint} \to \text{No-Seq-Apply} \to \text{No-Itch-Parse} \to \text{No-Block-Walk} \to \text{Poll-Only}$$
Code asserts: rates must be **monotonically non-decreasing** down the chain:
$$\text{rate}(A_0) \le \text{rate}(A_1) \le \text{rate}(A_2) \le \dots \le \text{rate}(A_6)$$
Any inversion triggers an immediate runtime abort (`assert!`).

### Law B-2: Elimination Guards
When a stage is removed, its inputs are still traversed and its would-be outputs consumed via `std::hint::black_box`.
The sink decomposition explicitly splits $\Delta_{\text{sink}}$ into:
- **`HashSink`**: Computes FNV-1a hash of every message payload (conformance test harness).
- **`CountSink`**: Increments message counter and calls `black_box(msg.as_ptr())` with zero hash math, keeping the entire engine emit path alive.

### Law B-3: Non-Tautological Reconciliation ($R_1 / R_2$)
Residual is computed **ONLY** between independent measurement instruments:
$$R_1 = \frac{|\text{rate\_total\_cyc} - \text{mean}(\text{bracket\_samples})|}{\text{rate\_total\_cyc}} \le 2.0\%$$
$$R_2 = \frac{|\text{rate\_total\_cyc} - \text{perf\_stat\_cyc}|}{\text{rate\_total\_cyc}} \le 2.0\%$$
Telescoping identities ($c_0 - c_1 + c_1 - c_2 + \dots = c_0$) are strictly forbidden as reconciliation evidence.
Bracket readouts report **MEAN** (rate space is mean-based); medians are retained for distribution shape only.

### Law B-4: Gate Self-Test
The gate framework must include negative tripwire tests injecting known-bad values and asserting `FAIL` verdicts to guarantee falsifiability.

### Law B-5: Runner Identity & Silicon Frequency
Every verdict line carries CPU model name (`/proc/cpuinfo`) and calibrated frequency. Phase A and Phase B numbers must be co-measured within the same runner session.

---

## 2. Findings Register (F-35..F-38)

| Finding ID | Title | Status | Resolution & Lineage |
|---|---|---|---|
| **F-35** | Telescoping Identity Laundering | **CLOSED** | Replaced telescoping sum with independent instrument reconciliation $R_1/R_2 \le 2.0\%$ (Law B-3). Gate self-test codified in `gates.rs` (Law B-4). |
| **F-36** | Nesting Chain Inversion ($r_4 < r_3$) | **CLOSED** | Rebuilt single strictly nested chain with runtime monotonicity assertion (Law B-1). |
| **F-37** | Dead-Code Elimination (DCE) in Sink | **CLOSED** | Added `black_box` elimination guards and separated `HashSink` (harness) from `CountSink` (engine emit path) (Law B-2). |
| **F-38** | Cross-Runner Frequency Jitter | **CLOSED** | Runner identity (`/proc/cpuinfo` model + calibrated MHz) printed on every verdict line; co-measured in single session (Law B-5). |
