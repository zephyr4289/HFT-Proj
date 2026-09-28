# HFT-Proj: Ultra-Low-Latency NASDAQ ITCH 5.0 Feed Handler & Arbitrator

[![CI Status](https://github.com/zephyr4289/HFT-Proj/actions/workflows/ci.yml/badge.svg)](https://github.com/zephyr4289/HFT-Proj/actions/workflows/ci.yml)
[![License: MIT/Apache-2.0](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](file:///data/data/com.termux/files/home/HFT-Proj/Cargo.toml)

**`HFT-Proj`** is an ultra-low-latency, deterministic, zero-allocation **Nasdaq TotalView-ITCH 5.0 over MoldUDP64 feed arbitrator**, reorder engine, and gap-recovery sequencer implemented in modern Rust.

Originally engineered for **25M msg/s** on standard cloud VMs, the project evolved through two major architectural breakthroughs:
1. **The TITAN Program (Single-Core)**: Scaling single-core in-window verified throughput to **235M–259M+ msg/s** (~3.85–4.25 ns/msg) and raw engine arbitration to **4.41 cyc/msg** (1.80 ns classic) / **1.77 cyc/msg** (0.72 ns vectorized span).
2. **The HYDRA Program (Multi-Core Fabric)**: Breaking the single-core x86 instruction throughput ceiling via a lock-free parallel verification fabric, scaling verified in-window throughput to **561M–603M+ msg/s** (>3.01 Billion messages in 5.0s) with **bit-exact conformance** and **zero heap allocations** (`ALLOC_DELTA = 0`).

The **GIGAHFT program (R7, Project 1.0B)** then attacked the remaining budget with four levers — a **bit-exact VPCLMULQDQ mirror-domain CRC32C fold kernel** (the final reduction collapses to two chained hardware `crc32` instructions; no Barrett, no length-dependent constants), **zero-copy in-place 128-bit ring descriptor stores**, a **fused hot-path header/session decode**, and a **cross-pass double-buffered fabric** whose span ids never reset: pass N+1's submission overlaps pass N's residual worker tail fold, the pipeline never drains mid-run, and every completed pass must reproduce the pinned per-pass `(count, hash, msg_hash)` tuple exactly — gated at **≥ 1B msg/s sustained** (`PR1_GIGAHFT_MIN_MSG_PER_SEC`) with `ALLOC_DELTA = 0` and every byte still read and verified in-window (see [`docs/21-gigahft.md`](docs/21-gigahft.md) for the lever inventory, the physics audit, and the honest runner expectations).

---

## 1. Verified Benchmark Metrics

Measured on GitHub Actions reference hardware (**Intel Xeon / AMD EPYC @ 2.45–2.60 GHz**):

### A. Multi-Core HYDRA Verification Fabric (3 Workers, Unpinned)
*Full pipeline: Virtual clock pacing $\to$ Transport poll $\to$ MoldUDP64 framing $\to$ Session dispatch $\to$ Duplicate rejection $\to$ Watermark sequencing $\to$ [`HydraSpanSink`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-testkit/src/hydra.rs) (parallel chunked 8-lane hardware CRC32C, sequence continuity, and strict emission-order serial fold).*

| Benchmark Arm | Measured Throughput | Duration / Scale | Allocations | Conformance |
|---|---|---|---|---|
| **PR-1 HYDRA Burst** (7-run median) | **`561.87M msg/s`** *(Peak: `602.36M/s`)* | 505,849 msgs / pass | `0 bytes` | **`BIT-EXACT`** (`0x881639cead506f25`) |
| **PR-1 HYDRA Sustained** (5.0s loop) | **`603.31M msg/s`** | **3.016 Billion msgs** (5.00s) | `0 bytes` | **`BIT-EXACT`** (fresh sessions) |

### B. Single-Core TITAN Baseline (1 Core Pinned)
*Full pipeline with single-threaded in-window byte verification via [`SpanConformanceSink`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-testkit/src/sink.rs).*

| Benchmark Arm | Measured Throughput | Measured Latency | Target Gate | Verdict |
|---|---|---|---|---|
| **PR-1 TITAN Burst** (Single-pass) | **`235.18M msg/s`** | **`10.40 cyc/msg`** (4.25 ns) | $\ge 100\text{M msg/s}$ | **`PASS`** |
| **PR-1 TITAN Sustained** (5.0s loop) | **`259.49M msg/s`** | **`9.42 cyc/msg`** (3.85 ns) | $\ge 100\text{M msg/s}$ | **`PASS`** |

### C. Pure Engine Ingest Mechanics (Harness Hash Excluded)
*30-run statistical verification gate ([`crates/nf-engine/src/bin/hft_bench.rs`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-engine/src/bin/hft_bench.rs)) on `x86_64-unknown-linux-musl`, isolating raw sequencer and transport mechanics without downstream verification hash overhead.*

| Ingest Mode | Median Latency | p95 Latency | p99 Tail | StdDev (CV%) | Rate Equivalent |
|---|---|---|---|---|---|
| **Classic Ingest** (`CountSink`, per-msg callback) | **`4.41 cyc/msg`** (1.80 ns) | **`4.51 cyc`** | **`4.55 cyc`** | 0.049c (1.12%) | **`553.96M msg/s`** |
| **Span Ingest** (`SpanCountSink`, $O(1)$ batch span) | **`1.77 cyc/msg`** (0.72 ns) | **`1.89 cyc`** | **`1.91 cyc`** | 0.055c (3.12%) | **`1.38 Billion msg/s`** |

---

## 2. Technical Evolution & Architectural Decisions

```
[Baseline Campaign] ──────► [TITAN Program (Single-Core)] ──────► [HYDRA Program (Multi-Core)]
    ~24.4M msg/s                 235M–259M msg/s                      561M–603M+ msg/s
(Harness Hash Trap)            (Span Protocol + Memo)              (Lock-Free Chunked Fabric)
```

### 1. The 25M/s Baseline & The Hash Latency Trap (H10)
Early benchmarks showed ~24.4M msg/s. A forensic stage-ectomy decomposition ([`docs/artifacts/tail-study/study-report.md`](file:///data/data/com.termux/files/home/HFT-Proj/docs/artifacts/tail-study/study-report.md)) revealed:
* **The IMUL Serial Dependency Trap**: The benchmark sink used FNV-1a-64, which is a serial multiply-accumulate dependency chain. It ran at x86 `imul` latency (~3.7 cyc/byte across 29 bytes $\approx$ 107 cyc) rather than uop port throughput.
* **Finding**: Over 89% of measured latency was test-harness hash latency. The pure engine core ([`CountSink`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-engine/src/bin/bench.rs)) was already executing in **~26.5 cycles (~86.8M msg/s)**.

---

### 2. The TITAN Program (Single-Core: 25M $\to$ 250M+ msg/s)
To optimize the deterministic single-core replay path, five techniques were introduced ([`docs/19-titan.md`](file:///data/data/com.termux/files/home/HFT-Proj/docs/19-titan.md)):
* **R1 (Harness Modernization)**: Reused warm pre-rendered transport memory pages (eliminating in-window page fault churn) and adopted Q1 precomputed block indexing.
* **R2 (Verdict Memoization / `FrameMemo`)**: Precomputed ITCH validation verdicts at transport construction time over immutable frame bytes, dropping classic validation latency to **4.41 cyc/msg**.
* **R3 (Span Protocol & Closed-Form Emission)**: Proved that contiguous valid message sequences follow the constant state transition $(w, count) \to (w+n, count+n)$, collapsing $O(n)$ per-message callbacks into $O(1)$ batch span dispatch via [`Sink::on_span`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-arbitrator/src/types.rs).
* **R4 (8-Lane Hardware CRC32C & DLP Prefetching)**: Interleaved 8 CRC32C accumulators to saturate hardware execution ports at ~8 bytes/cycle, and added `_mm_prefetch(T0)` in `poll()` to resolve memory-level parallelism (MLP) stalls.

---

### 3. The HYDRA Program (Multi-Core Fabric: 250M $\to$ 600M+ msg/s)
Single-core in-window verification hit a physical instruction-set barrier:
* **The Single-Core Ceiling (H1)**: The x86 `crc32` hardware instruction sustains at most 8 bytes/cycle throughput. With average span bodies of ~31.65 bytes, the absolute single-core verification floor is **~3.96 cyc/msg ($\approx 617\text{M msg/s}$ ceiling at 2.45 GHz even with a 0-cycle sequencer)**.
* **The HYDRA Architecture ([`crates/nf-testkit/src/hydra.rs`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-testkit/src/hydra.rs))**:
  * **Purity vs. Serial Ordering Split**: Computing `span_crc32c_8lane(body)` is a *pure function* of immutable span bytes (can run in parallel across worker cores). The running fold `h ← (rotl(h,13) ^ v_i) * K` is serial in emission order, but lightweight $O(1)$ (~0.16 cyc/msg) and remains on the main core.
  * **Chunked SPSC Handoff (H4 — Anti-Ping-Pong Law)**: Naive per-span SPSC handoffs caused a 3.2x regression (138M vs 264M) due to cross-core cache line bouncing. HYDRA groups descriptors into **16-span ring-aligned chunks** with single atomic Release/Acquire fences, collapsing cross-core traffic by >10x.
  * **Worker Prefetch Pipeline (H5)**: Workers prefetch span body starts 4 spans ahead, while the main thread disables redundant body prefetching.
  * **3-Layer Bit-Parity Law**: An untimed sequential reference pass pins `(count, hash, msg_hash)`; HYDRA asserts bit-exact match against the reference on every single pass (`HYDRA_BITPARITY ... -> BIT-EXACT`).

---

## 3. Crate Topology

```
crates/
├── nf-protocol/       # Wire format parsers, ITCH 5.0, MoldUDP64, and quality gates
├── nf-arbitrator/     # Zero-alloc sequencer, watermark tracking, gap state machine
├── nf-engine/         # Replay harness, TSC calibration, static histograms, benchmarks
├── nf-transport/      # Pre-rendered replay transport & AF_XDP kernel-bypass socket
├── nf-recovery/       # TCP retransmission client for gap filling
└── nf-testkit/        # 17-cell matrix sweep, differential oracle, HYDRA fabric, fuzz harness
```

---

## 4. Key Invariants & Guarantees

* **Zero Allocation Window**: Zero dynamic memory allocation during ingest (`ALLOC_DELTA = 0`), enforced in CI.
* **Affine Token Security**: Downstream consumers receive a non-cloneable, unforgeable [`LiveFeedProof`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-arbitrator/src/types.rs) on every message, guaranteeing that out-of-order or corrupt data cannot reach execution engines.
* **Deterministic Conformance**: Produces bit-exact golden hash `0xF6EF154EFDE905D8` across all 17 matrix test configurations.
* **Invariant TSC Timing**: Sub-nanosecond time stamping with hardware `rdtscp` calibrated via Theil-Sen regression against `CLOCK_MONOTONIC_RAW`.

---

## 5. Quick Start & Verification

### Build Workspace
```bash
# Optimized release build
RUSTFLAGS="-C target-cpu=native" cargo build --workspace --release
```

### Run Benchmarks
```bash
# Run HYDRA multi-core benchmark (burst & 5s sustained)
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --hydra-only --runs 7

# Run single-core TITAN benchmark & stage-ectomy study
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --runs 5 --study

<<<<<<< HEAD
# Run 30-run statistical verification gate (hft_bench)
cargo run --release -p nf-engine --bin hft_bench -- --sample data/tests/sample-mini.itch --runs 30 --warmup 5
=======
# Run the R6 HYDRA multi-core arms (UNPINNED — the fabric spans all vCPUs)
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --hydra-only --runs 7
# Knobs: HFT_HYDRA_WORKERS=<N> (0 = inline/sequential mode); HFT_CRC_KERNEL=scalar|fold512 (span-CRC kernel override, values identical by D11); HFT_HYDRA_NULL=1 (pipeline-overhead diagnostic ONLY — disables parity asserts, never in CI)
>>>>>>> 8c2897e (feat(r7): GIGAHFT gates + D11 CRC-kernel differential oracle + docs)
```

### Run Full CI Suite
```bash
./scripts/ci.sh
```

---

## 6. Claims Scope (Honesty Split)

Per project policy ([`docs/11-bench.md §1`](file:///data/data/com.termux/files/home/HFT-Proj/docs/11-bench.md)):
* **What is claimed**: Single-core deterministic replay throughput ($\ge 200\text{M msg/s}$ verified, $\ge 500\text{M msg/s}$ raw classic ingest), multi-core parallel verification fabric throughput ($\ge 550\text{M–600M+ msg/s}$ verified), zero heap allocations in the ingest loop, and bit-exact golden hash verification.
* **What is NOT claimed**: This is not an FPGA hardware feed handler, not real-NIC kernel bypass zero-copy hardware, and does not make unsubstantiated marketing comparisons against dedicated hardware appliances.
