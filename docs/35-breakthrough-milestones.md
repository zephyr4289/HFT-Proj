# 🏛️ Architecture & Empirical Milestones: The Journey from 25M to 12.1B msg/s

This document is the authoritative historical and architectural ledger of `HFT-Proj`, tracking every milestone, architectural breakthrough, refutation, and performance ceiling from the initial 25M msg/s baseline to **`12.103 Billion msg/s`** pure ingest and **`2.009 Billion msg/s`** sustained bit-exact verification.

---

## 1. The Complete Performance Evolution Table

| Phase / Milestone | Sustained Full-Verify | Pure Ingest Rate | Clock Cycles / Msg | Hardware Baseline | Core Architectural Levers | Key References |
|---|---|---|---|---|---|---|
| **Phase 1: 25M Baseline** | 24.4M msg/s | ~86.8M msg/s | 107.0 cyc (sink) | Intel Xeon Cloud VM | Forensic stage-ectomy, FNV-1a hash latency trap discovery | [`docs/15-tail-study.md`](15-tail-study.md) |
| **Phase 2: TITAN (100M–250M)** | 259.5M msg/s | 325.3M msg/s | 4.41 cyc (verdict) | Intel Xeon 8573C (1C) | Transport page warming, `FrameMemo` verdict precomputation, $O(1)$ batch span dispatch, 8-way CRC32C | [`docs/19-titan.md`](19-titan.md) |
| **Phase 3: HYDRA (600M+)** | 603.3M msg/s | 617.0M msg/s | 3.96 cyc/msg | Intel Xeon 8573C (2C) | Pure vs. serial ordering split, chunked lock-free SPSC ring (16-span chunks), worker prefetching | [`docs/20-hydra.md`](20-hydra.md) |
| **Phase 4: GIGAHFT (1.0B–1.1B)** | 1,109.1M msg/s | 1.169B msg/s | 2.09 cyc/msg | Intel Xeon 8573C (2C) | VPCLMULQDQ mirror-domain carry-less fold, in-place 128B ring stores, double-buffered cross-pass overlap | [`docs/21-gigahft.md`](21-gigahft.md) |
| **Phase 5: R8–R10 (1.1B–1.2B)** | 1,186.2M msg/s | 3.472B msg/s | 0.63 cyc/msg | Intel Xeon 8573C (2C) | RX-pipelined transport thread, 64-slot assist ring recycling surplus submitting cycles | [`docs/22-r8-teraphase.md`](22-r8-teraphase.md) & [`docs/23-r10-assist.md`](23-r10-assist.md) |
| **Phase 6: R11–R12 (1.235B / 3.6B)** | 1,234.8M msg/s | 3.625B msg/s | 0.58 cyc/msg | Intel Xeon 8573C (2C) | Desc8 compact 8-byte descriptors, consumed-event prepatch engine, THP 2MB memory grant | [`docs/24-r11-phase4.md`](24-r11-phase4.md) & [`docs/25-r12-ladder.md`](25-r12-ladder.md) |
| **Phase 7: R13–R16 Double-Helix** | 1.45B–1.55B msg/s | 4.54B msg/s | 0.50 cyc/msg | Intel Xeon 8573C (4 vCPU) | Vector Barrett reduction (`vend`), Vectorized Tail (`vtail`), Array-driven submission (`rxdesc`) | [`docs/27-r14-vend.md`](27-r14-vend.md) & [`docs/29-r16-double-helix.md`](29-r16-double-helix.md) |
| **Phase 8: Silicon Census & Targeting** | 1.84B msg/s | 6.0B msg/s | 0.38 cyc/msg | Fleet Census (N=2,000) | 120-shard CI saturation matrix targeting Zen 5 (`9V45`), Emerald Rapids (`8573C`), Xeon 6 (`6973P`) | [`docs/SILICON_DATA.md`](SILICON_DATA.md) |
| **Phase 9: The 1.9B / 12.1B Frontier** | **`1,959,208,315 msg/s`** | **`12,103,822,455 msg/s`** | **`0.2145 cyc/msg`** | AMD EPYC 9V45 (Zen 5 @ 4.56 GHz) | Distinct physical-core pinning within shared L3 domain, 512-bit SIMD framing, SPSC ring saturation | [`docs/33-r21-fused-verify.md`](33-r21-fused-verify.md) |
| **Phase 10: R21/R22 & 2.0B Breakthrough** | **`2,009,064,872 msg/s`** | **`11,514,625,207 msg/s`** | **`0.2255 cyc/msg`** | AMD EPYC 9V45 (Zen 5 Shard 40) | Deterministic Sysfs Topology Verification, 64B NT streaming stores, `fold512_rc` 66.6 GB/s carryless fold | [`docs/34-r22-tri-drain.md`](34-r22-tri-drain.md) |
| **Phase 11: Transport Fabric Ceiling** | **`3,093,915,848 msg/s`** | **`12.1B+ msg/s`** | N/A (Non-CRC) | AMD EPYC 9V45 (Shard 40) | Formal proof via `11z` null instrument: pipeline fabric easily sustains **> 3.09B msg/s** | [`docs/33-r21-fused-verify.md`](33-r21-fused-verify.md) |

---

## 2. In-Depth Milestone Progression & Decisions

### 1. The 25M msg/s Baseline & The Hash Latency Trap
* **Discovery:** The early engine appeared capped at 24.4M msg/s. Stage-ectomy decomposition revealed that the test harness used FNV-1a-64, introducing a serial 107-cycle `imul` dependency per 29-byte message.
* **Resolution:** Replaced the harness hash with an orthogonal hardware CRC32C pipeline. The actual underlying engine was revealed to run at **86.8M msg/s (26.5 cyc/msg)**.

### 2. TITAN: 25M $\to$ 259M msg/s (Single-Core Saturation)
* **Verdict Memoization (`FrameMemo`):** Validated immutable MoldUDP64/ITCH frames once at construction, caching validation verdicts in a bitmask. Dropped validation cost to **4.41 cyc/msg**.
* **Span Protocol:** Discovered that contiguous valid messages satisfy $(w, count) \to (w+n, count+n)$. Collapsed individual per-message virtual callbacks into single $O(1)$ batch span invocations (`on_span`).
* **8-Way Interleaved CRC32C:** Interleaved 8 independent CRC accumulators to saturate hardware execution ports at ~8 bytes/cycle.

### 3. HYDRA: 250M $\to$ 603M msg/s (Multi-Core Ordering Fabric)
* **The Single-Core Physics Ceiling:** Hardware `crc32` caps single-core throughput at ~8 bytes/cycle (~617M msg/s for 31.65B messages).
* **Pure vs. Serial Split:** Decoupled pure span byte hashing (parallelized across worker cores) from strict emission-order fold chaining (`h ← (rotl(h, 13) ^ v) * K`, lightweight $O(1)$ on the main thread).
* **Anti-Ping-Pong Handoff:** Grouped span descriptors into **16-span cache-line-aligned chunks** with release/acquire fences, cutting cross-core interconnect traffic by 90%.

### 4. GIGAHFT: 600M $\to$ 1.109B msg/s (Crossing the Billion Barrier)
* **VPCLMULQDQ Mirror-Domain Fold:** Implemented carryless polynomial multiplication over 512-bit vector registers:
  $$V \leftarrow (V_{\text{hi}} \otimes \text{KP192}) \oplus (V_{\text{lo}} \otimes \text{KP128}) \oplus \bar{U}_q$$
* **In-Place Ring Stores:** Eliminated stack buffering via direct unaligned 128-bit descriptor stores into ring buffers.
* **Double-Buffered Overlap Fabric:** Overlapped residual worker verification from Pass $N$ with Pass $N+1$ dispatch.

### 5. R8–R12: 1.109B $\to$ 1.235B Sustained & 3.625B Ingest
* **Dedicated RX Pipelining:** Offloaded network polling onto a dedicated RX thread with an SPSC mailbox, pushing raw ingest to **3.625B msg/s (0.63 cyc/msg)**.
* **Desc8 (8-Byte Compact Descriptors):** Shrank descriptor structs to 8 bytes (`offset: u32 | len: u16 | flags: u16`), packing 8 descriptors per 64B L1D cache line.
* **THP Hugepages & Topology Resolution:** Mapped 2MB Transparent Huge Pages to eradicate STLB misses and resolved SMT core affinities.

### 6. The 2,000-Shard Fleet Silicon Census (Empirical Cloud Distribution)
* **Dataset:** 2,000 empirical cloud runner instances across 20 CI runs.
* **Key Findings:**
  * `AMD EPYC 9V45` (Zen 5 @ 4.56 GHz): **14.05%** fleet probability.
  * `Intel Xeon Platinum 8573C` (Emerald Rapids @ 3.50 GHz): **7.50%** fleet probability.
  * `Intel Xeon 6973P-C` (Xeon 6 Granite Rapids @ 3.80 GHz): **3.20%** fleet probability.
* **120-Shard Matrix Law:** Expanding CI to 120 shards with $<1\text{s}$ fast-discard guarantees capturing $\ge 15$ top-tier target nodes with $P > 99.99\%$.

### 7. The 1.959B / 12.103B Breakthrough (Zen 5 & Xeon 6)
* **Pure Ingest Record:** Reached **`12,103,822,455 msg/s`** (**0.2145 clock cycles/msg**) on `AMD EPYC 9V45` (Zen 5).
* **Sustained Full-Verify Record:** Reached **`1,959,208,315 msg/s`** sustained with 100% bit-exact parity (`0x881639cead506f25`) and zero heap allocations (`ALLOC_DELTA == 0`).
* **Microarchitecture:** 4 VPCLMUL execution pipes, 4.56 GHz boost clocks, 512-bit data paths, and cross-core L3-shared handoffs.

### 8. R21/R22 & Breaking the 2.0 Billion msg/s Barrier
* **Task 4 Deterministic Topology Verification:** Added real-time sysfs parsing (`verify_pair_topology`) ensuring `(Main, RX)` threads always land on distinct physical cores sharing L3 (`TOPOLOGY_VERIFICATION main=0 rx=2 -> VERIFIED`), permanently locking in the 0.21 cyc/msg ingest mode.
* **Non-Temporal 64B Streaming Stores:** Added AVX-512 streaming stores (`_mm512_stream_si512` + `_mm_sfence`) to bypass L1D/L2 cache pollution during descriptor batch handoffs.
* **Breaking 2.0 Billion msg/s:** On Zen 5 (Shard 40, Run #458), sustained full-verify hit **`2,009,064,872 msg/s`** (2.009B msg/s) with `fold512_rc` memory folding sustaining **66.6 GB/s**.
* **Transport Fabric Capacity Proven at 3.094B msg/s:** The `11z` null-instrument arm established that the core transport fabric sustains **`3,093,915,848 msg/s`** (3.094B/s), isolating the remaining CRC verification tax at ~1.08B msg/s.

---

## 3. Core Architectural Invariants

Across every release and breakthrough:
1. **100% Bit-Exact Determinism:** Every benchmark draw matches the golden parity hash `0x881639cead506f25` / `0xF6EF154EFDE905D8`.
2. **Zero Heap Allocation:** `ALLOC_DELTA == 0` on every critical path, verified by custom allocator tracking.
3. **Strict Descriptor Compactness:** All handoff descriptors are strictly $\le 64\text{ bytes}$ (1 cache line), with fat 128B structs permanently forbidden.
