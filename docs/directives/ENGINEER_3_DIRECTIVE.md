# Mission Directive: Engineer 3 — Transport Fabric & CI Fleet Systems Architect

**Classification:** High-Frequency Trading Ultra-Low-Latency Engineering  
**Role:** Lead Transport Fabric & CI Fleet Systems Architect  
**Domain:** Transport Ring Submission, Memory Fabric & Cache Hygiene, NUMA Affinity & CI Fleet Automation  
**Target Objective:** **Main-Thread Submission Overhead $\mathbf{\le 0.18\text{ cycles/message}}$** (via 512-bit wide descriptor commits) + Multi-Feature CI Fleet Matrix Orchestration  
**Assigned Branch:** `feat/fabric-widedesc-fleet`  
**Standing Baseline:** Main submission overhead at $0.40 - 0.60\text{ cyc/msg}$ (scalar 8-byte stores)

---

## 1. Sandbox Environment & GitHub Setup

You are operating in an independent, isolated sandbox environment. You have full ownership of your assigned domain and full authorization to make any memory transport, cache architecture, thread pinning, or CI fleet optimization decisions.

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
git checkout -b feat/fabric-widedesc-fleet origin/main
```

### 1.2 Remote Push Authentication
When pushing branches or triggering CI runs, use the authenticated origin:
```bash
git push https://${GITHUB_TOKEN}@github.com/zephyr4289/HFT-Proj.git feat/fabric-widedesc-fleet
```

---

## 2. Required Context & Mandatory Reading

Before modifying code, review the codebase and architectural history left by previous engineers to avoid re-deriving already refuted dead ends:

1. **`ENGINEERING_BLUEPRINT_2B_6B.md`**: Master blueprint for the 6.0B Ingest / 2.0B Sustained mission.
2. **`docs/29-r16-double-helix.md`**: Double Helix memory fabric, SPSC ring topology, and why large descriptor arrays failed in L2/L3 (§2.4).
3. **`docs/challenge/ROADMAP3.md`**: Comprehensive ledger of past breakthroughs, physical constraints, and the "Do-Not-Do" list (§6).
4. **`docs/challenge/CHECKLIST.md`**: 9 non-negotiable compliance rules (bit-exact parity, zero-allocation, full verification).
5. **Key Implementation Files:**
   - `crates/nf-transport/src/lib.rs`: SPSC queue submission, descriptor layouts, buffer management.
   - `crates/nf-transport/src/rxdesc.rs`: Descriptor array protocols and telemetry.
   - `crates/nf-testkit/src/affinity.rs`: Pinned CPU affinities and multi-core layout.
   - `.github/workflows/ci.yml`: 80-shard continuous saturation queue and automated scoreboard.

---

## 3. Domain Ownership & Boundaries

You have **exclusive write ownership** over the following crates and modules:
- `crates/nf-transport/` (SPSC ring submission, buffer pools, descriptor structures)
- `crates/nf-testkit/src/affinity.rs` (Core topologies, NUMA pinning)
- `.github/workflows/ci.yml` (CI orchestration, matrix strategies, build logs)

### Frozen Struct Contracts (Do Not Break)
To guarantee seamless 3-way integration with Engineer 1 and Engineer 2:
- `Desc8` (8 bytes) and `Chunk16` (128 bytes) interface definitions are locked.
- Provide clean, zero-overhead SPSC queue primitives consumed by Engineer 1 (producer) and Engineer 2 (worker consumer).
- **Bit Parity Invariance:** Golden hashes (`0x881639cead506f25` and `0xF6EF154EFDE905D8`) must remain 100% bit-exact across all differential test suites.
- **Zero Allocations:** No heap allocation (`malloc`/`Box`/`Vec`) is permitted on any message hot path.

---

## 4. Technical Objective & Engineering Vectors

Your mission is to eliminate producer-side submission bottlenecks, guarantee zero cache pollution, and automate multi-branch matrix CI testing.

### Priority 1: Wide-Store Descriptor Packing on Main (`HFT_DESC_WIDE`)
- **Problem:** The main producer thread currently executes sixteen individual 8-byte scalar stores per chunk + boundary checks, consuming $0.40 - 0.60\text{ cyc/msg}$.
- **Solution:**
  - Stage 16 `Desc8` descriptors into a 128-byte stack buffer.
  - Commit the entire 16-span chunk to the SPSC ring using **two 512-bit `vmovdqu64` streaming stores**.
  - Make ring space boundary checks once per chunk (every 16 spans) instead of per span.
- **Target:** Drops main-thread submission overhead from $0.60\text{ cyc/msg}$ to $\mathbf{\le 0.18\text{ cyc/msg}}$, lifting main throughput capacity to $> 2.5\text{B msg/s}$.
- **Feature Flag:** Gate under `HFT_DESC_WIDE=1`.

### Priority 2: Cache Residency & SPSC Ring Footprint Invariance
- **Law:** At $> 50\text{ GB/s}$, the 15 MB ITCH test corpus must stay LLC-resident. Every extra byte cycling through L2/L3 directly steals fold memory bandwidth.
- **Solution:**
  - Strictly enforce the **$16\text{ KB/lane}$ L1-resident SPSC descriptor ring**.
  - Reject large multi-megabyte descriptor structures that dilute CPU cache lines.

### Priority 3: CI Fleet Matrix & Multi-Flag Permutation Testing
- Update `.github/workflows/ci.yml` to automatically test feature permutations across acquired Xeon runners:
  - Baseline (Ring + Distinct)
  - `HFT_ENDPIPE=1` (Engineer 2's pipelined drain)
  - `HFT_DESC_WIDE=1` (Engineer 3's wide commit)
  - `HFT_VEC_INGEST=1` (Engineer 1's vector stride scan)
  - Full Synergy (`HFT_ENDPIPE=1 HFT_DESC_WIDE=1 HFT_VEC_INGEST=1`)
- Ensure the Python aggregator automatically extracts and highlights all feature flags in the GitHub summary scoreboard.

---

## 5. Development, CI Iteration & Git Protocol

### 5.1 Commit Discipline (Never Purge History in Bulk)
- **Do not make massive monolithic commits.**
- Make atomic, meaningful commits explaining the exact microarchitectural rationale:
  - `feat(transport): implement 512-bit wide-store descriptor chunk commit (HFT_DESC_WIDE)`
  - `perf(affinity): refine SMT distinct core pinning for worker thread isolation`
  - `ci: add automated multi-arm feature matrix testing to 80-shard fleet`
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

# Inspect main-thread submission & fabric diagnostics for a specific shard
git show origin/build-log:ci-logs/draw-shard44.log | grep -E "DIAG rx|ADVANCE_DIAGNOSTIC|PR1_HYDRA"
```

---

## 6. Documentation & Verification Deliverables

You have complete authority over your implementation. With this freedom comes the responsibility to document your findings:
1. **Document your derivations**: Maintain an engineering log in `docs/32-r20-fabric-widedesc.md` describing your descriptor staging layouts, cache miss measurements, and CI matrix results.
2. **Run local sanity & differential tests** before pushing:
   ```bash
   cargo test --release --target-dir /tmp/cargo-build
   ./scripts/ci.sh
   ```
3. **Submit PR**: When your branch demonstrates verified main-thread speedup and robust fleet automation, open a PR from `feat/fabric-widedesc-fleet` to `main`.
