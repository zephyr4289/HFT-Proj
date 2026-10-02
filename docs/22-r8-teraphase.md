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

## 5. The full-verification war (phase 2 — 223M -> ~675M sustained)

The 1B sustained gate remains OPEN. This section is the honest record of the
second campaign (commits 586ddce..8bd07ec), measured on the 5s sustained arm
(`PR1_R8_FULL_VERIFY_VERDICT`), bit-exact against the pinned golden tuple on
every run:

| Runner (pool draw) | kernel | start | after phase 2 |
|---|---|---|---|
| Intel Xeon 8573C (SPR) | fold512 | 408M* | **726.7M** (single-span eval + no-spray) |
| AMD EPYC 7763 (Zen3, 2p+HT) | scalar8lane | 223M | **675.4M** (with prepatch) / 607M prepatch-safe |
| AMD EPYC 9V45 (Zen5) | fold512 | 229M | **654.5M** |
| AMD EPYC 9V74 | scalar8lane | — | **516.0M** |

*earlier pool draw, pre-campaign.

### 5.1 Measured physics (the kbench step, every CI run)

Per-kernel ceilings on the actual runner silicon (nominal verified bytes; the
crc32:pclmul mix probe answered the Zen3 hybrid-kernel question — no port
separation, 25.3 vs 24.1 GB/s):

| Runner | scalar8lane | fold512 | 2 cores |
|---|---|---|---|
| Zen3 7763 | 24.1 GB/s | n/a | 47.9 GB/s |
| Intel 8573C | 25.5 GB/s | 34.2 GB/s | 67.9 GB/s |
| 9V74 | 21.4 GB/s | n/a | 42.8 GB/s |

Demand: 27.65 GB/s at 1B msg/s (13,988,327 body bytes / 505,849 msgs — the
DIAG line computes it from the tape every run). The scalar-only runners sit
at 65-70% of their machine-wide CRC ceiling before a single cycle of ingest,
scan, fold or transport is paid — the knife edge is the program's honest
physics. The fold512 runners clear the byte budget 2.4x but their fabrics
still feed the kernels at ~50% of the measured ceiling (see 5.3).

### 5.2 The levers that landed

1. **Diagnostics first** (586ddce): kbench kernel-ceiling microbenchmark as a
   CI telemetry step; always-on RX/consumer/worker telemetry; exact
   bytes-per-msg. Every subsequent decision was made from runner data, not
   folklore.
2. **16-deep mailbox + timed-futex parks** (33f787a): the 4-buffer mailbox
   kept the RX at the consumer's elbow; every free had to be repaid with a
   fresh publication through a sched_yield storm (2.87s of the 5s window was
   futex-park time, ~141us per park). Mid-pass wait: 2874ms -> 2.5ms.
3. **RX auto-advance** (3bd9a1a): the session program (sess_fn) lets the RX
   re-bake and re-render the next pass by itself at every EOS; reset_pass()
   never handshakes and fail-stops on any baked-vs-requested divergence. The
   unstick loop's TIMED park was the lost-wake fix (a multi-pass test caught
   the deadlock before CI).
4. **THE PLACEMENT TRAP** (850da1b) — the campaign's decisive bug:
   `cpu_order()` read the CALLING thread's mask, so after the burst arm
   pinned main, the sustained arm's fabric_placement saw a single-cpu machine
   and pinned BOTH WORKERS ON MAIN'S HYPERTHREAD while the second physical
   core sat idle. `capture_topology()` (a OnceLock captured at process start,
   before any pinning) fixed it: 229M -> 641M on the next Zen5 draw.
5. **Prepatch** (03fb04c): bake the next session into freed publications
   incrementally. KILL-SWITCHED (12a4648) after a rare (~1/10) sustained-pass
   count divergence (+39) that the byte-level audit could not eliminate — a
   verification fabric does not ship a maybe. The synchronous bake remains,
   now PREFETCH-W pipelined (84da442: the ~12.6k RFOs retire at throughput;
   540 -> 157us on the sandbox).
6. **Full-span worker prefetch** (9b95f9f): the old 2-line x 4-span window
   predates continuous ingest; the workers' demand loads were MLP-bound at
   ~10 lines in flight (8.5 GB/s per core against the 24 measured). The
   persistent line-granular cursor with burst >= per-span demand doubled the
   Zen3 fabric (301M -> 645M). Kernel-aware (cccfebe): fold512 defaults to
   NO spray (its dense sequential loads + the spray flood the L2 request
   path — 34.2 GB/s ceiling, 9 delivered).
7. **Main-core work-assist** (ec9b6fe): when a chunk's lane ring is full,
   the submitting core evaluates the chunk itself instead of spinning — a
   4-slot sealed inline ring feeds the ordered fold (the sealed flag is
   load-bearing: a mid-chunk drain once applied a partial len and freed the
   slot; the fold-order assert caught it). Self-balancing by construction:
   the assist only fires when the workers cannot keep up, spending exactly
   the cycles the spin was wasting.
8. **EOS wake + 15us park quantum + 2048-frame publications** (936ebad,
   a193be5): the advance trigger wakes the RX explicitly (one syscall per
   pass); the RX's wake quantum tracks the consumer's drain cycle.
9. **Third worker on the RX hyperthread** (11a8be3): SMT scavenging whose
   straggler risk the assist contains — the slow lane's overflow converts to
   submitting-core CRC.

### 5.3 What separates ~675M from 1B

The equilibrium on every runner: workers at 65-98% busy delivering
50-75% of their measured per-core kernel ceiling, main at 77-91% work,
fold free (<1%), reset ~20-40us/pass (post-prefetchW). The remaining
gaps, in measured order:

* **Feeding fold512** (Intel/Zen5): the no-spray default plus the
  single-span eval lifted the Intel pair to 20.1 GB/s delivered (726.7M
  sustained — the pool record) against 34.2 measured. The remaining 1.7x
  gap and the 147us/pass synchronous bake (15% of an Intel pass — the
  kill-switched prepatch's exact value) are the two known levers.
* **Scalar physics** (Zen3/9V74): 27.65 GB/s demand vs 42.8-47.9 machine
  ceiling. At perfection (workers at 100% of ceiling + assist absorbing the
  rest + main's ingest at its pure-pipeline rate) the arithmetic closes
  within ~5%; every percent of worker overhead is a percent of the gate.
* **The prepatch's ~8%**: parked behind the kill switch pending the race
  root cause (CI run 36968390363, +39 count divergence).

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

Phase 2 (the full-verify war):

| Commit | Lever |
|---|---|
| 586ddce | kbench kernel ceilings + full fabric/pipeline telemetry (CI step 11c) |
| 33f787a | 16-deep mailbox + timed-futex parks — the park chain dies |
| 3bd9a1a | RX auto-advance (session program, reset_pass, unstick loop) |
| 850da1b | THE PLACEMENT TRAP — capture_topology(); workers off main's cpu |
| 03fb04c | prepatch: incremental session bake on freed publications |
| 9b95f9f | full-span worker prefetch, burst >= per-span demand |
| ec9b6fe | main-core work-assist (sealed inline ring) |
| 936ebad | EOS wake + prefetch walk fixes |
| cccfebe | kernel-aware prefetch (fold512: hardware streamer, no spray) |
| a193be5/044e82e | RX smoothness (15us parks, 2048-frame pubs) + EntryCap fix |
| 11a8be3 | third worker on the RX hyperthread (assist-contained) |
| 12a4648 | prepatch kill switch (the +39 flake — no maybes in verification) |
| 84da442/8bd07ec | prefetchW session bake (the RFOs pipeline) |
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
