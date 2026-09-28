# Doc 21 — GIGAHFT: The 1-Billion msg/s Milestone (PR-1 GIGAHFT, R7)

## 1. Mandate

**Engineering Directive: Project 1.0B.** Take HYDRA R6 (603.31M msg/s
sustained on the 4-vCPU GitHub Actions runner — AMD EPYC 7763 / Intel Xeon
@ 2.45–2.60 GHz) to **≥ 1,000,000,000 messages/second sustained**, under
the unchanged foundational laws:

| Invariant | Enforcement |
|---|---|
| Zero heap allocations (`ALLOC_DELTA = 0`) | asserted by every measured arm; all rings/records are fixed-size, constructed at spawn |
| 100% in-window byte reading & verification (no skipping, no hash memoization) | workers read and CRC-verify every emitted byte of every span inside the measured window; nothing is memoized across passes |
| Bit-exact conformance (identical golden hash) | hydra == sequential `SpanConformanceSink` asserted every invocation; per-pass tuple asserts in the sustained arm; D1..D11 oracles |

The directive prescribed four levers. All four shipped — with two
engineering deviations forced by project law or silicon reality, documented
in §3 and §5.

## 2. Physics audit (what the cycle budget actually allows)

The directive's table targets a total effective cost of ≤ 2.00 cyc/msg at
2.45 GHz. Our independent audit agrees on the decomposition but sharpens
two numbers:

* **PCLMUL folding floor.** A 128-bit fold state consumes at most 16 bytes
  per step (2 × PCLMULQDQ + 2 × XOR). This is a hard structural limit of
  carry-less CRC folding: **8 bytes per PCLMULQDQ op**. On Zen 3
  (EPYC 7763) PCLMULQDDQ throughput is 1/cycle — the same 8 B/cycle as the
  scalar `crc32` chain, so on AMD silicon Lever 1 cannot beat the scalar
  kernel per core; its value there is eliminating the FNV tail and feeding
  the (worker-side) latency headroom. On AVX-512 + GFNI silicon (Ice Lake+,
  Sapphire Rapids, Zen 4) VPCLMULQDQ processes 4 clmuls per zmm
  instruction at 0.5c throughput, and the kernel runs at the issue limit.
* **The binding constraint at 603M was not the workers.** Three scalar
  workers at 8 B/cycle sustain 24 B/cycle ≈ 1.86B msg/s of CRC work —
  workers were never the ceiling. The ceiling was the main core
  (~2.2 cyc/msg of transport/framing/sequencing/submit/fold) plus the
  ~0.79 cyc/msg of end-of-pass tail serialization: with a blocking
  `finish()` per pass, the pipeline drained completely every pass —
  workers idled during the main's head-of-pass ramp, the main idled during
  the workers' tail. **Lever 4 (overlap) is therefore the dominant lever,
  not Lever 1.** This is the opposite emphasis of the directive's table,
  and it is what the measurements support.

## 3. The four levers as implemented

### Lever 1 — VPCLMULQDQ mirror-domain CRC32C fold (`crcfold.rs`)

A second, bit-exact implementation of `span_crc32c_8lane` computing the
same eight per-lane raw CRC32C values via carry-less polynomial folding:

* **Mirror-domain identity** (derived and verified symbolically in
  `scripts/crc32c_final_derive.py`): with X = the lane stream as a
  little-endian bit string, X̄ = its full bit-mirror, P = 0x11EDC6F41:
  `CRC32C_raw(X) = rev32(X̄ · y³² mod P)`. The division consumes X̄ from
  its top degree — i.e. front-to-back over the stream — so the fold needs
  **no length-dependent constants** (the little-endian domain would need
  `y^(−8n)` factors, an unbounded constant family).
* **Fold**: state V (deg < 128) starts at Ū₀ = rev128(stream[0..16)) and
  advances `V ← (V_hi ⊗ KP192) ⊕ (V_lo ⊗ KP128) ⊕ Ū_q` with just two
  constants (`KP128 = y¹²⁸ mod P = 0x18571d18`, `KP192 = y¹⁹² mod P =
  0x6503ea99`).
* **Ending — the elegant part**: because congruences mod P survive
  multiplication and XOR, and `V ≡ X̄_full (mod P)`, the final reduction
  collapses to **two chained hardware `crc32` instructions** over
  `rev64(V_hi)`, `rev64(V_lo)` plus the stream's last `r = len mod 16`
  bytes. No Barrett reduction, no advance-constant tables, no matrices.
* **Hardware mapping**: the eight lane-fold states live in two zmm
  registers (even/odd lane split); one iteration advances all eight lanes
  by one 16-byte unit using two strided 64 B loads (a MoldUDP64 block
  pair — qword k of a block IS lane k's word), two GFNI bit-reverse
  affines, two `vpunpckl/hqdq`, two `vpshufb` qword-swaps, and four
  VPCLMULQDQ. `eval2` interleaves two spans to hide clmul latency.
  Dispatch is CPUID-based (`CrcKernel::detect`, `HFT_CRC_KERNEL` override),
  evaluated once at fabric spawn — outside every window; the choice affects
  speed only (D11 proves value equality).
* **Measured** (Sapphire Rapids dev box, crc_probe): 1.55–1.62× per span
  vs the scalar kernel at representative 1360–1380 B bodies — the kernel is
  issue-bound at ~31 uops/256 B on that part; the directive's 0.65 cyc/msg
  worker figure is not reachable on any current silicon at 128-bit
  granularity, and the audit in §2 shows it is not needed.

**Deviation from the directive**: `fold128` (PCLMULQDQ 128-bit) and the
hybrid crc32/clmul kernel were analyzed and dropped: on Zen 3 both are
port-bound at the same 8 B/cycle as the scalar kernel (the hybrid only
wins if crc32 and pclmulqdq co-issue on disjoint ports — unverifiable
without the runner, and the workers have surplus headroom there anyway).

### Lever 2 — zero-copy in-place SPSC ring stores

`submit_span` writes the 16-byte `(ptr, len, span_id)` descriptor directly
into its ring slot with one unaligned 128-bit store — no staging buffer,
no flush copy loop. The whole-chunk space reservation moved to chunk start
(in-place writes can never touch worker-owned slots); `flush_pending`
collapses to a single Release store. The descriptor ring stays L1-local on
the main core.

### Lever 3 — fused hot-path header/session decode

`ingest_indexed` decodes seq/count inline and proves session equality via
two overlapping little-endian u64 template words + a `session_live` flag:
the `[u8;10]` materialization, `parse_header`, and the `session_dispatch`
comparison ladder run only on adoption/boundary (cold). Zero sessions
intentionally stay cold so `session_dispatch`'s re-adoption branch keeps
its exact observable semantics (counters included).

**Deviation from the directive**: the prescribed `_mm_loadu_si128 +
_mm_cmpeq_epi8 + _mm_movemask` formulation requires unsafe code;
`nf-arbitrator` is `#![forbid(unsafe_code)]` by law, so the fused decode
stays in safe Rust (LLVM fuses the fixed-index byte arrays into unaligned
loads — within ~1 uop of the SIMD sequence). The law wins over the
instruction selection.

### Lever 4 — cross-pass double-buffered fabric (the big one)

`HydraSpanSink` gains `begin_pass` / `end_pass` / `harvest_completed` and a
fixed 8-slot `PassRec` ring:

* Span ids are **global** across the sink's life. Each pass records its
  boundary; the ordered fold caps every batch at the pending boundary and
  snapshots the chain value exactly at the crossing, resetting to
  SPAN_SEED. Pass N+1's submission overlaps pass N's residual worker tail
  fold — **the fabric never drains mid-run**; the only blocking drain is
  the harness's final `finish()`, inside the measured window.
* `end_pass` publishes the partial chunk with one non-blocking Release
  store so workers start the tail immediately.
* `CHUNK` 16 → 64 and `WORKER_BATCH` 64 → 128: 4× fewer per-chunk atomic
  publishes; ring capacities unchanged (2048/4096 slots).
* **Fold drain every poll** (both arms): with CHUNK=64 one poll fills
  exactly one chunk. Draining every 8 polls left ~512 spans of result-ring
  backlog that throttled the entire pipeline through the worker's
  result-space check — fixing this alone moved the dev box's NULL-mode
  burst from 336M to ~450M msg/s.
* **Critical correctness discovery**: the flush trigger must be *global
  chunk completion* (`submit_rem == 0`), not `pending_len == CHUNK` — a
  chunk split across a pass boundary has a short window, and the next
  chunk must open its own window on its own lane with its own reservation.
  The wrong trigger produced wrong-lane routing and reservation overflow
  (fold-order violation + SIGSEGV), caught by the new
  `t_hydra_crosspass_overlap_bitparity` test.
* The sustained bench arm pins the per-pass `(count, hash, msg_hash)`
  tuple with an untimed reference pass and asserts it for **every**
  completed pass — stronger than R6's count-only assert.

## 4. Verification matrix

| Gate | What it proves |
|---|---|
| `t_fold_differential_exhaustive` (unit) | fold == scalar on lengths 0..=600 × 4 patterns, long sizes, random stress, mismatched `eval2` pairs |
| `t_gfni_bitrev_matrix` (unit) | the GFNI affine matrix pinned against silicon semantics (LLVM's constant-fold emulation disagrees with the hardware packing — defeated with `black_box`) |
| `t_golden_vectors`, `t_reference_matches_standard_vector` | the scalar kernel anchored to the true CRC32C definition (0xE3069283 test vector) |
| **D11** (diff_oracle) | scalar == reference lane semantics + fold512 == scalar across 2101 bodies (auto-skips on non-AVX-512 silicon) |
| `t_hydra_bitparity_*` (6 fabric tests) | fabric == inline == sequential across default/chaos/Fixed(1)/SeededRange/ring-wraparound/worker-count schedules |
| `t_hydra_crosspass_overlap_bitparity` | multi-pass overlap on ONE sink: every harvested per-pass tuple == fresh sequential tuple (fabric + inline) |
| D1..D10 | the full R6 differential suite re-verified with all levers active |
| Sustained arm | per-pass tuple equality + `ALLOC_DELTA = 0` on every run |
| Golden hash | 0xF6EF154EFDE905D8 on all 17 matrix cells |

## 5. Honest expectations on the runner

The dev sandbox is a 2-vCPU part (1 main + 1 worker) with ±20% co-tenant
noise; it validated **correctness** and **mechanism** (e.g. with the
overlap active, the real and NULL-diagnostic sustained rates converge —
the worker CRC is fully hidden behind the main core), not the 4-vCPU
projection. On the runner:

* **AMD EPYC 7763 (Zen 3)**: workers run the scalar kernel (8 B/cycle
  each; 3 workers have ~1.9× surplus CRC headroom). The gains come from
  Levers 2+3 (main-core cost) and Lever 4 (tail elimination + backpressure
  fix). The 1B gate requires the main core to sustain ≤ ~2.4 cyc/msg —
  at the boundary of what the fused paths deliver on 2.45 GHz Zen 3.
* **Intel Xeon with AVX-512 + GFNI**: workers run the fold kernel
  (~1.6×/core locally) with large headroom; the same main-core bound
  applies.

`PR1_GIGAHFT_MIN_MSG_PER_SEC = 1_000_000_000` (gates.rs) is the
authoritative sustained gate in CI step 11b; the R6 HYDRA 800M verdicts
remain as program-level reference lines. If the runner lands under 1B,
the gate fails honestly — the next lever family (in priority order) is:
(1) main-core batching of the sequencer apply loop (the remaining ~1.8
cyc/msg is dominated by per-frame counter/proof/blocks bookkeeping),
(2) worker core affinity to stop scheduler migrations mid-chunk,
(3) `vpermt2q`-free unit construction via a deeper even/odd split.

## 6. Commit trail

| Commit | Lever | Content |
|---|---|---|
| 82a13b2 | 1 | crcfold.rs (mirror-domain fold + crc32 ending), dispatch, D11-core unit tests, CHUNK 16→64 |
| cae9496 | 2+3 | in-place 128-bit ring stores; fused hot-path header/session decode |
| d6f58d1 | 4 | cross-pass double-buffered fabric, non-blocking pass drain, per-pass tuple gate, drain cadence fix |
| (this doc) | — | gates (PR1_GIGAHFT), docs, D11 oracle, CI wiring |
