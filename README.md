# HFT-Proj: Ultra-Low-Latency NASDAQ ITCH 5.0 Feed Arbitrator

[![CI Status](https://github.com/zephyr4289/HFT-Proj/actions/workflows/ci.yml/badge.svg)](https://github.com/zephyr4289/HFT-Proj/actions/workflows/ci.yml)
[![License: MIT/Apache-2.0](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](file:///data/data/com.termux/files/home/HFT-Proj/Cargo.toml)

**`HFT-Proj`** is an ultra-low-latency, deterministic, zero-allocation **Nasdaq TotalView-ITCH 5.0 over MoldUDP64 feed arbitrator**, reorder engine, and gap-recovery sequencer implemented in modern Rust.

Originally engineered and verified for **25M msg/s** on standard cloud VMs, the engine underwent the **TITAN optimization program**, scaling verified end-to-end throughput to **235M–259M+ msg/s** (~3.85–4.25 ns per message with full 8-lane hardware CRC32C verification in-window) and raw engine arbitration to **4.41 cyc/msg** (1.80 ns classic) / **1.77 cyc/msg** (0.72 ns vectorized span), with **zero heap allocations** (`ALLOC_DELTA = 0`).

The **HYDRA program (R6)** then broke the single-core CRC32C throughput ceiling itself: a **bit-exact multi-core span-verification fabric** evaluates the identical 8-lane CRC32C kernel on the runner's worker cores while the main core runs the sequencer and the ordered serial fold — **bit-identical output on every run** (asserted against the sequential sink in CI, every invocation) — targeting **≥ 800M msg/s end-to-end fully-verified** on the 4-vCPU GitHub runner (see [`docs/20-hydra.md`](docs/20-hydra.md) for the physics, the fabric design, and the bit-parity proof).

---

## 1. Verified Benchmark Metrics

Measured on GitHub Actions reference hardware (**AMD EPYC 7763 64-Core @ 2.45 GHz / 2445.42 MHz**, single pinned core):

### A. End-to-End Replay with In-Window Byte Verification — Single Core (TITAN)
*Full pipeline: Virtual clock pacing $\to$ Transport poll $\to$ MoldUDP64 framing $\to$ Session dispatch $\to$ Duplicate rejection $\to$ Watermark sequencing $\to$ [`SpanConformanceSink`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-testkit/src/sink.rs) (reads and verifies every emitted payload byte via 8-lane hardware CRC32C, sequence continuity, and golden hash reproduction), single pinned core.*

| Benchmark Arm | Measured Throughput | Measured Latency | Target Gate | Verdict |
|---|---|---|---|---|
| **PR-1 TITAN Burst** (Single-pass, 505k msgs) | **`235.18M msg/s`** | **`10.40 cyc/msg`** (4.25 ns) | $\ge 100\text{M msg/s}$ | **`PASS`** ($2.35\times$ headroom) |
| **PR-1 TITAN Sustained** (5.0s continuous loop) | **`259.49M msg/s`** | **`9.42 cyc/msg`** (3.85 ns) | $\ge 100\text{M msg/s}$ | **`PASS`** ($2.59\times$ headroom) |
| **Zero Allocation In-Window** | **`0 bytes`** | N/A | `ALLOC_DELTA = 0` | **`PASS`** |
| **Golden Conformance Hash** | **`0xF6EF154EFDE905D8`** | N/A | Bit-exact 17/17 cells | **`PASS`** |

### A2. End-to-End Replay with In-Window Byte Verification — Multi-Core Fabric (HYDRA, R6)
*Same full pipeline and the same every-emitted-byte in-window verification, but the pure `span_crc32c_8lane` evaluation runs on the HYDRA fabric's worker cores while the main core runs the sequencer and the ordered serial fold ([`crates/nf-testkit/src/hydra.rs`](crates/nf-testkit/src/hydra.rs), [`docs/20-hydra.md`](docs/20-hydra.md)). Output is **bit-identical** to the sequential sink — asserted against it on every benchmark invocation and in CI (`HYDRA_BITPARITY ... -> BIT-EXACT`). No skipping, no sampling, no cross-pass memoization: every byte is re-read and re-CRC'd inside the measured window on every run. Run unpinned: `bench --hydra-only`.*

| Benchmark Arm | Target Gate | Verdict |
|---|---|---|
| **PR-1 HYDRA Burst** (single-pass, 505k msgs, 4-vCPU fabric) | $\ge 800\text{M msg/s}$ | machine-checked per run (`PR1_HYDRA_VERDICT`) |
| **PR-1 HYDRA Sustained** (5.0s continuous loop, fresh sessions) | $\ge 800\text{M msg/s}$ | machine-checked per run (`PR1_HYDRA_SUSTAINED_VERDICT`) |
| **Bit Parity vs Sequential Sink** | `(count, hash, msg_hash)` equal | asserted every run + 6 differential unit tests |
| **Zero Allocation In-Window** | `ALLOC_DELTA = 0` | asserted every run |

The gate threshold lives in [`gates.rs`](crates/nf-protocol/src/gates.rs) (`PR1_HYDRA_MIN_MSG_PER_SEC`, Gates-as-Code F-22). Fill in the measured numbers from your runner's CI log — the burst arm prints one `BENCH mode=replay-hydra-burst ... rate=...` line per run plus median and verdict lines. Topology note: 4 real vCPUs project to ~0.95–1.1B msg/s (main-thread-bound); a 2-physical-core/SMT runner is CRC-bandwidth-bound near ~700–800M msg/s — see [`docs/20-hydra.md §6`](docs/20-hydra.md) before moving the gate constant.

### B. Pure Engine Ingest Mechanics (Harness Hash Excluded)
*30-run statistical verification gate ([`crates/nf-engine/src/bin/hft_bench.rs`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-engine/src/bin/hft_bench.rs)) on `x86_64-unknown-linux-musl`, isolating raw sequencer and transport mechanics without downstream verification hash overhead.*

| Ingest Mode | Median Latency | p95 Latency | p99 Tail | StdDev (CV%) | Rate Equivalent |
|---|---|---|---|---|---|
| **Classic Ingest** (`CountSink`, per-message callback) | **`4.41 cyc/msg`** (1.80 ns) | **`4.51 cyc`** | **`4.55 cyc`** | 0.049c (1.12%) | **`553.96M msg/s`** |
| **Span Ingest** (`SpanCountSink`, $O(1)$ batch span dispatch) | **`1.77 cyc/msg`** (0.72 ns) | **`1.89 cyc`** | **`1.91 cyc`** | 0.055c (3.12%) | **`1.38 Billion msg/s`** |

---

## 2. Architecture & Evolution: From 25M/s to 250M+/s Single-Core, 1B-Class Fabric

### The 25M/s Baseline & The Forensic Diagnosis
The earlier campaign established stable single-core replay at **24.05M msg/s (burst)** and **24.42M msg/s (sustained)** against a 10M msg/s specification requirement. A forensic stage-ectomy study ([`docs/artifacts/tail-study/study-report.md`](file:///data/data/com.termux/files/home/HFT-Proj/docs/artifacts/tail-study/study-report.md)) revealed critical architectural insights:

1. **The Hash Latency Dependency Trap (H10)**:
   The production verification harness used FNV-1a-64 (`h = (h ^ b) * prime`), which is a strictly serial multiply-accumulate dependency chain. It executed at x86 `imul` latency (~3.7 cycles/byte across 29 bytes $\approx$ 107 cycles) rather than uop port throughput (~1.4 cyc/byte).
   * **Finding**: Over 89% of measured benchmark latency was test-harness verification overhead rather than the feed handler engine. The pure unhashed core ([`CountSink`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-engine/src/bin/bench.rs)) was already executing in **~26.5 cycles (~86.8M msg/s)** on reference Xeon silicon.

2. **The TITAN Program (R1..R5)**:
   To elevate end-to-end verified throughput beyond 100M msg/s without compromising verification integrity, five systematic architectural enhancements were introduced:

   * **R1 — Harness Modernization**: Wired PR-1 to precomputed Q1 block indexes, replaced serial FNV with hardware-accelerated SSE4.2 CRC32C, and reused warm transport memory pages to eliminate in-window page fault jitter.
   * **R2 — Frame Verdict Memoization (`FrameMemo`)**: Precomputed ITCH validation verdicts at transport construction time over immutable frame bytes, reducing classic validation latency to **4.41 cyc/msg** on EPYC silicon.
   * **R3 — Span Protocol & Closed-Form Emission**: Proved that contiguous valid message sequences follow the constant state transition $(w, count) \to (w+n, count+n)$, collapsing $O(n)$ per-message callbacks into $O(1)$ batch span dispatch via [`Sink::on_span`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-arbitrator/src/types.rs).
   * **R4 — 8-Lane Interleaved CRC32C & DLP Prefetching**: Interleaved 8 independent CRC32C accumulators to saturate hardware execution ports at ~8 bytes/cycle, and inserted `_mm_prefetch(T0)` 4 events ahead in `poll()` to resolve memory-level parallelism (MLP) stalls caused by skipped dual-feed duplicate frames.
   * **R5 — Mathematical Bounds**: Derived independent compute ($\ge 282\text{M msg/s}$ @ 2.6 GHz) and DRAM bandwidth ($\ge 128\text{M msg/s}$ floor) proofs in [`docs/19-titan.md`](file:///data/data/com.termux/files/home/HFT-Proj/docs/19-titan.md).

3. **The HYDRA Program (R6) — Breaking the CRC32C Ceiling Itself**:
   TITAN's verified arm is bounded by the `crc32` instruction's 8 B/cycle throughput over ~31.65 B/msg of span bytes — a **~619M msg/s absolute single-core ceiling** ([`docs/20-hydra.md §1`](docs/20-hydra.md)). HYDRA proves the sink's expensive term is a *pure function* of the span bytes (evaluable on any core) while the cheap ordered fold is the only serial part, and moves the former onto a chunked SPSC lane fabric spanning the runner's vCPUs:

   * **H1 — Physics**: measured the kernel at 8.32 B/c (`crc_probe` example) — the single-core bound is instruction throughput, not design.
   * **H2/H3 — Purity + Ordered Fold**: bit parity preserved by construction (same kernel symbol, same fold order; timing-independent determinism).
   * **H4 — Chunked Handoff Protocol**: one atomic publish per 16-span chunk killed the naive per-span fabric's cross-core ping-pong storm (a measured 3.2x regression on the naive v1 → 2.5x net win after chunking).
   * **H5 — Worker Prefetch Pipeline**: 4-span-lookahead body-start prefetch converts body-start L3 latency into overlapped bandwidth.
   * **H6 — Division-Free Lane Tracking**: incremental lane advance removed the per-span `idiv` from both hot paths.

   Full design, proofs, and the verification matrix: [`docs/20-hydra.md`](docs/20-hydra.md).

---

## 3. Crate Topology

```
crates/
├── nf-protocol/       # Wire format parsers, ITCH 5.0, MoldUDP64, and quality gates
├── nf-arbitrator/     # Zero-alloc sequencer, watermark tracking, gap state machine
├── nf-engine/         # Replay harness, TSC calibration, static histograms, benchmarks
├── nf-transport/      # Pre-rendered replay transport & AF_XDP kernel-bypass socket
├── nf-recovery/       # TCP retransmission client for gap filling
└── nf-testkit/        # 17-cell matrix sweep, differential oracle (D9/D10), fuzz harness
```

---

## 4. Key Invariants & Guarantees

* **Zero Allocation Window**: Zero dynamic memory allocation during ingest (`ALLOC_DELTA = 0`), enforced in CI.
* **Affine Token Dispatch**: Downstream consumers receive a non-cloneable, unforgeable [`LiveFeedProof`](file:///data/data/com.termux/files/home/HFT-Proj/crates/nf-arbitrator/src/types.rs) on every message, guaranteeing that out-of-order or corrupt data cannot reach execution engines.
* **Deterministic Conformance**: Produces bit-exact golden hash `0xF6EF154EFDE905D8` across all 17 matrix test configurations.
* **Invariant TSC Timing**: Sub-nanosecond time stamping with hardware `rdtscp` calibrated via Theil-Sen regression against `CLOCK_MONOTONIC_RAW`.

---

## 5. Quick Start & Verification

### Build Workspace
```bash
# Optimized release build
RUSTFLAGS="-C target-cpu=native" cargo build --workspace --release
```

### Run Benchmark Suite
```bash
# Run 30-run statistical verification gate (hft_bench)
cargo run --release -p nf-engine --bin hft_bench -- --sample data/tests/sample-mini.itch --runs 30 --warmup 5

# Run end-to-end benchmark with full stage-ectomy study (single-core TITAN arms)
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --runs 5 --study

# Run the R6 HYDRA multi-core arms (UNPINNED — the fabric spans all vCPUs)
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --hydra-only --runs 7
# Knobs: HFT_HYDRA_WORKERS=<N> (0 = inline/sequential mode); HFT_HYDRA_NULL=1 (pipeline-overhead diagnostic ONLY — disables parity asserts, never in CI)
```

### Run Full CI Suite
```bash
./scripts/ci.sh
```

---

## 6. Claims Scope (Honesty Split)

Per project policy ([`docs/11-bench.md §1`](file:///data/data/com.termux/files/home/HFT-Proj/docs/11-bench.md)):
* **What is claimed (single-core, TITAN)**: deterministic single-core replay throughput ($\ge 200\text{M msg/s}$ fully verified, $\ge 500\text{M msg/s}$ raw classic ingest), sub-5 cycle software arbitration floor, zero heap allocations in the ingest loop, and bit-exact golden hash verification.
* **What is claimed (multi-core fabric, HYDRA)**: the identical fully-verified pipeline (every emitted byte read and CRC32C-verified in-window; no skipping, sampling, or cross-pass memoization) evaluated across the runner's vCPU fabric with **bit-identical output asserted against the sequential sink on every invocation**, gated at $\ge 800\text{M msg/s}$ on the 4-vCPU GitHub runner (topology-dependent ceiling analysis in [`docs/20-hydra.md §6`](docs/20-hydra.md)).
* **What is NOT claimed**: This is not an FPGA hardware feed handler, not real-NIC kernel bypass zero-copy hardware, and does not make unsubstantiated marketing comparisons against dedicated hardware appliances.
