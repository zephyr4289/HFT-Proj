# Mission Directive: Engineer 2 — Worker Microarchitecture & SIMD Kernel Architect

**Classification:** High-Frequency Trading Ultra-Low-Latency Engineering  
**Role:** Lead Worker Kernel & Microarchitecture Specialist  
**Domain:** Worker Compute Loop, Vector Fold Engine, Ending Chain Pipeline, Memory Pre-fetch  
**Target Objective:** **$+15\% - 20\%$ Worker Evaluation Throughput** (Eliminate the 26% serial ending latency blob to push Sustained Verification toward **$\mathbf{\ge 2.0\text{ Billion msg/s}}$**)  
**Assigned Branch:** `feat/worker-endpipe-2b`  
**Standing Baseline:** `1,331,355,913 msg/s` (36.82 GB/s delivered CRC, 0 allocs, Shard 44)

---

## 1. Sandbox Environment & GitHub Setup

You are operating in an independent, isolated sandbox environment. You have full ownership of your assigned domain and full authorization to make any algorithmic, SIMD kernel, or assembly-level decisions.

### 1.1 Git Authentication & Clone
Configure your sandbox environment with the provided project credentials:

```bash
# Set Git Identity
git config --global user.name "zephyr4289"
git config --global user.email "zephyr4289@gmail.com"

# Clone Repository (Use your provided PAT token)
# export GITHUB_TOKEN="<YOUR_PROVIDED_PAT_TOKEN>"
git clone https://${GITHUB_TOKEN}@github.com/zephyr4289/HFT-Proj.git
cd HFT-Proj

# Create and switch to your designated branch
git checkout -b feat/worker-endpipe-2b origin/main
```

### 1.2 Remote Push Authentication
When pushing branches or triggering CI runs, use the authenticated origin:
```bash
git push https://${GITHUB_TOKEN}@github.com/zephyr4289/HFT-Proj.git feat/worker-endpipe-2b
```

---

## 2. Required Context & Mandatory Reading

Before modifying code, review the codebase and architectural history left by previous engineers to avoid re-deriving already refuted dead ends:

1. **`ENGINEERING_BLUEPRINT_2B_6B.md`**: Master blueprint for the 6.0B Ingest / 2.0B Sustained mission.
2. **`docs/23-kernel-refactor.md` & `docs/28-r15-vtail.md`**: Vector fold kernel evolution, p5 uop census, and vtail absorption.
3. **`docs/challenge/ROADMAP3.md`**: Comprehensive ledger of all past breakthroughs, physical constraints, and the "Do-Not-Do" list (§6).
4. **`docs/challenge/CHECKLIST.md`**: 9 non-negotiable compliance rules (bit-exact parity, zero-allocation, full verification).
5. **Key Implementation Files:**
   - `crates/nf-testkit/src/hydra.rs`: Worker thread drain loop and chunk execution.
   - `crates/nf-testkit/src/crcfold.rs`: AVX-512 `VPCLMULQDQ` reflection kernel.
   - `crates/nf-testkit/src/kbench.rs`: Memory bandwidth and micro-benchmarking harnesses.

---

## 3. Domain Ownership & Boundaries

You have **exclusive write ownership** over the following crates and modules:
- `crates/nf-testkit/src/hydra.rs` (Worker drain loop & batch processing)
- `crates/nf-testkit/src/crcfold.rs` (Vector fold steps, tail absorptions, ending pipelines)
- `crates/nf-testkit/src/kbench.rs` (Ending-only and kernel micro-benchmarks)

### Frozen Struct Contracts (Do Not Break)
To guarantee seamless 3-way integration with Engineer 1 and Engineer 3:
- The worker input format remains the standard 16-span SPSC chunk.
- **Bit Parity Invariance:** Golden hashes (`0x881639cead506f25` and `0xF6EF154EFDE905D8`) must remain 100% bit-exact across all differential test suites.
- **Zero Allocations:** No heap allocation (`malloc`/`Box`/`Vec`) is permitted on any message hot path.

---

## 4. Technical Objective & Engineering Vectors

Your mission is to eliminate the **26% serial ending latency tax** and optimize memory pre-fetch to drive sustained verification from $1.33\text{B msg/s}$ to $\mathbf{2.0\text{B msg/s}}$.

### Priority 1: Software-Pipelined Chunk Drain (`HFT_ENDPIPE`) — *The Primary Lever*
- **Problem:** Inside each 16-span chunk, workers spend $\sim 45 - 50\text{ cycles/span}$ on the serial ending chain:
  - 16 serial `crc32` tail continuation steps ($3\text{ cycles}$ latency each on Port 1).
  - 8-lane FNV-1a-64 `imul` combine chain ($3\text{ cycles}$ latency each on Port 1).
- **Solution:**
  - Because all 16 spans in a chunk are mutually independent, restructure the drain loop into a **4-stage software pipeline** (`ILP = 4`):
    - *Stage 1*: Vector fold for Spans $k+2, k+3$ (`VPCLMULQDQ` on Port 5).
    - *Stage 2*: Tail `crc32` chains for Span $k+1$ (Port 1).
    - *Stage 3*: `imul` FNV combines for Span $k$ (Port 1).
- **Target:** Completely overlaps serial ending latency with vector math, saving **$25 - 30\text{ cyc/span}$ ($+13\% - 16\%$ worker throughput)**.
- **Feature Flag:** Gate under `HFT_ENDPIPE=1`.

### Priority 2: Memory-Level Parallelism (MLP) Prefetch Deepening
- **Problem:** At $> 50\text{ GB/s}$ streaming throughput, CPU Line Fill Buffers (LFBs) experience micro-stalls if prefetch is only 4 spans ahead ($1.1\text{ KB}$).
- **Solution:**
  - Sweep prefetch lookahead from 4 spans to **$8 - 12\text{ spans}$ ($2.2 - 3.3\text{ KB}$)** to keep 10+ memory requests active concurrently.
- **Target:** Eliminates memory stalls on strong-band Xeon hosts ($34+\text{ GB/s}$).

### Priority 3: Ending-Only Micro-Benchmark Harness
- Add a dedicated `kbench` row comparing serial vs pipelined endings on 16-span chunks to verify theoretical gains on target silicon before full-system execution.

---

## 5. Development, CI Iteration & Git Protocol

### 5.1 Commit Discipline (Never Purge History in Bulk)
- **Do not make massive monolithic commits.**
- Make atomic, meaningful commits explaining the exact microarchitectural rationale:
  - `feat(hydra): implement 4-stage software-pipelined chunk drain (HFT_ENDPIPE)`
  - `perf(crcfold): overlap lane-0 tail crc32 with vpclmulqdq fold`
  - `bench(kbench): add serial vs pipelined ending micro-benchmark row`
- For intermediate/WIP documentation commits where CI is unnecessary, append `[skip ci]`.
- For benchmark validation pushes, omit `[skip ci]` to trigger the full 80-shard CI fleet.

### 5.2 Leveraging the 80-Shard CI Fleet
Our GitHub Actions CI runs an **80-shard continuous saturation queue** (20 slots max parallel):
- As non-target AMD runners fast-discard in $<1\text{s}$, your push immediately acquires **10–15 certified Intel Xeon hosts (`8573C` / `8370C`)** in ~10 minutes.
- When CI completes, it automatically aggregates results and pushes consolidated logs to the `origin/build-log` branch.

### 5.3 Fetching & Inspecting CI Results via REST / Git
To inspect the latest fleet benchmark output from your sandbox:
```bash
# Fetch latest build-log branch
git fetch origin build-log

# Check recent CI run logs
git show origin/build-log:ci-logs/run-metadata.txt

# Inspect sustained verification & worker diagnostics for a specific shard
git show origin/build-log:ci-logs/draw-shard44.log | grep -E "PR1_HYDRA|DIAG sustained|DIAG worker"
```

---

## 6. Documentation & Verification Deliverables

You have complete authority over your implementation. With this freedom comes the responsibility to document your findings:
1. **Document your derivations**: Maintain an engineering log in `docs/31-r19-worker-endpipe.md` describing your pipeline stages, register assignments, cycle savings per span, and benchmark results.
2. **Run local sanity & differential tests** before pushing:
   ```bash
   cargo test --release --target-dir /tmp/cargo-build
   ./scripts/ci.sh
   ```
3. **Submit PR**: When your branch demonstrates verified worker speedup with bit-exact golden parity, open a PR from `feat/worker-endpipe-2b` to `main`.
