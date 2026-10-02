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

## 7. R9 — the layout discovery (research mode)

The campaign's decisive measurement came from a new instrument, not a knob
turn: **fbench**, a fabric-shape kernel ablation that evaluates the REAL
tape bodies (post-dedup span slices of the actual rendered blob) under the
worker's exact execution shape, one mechanism at a time:

| Stage | Adds | Pre-R9 (local SPR) | Post-R9 |
|---|---|---|---|
| P | packed control (contiguous copy of the same bodies) | 335 cyc/span | 305 |
| K | the real blob layout | **517 cyc/span (+55%)** | **317 (+4%)** |
| D | + cross-core descriptor ring | 523 | 288 |
| R | + result publication ring | 516 | 306 |
| F | full submit/eval/fold replica | 549 | 298 |

### 7.1 The 35% nobody could see

The pre-R9 blob interleaved **byte-identical duplicate-feed frames** between
the emitted ones (`guarantee_coverage` dual feed: every message range
rendered on feed A AND feed B — 21,984 data frames, 10,992 emitted). The
verification workers therefore read the blob as **read-1.4KB / skip-1.4KB** —
a stride pattern no hardware streamer tracks. kbench's packed 8MB buffer —
the instrument every prefetch decision had been made with — modeled the
stream as sequential, so the "no-spray for fold512" conclusion (cccfebe)
was calibrated against the wrong layout. The workers' real L3 latency was
never hidden, on any runner.

### 7.2 The fix: blob aliasing (render.rs)

A duplicate delivery of the same `(first_seq, first_msg, count)` renders
BYTE-IDENTICAL frame bytes (same session prefix, same seq/count header,
same gt slice — the count arithmetic 2x10,992 = 21,984 proves the framing
identity). R9 aliases each duplicate onto its primary's blob region at
construction, guarded by a memcmp of the immutable suffix:

* the blob halves (30MB -> 15MB on the mini tape): the emitted span bodies
  become a near-contiguous stream (only the ~20B frame headers between
  them) — the K-vs-P layout penalty collapsed from 35% to 4%;
* `patch_offsets` dedups to one site per UNIQUE frame — the per-pass
  session bake halves with it (local: 241us -> 148us per pass);
* the triple store halves (duplicates reuse the primary's `(blk_base,
  blk_count, valid)` — identical bytes, identical chain, identical verdict);
* every delivery still poll()s, the scan still dedups, every emitted byte
  is still CRC-verified in-window — the aliasing is invisible to D1..D12,
  the 17-cell matrix (including M-DUP2 and the dual-feed loss cells), and
  the sustained arm's per-pass bit-exact tuple asserts.

The prepatch (kill-switched since 12a4648) is REFUSED when aliasing is
active: blob offsets are no longer monotone in event order, so the
turn-end-offset frontier mapping would be unsound (the RX logs the
refusal). An event-indexed frontier is the prerequisite for reviving it.

### 7.3 The spray, re-decided on the right layout

With the layout fixed, the worker spray was re-measured on the real arm:
locally, aliasing-only 255M sustained vs aliasing+spray(2,22,24) 348M
(+36%) — the fold512 default flips back to the scalar kernel's full-span
spray. The residual gap to the packed control (317 vs 305 cyc/span) is the
~20B header gaps plus L3 latency the spray now mostly hides.

### 7.4 Measured effect (single-physical-core sandbox — conservative)

| Configuration | sustained (local) |
|---|---|
| pre-R9 baseline (no aliasing, no spray) | 220.2M |
| + blob aliasing | 254.9M (+16%) |
| + spray default flip | 332.6M (+51% total) |

The sandbox runs the fabric on ONE physical core (main+RX+worker SMT-packed,
hypervisor-hidden siblings) — on the CI runners' two physical cores the
worker pair was the binding constraint (61% of its SMT-pair ceiling on the
726.7M record), so the layout fix compounds there: the pair's delivered
bandwidth moves toward the ~28-30 GB/s kbench pair ceiling against the
27.65 GB/s 1B demand, and main's bake wait halves. fbench joined ci.sh as
step 11d so every future run carries the layout attribution next to the
kernel ceilings.

### 7.5 The prepatch, rebuilt on an event frontier (R9b)

The kill-switched prepatch's replacement shipped env-gated
(`HFT_PREPATCH=1`, default OFF — the switch stands until evidence closes
the case). Three structural changes:

1. **The frontier is a consumed-EVENT index, not a blob offset.** The RX
   records each publication's exclusive end event index
   (`current_event_idx()`); a freed turn maps to the events whose frames
   it carried — monotone by construction (frees are in turn order; turns
   release events in schedule order), so the old design's offset
   inference (and its over-shoot hazard) is gone entirely.
2. **The patch list is event-ordered with LAST-EVENT gates.** Under blob
   aliasing a region's bytes are read by every delivery that references
   them — the primary's render AND each duplicate's. A shared site's gate
   is therefore max(referencing events), not the primary's own event:
   patching a region whose dup delivery is still pending would hand that
   delivery's entry the NEXT session (the exact stale-session divergence
   class the kill switch exists for — caught by inspection before it ever
   ran). memcmp-rejected independent re-renders carry their own sites.
3. **The RX's per-frame end-offset computation left the hot path** (the
   entry-build loop no longer computes blob offsets at all).

Measured locally (armed): reset-wait 150us → 20us per pass (7.6x), the
bake now overlapping the pass; 12/12 armed sustained runs bit-exact
(~36k armed passes), D1..D12 and the 17-cell matrix green under
HFT_PREPATCH=1. ci.sh step 11e runs an armed soak on every push — the
evidence accumulator for flipping the default.

### 7.6 The Intel draw (R9's verdict)

CI run 36998561311 (4fb49b9, Intel Xeon Platinum 8573C, fold512):

| Arm | sustained | crc demand |
|---|---|---|
| pre-R9 record (same pool) | 726.7M | 20.1 GB/s |
| **R9 unarmed** | **955.4M** | **26.42 GB/s** |
| **R9 armed (prepatch soak)** | **938.7M** | 25.96 GB/s |

The layout fix + spray moved the worker pair to 80% of its measured
33 GB/s SMT-pair ceiling — the +31% end-to-end the fbench ablation
projected. The 1B gap is 4.7%, and the diagnostics say exactly where it
lives: the workers (26.42 of the needed 27.65 GB/s) and main's window
(the unarmed reset wait was 99us/pass; the armed prepatch cuts it to
24us — the last ~4% of main's budget).

### 7.7 R9c — the prepatch default flips ON

The kill switch closes with the evidence ledger: the mechanism is REBUILT
(consumed-event frontier, last-event gates — not the offset-inference
design that flaked), and the armed configuration is bit-exact across 12
local sustained runs (~36k passes), D1..D12 + the 17-cell matrix armed,
and three CI armed soaks (~22k passes, including the Intel draw above at
938.7M). `HFT_PREPATCH=0` stays as the opt-out and ci.sh's negative
control; the per-pass tuple asserts remain the tripwire. ci.sh 11f adds
the third-worker placement sweep (the RX-hyperthread lane was only ever
measured on AMD — the sweep now rides every runner draw).

### 7.8 R9d — the prepatch default reversed by measurement

The R9c default-on flip lasted exactly one Intel draw. The 8573C data:
unarmed 955.4M (reset-wait 99us/pass, main batch-wait 322ms) vs armed
938.7M (reset 24us/pass, but batch-wait 813ms — end-to-end -1.7%). The
same ordering held on Zen3 and 9V74 draws. Post-aliasing, the synchronous
bake is cheap enough that the RX absorbs it at the advance idle; pushing
the same RFOs onto the render path delays every publication instead.
Default OFF again; the mechanism, its budget pacing (<=64 sites per
publication step, <=1024 in the EOS-drain wait), and the armed CI soak
stay — the evidence step keeps the option alive for a runner class with
real RX idle time.

### 7.9 The last 4.7% (the open front)

At 955.4M the Intel pair delivered 26.42 GB/s (80% of its 33.08 kbench
ceiling; the assist carried ~1.3 more on main). The remaining levers,
all now ride-along CI sweeps: the worker eval2 interleave (11g — kbench
parity was measured on packed buffers; the L3-bound real layout may
prefer two streams in flight), the deeper prefetch lead (11h — the
(2,22,24) default was tuned on the shared-core sandbox), and the
third-worker shape (11f — Intel fold512 data pending; scalar AMD draws
measured -2.9% on 9V74).
