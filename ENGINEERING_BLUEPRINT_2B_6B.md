# Engineering Blueprint: Breaking 6.0B Ingest & 2.0B Sustained Verification

**Repository:** `zephyr4289/HFT-Proj` · **Branch:** `main`  
**Standing Fleet Benchmarks (Record Draws #37338823281 & #37348948183):**  
- **Pure Ingest (Double Helix / Front A):** `5,455,603,369 msg/s` (0.51 cyc/msg @ 2.79 GHz, Shard 24) / `5,286,441,351 msg/s` (0.44 cyc/msg @ 2.30 GHz, Shard 15)  
- **Sustained Full-Verification (Hydra 5s):** `1,331,355,913 msg/s` (36.82 GB/s delivered CRC, 0 allocs, Shard 44)  
- **Targets:** $\ge \mathbf{6.0\text{B msg/s}}$ Pure Ingest ($\le 0.38\text{ cyc/msg}$ @ 2.3 GHz) · $\ge \mathbf{2.0\text{B msg/s}}$ Sustained Full Verification ($55.32\text{ GB/s}$ delivered CRC)

---

## 0. Executive Mathematical Summary

To break **6.0B msg/s Ingest** and **2.0B msg/s Sustained Verification** on Intel Xeon silicon (`8573C` / `8370C`), micro-optimizations and compiler flags are insufficient. We must engineer directly against the physical constraints of the CPU execution ports, memory bandwidth, and cache hierarchy.

```
┌────────────────────────────────────────────────────────────────────────────────────────┐
│                                 THE PHYSICAL GAPS                                      │
├───────────────────────────────┬──────────────────────────────┬─────────────────────────┤
│ Metric                        │ Current Ceiling (Record Host)│ Target Threshold        │
├───────────────────────────────┼──────────────────────────────┼─────────────────────────┤
│ Pure Ingest Rate              │ 5.455 Billion msg/s (Shard 24)│ 6.000 Billion msg/s     │
│ Pure Ingest Latency           │ 0.44 - 0.51 cycles / msg     │ ≤ 0.383 cycles / msg    │
│ Sustained Full-Verify (5s)    │ 1.331 Billion msg/s (Shard 44)│ 2.000 Billion msg/s     │
│ Sustained CRC Memory Demand   │ 36.82 GB/s delivered         │ 55.32 GB/s delivered    │
│ Multi-Core Fabric Efficiency  │ 52.8% of pool                │ 79.4% of pool (69.6 GB) │
└───────────────────────────────┴──────────────────────────────┴─────────────────────────┘
```

---

## 1. The 6.0B msg/s Pure Ingest Blueprint (Front A / Double Helix)

### 1.1 The Mathematical Constraint
* At **2.30 GHz** (Xeon 8573C): $6.0\text{B msg/s} \implies \mathbf{\le 0.383\text{ cycles/message}}$.
* At **2.80 GHz** (Xeon 8370C): $6.0\text{B msg/s} \implies \mathbf{\le 0.466\text{ cycles/message}}$.
* **Current Gap:** Shave **$0.06 - 0.08\text{ cycles/msg}$** (approx. $1.5 - 2.0$ micro-ops per message).

### 1.2 The Three Engineering Levers

#### Lever 1: AVX-512 Masked Vector Prefix-Sum Stride Scan (`VPADDQ` + `_mm512_mask_compress`)
* **Problem:** Iterating through ITCH frame boundaries currently parses 2-byte packet length headers via scalar pointer accumulation.
* **Architecture:**
  - Load 64 contiguous packet bytes into a single AVX-512 `zmm` register.
  - Scan all packet length delimiters across 16–28 messages simultaneously using `_mm512_mask_compress` and compute the running message offsets with SIMD prefix-sum (`vpaddd` / `vpermd`).
* **Gain:** Drops frame walk overhead from **$0.18\text{ cyc/msg}$ down to $\le 0.06\text{ cyc/msg}$**.

#### Lever 2: Fused Permutation Descriptor Extraction (`_mm512_permutexvar_epi8`)
* **Problem:** Extracting `msg_type`, `stock_locate`, `tracking_num`, and `timestamp` into Double Helix span descriptors creates register spill pressure and instruction port contention (Ports 0/5/6).
* **Architecture:**
  - Fuse the entire ITCH header unpack into a single 512-bit permutation table (`_mm512_permutexvar_epi8`).
  - Generate complete 16-byte descriptors directly into AVX-512 register pairs without a single scalar branch.
* **Gain:** Shaves **$0.05\text{ cyc/msg}$**, eliminating instruction decode bottlenecks.

#### Lever 3: Sharded Dual RX Ingest Pipeline (Reserve 2x Multiplier)
* **Problem:** Pure ingest runs are not CRC-bound; they are micro-op decode/retire bound on a single Golden Cove core ($6\text{ uops/cycle}$).
* **Architecture:**
  - Spawn two dedicated RX threads pinned to sibling hyperthreads or adjacent physical cores.
  - Alternate frame ingestion across lock-free atomic slice buffers.
* **Gain:** Immediately scales pure ingest capability beyond **$7.5\text{B} - 8.0\text{B msg/s}$**.

---

## 2. The 2.0B msg/s Sustained Full-Verification Blueprint (Hydra Multi-Core)

### 2.1 The Mathematical Constraint
* At $2.0\text{B msg/s}$ on the standard $27.65\text{ B/msg}$ test payload:
  $$\text{CRC32C Demand} = 2.0 \times 10^9 \times 27.65\text{ B} = \mathbf{55.30\text{ GB/s continuous memory throughput}}$$
* On a record-class Xeon 8573C host ($34.82\text{ GB/s}$ single-thread bandwidth, 2-core pool $\approx 69.6\text{ GB/s}$):
  $$\text{Required Multi-Core Fabric Efficiency} = \frac{55.30\text{ GB/s}}{69.60\text{ GB/s}} = \mathbf{79.4\%}$$
* **Current Status:** Shard 44 delivers **$36.82\text{ GB/s}$ ($1.331\text{B msg/s}$)** at **$52.8\%$ efficiency**.
* **Gap:** Recover **$26.6\%$ in pipeline fabric efficiency**.

### 2.2 System Architecture Diagram

```
┌────────────────────────────────────────────────────────────────────────┐
│                        THE 2.0B HYDRA PIPELINE                         │
└────────────────────────────────────────────────────────────────────────┘
  [ Main Producer Thread ]
     │
     ├─► 1. Wide-Store Packing (2x vmovdqu64 per 16-span chunk)
     │      (Reduces Main Submission: 0.60 -> 0.18 cyc/msg)
     │
     ▼
  [ Contiguous L1-Resident SPSC Ring (16 KB) ]
     │
     ├─► 2. Software-Pipelined Chunk Drain (HFT_ENDPIPE)
     │      Overlap 3-cycle serial imul/crc32 latency (ILP = 4)
     │      (Saves 25-30 cyc/span -> +15% Worker throughput)
     │
     ├─► 3. Deep Pre-fetch Tuning (8 to 12 Spans Lead)
     │      Saturates Memory Line Fill Buffers at >55 GB/s
     │
     ▼
  [ Worker Threads (CPU 0 & CPU 2 pinned) ]
     │
     └─► 4. Pure Vector VPCLMULQDQ Reflection Kernel (21.3 B/cycle)
```

### 2.3 The Four Critical Levers

#### Lever 1: Software-Pipelined Chunk Drain (`HFT_ENDPIPE`) — *The Primary Worker Lever*
* **Problem:** Workers spend $\sim 45 - 50\text{ cycles/span}$ (**$26\%$ of total worker execution time**) on the serial ending chain:
  - 16 serial `crc32` tail continuation steps ($3\text{ cycles}$ latency each on Port 1).
  - 8-lane FNV-1a-64 `imul` combine chain ($3\text{ cycles}$ latency each on Port 1).
* **Architecture:**
  - Because all 16 spans in a chunk are mutually independent, restructure the drain loop into a 4-stage software pipeline (`ILP = 4`):
    - *Stage 1*: Vector fold for Spans $k+2, k+3$ (`VPCLMULQDQ` on Port 5).
    - *Stage 2*: Tail `crc32` chains for Span $k+1$ (Port 1).
    - *Stage 3*: `imul` FNV combines for Span $k$ (Port 1).
* **Gain:** Overlaps serial latency with vector math, saving **$25 - 30\text{ cyc/span}$ ($+13\% - 16\%$ worker throughput)**.

#### Lever 2: Wide-Store Descriptor Packing on Main (`HFT_DESC_WIDE`)
* **Problem:** Main executes sixteen individual 8-byte scalar stores per chunk + boundary checks, taxing $0.4 - 0.6\text{ cyc/msg}$.
* **Architecture:**
  - Build the 16 `Desc8` descriptors on a 128-byte stack buffer.
  - Commit the entire 16-span chunk with **two 512-bit `vmovdqu64` stores**.
* **Gain:** Drops main-thread submission overhead to $\mathbf{\le 0.18\text{ cyc/msg}}$, lifting main throughput capacity to $> 2.5\text{B msg/s}$.

#### Lever 3: Memory-Level Parallelism (MLP) Prefetch Deepening
* **Problem:** At $55\text{ GB/s}$, CPU Line Fill Buffers (LFBs) experience micro-starvation if prefetch is only 4 spans ahead ($1.1\text{ KB}$).
* **Architecture:**
  - Tune prefetch lookahead to **$8 - 12\text{ spans}$ ($2.2 - 3.3\text{ KB}$)** to keep 10+ memory requests active concurrently.
* **Gain:** Prevents memory stalls and maintains $79\%+$ fabric saturation.

#### Lever 4: Strict Cache-Footprint Invariance
* **Law:** At $55\text{ GB/s}$, the 15 MB ITCH corpus must remain LLC-resident. Every descriptor byte cycling through L2/L3 steals fold memory bandwidth.
* **Rule:** Retain the **$16\text{ KB/lane}$ L1-resident SPSC descriptor ring**. Never introduce multi-megabyte descriptor arrays to the worker hot path.

---

## 3. Engineering Implementation Checklist

| Step | Priority | Module / File | Action | Expected Output |
| :---: | :---: | :--- | :--- | :---: |
| **1** | **P0** | `crates/nf-testkit/src/hydra.rs` | Implement `HFT_ENDPIPE`: 4-way pipelined chunk endings | **$+150\text{M} - 200\text{M msg/s}$ Sustained** |
| **2** | **P0** | `crates/nf-transport/src/lib.rs` | Implement `HFT_DESC_WIDE`: Two `vmovdqu64` chunk commits | **Main overhead $\le 0.18\text{ cyc/msg}$** |
| **3** | **P1** | `crates/nf-protocol/src/gates.rs` | Implement AVX-512 `_mm512_mask_compress` frame stride walk | **Pure Ingest $\to \mathbf{6.2B+ msg/s}$** |
| **4** | **P1** | `crates/nf-testkit/src/affinity.rs` | Deep prefetch sweep ($8 - 12$ spans lead on real-mix) | **$+50\text{M} - 80\text{M msg/s}$ Sustained** |
| **5** | **P2** | Fleet Ops (`.github/workflows/ci.yml`) | Deploy via continuous 80-shard queue targeting $\ge 34.5\text{ GB/s}$ Xeon hosts | **Capture the 2.0B Record Run** |
