# HFT-Proj: Ultra-Low-Latency NASDAQ ITCH 5.0 Feed Handler & Arbitrator

[![CI Status](https://github.com/zephyr4289/HFT-Proj/actions/workflows/ci.yml/badge.svg)](https://github.com/zephyr4289/HFT-Proj/actions/workflows/ci.yml)
[![License: MIT/Apache-2.0](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](file:///data/data/com.termux/files/home/HFT-Proj/Cargo.toml)

**`HFT-Proj`** is an ultra-low-latency, deterministic, zero-allocation **Nasdaq TotalView-ITCH 5.0 over MoldUDP64 feed arbitrator**, reorder engine, and gap-recovery sequencer implemented in modern Rust.

Originally engineered for a **25M msg/s** target on standard cloud VMs, the project evolved through systematic architectural breakthroughs to reach **1.2348 Billion messages/second** sustained full verification and **3.625 Billion messages/second** pure RX ingest:

1. **[25M msg/s Baseline (The Hash Latency Trap)](docs/15-tail-study.md)**: Forensic stage-ectomy decomposed harness latency; discovered the FNV-1a serial dependency trap (107 cyc `imul` chain).
2. **[250M+ msg/s Single-Core (TITAN Program)](docs/19-titan.md)**: Memory page warming, `FrameMemo` verdict precomputation (4.41 cyc/msg), closed-form $O(1)$ batch span dispatch, and 8-lane interleaved hardware CRC32C.
3. **[600M+ msg/s Multi-Core Fabric (HYDRA Program)](docs/20-hydra.md)**: Pure vs. serial ordering split, chunked SPSC lock-free ring handoff (anti-ping-pong batching), and worker lookahead prefetching.
4. **[1.1B+ msg/s Multi-Core Fabric (GIGAHFT Program)](docs/21-gigahft.md)**: VPCLMULQDQ mirror-domain carry-less CRC32C folding, zero-copy in-place 128-bit ring descriptor stores, and cross-pass double-buffered fabric overlapping.
5. **[726M -> 1.109B msg/s Assist Equilibrium (R8 / R10 Programs)](docs/22-r8-teraphase.md)**: Dedicated RX-pipelined transport ([`docs/22-r8-teraphase.md`](docs/22-r8-teraphase.md)) and the 64-slot deep assist ring ([`docs/23-r10-assist.md`](docs/23-r10-assist.md)) recycling surplus submitting-core cycles into SIMD CRC.
6. **[1.2348B msg/s Sustained & 3.625B Ingest (R11 / R12 Records)](docs/24-r11-phase4.md)**: Desc8 compact 8-byte descriptors ([`docs/25-r12-ladder.md`](docs/25-r12-ladder.md)), consumed-event prepatch engine, THP 2MB memory grant, and placement topology resolution.

---

## 1. Verified Benchmark Metrics

Measured on GitHub Actions reference hardware (**Intel Xeon Platinum 8573C Sapphire Rapids / 8370C Ice Lake @ 2.30–2.60 GHz**):

### A. Pure Engine Ingest Mechanics (Harness Hash Excluded)
*Isolates raw transport staging, MoldUDP64 framing, session arbitration, duplicate rejection, and watermark sequencing without downstream verification hash overhead.*

| Ingest Mode | Measured Latency | p95 Latency | p99 Tail | StdDev (CV%) | Verified Throughput | Reference Document |
|---|---|---|---|---|---|---|
| **Classic Ingest** (`CountSink`, per-msg callback) | **`7.51 cyc/msg`** (3.07 ns) | **`8.01 cyc`** | **`8.13 cyc`** | 0.39c (5.19%) | **`325.25M msg/s`** | [`docs/19-titan.md`](docs/19-titan.md) |
| **Span Ingest** (`SpanCountSink`, $O(1)$ batch span) | **`2.09 cyc/msg`** (0.85 ns) | **`2.17 cyc`** | **`2.34 cyc`** | 0.06c (3.16%) | **`1.169 Billion msg/s`** | [`docs/21-gigahft.md`](docs/21-gigahft.md) |
| **R8 RX-Pipelined Pure Ingest** (Front A) | **`0.63 cyc/msg`** (0.27 ns) | **`0.65 cyc`** | **`0.67 cyc`** | 0.01c (1.50%) | **`3.625 Billion msg/s`** 🚀 | [`docs/24-r11-phase4.md`](docs/24-r11-phase4.md) |

### B. Multi-Core Verification Fabric (Every Emitted Byte CRC32C-Verified In-Window)
*Full production pipeline with strict in-window verification: Virtual clock pacing $\to$ Transport poll $\to$ MoldUDP64 framing $\to$ Session dispatch $\to$ Duplicate rejection $\to$ Watermark sequencing $\to$ [`HydraSpanSink`](crates/nf-testkit/src/hydra.rs) (parallel chunked AVX-512 / VPCLMULQDQ mirror-domain CRC32C fold, sequence continuity, and strict emission-order serial fold).*

| Benchmark Arm | Verified Throughput | Messages in 5.0s Run | Delivered CRC Bandwidth | Allocations | Bit-Exact Integrity | Reference Document |
|---|---|---|---|---|---|---|
| **PR-1 R11/R12 Sustained Record** (Intel 8573C) | **`1,234,801,472 msg/s`** (1.235B/s) | **6.174 Billion msgs** | **`34.15 GB/s`** | `0 bytes` | **`PASS`** (`0x881639cead506f25`) | [`docs/24-r11-phase4.md`](docs/24-r11-phase4.md) & [`docs/25-r12-ladder.md`](docs/25-r12-ladder.md) |
| **PR-1 R10 Gate-Break Sustained** (Intel 8573C) | **`1,109,130,234 msg/s`** (1.109B/s) | **5.545 Billion msgs** | **`30.67 GB/s`** | `0 bytes` | **`PASS`** (`0x881639cead506f25`) | [`docs/23-r10-assist.md`](docs/23-r10-assist.md) |
| **PR-1 HYDRA Sustained** (Multi-core baseline) | **`603.31M msg/s`** | **3.016 Billion msgs** | **`16.68 GB/s`** | `0 bytes` | **`PASS`** (`0x881639cead506f25`) | [`docs/20-hydra.md`](docs/20-hydra.md) |
| **PR-1 TITAN Single-Core** (1 Core Pinned) | **`259.49M msg/s`** | **1.297 Billion msgs** | **`7.18 GB/s`** | `0 bytes` | **`PASS`** (`0x881639cead506f25`) | [`docs/19-titan.md`](docs/19-titan.md) |

---

## 2. Technical Evolution & Architectural Journey

```
[1. Baseline] ──► [2. TITAN] ──────► [3. HYDRA] ──────► [4. GIGAHFT] ────► [5. R8/R10 ASSIST] ──► [6. R11/R12 RECORD]
   24.4M/s           259M/s             603M/s             1.109B/s               1.186B/s              1.235B/s Sustained
(Hash Trap)    (Span Protocol+Memo) (Chunked Lock-Free) (VPCLMULQDQ Fold)   (64-Slot Assist Ring)  (3.625B Ingest / Desc8)
```

---

### Phase 1. The 25M/s Baseline & The Hash Latency Trap
* **Documentation**: [`docs/15-tail-study.md`](docs/15-tail-study.md) & [`docs/artifacts/tail-study/study-report.md`](docs/artifacts/tail-study/study-report.md)
* **The Problem**: Early benchmarks stalled at ~24.4M msg/s. Forensic stage-ectomy decomposition revealed that the benchmark sink was using FNV-1a-64, an `imul` serial dependency chain costing ~107 cycles per 29-byte message.
* **Finding**: 89% of measured latency was test-harness artifact; the pure engine core was already executing in **~26.5 cycles (~86.8M msg/s)**.

---

### Phase 2. The TITAN Program (Single-Core: 25M $\to$ 259M msg/s)
* **Documentation**: [`docs/19-titan.md`](docs/19-titan.md)
* **R1 (Harness Modernization)**: Reused warm pre-rendered transport memory pages, eliminating page fault churn, and introduced precomputed block indexing.
* **R2 (Verdict Memoization / `FrameMemo`)**: Precomputed ITCH validation verdicts at transport construction time over immutable frame bytes, dropping classic validation latency to **4.41 cyc/msg**.
* **R3 (Span Protocol & Closed-Form Emission)**: Proved that contiguous valid message sequences follow $(w, count) \to (w+n, count+n)$, collapsing $O(n)$ per-message callbacks into $O(1)$ batch span dispatch via [`Sink::on_span`](crates/nf-arbitrator/src/types.rs).
* **R4 (8-Lane Hardware CRC32C & DLP Prefetching)**: Interleaved 8 CRC32C accumulators to saturate hardware execution ports at ~8 bytes/cycle, and added `_mm_prefetch(T0)` in `poll()` to resolve memory-level parallelism (MLP) stalls.

---

### Phase 3. The HYDRA Program (Multi-Core Fabric: 250M $\to$ 603M msg/s)
* **Documentation**: [`docs/20-hydra.md`](docs/20-hydra.md) & [`crates/nf-testkit/src/hydra.rs`](crates/nf-testkit/src/hydra.rs)
* **The Single-Core Ceiling**: The x86 `crc32` hardware instruction sustains at most 8 bytes/cycle throughput. With average span bodies of ~31.65 bytes, the absolute single-core verification floor is **~3.96 cyc/msg ($\approx 617\text{M msg/s}$ ceiling at 2.45 GHz)**.
* **Purity vs. Serial Ordering Split**: Computing `span_crc32c(body)` is a *pure function* of immutable span bytes and parallelizes across worker cores. The running fold `h ← (rotl(h,13) ^ v_i) * K` is serial in emission order, but lightweight $O(1)$ (~0.16 cyc/msg) and remains on the main core.
* **Chunked SPSC Handoff (Anti-Ping-Pong Law)**: Naive per-span handoffs caused cache-line thrashing (138M vs 264M). HYDRA groups descriptors into **16-span ring-aligned chunks** with atomic Release/Acquire fences, collapsing cross-core bus traffic by >10x.
* **Worker Lookahead Prefetching**: Workers prefetch span bodies 4 spans ahead, while the main thread disables redundant body prefetching.

---

### Phase 4. The GIGAHFT Program (Project 1.0B: Crossing 1.0 Billion msg/s)
* **Documentation**: [`docs/21-gigahft.md`](docs/21-gigahft.md) & [`crates/nf-testkit/src/crcfold.rs`](crates/nf-testkit/src/crcfold.rs)
* **Lever 1 — VPCLMULQDQ Mirror-Domain CRC32C Fold Kernel**: Carry-less polynomial folding over the bit-mirrored domain ($\text{CRC32C}_{\text{raw}}(X) = \text{rev32}(\bar{X} \cdot y^{32} \bmod P)$) advancing via $V \leftarrow (V_{\text{hi}} \otimes \text{KP192}) \oplus (V_{\text{lo}} \otimes \text{KP128}) \oplus \bar{U}_q$ with constants `0x18571d18` and `0x6503ea99`, ending in two chained hardware `crc32` instructions with zero Barrett reduction overhead.
* **Lever 2 — Zero-Copy In-Place 128-bit Ring Stores**: Descriptors written directly into SPSC ring slots with unaligned 128-bit stores (`ptr | len<<64 | span_id<<96`), eliminating stack buffer copies and store-forwarding stalls.
* **Lever 3 — Fused Inline Header & Session Decode**: Inline 64-bit fused template matching and sequence decode in safe Rust inside `nf-arbitrator` (`#![forbid(unsafe_code)]` preserved).
* **Lever 4 — Cross-Pass Double-Buffered Overlap Fabric**: Overlaps Pass $N+1$ dispatch with Pass $N$'s residual worker verification tail fold with global monotonic span IDs.

---

### Phase 5. R8 & R10 Programs (RX-Pipelining & The Assist Equilibrium)
* **Documentation**: [`docs/22-r8-teraphase.md`](docs/22-r8-teraphase.md) & [`docs/23-r10-assist.md`](docs/23-r10-assist.md)
* **Dedicated RX Transport Thread**: Decoupled poll staging onto a dedicated core communicating with arbitration via a 4-buffer SPSC entry mailbox with topology-aware affinity, breaking raw RX throughput into **3.47B–3.62B msg/s**.
* **64-Slot Assist Ring**: The original 4-slot assist ring clogged under backpressure. R10 introduced a deep 64-slot assist ring (`HFT_ASSIST_SLOTS`), allowing the submitting core to recycle its surplus cycles during backpressure into in-window SIMD CRC folding, breaking the gate at **`1.109B msg/s`** sustained.

---

### Phase 6. R11 & R12 Programs (The 1.2348B Sustained & 3.625B Ingest Record)
* **Documentation**: [`docs/24-r11-phase4.md`](docs/24-r11-phase4.md) & [`docs/25-r12-ladder.md`](docs/25-r12-ladder.md)
* **Desc8 Compact Descriptors ([`docs/25-r12-ladder.md`](docs/25-r12-ladder.md))**: Shrank span descriptors from 16 bytes to 8 bytes (`offset: u32 | len: u16 | flags: u16`), packing 8 descriptors per 64-byte L1 cache line instead of 4. Delivered **+2.1% (Intel 8573C) / +4.1% (Intel 8370C)** sustained throughput gains with zero correctness cost (shipped default ON).
* **Consumed-Event Prepatch Engine**: Rebuilt prepatching on consumed-event frontiers, eliminating synchronous reset latency from the critical path (+3.3% gain across all silicon classes).
* **THP 2MB Memory Grant**: Pre-fault `madvise(MADV_HUGEPAGE)` grants 2MB huge pages ($14.3\text{ MB}$ footprint), eliminating STLB page walks.
* **Placement & Physics Resolution ([`docs/24-r11-phase4.md`](docs/24-r11-phase4.md))**: Proved SMT sibling placement for main+RX and worker hyperthreads yields optimal supply-to-fold balance on 2-core/4-thread cloud runners.
* **All-Time Milestone**: **`1,234,801,472 msg/s` sustained full verification** (6.174 Billion messages in 5.00s, 34.15 GB/s CRC bandwidth) and **`3,624,572,766 msg/s` pure RX ingest** ($0.63\text{ cyc/msg}$).

---

## 3. Engineering Documentation Directory

Every architectural phase, design thesis, failure ledger, and benchmark record is cataloged in the repository:

| Document | Topic & Milestone |
|---|---|
| [`docs/00-spec.md`](docs/00-spec.md) | Formal Engineering Specification & System Invariants |
| [`docs/01-architecture.md`](docs/01-architecture.md) | End-to-End Pipeline Architecture & Component Topology |
| [`docs/02-moldudp64.md`](docs/02-moldudp64.md) | MoldUDP64 Wire Protocol Framing & Parsing Rules |
| [`docs/03-itch5.md`](docs/03-itch5.md) | NASDAQ TotalView-ITCH 5.0 Message Specifications |
| [`docs/04-replay.md`](docs/04-replay.md) | Deterministic Replay Engine & Virtual Clock Mechanics |
| [`docs/05-sequencer.md`](docs/05-sequencer.md) | Watermark Sequencer, Gap SM & Out-of-Order Engine |
| [`docs/06-livefeedproof.md`](docs/06-livefeedproof.md) | Affine Token Safety & Non-Cloneable Zero-Cost Proofs |
| [`docs/07-zeroalloc.md`](docs/07-zeroalloc.md) | Zero-Allocation Verification & Custom Fixed Allocator |
| [`docs/08-recovery.md`](docs/08-recovery.md) | TCP Gap Recovery Client & Active Retransmission Protocol |
| [`docs/09-afxdp.md`](docs/09-afxdp.md) | Linux Kernel-Bypass AF_XDP Zero-Copy Ingest Subsystem |
| [`docs/11-bench.md`](docs/11-bench.md) | Benchmarking Discipline, TSC Calibration & Honesty Policy |
| [`docs/12-gates.md`](docs/12-gates.md) | Hard Verification Gates & Differential Oracle Defenses |
| [`docs/13-journal.md`](docs/13-journal.md) | Daily Engineering Work Log & Decision History |
| [`docs/15-tail-study.md`](docs/15-tail-study.md) | Phase 1 Forensic Stage-Ectomy & FNV-1a Hash Trap Decomposition |
| [`docs/16-reference-arbitrator.md`](docs/16-reference-arbitrator.md) | Reference Unoptimized Arbitrator for Differential Testing |
| [`docs/18-target1.md`](docs/18-target1.md) | Target 1 Milestone Specification & Verification Criteria |
| [`docs/19-titan.md`](docs/19-titan.md) | **Phase 2: TITAN Program** (25M $\to$ 259M/s Single-Core Optimization) |
| [`docs/20-hydra.md`](docs/20-hydra.md) | **Phase 3: HYDRA Program** (250M $\to$ 603M/s Multi-Core Fabric) |
| [`docs/21-gigahft.md`](docs/21-gigahft.md) | **Phase 4: GIGAHFT Program** (Crossing 1.0B msg/s with VPCLMULQDQ) |
| [`docs/22-r8-teraphase.md`](docs/22-r8-teraphase.md) | **Phase 5a: R8 RX-Pipeline** (Decoupled Transport & 726M Multi-Core) |
| [`docs/23-r10-assist.md`](docs/23-r10-assist.md) | **Phase 5b: R10 Assist Ring** (64-Slot Ring & 1.109B Sustained Draw) |
| [`docs/24-r11-phase4.md`](docs/24-r11-phase4.md) | **Phase 6a: R11 Record** (1.2348B Sustained Record & Topology Resolution) |
| [`docs/25-r12-ladder.md`](docs/25-r12-ladder.md) | **Phase 6b: R12 Compact Descriptors** (Desc8 Shipped, Ladder Ledger, R13) |
| [`docs/26-r13-p5-wall.md`](docs/26-r13-p5-wall.md) | **Phase 7a: R13 Reflect Kernel** (The Natural-Domain Fold — the p5 Fix) |
| [`docs/27-r14-vend.md`](docs/27-r14-vend.md) | **Phase 7b: R14 Vend Ending** (The Vector Barrett, Class-Conditional) |
| [`docs/28-r15-vtail.md`](docs/28-r15-vtail.md) | **Phase 8a: R15 Vtail** (The Vectorized Lane-0 Tail, r≥16-Gated) |
| [`docs/29-r16-double-helix.md`](docs/29-r16-double-helix.md) | **Phase 9b: R16 Double Helix** (rxdesc Array Submission + the Distinct Placement Flip + the R16e RX Desc Diet — the 2B/5B Program) |

---

## 4. Crate Topology

```
crates/
├── nf-protocol/       # Wire format parsers, ITCH 5.0, MoldUDP64, gates (PR-1 GIGAHFT)
├── nf-arbitrator/     # Fused decode, watermark sequencer, gap state machine
├── nf-engine/         # Replay harness, TSC calibration, static histograms, benchmarks
├── nf-transport/      # Pre-rendered replay transport & AF_XDP kernel-bypass socket
├── nf-recovery/       # TCP retransmission client for gap filling
└── nf-testkit/        # D1..D12 differential oracles, crcfold, HYDRA multi-core fabric
```

---

## 5. Key Invariants & Guarantees

* **Zero Allocation In-Window**: Zero dynamic memory allocation during ingest (`ALLOC_DELTA = 0`), verified in CI.
* **Affine Token Security**: Downstream consumers receive a non-cloneable, unforgeable [`LiveFeedProof`](crates/nf-arbitrator/src/types.rs) on every message, guaranteeing that out-of-order or corrupt data cannot reach execution engines.
* **Deterministic Conformance**: Produces bit-exact golden hash `0x881639cead506f25` / `0xF6EF154EFDE905D8` across all matrix test configurations.
* **Invariant TSC Timing**: Sub-nanosecond time stamping with hardware `rdtscp` calibrated via Theil-Sen regression against `CLOCK_MONOTONIC_RAW`.
* **Differential Verification**: Anchored by D1 through D12 oracles against software reference tables across all body lengths and byte alignments.

---

## 6. Quick Start & Verification

### Build Workspace
```bash
# Optimized release build
RUSTFLAGS="-C target-cpu=native" cargo build --workspace --release
```

### Run Benchmarks
```bash
# Run 30-run statistical verification gate (hft_bench — pure span ingest gate)
cargo run --release -p nf-engine --bin hft_bench -- --sample data/tests/sample-mini.itch --runs 30 --warmup 5

# Run HYDRA / GIGAHFT sustained full-verification fabric (5s loop, CRC32C verified)
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --hydra-only --runs 7

# Run single-core TITAN benchmark & stage-ectomy study
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --runs 5 --study

# Run full differential oracle suite (D1..D12)
cargo run --release -p nf-testkit --bin diff_oracle
```

### Run Full CI Suite
```bash
./scripts/ci.sh
```

---

## 7. Claims Scope (Honesty Split)

Per project policy ([`docs/11-bench.md §1`](docs/11-bench.md)):
* **What is claimed (Pure Engine Ingest)**: Single-core deterministic replay mechanics exceeding **1.169 Billion msg/s** ($2.09\text{ cyc/msg}$ span ingest) and **3.625 Billion msg/s** ($0.63\text{ cyc/msg}$) via RX-pipelined transport on Intel Xeon 8573C / Zen5, sub-5 cycle software arbitration floor, zero heap allocations, and bit-exact golden hash verification.
* **What is claimed (Multi-Core Verification Fabric)**: Multi-core parallel verification fabric sustaining **1,234,801,472 msg/s** (6.174 Billion messages in 5.00s, 34.15 GB/s verified CRC throughput, `ALLOC_DELTA = 0`, bit-exact `0x881639cead506f25`) on a 2-vCPU Intel Xeon 8573C runner with **every emitted byte read and CRC32C-verified in-window**.
* **What is NOT claimed**: This is not an FPGA hardware feed handler, not real-NIC kernel bypass zero-copy hardware, and does not make unsubstantiated marketing comparisons against dedicated hardware appliances.
