# GitHub Actions CI Architecture & Silicon Verification Guide

This directory contains the GitHub Actions workflow definition for the high-frequency trading (HFT) engine repository.

---

## 1. Overview & Problem Context

The core engine relies heavily on microarchitectural execution unit optimization, AVX-512 vector pipelines, and dual 512-bit `VPCLMULQDQ` carry-less multiplication units (e.g., Intel Xeon Platinum 8573C Sapphire Rapids / Emerald Rapids).

Standard GitHub-hosted cloud runners (`ubuntu-latest`) randomly draw from heterogeneous Azure VM host pools:
- **Intel Xeon Platinum 8573C** (Sapphire Rapids / Emerald Rapids — Dual 512-bit ports, target record silicon)
- **Intel Xeon Platinum 8370C** (Ice Lake — Single 512-bit clmul port, validated baseline)
- **AMD EPYC 7763 / 7B13 / Zen series** (Non-target for AVX-512 / VPCLMULQDQ peak records)

Running sequentially or manually pushing re-roll commits requires 15–30+ attempts to hit multiple target silicon draws.

---

## 2. The 10-Shard Fast-Discard Architecture

To maximize the probability of acquiring target silicon in a single run with zero wasted runner minutes, CI uses a **Fast-Discard Parallel Matrix + Aggregator** design.

```mermaid
flowchart TD
    Push([Commit Push / PR]) --> Matrix[10-Shard Parallel Matrix]
    
    subgraph ShardMatrix ["Parallel Silicon Probe (10 Shards)"]
        S1["Shard 1 (AMD EPYC)"] -->|Fast Gate (< 1s)| D1[Discard / Skip]
        S2["Shard 2 (Intel 8573C)"] -->|Target Match| R2[Build, Test & Benchmark]
        S3["Shard 3 (Intel 8370C)"] -->|Target Match| R3[Build, Test & Benchmark]
        S4["Shard 4 (AMD EPYC)"] -->|Fast Gate (< 1s)| D4[Discard / Skip]
        S5["Shard 5..10"] -->|Fast Gate (< 1s)| D5[Discard / Skip]
    end

    R2 -->|Upload Shard Chunk| Aggregator["Collector & Aggregator Job"]
    R3 -->|Upload Shard Chunk| Aggregator

    subgraph Aggregation ["Aggregator Phase"]
        Aggregator --> Summary["Render Live Scoreboard in $GITHUB_STEP_SUMMARY"]
        Aggregator --> SingleZip["Package 1 Clean Artifact: consolidated-silicon-draws"]
        Aggregator --> BuildLog["Update build-log Branch"]
    end
```

### Mathematical Probability Shift
With target acquisition probability $p \approx 0.20$ per runner:
$$P(\text{at least 1 target hit in 10 shards}) = 1 - (1 - 0.20)^{10} \approx \mathbf{89.3\%}$$
Multiple shards frequently hit target silicon simultaneously, providing immediate multi-draw replication (the "3-to-5 draw rule") in $< 3\text{ minutes}$ total wall-clock time.

---

## 3. Workflow Jobs Breakdown

### Phase 1: `filter-and-benchmark` (Matrix: 10 Shards)
1. **Step 0 — Fast-Discard Gate (`< 1s`)**:
   - Inspects `/proc/cpuinfo` for model substrings (`8573C` or `8370C`).
   - If non-matching, immediately sets `matched=false` and skips all subsequent steps.
   - Discarded jobs finish in $\sim 2\text{--}4\text{ seconds}$, consuming virtually no billing minutes.
2. **Step 1 — Full Build & Execution (Matching Shards Only)**:
   - Restores shared Rust Cargo cache via `Swatinem/rust-cache@v2`.
   - Runs `./scripts/ci.sh` executing differential suites D1–D12, `hft_bench`, `kbench`, `fbench`, and matrix sweeps.
   - Extracts key metrics (`sustained_rate`, `kbench_1t`, `bit_parity`, `allocs`) into `shard_meta.json`.
   - Uploads an ephemeral per-shard result chunk.

### Phase 2: `aggregate-and-report` (Collector)
1. **Downloads all shard result chunks**.
2. **Renders Live Scoreboard**: Populates `$GITHUB_STEP_SUMMARY` with a formatted Markdown table comparing all winning draws on the run summary page.
3. **Consolidates Artifacts**: Merges all draw logs into **1 single clean artifact bundle**:
   ```
   📦 consolidated-silicon-draws.zip
      ├── draw-shard2.log
      ├── draw-shard5.log
      ├── bench_hydra.txt
      ├── kbench.txt
      └── bench_results.json
   ```
4. **Updates `build-log` Branch**: Automatically syncs execution logs and run metadata to the orphan `build-log` branch with zero force-push race conditions.

---

## 4. Key Invariant Gates Enforced in Every Draw

Every successful silicon draw must pass all strict invariants:
- **Bit-Exact Golden Parity**: `HYDRA_BITPARITY == 0x881639cead506f25` / `0xF6EF154EFDE905D8`.
- **Zero Allocations**: `ALLOC_DELTA == 0` across all replay and benchmark windows.
- **Differential Oracle**: 100% pass across D1–D12 suites and 17-cell matrix sweep.
- **Clean Architecture Audits**: `#![forbid(unsafe_code)]` compliance on protocol/arbitrator and no mailbox allocations.

---

## 5. Local Emulation

To run the full suite locally on an x86_64 machine:
```bash
# Run complete test & verification suite
./scripts/ci.sh

# Run specific microbenchmarks
cargo run --release -p nf-engine --bin kbench
cargo run --release -p nf-engine --bin bench -- --hydra-only
```
