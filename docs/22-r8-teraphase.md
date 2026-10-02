# 22 — R8 TERAPHASE: 2B/s pure ingest, the RX-pipelined transport, and the 1B full-verification frontier

## 1. The program

R8 (this document's program) set two gates (gates.rs, single threshold source):

| Gate | Constant | Meaning |
|---|---|---|
| `PR1_R8_PURE_INGEST_MIN_MSG_PER_SEC` | 2,000,000,000 | The single-process ingest pipeline — transport staging + MoldUDP64 framing + session arbitration + duplicate rejection + watermark sequencing + span emission — on the CI runner, golden population emitted and counted every pass. No byte-level verification claimed. |
| `PR1_R8_FULL_VERIFY_MIN_MSG_PER_SEC` | 1,000,000,000 | The complete HYDRA/GIGAHFT invariant set sustained ≥ 5 s: bit-exact three-layer parity, every emitted byte read + CRC32C-verified in-window, ALLOC_DELTA = 0. |

Law B-4 tripwires pin both (FAIL below threshold, PASS at/above).

## 2. What it took: the lever ledger

Starting point (main, AMD EPYC 7763 runner): pure ingest 1.169B msg/s (2.09 cyc/msg, single-core pinned); full verify 366M msg/s (3-worker fabric, scalar8lane).

| # | Lever | Measured effect |
|---|---|---|
| 1 | 16B frame directory + parallel vt stream + inline per-slot index (`FrameView` carries blocks/memo/first_seq/session words) | +5% span |
| 2 | `ingest_batch`/`steady_scan` — the doc-21 main-core-batching lever as a split-borrow free function (sequencer state hoisted to registers; cold ladder out-of-line) | codegen-sensitive; kept as the correctness-reviewed apply path (8-test differential suite) |
| 3 | Triple-free steady scan: the tombstone rule forces `body == frame[22..len]` and `last == first + n - 1`, so the scan reads ZERO triple-store lines (previously 4 scattered loads/frame, the last-triple line unprefetched) | +12% span (the single biggest single-core cut) |
| 4 | Reset-time session bake (the 10B frame patch left poll's release loop; precomputed patch-offset list) | poll head slimming; sustained-arm reset cost ~3% |
| 5 | NAPI-style RX coalescing (opt-in; exact pacing preserved for every conformance/golden/differential path) | per-poll fixed cost amortization; enables the pipeline's batch economics |
| 6 | **The RX-pipelined transport** — the structural lever: poll staging on a dedicated thread, arbitration on the main core; 4-buffer SPSC entry mailbox (RX-built `FrameEntry` arrays, use-count protocol, futex reset handshake, polite consumer) | 2.04–2.48B on the Zen3 runner (from 1.37B single-thread best), 4.08B on Zen5 |
| 7 | Topology-aware affinity: L3 groups, SMT sibling pairs, physical-core counts (threads inherit the creator's mask — the ordering trap) | un-measurable separately from 6; wrong placements measured 4–100x collapses |

**Pure ingest: PASS.** Six consecutive CI passes across the three observed runner
types (EPYC 7763 Zen3 2.445 GHz: 2.04/2.14/2.20/2.44/2.48B; EPYC 9V45 Zen5:
4.08B; EPYC 9V74 Zen5: 2.09B). CI step 16 enforces the verdict.

## 3. The pipeline (nf-transport/src/pipeline.rs)

The feed-handler shape real deployments run, as an SPSC ownership-transfer
protocol in the HYDRA lane tradition:

* the RX thread owns the whole `ReplayTransport` (directory, pacing, blob) and
  publishes ready-to-scan `FrameEntry` arrays — built from its locally-hot
  lines — into a 4-buffer mailbox, up to 1024 frames per batch;
* the consumer (the sequencer's owner — the single-writer law is intact)
  runs `ingest_entries`: the by-reference steady ladder, zero per-frame entry
  construction, zero cross-core frame-line reads (the slot carries the session
  compare words);
* resets hand-shake over a futex (the RX bakes the new session while the
  consumer parks holding no entries); the consumer aligns turn counters and
  skip-frees stale publications;
* every spin in the system is SMT-polite: bounded PAUSE, then `sched_yield`.
  Three separate measured collapses (14M, 8M, 6M msg/s) traced to uncapped
  PAUSE-spins starving SMT siblings on Zen3, whose PAUSE hint is weak.

## 4. D12: the differential oracle leg

`diff_oracle` now proves `classic == ingest_batch == RX-pipelined` on the
canonical sample — counters, watermark, count, hash — including a multi-pass
reset cycle with fresh sessions (the handshake + RX-side bake are the new
moving parts). The unit-level battery (batch_parity, 10 tests) covers
clean/lossy/reordered/session-split/single-feed schedules, both sink shapes,
and the coalesced-pacing equivalence.

## 5. The full-verification frontier (open)

The 1B sustained gate is NOT yet met. Honest state and the physics:

* Best measured: 744M burst / **288M sustained** (Zen5 9V74, fold512, 2
  workers) after the SMT-politeness fixes unlocked a 36x recovery from the
  spin-starvation cascade (8M → 288M).
* The runner pool is **2-physical-core SMT** (topology logs: cpu0-1 /
  cpu2-3 sibling pairs, one L3). On the Zen3 host the CRC physics is hard:
  1B msg/s × 31.65 B/msg = 31.65 GB/s of verified bytes against a
  machine-wide crc32 ceiling of ~19–39 GB/s (2 physical cores, 4–8 B/cycle
  depending on the silicon's crc32 throughput) — the control plane
  (scan ≈ 0.6 cores at 1B, submit+fold, RX) does not fit alongside it.
  The Zen5/Intel hosts' VPCLMULQDQ kernels clear the byte budget 3–4x —
  when the hypervisor exposes AVX-512 (one Zen5 runner hid it:
  `crc_kernel=scalar8lane` on an EPYC 9V74).
* The fabric's current equilibrium is ring-traffic-bound: pending rides the
  descriptor-ring limit, the fold's per-span `Res` loads cross cores, and
  the workers' per-span ring traffic (~0.5 lines each direction) sets
  ~8–17M spans/s/worker. The next lever family: chunk-level descriptor
  compaction (one range-descriptor per contiguous run), fold-side result
  batching, and worker-count/placement sweeps on ≥4-physical runners.

The verdict line prints honestly on every CI run (`PR1_R8_FULL_VERIFY_VERDICT`,
step 11b asserts its presence); enforcement lands when the gate is met.

## 6. Commit trail (this program)

| Commit | Lever |
|---|---|
| b38330e | gates + verdict scaffolding (2B/1B, Law B-4 tripwires) |
| 33fbeb0 | FrameBatch v2 — inline slot index, 16B directory, fused poll |
| 1853251 | sequencer batch ingest + 8-test differential parity suite |
| e7833ae | slot-direct accessor + vt-group poll + RX-coalesce knob |
| 0b0ea6c | reset-time session bake, batched span emission (SpanRec), hdr_seq deferral |
| 9c50ede | triple-free steady scan + fabric RX coalescing |
| 8cd7bbd | append-order prefetch, pacing gating, cheap reset bake |
| 0fee290 | the RX-pipelined transport + topology affinity (4 protocol bugs found by tests) |
| 2ecd871 | SMT-aware sizing/placement (the 55M/97M collapse fixes) |
| f2c35b3 | CI unpins hft_bench (2-thread span arm); topology threading |
| ed3ae7e | worker prefetch sweep (reverted: 3.5x regression documented) |
| d072ef4 | 1024-frame batches + EOS-turn fix (the 2B crossing) |
| 19c077f | 2-workers-on-SMT sizing |
| 6dae983 | L3-aware placement |
| f65ac14 | fabric placement: main+RX siblings |
| f9df18c | futex reset handshake + drop wake |
| 25aed11/95ea274 | sustained-arm phase telemetry (wait/work split) |
| c190d7c | polite consumer (bounded spin + futex park) |
| 060acf1 | SMT-polite spins across the fabric (the 36x sustained recovery) |
| (this) | D12 oracle leg, CI enforcement of the pure-ingest gate, docs |
