# 🛠️ Engineering Directive: Fused In-Register Vector Verification & The 2.5B+ Frontier
**Target Baseline:** `main` branch | **Target Milestone:** $\ge 2.5\text{B – }3.0\text{B msg/s Sustained Full-Verify}$

---

## 1. Executive Briefing & Context

Our 2,000-shard empirical cloud silicon census ([`docs/SILLICON_DATA.md`](../SILLICON_DATA.md)) and 120-shard CI runs ([`docs/SILLICON_DATA.md`](../SILLICON_DATA.md)) have established our current production baseline:

* **Pure Ingest Ceiling:** **`12.103 Billion msg/s`** (0.2145 clock cycles/msg) on `AMD EPYC 9V45` (Zen 5 @ 4.56 GHz).
* **Sustained Full-Verify Ceiling:** **`1,959,208,315 msg/s`** (**1.959B msg/s**) with **100% bit-exact parity** (`0x881639cead506f25`) and **0 heap allocations**.

### The Engineering Challenge
Pure ingest parsing is already running at 12B/s. The bottleneck capping our end-to-end trading pipeline at 1.959B msg/s is the **downstream byte verification and cross-core memory handoff** (currently consuming ~62 GB/s of memory bandwidth to read payload bytes a second time for CRC32C / carryless hashing).

Your mission is to break the **2.5B–3.0B msg/s Sustained Full-Verify barrier**.

---

## 2. Repository & Remote Access

* **Repository Clone URL:**
  ```bash
  git clone https://github.com/zephyr4289/HFT-Proj.git
  cd HFT-Proj
  ```
* **Git Remote with Push Access:**
  ```bash
  # Authenticate with the provided Personal Access Token (PAT)
  git remote set-url origin https://<PAT_TOKEN>@github.com/zephyr4289/HFT-Proj.git
  ```
* **Branch Strategy:**
  1. Branch off the latest `main`:
     ```bash
     git checkout main && git pull origin main
     git checkout -b feat/fused-vector-verify
     ```
  2. Open a Pull Request from `feat/fused-vector-verify` targeting `main`.

---

## 3. CI Workflow & Fast Feedback Loop

* **120-Shard Continuous Saturation CI:**
  * Every CI run launches a 120-shard matrix with `max-parallel: 20` and dynamic replenishment.
  * Non-target runners fast-discard in $< 1\text{s}$, guaranteed to capture **$\ge 15$ top-tier silicon nodes** (`AMD EPYC 9V45` Zen 5, `Intel Xeon Platinum 8573C`, `Intel Xeon 6973P-C`).
* **CI Push Conventions:**
  * Use `[skip ci]` in commit messages while iterating locally:
    ```bash
    git commit -m "wip: test 8-stream accumulator [skip ci]"
    ```
  * Push without `[skip ci]` when ready to run the 120-shard benchmark:
    ```bash
    git push origin feat/fused-vector-verify
    ```
* **Fetching Consolidated CI Logs:**
  CI automatically consolidates all target draws onto the `build-log` branch:
  ```bash
  git fetch origin build-log
  git show origin/build-log:ci-logs/bench_results.json
  git show origin/build-log:ci-logs/bench_hydra.txt
  git ls-tree origin/build-log:ci-logs/
  ```

---

## 4. Core Engineering Directives

### 🎯 Task 1: 8-Stream Parallel VPCLMUL Accumulator Folding (`crates/nf-testkit/src/crcfold.rs`)
* **Problem:** Serial carryless multiplication dependency chains introduce a 3–4 cycle latency floor, limiting Intel Xeon folding to ~32 GB/s.
* **Solution:**
  * Refactor `crcfold.rs` to maintain **8 independent parallel accumulators** (`ZMM0`–`ZMM7`):
    $$H_0 \leftarrow \text{Fold}(B_0), \quad H_1 \leftarrow \text{Fold}(B_1), \quad \dots, \quad H_7 \leftarrow \text{Fold}(B_7)$$
  * Fold 512 bytes per iteration before reducing the 8 accumulators into the final CRC with a logarithmic tree fold.
  * Target: Push memory folding throughput from 32 GB/s to **60+ GB/s** across all target CPUs.

### 🎯 Task 2: Fused Vector Ingest + In-Register VPCLMUL Folding
* **Problem:** Ingest parses frames $\to$ writes to memory $\to$ workers read payload bytes *a second time* from memory to compute CRC.
* **Solution:**
  * In [`crates/nf-engine/src/bin/hft_bench.rs`](../../crates/nf-engine/src/bin/hft_bench.rs) and [`crates/nf-testkit/src/hydra.rs`](../../crates/nf-testkit/src/hydra.rs), compute the `_mm512_clmulepi64_epi128` hash fold **while the 512-bit message chunk is already hot inside vector registers** during framing.
  * Eliminate the 2nd read pass entirely.

### 🎯 Task 3: Non-Temporal Streaming Stores for Handoff Descriptors
* **Problem:** Standard descriptor stores to the SPSC ring invalidate L1D/L2 cache lines for the order book.
* **Solution:**
  * Keep descriptor structs compact ($\le 64\text{ bytes}$—**never use fat 128B structs**).
  * Use 64-byte non-temporal streaming stores (`_mm_stream_si64` / `_mm512_stream_si512`) when publishing completed batches to the worker ring.

### 🎯 Task 4: Deterministic Physical-Core Pinning (`crates/nf-testkit/src/affinity.rs`)
* Upgrade topology capture in `affinity.rs` to dynamically verify that the `(Main Sequencer, RX Ingest Producer)` thread pair always lands on **distinct physical cores sharing L3**, locking in the **12B msg/s** (0.21 cyc/msg) mode on every run.

---

## 5. Non-Negotiable Invariants

1. **Bit-Exact Determinism:** Every draw must produce golden parity `0x881639cead506f25` / `0xF6EF154EFDE905D8`.
2. **Zero Heap Allocation:** `ALLOC_DELTA == 0` across all benchmarks and replay loops.
3. **Documentation:** Document architectural design in `docs/` with a numbered report (e.g. `docs/33-r21-fused-verify.md`).
