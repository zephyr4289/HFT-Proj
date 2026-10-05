# Mission Directive: Engineer 1 — Vector Ingest & AVX-512 Parser Architect

**Classification:** High-Frequency Trading Ultra-Low-Latency Engineering  
**Role:** Lead Ingest & SIMD Parsing Architect  
**Domain:** Protocol Ingestion, Frame Boundary Scanning, SIMD Message Descriptors  
**Target Objective:** **$\mathbf{\ge 6.0\text{ Billion msg/s}}$ Pure Ingest** ($\mathbf{\le 0.383\text{ cycles/message}}$ @ 2.30 GHz)  
**Assigned Branch:** `feat/ingest-6b-simd`  
**Standing Baseline:** `5.455B msg/s` (0.51 cyc/msg @ 2.79 GHz, Shard 24) / `5.286B msg/s` (0.44 cyc/msg @ 2.30 GHz, Shard 15)

---

## 1. Sandbox Environment & GitHub Setup

You are operating in an independent, isolated sandbox environment. You have full ownership of your assigned domain and full authorization to make any architectural, algorithmic, or assembly-level decisions.

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
git checkout -b feat/ingest-6b-simd origin/main
```

### 1.2 Remote Push Authentication
When pushing branches or triggering CI runs, use the authenticated origin:
```bash
git push https://${GITHUB_TOKEN}@github.com/zephyr4289/HFT-Proj.git feat/ingest-6b-simd
```

---

## 2. Required Context & Mandatory Reading

Before modifying code, review the codebase and architectural history left by previous engineers to avoid re-deriving already refuted dead ends:

1. **`ENGINEERING_BLUEPRINT_2B_6B.md`**: Master blueprint for the 6.0B Ingest / 2.0B Sustained mission.
2. **`docs/29-r16-double-helix.md`**: Double Helix pure ingest architecture and Front A benchmark specification.
3. **`docs/challenge/ROADMAP3.md`**: Comprehensive ledger of all past breakthroughs, physical constraints, and the "Do-Not-Do" list (§6).
4. **`docs/challenge/CHECKLIST.md`**: 9 non-negotiable compliance rules (bit-exact parity, zero-allocation, full verification).
5. **Key Implementation Files:**
   - `crates/nf-protocol/src/gates.rs`: ITCH message parsing & frame stride scanning.
   - `crates/nf-protocol/src/parser.rs` / `itch.rs`: Header layout and packet schemas.
   - `crates/nf-engine/src/bin/hft_bench.rs`: Front A benchmark harness & span arm logic.

---

## 3. Domain Ownership & Boundaries

You have **exclusive write ownership** over the following crates and modules:
- `crates/nf-protocol/` (All parser, gates, and ITCH extraction logic)
- `crates/nf-engine/src/bin/hft_bench.rs` (Pure ingest / Front A benchmark arms)

### Frozen Struct Contracts (Do Not Break)
To guarantee seamless 3-way integration with Engineer 2 and Engineer 3:
- The output format must produce standard `Desc8` (8-byte descriptor) or `Span` (16-byte bounds).
- **Bit Parity Invariance:** Golden hashes (`0x881639cead506f25` and `0xF6EF154EFDE905D8`) must remain 100% bit-exact across all differential test suites.
- **Zero Allocations:** No heap allocation (`malloc`/`Box`/`Vec`) is permitted on any message hot path.

---

## 4. Technical Objective & Engineering Vectors

Your mission is to shave the remaining **$0.06 - 0.08\text{ cycles/message}$** to break through **6.0 Billion msg/s**.

### Priority 1: AVX-512 Masked Vector Frame Stride Scan
- **Problem:** Currently, walking through ITCH frame boundaries parses 2-byte packet length headers via scalar pointer math.
- **Solution:** 
  - Load 64 bytes of packet payload into AVX-512 `zmm` registers.
  - Extract all packet length delimiters across 16–28 messages simultaneously using `_mm512_mask_compress` and compute the running message offsets with SIMD prefix-sum (`vpaddd` / `vpermd`).
- **Target:** Drops frame walk latency from $0.18\text{ cyc/msg}$ to $\le 0.06\text{ cyc/msg}$.

### Priority 2: Fused SIMD Permutation Descriptor Extraction
- **Problem:** Unpacking `msg_type`, `stock_locate`, `tracking_num`, and `timestamp` into Double Helix span descriptors creates register spill pressure and instruction port contention.
- **Solution:**
  - Fuse the entire ITCH header unpack into a single 512-bit permutation table (`_mm512_permutexvar_epi8`).
  - Assemble complete 16-byte descriptors directly into AVX-512 register pairs without a single scalar branch.
- **Target:** Shaves $0.05\text{ cyc/msg}$ of instruction decode overhead.

### Priority 3: Sharded Dual RX Pipelines (If single-thread decode saturates)
- If single-thread instruction decode limits Golden Cove at ~5.7B msg/s, implement dual-threaded lock-free RX ingestion across alternating frame buffers.

---

## 5. Development, CI Iteration & Git Protocol

### 5.1 Commit Discipline (Never Purge History in Bulk)
- **Do not make massive monolithic commits.**
- Make atomic, meaningful commits explaining the exact microarchitectural rationale:
  - `feat(protocol): implement avx512 vector prefix-sum frame scanner`
  - `perf(parser): vectorize itch header unpack with permuted zmm shuffle`
  - `bench(ingest): add micro-benchmark for 64-byte frame batching`
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

# Inspect benchmark output for a specific shard
git show origin/build-log:ci-logs/draw-shard15.log | grep -E "PR1_|HFT_BENCH|KBENCH"
```

---

## 6. Documentation & Verification Deliverables

You have complete authority over your implementation. With this freedom comes the responsibility to document your findings:
1. **Document your derivations**: Maintain an engineering log in `docs/30-r18-vector-ingest.md` describing your SIMD layouts, instruction port distributions, and benchmark results.
2. **Run local sanity tests** before pushing:
   ```bash
   cargo test --release --target-dir /tmp/cargo-build
   ./scripts/ci.sh
   ```
3. **Submit PR**: When your branch breaks $\ge 6.0\text{B msg/s}$ with bit-exact golden parity, open a PR from `feat/ingest-6b-simd` to `main`.
