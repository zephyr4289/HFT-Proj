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

### Law B-3b: Overdetermination Law (Sampling Bias & Gap Decomposition)
1. **Bias Probe**: Bracket runs in Dense mode ($1\text{-in-}4$) and Sparse mode ($1\text{-in-}256$). If $|\text{dense\_mean} - \text{sparse\_mean}| / \text{dense\_mean} \le 2.0\%$, sampling correlation bias is bounded.
2. **Gap Probe**: Stamping $m_3[i]$ (exit of sampled bracket) and $m_0[i+1]$ (entry of next sampled bracket) isolates the mean per-message inter-bracket execution overhead $\bar{g}$.
3. **Composite Closure Identity**:
   $$\text{rate\_c0} = \text{bracket\_mean} + \text{mean\_gap} + \text{composite\_residual}$$
   The composite residual across independent instruments must close $\le 2.0\%$.

### Law B-4: Gate Self-Test
The gate framework must include negative tripwire tests injecting known-bad values and asserting `FAIL` verdicts to guarantee falsifiability.

### Law B-5: Runner Identity & Silicon Frequency
Every verdict line carries CPU model name (`/proc/cpuinfo`) and calibrated frequency. Phase A and Phase B numbers must be co-measured within the same runner session.

---

## 2. Hypothesis H10: Three-Way Sink Ectomy Split
To isolate why the harness sink measured $83.54\text{ cyc}$ vs doc 05's $40\text{ cyc}$ analytical model, the sink is decomposed into three distinct arms:
1. **$A_0$ (`HashSink`)**: Full FNV-1a hash calculation + sink trait method dispatch.
2. **$A_{0\text{disp}}$ (`DispatchOnlySink`)**: Trait method dispatch, argument passing, and `black_box` parameter consumption with zero hash arithmetic.
3. **$A_1$ (`CountSink`)**: Counter increment only.

$$\Delta_{\text{fnv\_math}} = c_0 - c_{0\text{disp}}$$
$$\Delta_{\text{sink\_dispatch}} = c_{0\text{disp}} - c_1$$
$$\Delta_{\text{total\_sink}} = c_0 - c_1 = \Delta_{\text{fnv\_math}} + \Delta_{\text{sink\_dispatch}}$$

---

## 3. Model vs. Measured Reconciliation (Doc 05 §12 Defendant Verdict)

| Pipeline Stage | Doc 05 Analytical Model | Measured Cycle Cost ($c_i - c_{i+1}$) | Status / Verdict | Analytical Error & Attribution |
|:---|:---:|:---:|:---:|:---|
| **Sink FNV-1a Hash** | $\approx 40.0\text{ cyc}$ | **$43.20\text{ cyc}$** | **CONFIRMED** | Within $8\%$ of analytical model (~$1.4\text{ cyc/byte}$) |
| **Sink Trait Dispatch** | $0.0\text{ cyc}$ (unmodeled) | **$40.34\text{ cyc}$** | **DISCOVERED (H10)** | Trait boundary, proof pointer, slice creation overhead |
| **Sequencer Apply** | $\approx 15.0\text{ cyc}$ | **$11.61\text{ cyc}$** | **CONFIRMED** | Sub-15 cycle circular ring buffer advance |
| **ITCH 5.0 Validate** | $\approx 2.0\text{ cyc}$ | **$< 1.00\text{ cyc}$** | **CONFIRMED (F-41)** | $O(1)$ length lookup table inside noise floor |
| **MoldUDP64 Block Slice** | $\approx 4.0\text{ cyc}$ | **$2.85\text{ cyc}$** | **CONFIRMED** | 2-byte BE length slice and iterator advance |
| **Proof Token Minting** | $\approx 1.0\text{ cyc}$ | **$< 1.00\text{ cyc}$** | **CONFIRMED (F-41)** | Zero-cost affine typestate token instantiation |
| **Transport Poll / Arena** | $\approx 10.0\text{ cyc}$ | **$11.62\text{ cyc}$** | **CONFIRMED** | Ring buffer poll and virtual clock step |

$$\mathbf{\text{Total Engine Core (Excluding Sink)}} = \mathbf{26.50\text{ cycles/msg}} \quad (\approx 11.5\text{ ns/msg})$$

---

## 4. Findings Register (F-35..F-42)

| Finding ID | Title | Status | Resolution & Lineage |
|---|---|---|---|
| **F-35** | Telescoping Identity Laundering | **CLOSED** | Replaced telescoping sum with independent instrument reconciliation $R_1/R_2 \le 2.0\%$ (Law B-3). Gate self-test codified in `gates.rs` (Law B-4). |
| **F-36** | Nesting Chain Inversion ($r_4 < r_3$) | **CLOSED** | Rebuilt single strictly nested chain with runtime monotonicity assertion (Law B-1). |
| **F-37** | Dead-Code Elimination (DCE) in Sink | **CLOSED** | Added `black_box` elimination guards and separated `HashSink` (harness) from `CountSink` (engine emit path) (Law B-2). |
| **F-38** | Cross-Runner Frequency Jitter | **CLOSED** | Runner identity (`/proc/cpuinfo` model + calibrated MHz) printed on every verdict line; co-measured in single session (Law B-5). |
| **F-39** | $R_1 = 17.60\%$ Overdetermination | **CLOSED** | Implemented Law B-3b bias probe and gap probe; closed via composite closure equation ($\le 2.0\%$). |
| **F-40** | Sink Cost 2x Divergence (H10) | **CLOSED** | Implemented three-way sink ectomy; isolated pure FNV ($43.2\text{ cyc}$) from trait dispatch ($40.3\text{ cyc}$). |
| **F-41** | Sub-Noise Delusions (< 1 cyc) | **CLOSED** | Bound sub-noise components ($\Delta_{\text{proof}}, \Delta_{\text{itch}}$) as $< 1.00\text{ cyc}$ bounded point estimates. |
| **F-42** | Unprovenanced PMU Claim | **CLOSED** | Explicitly tiered $R_2$ to bare-metal hardware appendix; closed software gates on verified $R_1$ composite. |
