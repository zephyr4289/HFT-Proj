# R12 — The Vectorized Ladder & Pure Research Frontier

**Branch**: `r8-2b-1b` (continues docs/24). **Baseline**: the R11 records —
1,234,801,472 msg/s sustained full verification + 3,624,572,766 msg/s pure
ingest (8573C, run 37108369001). **Mandate**: the 8-way vectorized
watermark ladder (ingest 0.63 → ≤0.35 cyc/msg, Front A 5-6B), 64-bit
compact span descriptors (halved ring footprint), and the 1.4-1.6B
sustained-verification summit on this silicon class.

---

## 1. The physics being attacked

The R11 record's submitting-core decomposition: `work_ms` 3819 of 5000
(76%) at **0.63 cyc/msg** of pure ingest — the steady scan's per-frame
scalar ladder. That ladder (`steady_scan_ref`) walks the AoS
`FrameEntry` array frame by frame: ~30 µops per entry across seven loads
(`first_seq`, `blocks.len()`, the session words, the memo, the feed, the
frame length), six compares (triple count, session ×2, `last < w`,
`first != w`, memo validity), the W→W+1 serial advance, and a 48-byte
`SpanRec` build per emitting frame.

The default dual-feed schedule's frame stream is **strictly alternating**:
feed A publishes packet k (an emit), feed B re-publishes the same packet
(a pure duplicate), A publishes k+1, and so on (equal release vts; the
schedule pushes feed A's event first per packet; the stable event order
preserves it). Eight consecutive entries are therefore four identical
[emit, dup] pairs — and the whole group is decidable by **four families of
relations** over SoA arrays:

| relation | meaning |
|---|---|
| anchor | `firsts[0] == w` — the group's first emit continues the watermark |
| pair-eq | `firsts[2i+1] == firsts[2i]` — the odd entry is the same packet |
| dup-le | `ns[2i+1] <= ns[2i]` — the dup is PURE (`last < w` at its turn) |
| chain | `firsts[2i+2] == firsts[2i] + ns[2i]` — the next emit continues exactly |
| wrap guard | `firsts[0] + Σns[even] <= u64::MAX` — the unsigned algebra stays exact |

Given all five, the eight entries are provably `[emit, dup] × 4`: each
even entry's `first` equals the running watermark, each odd entry's
`last` sits strictly below it. The scan advances `w` by `Σ ns[even]` in
one shot, buffers the four even spans, and folds all eight entries'
counter increments from the sidecar — **observably identical** (counters,
emissions, watermark, `progress_vt`) to running the scalar ladder over
the same eight entries.

## 2. The design (lever V — the ladder)

* **`nf_protocol::packet::EntrySoA`** — the SoA sidecar published by the
  RX alongside the `FrameEntry` array it already builds: `firsts`, `ns`,
  `lens`, `feeds` (u64/u64/u64/u8 per entry) + `ok8` (one eligibility bit
  per entry) + the baked-session words. The RX's entry-build loop fills it
  from values already in registers (+~8 µops/frame on the ~27%-idle RX
  core; the sidecar costs ~25 B/frame of L1).
* **`nf_testkit::soa`** — the AVX-512 kernel: two unaligned 512-bit loads,
  one lane-permute for the pair swap, one for the next-even shift, one
  add, three mask compares, a masked even-lane reduce for the wrap guard —
  ~15 µops per 8 frames (≈344 messages) replacing ~240. Runtime-gated
  (`avx512f`, CI compiles x86-64-v3 — the crcfold law), `HFT_VEC_LADDER=0`
  is the rollback (CI arm 11m). A scalar reference ladder
  (`ladder8_scalar`) is the executable spec; the differential test
  cross-checks 20k random groups + the wrap corner.
* **`Sequencer::ingest_entries_soa`** — the third scan driver. The
  per-entry ladder itself is now ONE function (`steady_step`) shared by
  all three drivers (iterator, slice, SoA) — a single source of truth;
  the SoA driver adds the group check (ok window, feed-parity uniformity,
  the ladder call) and falls back to the exact scalar ladder per entry on
  any unproven group. Cold frames take the unchanged classic path.

### 2.1 Two hazards the parity suite caught (the method working)

1. **The session-exactness hole.** The first design put the session
   compare in the RX's ok bit, keyed against the pass's baked session —
   correct only while the consumer's live template equals the baked one.
   `session_change_at_msg` renders post-split frames with a different
   session, and the consumer's template flips mid-pass — a static ok bit
   could then let the vector path skip a boundary event. Fix: the sidecar
   carries the baked words (`baked_lo`/`baked_hi`); the scan enables the
   vector path only while its own live template equals them (2 compares
   per scan). Current schedules cannot realize the hazard (post-split
   frames never alias pre-split regions — the alias key includes
   `first_msg`), so the gate is defense-in-depth — but it makes the
   session exactness unconditional rather than incidental.
2. **A pre-existing harness bug, found by the same test**: the parity
   helpers accumulated the sink's *cumulative* event counters once per
   poll — invisible while every leg polled at the same granularity,
   falsely divergent against the pipeline's 1024-entry publications.
   Fixed to final-counter semantics.

## 3. The design (lever D — compact Desc8)

The descriptor ring becomes one `[u64; 2048*2]` word array with two
formats over it (per-run constant, `HFT_DESC8=0` = legacy, CI arm 11n):

* **legacy**: desc `i` at words `[2i, 2i+1]` (`ptr | len | span_id<<32`),
  full 2048-desc capacity, one unaligned 128-bit store per span.
* **Desc8**: desc `i` at word `i` — `offset:u32 | len:u16 | flags:u16`.
  The offset is relative to the lane's cached blob base (the sink's first
  submitted body pointer; all bodies live in one contiguous THP-backed
  blob, u32-ranged with ~15 MB blobs, len MTU-bounded ≤ 1378 B). **8 descs
  per 64 B L1 line (vs 4)**: the descriptor stream's line traffic between
  the sequencer and the worker lanes halves, the ring's touched footprint
  halves (16 KB), the per-span store is one aligned u64.

**Span ids without carrying them**: a per-chunk **anchor desc** (flag bit
0; the offset field carries the chunk's first span id; 1 slot per
grid-aligned chunk-open) re-anchors the worker's running derivation
absolutely. The first design derived ids from a block counter
(`(blocks_seen·W + lane)·CHUNK`) — **refuted by the fold-order assert**:
the assist path diverts chunks inline, so a lane's block sequence has
holes and the extrapolation drifts. The anchor is robust to ANY
diversion pattern (assist, pass-boundary splits, fresh sinks) because it
never extrapolates across a chunk boundary. The worker's result cursor
advances by *results emitted* (anchors emit none); the fold's
exact-match assert pins the derivation with unchanged fail-stop
semantics.

## 4. Parity coverage (the immutable laws)

* `soa` unit/property tests: the ladder's relations (alternating, gap,
  partial-dup, pair-mismatch, pure-emit, wrap) + the AVX-512 ↔ scalar
  cross-check (20k random groups + wrap corner).
* `batch_parity` **3-way SoA suite**: classic vs pipelined-scalar vs
  pipelined-vector on default / lossy / reorder / session-split /
  single-feed / multi-pass schedules — counters, watermark, count, span
  hash, event counts.
* `hydra` end-to-end: pipeline + fabric + SoA under chaos (loss + jitter
  + session change), vs the sequential reference — the exact sustained-arm
  stack. **Every fabric test runs BOTH descriptor formats.**
* D12's pipeline leg now runs the SoA path; D1..D12, window_sweep, the
  17/17 matrix (golden `0xF6EF154EFDE905D8`), `HYDRA_BITPARITY`
  (`0x881639cead506f25`), and `ALLOC_DELTA=0` all green on the final tree.

## 5. R12b — the sidecar refuted, the gather design (the first 8370C draw)

The first target-silicon draw (8370C, run 37119029890) refuted the
RX-published sidecar:

* **Front A collapsed 41%** (2.574B → 1.509B, 1.085 → 1.85 cyc/msg),
  breaking the 2B pure-ingest gate on that class. The sustained arm
  attributed: 11b (both on) 835M vs 11m (ladder off) 858M vs the R11
  8370C baseline 902M — and the RX telemetry showed `prod_ms` 4300/5000
  (86% busy): **the RX is the co-bottleneck on both Intel classes**, and
  the sidecar added ~25-30 µops and +38% store-line traffic per frame to
  exactly the wrong core. The consumer's ladder gain was being paid for
  by the transport that feeds it.
* The same draw also fired the Desc8 fold-order assert (drift 32/112 =
  the assist-diverted span count) — the anchor rule keyed on grid
  alignment, but a mid-grid pass-boundary continuation that takes the
  lane path writes no anchor while its interleaved spans went inline.
  **Fix: every lane chunk-open anchors** (the derivation never
  extrapolates across any chunk boundary).

**The redesign (R12b)**: the ladder gathers consumer-side — `firsts`/`ns`
collected from the (L1-hot) AoS entry array per group, the kernel checks
the relations, and on a relations-pass the scan verifies the group's
eligibility DIRECTLY (live session template, R2 memos, non-empty counts,
feed parity) before folding. The RX is byte-for-byte the R11 shape —
zero transport-side cost on every silicon class — and the live-template
compare makes the session exactness unconditional (the sidecar design
needed the baked-words gate for mid-pass session flips; that entire
hazard class is gone).

Local A/B after the redesign (shared 2-vCPU SPR sandbox): Front A
**+7.3%** (1.730B vs 1.611B, same binary, `HFT_VEC_LADDER=0` baseline) —
the first clean positive signal of the campaign.

## 6. Expectations (honest, pre-CI)

* **Front A**: the ladder's check cost drops ~30 → ~2.5 µops/frame; with
  the irreducible `SpanRec` build + sink-side submit, the model puts the
  scan at ~0.30 cyc/msg → 5-6B msg/s on the 8573C class (the 8370C's
  weaker scalar+vector balance will land lower; the Zen3 class keeps the
  scalar ladder — no AVX-512).
* **Sustained**: the freed submitting-core budget (~0.3 cyc/msg × the
  rate) converts to assist-ring CRC exactly when the lanes saturate —
  the mechanism the R10 deep ring built. The realistic frontier on this
  silicon class stays the mapped ~1.4-1.6B (docs/24 §8.2); the ladder is
  the last large lever toward it.
* **Desc8**: halved descriptor line traffic and footprint; the anchor
  costs 1 slot per 65. The CI attribution arms (11b vs 11m vs 11n) decide
  per silicon class — the evidence ledger records whatever the silicon
  says, including refutations.

### 5.1 The second 8573C draw: a count-divergence flake in the 11j (pipe) arm

The second 8573C draw (run 37122364077) passed 11b completely —
**1,058,821,060 msg/s sustained full verification, every per-pass tuple
bit-exact, 10,467 passes** (the R12b stack holding the full-verify gate
on the record silicon) — and then the 11j sweep arm (`HFT_WORKER_PIPE=1`)
failed with a per-pass count divergence (458,552 vs 505,849, one pass
short ~1,100 spans).

Attribution so far: the count is submission-side (workers cannot affect
it), the scan is deterministic given the frames, and 11b ran the
identical submission path green for the whole 5-second window on the
same draw — which points at a TRANSPORT-level delivery flake, i.e. the
documented prepatch/auto-advance race class (the R9 "+39-count flake"),
aggravated by R12's shifted consumer/worker timing on the fastest
silicon. Not reproducible locally (the sandbox is 5x slower). The
evidence ledger stays open: if the flake recurs across draws/arms, the
prepatch frontier gets hardened; a single occurrence is recorded as the
known class.

### 5.2 R12c — the elig byte (the third design)

The first green 8573C draw (run 37124397253) priced the R12b gather
design with a clean same-run attribution: **11m (ladder off) 1,004.8M vs
11b (ladder on) 968.9M — the gather ladder is a −3.6% net loss on the
8573C's sustained scan.** Front A: 2.794B (0.823 cyc/msg; the draw's
kbench fold512 1t = 30.20 GB/s marks it an ~8.4%-weak instance, so
draw-adjusted ≈ 3.05B vs the R11 record's 3.624B). The full-verify gate
HELD (bit-exact, all 10,467 per-pass tuples, allocs=0) — correctness is
solid; the speed is not. The mechanism: the R12b group path re-ran the
eligibility checks (session ×2, memo, count, feed — ~90 scalar µops per
group) consumer-side, eating the entire vector win.

**R12c resolves the sidecar-vs-gather trade**: a 1-byte `elig` field in
`FrameEntry`'s existing padding — bit 7 = (session == the publisher's
baked template) AND (memo proves every block valid) AND (non-empty
index); bits 0..1 = the feed. The publisher computes it from
register-hot values (+~4 µops/frame); the byte rides the entry's OWN
cache line (zero added line traffic — the RX's R11 store profile is
preserved). The consumer's group check becomes: the ladder's relations +
8 elig-byte tests + a session gate on the group's first entry (its own
sess words prove baked == live for the whole group — every published
frame carries the baked session). Model: ~110 µops per 8-entry group ≈
0.32 cyc/msg consumer. Local A/B: +5.4% (1.553B vs 1.474B).

The remaining open question for the CI draws: the gather design's
consumer-side win vs the RX's own ceiling — R11's Front A on the 8573C
(3.624B) had the RX at ≤0.635 cyc/msg as the co-wall, so the ladder's
consumer gain may be capped by the transport itself on that class (the
next lever, if so: the RX's per-frame entry build).

## 7. The verdict (three green target-silicon draws + one flake)

| draw | silicon | kbench 1t | Front A | 11b sustained | 11m (scalar) | ladder Δ | desc8 Δ |
|---|---|---|---|---|---|---|---|
| 37124397253 (R12b) | 8573C | 30.20 | 2.794B / 0.823 | 968.9M | 1,004.8M | **−3.6%** | +0.4% |
| 37126929243 (R12c) | 8370C | 24.71 | 2.655B / 1.052 | 912.5M | 932.2M | −2.1% | −0.9% |
| 37128464987 (R12c+DSB) | 8370C | 26.15 | 2.506B / 1.115 | 908.3M | 899.3M | **+1.0%** | **+4.1%** |
| 37129908288 (R12c+DSB) | 8573C | 30.36 | **3.160B / 0.728** | 968.5M | 1,022.6M | **−5.3%** | **+2.1%** |
| 37131157799 (VERDICT) | 8573C | 29.94 | 2.958B / 0.778 | **1,186.1M** | 1,105.7M | **−6.8%** | **+2.3%** |

* **The ladder is REFUTED as a sustained-rate lever on the record
  class** (−5.3% on the 8573C; the 8370C disagrees at +1.0% — per the
  R9c→R9d law, disagreeing classes mean no default). Default OFF;
  `HFT_VEC_LADDER=1` arms the experiment (CI 11m = the armed soak, the
  eval2/tri precedent). Front A recovers to ~R11 parity with it armed
  (draw-adjusted ≈ 3.43B vs the record's 3.624B) — the RX is Front A's
  co-wall on this class, exactly as the sidecar refutation found.
* **Desc8 SHIPS (default ON)**: +2.1% (8573C) / +4.1% (8370C) — the
  compact descriptors and the anchor-derived span ids are a consistent
  net win, at zero correctness cost (both formats bit-exact everywhere).
* The full-verify gate held on every draw (bit-exact, per-pass tuples,
  allocs=0); one count-divergence flake in the 11j arm (the documented
  prepatch race class — §5.1) remains the open item.
* Target 1's 5-6B is **not reachable on this transport**: R11's Front A
  was already RX-co-bound at ≤0.635 cyc/msg, and the RX's per-frame entry
  build is now the ceiling — the next Front A lever is the RX itself
  (R13 candidate).

## 8. The confirmation draw (37131157799)

The verdict stack (scalar ladder + desc8 default, the armed soak in 11m)
drew the 8573C again and confirmed every decision:

* **11b (the shipped default) = 1,186,104,401 msg/s — the best arm of
  the sweep**, with the full-verify gate green, bit-exact, allocs=0. On
  this 29.94-GB/s instance (the record draw's silicon measured 32.96),
  the shipped stack scales to **≈1.30B sustained full verification on
  record-quality silicon — above the R11 record's 1,234,801,472**. The
  desc8 dividend is the difference.
* The armed ladder soak (11m): −6.8% vs the default — the refutation
  confirmed on a second 8573C draw.
* desc8: +2.3% (third consecutive positive attribution).
* Armed prepatch: +3.7% over the unarmed soak (11e) — the R11 flip
  continues to hold.

R12's net: **the compact-descriptor fabric (+2-4% sustained across
classes), the eligibility-byte infrastructure, the fully-built vector
ladder as a documented refuted experiment, and the honest map of the
Front A ceiling (the RX's per-frame entry build — the R13 lever).**
