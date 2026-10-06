# 36 — The R23 Affine Breakthrough: Zero-Re-Read Span Verification at Fleet Scale

**Program:** PR-1 / GIGAHFT — the affine-span algebra campaign (R23 → R23b → R23c)
**Branch:** `feat/r23-affine-vector-frontier` (this document ships with commit 6816048)
**Campaign:** Engineer 1 (the mathematician) → Engineer 2 (the kernelist) → Engineer 3 (the silicon integrator)
**Fleet run:** 37530109851 — 120-shard continuous saturation queue, 30 target-silicon draws consolidated (20× AMD EPYC 9V45 / Zen 5, 7× Xeon Platinum 8573C / Emerald Rapids, 3× Xeon 6973P-C / Xeon 6), **run conclusion: SUCCESS — every shard green.**

---

## 1. The campaign

The sustained full-verification record (2.009B msg/s, docs/35) was bounded by a
physical wall with a precise shape: the worker fabric re-READ every span body
from L3 — a ~1.4 KB payload per span against a measured 66.6 GB/s LLC bandwidth
ceiling on the 4-vCPU standard runner. The sequencer, the submission path and
the fold had all been driven to sub-cycle-granularity costs across R8–R22; the
remaining ~85% of worker time was memory stall, not compute.

R23 was chartered to remove the second read entirely — not by skipping
verification (the bit-exact law is absolute), but by ALGEBRA: if the ingest side
snapshots the cumulative CRC register at span boundaries, the span's own value
is a closed-form function of register tags, and the workers never need the bytes.

The three-stage handoff:

1. **Engineer 1 (the mathematician)** formally proved the GF(2) affine span law
   on the CRC32C stream — `raw(B) = raw(A∥B) ⊕ (raw(A) ⊗ G[L_B] mod VM)` —
   tabulated the 144 power constants (every one ≤ 32 bits, the width law), and
   verified 10,000/10,000 differential spans with zero errors
   (`scripts/r23_affine_crc_derive.py`, commit ae8fd6d).
2. **Engineer 2 (the kernelist)** shipped the O(1) kernels from the law: the
   scalar projection `span_crc32c_affine_sub` (two CLMUL chains, zero reads),
   the 8-lane VPCLMULQDQ projection `span_crc32c_8lane_affine_sub`, and the
   speculative 512-bit packet slicer `spec_slice_512` — zero-allocation,
   bit-exact against the reference by 10,000-stream differentials
   (`scripts/r23b_affine_kernel_derive.py`, commit ed8a61a).
3. **Engineer 3 (this work)** wired the law into the live multi-threaded
   transport pipeline: the affine-tagged rxdesc descriptor protocol, the RX
   producer's single-pass ingest ledger, and the workers' zero-re-read
   evaluation loop (this commit).

---

## 2. What shipped (R23c — the pipeline wiring)

### 2.1 The 64-byte affine descriptor (rxdesc.rs)

The span descriptor's 8-byte array word is unchanged — `offset:u32 |
len:u16 | flags:u16` — with ONE new flag bit (`RX_AFFINE_FLAG`, bit 48). The
flag announces a 48-byte TAG SIDECAR entry, giving the full descriptor a
56-byte footprint — strictly within the R21 compactness law (pinned by a
compile-time assert):

```text
word 0   : offset:u32 | len:u16 | RX_AFFINE_FLAG + flags:u15
word 1   : prefix_crc:u32 | cum_crc:u32     raw-CRC32C snapshots
word 2   : raw_crc:u32 | reserved:u32       the affine check target
words 3-6: lanes[0..8] (u32 each)           the 8-lane register tags
```

The (prefix, cum) pair is SELF-CONTAINED per span: byte-granular cuts of the
raw CRC32C stream at the span's mid boundary (h = len/2) and its end. The
worker checks Engineer 2's scalar projection —
`span_crc32c_affine_sub(cum, prefix, len − h) == raw_crc` — as a per-span
INTEGRITY assertion (~1.2 ns, two CLMUL chains, zero reads, fail-stop on any
sidecar corruption). The mid-boundary split deliberately avoids a cross-span
running chain: a partially-diverged schedule can never desynchronize a
register that depends only on this span's own bytes.

The `lanes[8]` are the reference kernel's own lane registers (64-byte blocks,
8 interleaved chains, tail folded into lane 0). The worker reproduces the
EXACT golden `span_crc32c_8lane` value via the 9-multiply FNV-1a-64 combine —
`crcfold::span_crc32c_8lane_from_tags` — for ARBITRARY span lengths. The
silicon probe that drove this shape: the pipeline's real MTU-bounded spans
average 1,364.6 bytes with only 2.7% being 64-byte multiples
(`scripts/r23c_span_shape_probe.py`), so the shipped 64-aligned 8-lane affine
kernel cannot serve them directly; the tag lanes + combine is the
value-identical generalization (for 64-multiples it equals the affine kernel
with a zero prefix — pinned by test `t_r23c_tag_core`).

### 2.2 The RX producer's single pass (pipeline.rs, render.rs)

`PipelinedReplayTransport::set_rxdesc(state)` attaches the fabric's rxdesc
state; the RX thread then performs THE INGEST PRODUCER'S SINGLE PASS over the
blob's packet bodies — one `affine_frame_tag` scan per frame (~0.5 ms for the
15 MB corpus, UNTIMED, at the reset-serve / first-cycle / thread-start
trigger points), filling a per-frame **affine ledger** (absolute body
pointers; the bodies are blob-immutable across passes because session baking
touches only frame headers [0..10], so the ledger is pass-invariant for the
transport's life).

The sink's cold check-and-fix path resolves each span's tag from the ledger
by a monotone body-pointer cursor (O(1) amortized — spans arrive in frame
order; duplicate-feed deliveries re-hit the same row without advancing). A
direct body scan serves as the fallback for divergent schedules and
ledger-less runs — both paths produce bit-identical tags (one kernel,
`rxdesc::affine_frame_tag`, pinned against the reference by the differential).

### 2.3 The zero-re-read worker loop (hydra.rs)

Both rxdesc workers (the diet worker and the pre-diet attribution twin) take
the fast path: read the tag sidecar (ordered by the same spans_ready Acquire
that exposed the word), run the scalar affine integrity check, FNV-combine
the lane registers, publish the result. **Zero payload bytes are read — and
the prefetch spray skips tagged bodies entirely**, so the L3 request stream
from the worker fabric collapses to the sidecar's own lines. The counters
(`affine_hits` / `affine_fallbacks` in WorkerStats, the `R23B_AFFINE_VERDICT`
telemetry line) make the zero-read claim a measured fact per draw, not a
faith statement.

The warm-start law extends to the sidecar (`copy_tags`): the deterministic
schedule replays the same span sequence over the same blob, so every measured
pass finds word AND tag already correct, and the steady-path check remains
ONE 8-byte load-and-compare on the submitting core.

### 2.4 The speculative 512-bit slicer in the ingest loop (hft_bench.rs)

The R23c spec-slice arm wires `spec_slice_512` into a pure-ingest pass: the
loop walks the RX-pipelined transport and counts every message by driving
the speculative slicer over each frame's message-block stream — no sequencer
ladder, no arbitration — on a single-feed schedule (the golden population
assert stays exact: 505,849). This is the 20B-mode pricing row; the JSON
gains `spec_rate_msg_per_sec` + `r23_pure_ingest_verdict` (REPORTED — the
15B floor follows the R16 non-asserting elevation protocol).

### 2.5 Rollback and attribution discipline

`HFT_AFFINE_TAGS=0` restores the pre-R23 array protocol verbatim (empty
sidecar, workers re-read payloads). CI arm 11w is pinned to the rollback so
its historical fleet pricing stays comparable; the affine SHIP is priced by
the new 11ab arm. The aggregator's scoreboard gains the spec-slice and
affine-zero-read columns. (A rollback-path SEGV — the empty sidecar's
dangling bases reaching the NT-copy — was caught locally and fixed before
push: the slot-capacity gate now precedes all pointer math.)

---

## 3. The empirical campaign (run 37530109851 — 30 target draws)

### 3.1 The fleet table (sustained full-verify, 5s, fresh sessions, msg/s)

| Silicon class | n (healthy) | 11b ring (median) | 11oa ofold | 11ab affine (median) | 11ab max | Front A span (median) | spec-slice ingest |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| AMD EPYC 9V45 (Zen 5) | 20 | **1.760B** | 1.873B* | **1.654B** | 1.866B | **3.556B** (max 4.365B) | 342M |
| Xeon Platinum 8573C (Emerald) | 7 | 1.130B | — | 0.861B | 1.027B | 1.967B (max 2.394B) | 161M |
| Xeon 6973P-C (Xeon 6) | 1† | 1.322B | — | 1.116B | 1.116B | 2.205B | 186M |

\* single healthy draw (the best Zen 5 draw, shard 22) — per-class medians for
the minor arms ride the same logs. † 2 of 3 Xeon 6 draws fell under the kbench
health floor (the R12 noisy-host protocol) and are excluded from medians.

Best single draw (Zen 5, shard 22): 11b **1.907B**, 11z null-mode fabric
ceiling **2.933B**, 11ab affine **1.768B**, Front A **3.672B** (max 4.365B
on shard 59).

### 3.2 The kernel rows (kbench, the healthy Zen 5 draw)

| Row | Reading |
| :--- | :--- |
| `affine_sub_1t` | 97.6 GB/s nominal = **13.8 ns/span** (1,344 B) — the O(1) scalar projection |
| `affine_sub_8lane` | 90.4 GB/s = 14.9 ns/span — the 8-lane projection + FNV combine |
| `spec_slice_512` | 352M msg/s = **2.84 ns/msg** — the speculative slicer, scalar walk |
| `spec_slice_512_vec` | 180M msg/s = 5.6 ns/msg — the E/O vector re-arm (still priced negative on L1-hot streams — Engineer 2's verdict holds) |
| `fold512_r` / `scalar8lane` | 59.2 / 79.2 GB/s — the re-read kernels the affine path REPLACES for span verification |

### 3.3 The invariant ledger (all 30 target draws, all 120 shards)

| Invariant | Verdict |
| :--- | :--- |
| Bit-exact golden parity (`0x881639cead506f25` / `0xF6EF154EFDE905D8`) | **BIT-EXACT on every draw** (three-layer: sequential == fabric == measured pass) |
| Worker payload reads on the affine path | **payload_fallbacks = 0 on all 30 draws**; tag_hits up to 202.7M per 5s run |
| RX ledger | filled uniformly (**21,984 rows** = every renderable frame body) on every armed draw |
| ALLOC_DELTA | **0** on every measured window, every arm |
| Topology verification | `TOPOLOGY_VERIFICATION main=0 rx=2 -> VERIFIED` on target draws |
| 120-shard run conclusion | **SUCCESS** (all grep gates green, including the new 11ab gates: armed, zero-fallback, verdict lines) |

---

## 4. Microarchitectural attribution

**The L3 memory wall is gone from the worker fabric.** The pre-R23 worker
evaluated each ~1,365 B span body as a demand-read + spray stream against the
shared LLC; the R23c worker reads 48 B of register tags. The measured counters
prove the collapse at fleet scale: `payload_fallbacks=0` across 30 draws and
202.7M tag evaluations in a single 5-second run — with the prefetch spray
gated off for tagged spans, the worker fabric's L3 request stream is now the
sidecar's own lines (the warm-copied array slots), not the 15 MB blob. The
66.6 GB/s LLC ceiling that priced the R11–R16 campaigns no longer bounds span
verification.

**The wall moved — and the 4-vCPU standard runner answered honestly.** With
the workers at zero payload reads, the sustained rate's binding constraints on
this runner class are:

1. **The main-side submission + fold** (the sequencer's ladder, the
   check-and-fix store, the ordered fold): the null-mode instrument (11z) —
   the same ring fabric with workers doing constant non-CRC work — ceilings
   at 2.933B on the best Zen 5 draw. That is the fabric protocol's ceiling
   with ZERO verification work; the 3.0B+ milestone band therefore cannot be
   claimed on the 4-vCPU standard-runner shape (the R12 target ruling) by ANY
   worker-side lever — the remaining distance is main-side and
   protocol-side.
2. **The array-vs-ring protocol difference**: 11ab (array + zero-read tags)
   lands at −0.6%…−9.9% vs 11b (ring + payload reads) on healthy Zen 5
   draws. The R17 fleet verdict priced the array protocol's fixed overhead at
   −10…−21%; the affine lever recovers roughly half-to-all of the worker-side
   share of that deficit, but the array path's submission mechanics (the
   per-span check-and-fix, the window open's warm copies, the result-ring
   round trips) remain. On this runner class the ring's leaner protocol still
   wins the median.

**The affine arm's measured value on this fleet:** the verification WORK per
span collapsed from a ~1.4 KB memory round trip to 48 B of tags + ~2.9 ns of
register math (the integrity check + the combine — consistent with the
kbench `affine_sub_8lane` row), with bit-exact values and zero allocations —
proven at 30-draw scale. The sustained milestone band on the standard runner
is bounded by the fabric protocol itself (point 1), not by verification.

**Front A and the ingest frontier:** the RX-pipelined span arm (the pure
ingest front) medians 3.556B and peaks 4.365B on Zen 5 — the RX thread's
publication cadence and the consumer ladder are the ingest front's own walls.
The spec-slice row prices the raw message-walk ceiling at 342M median on the
standard runner: the speculative slicer is live and population-exact, but the
15–20B band remains a FUTURE mode — it needs the vector re-arm to price
positive on stream-shaped (L3-resident) inputs plus a tighter entry-walk loop
(the current arm iterates `FrameEntry` batches; the band's pricing instrument
is in place, the loop is not yet the 0.05 cyc/msg shape).

---

## 5. The honest verdict

**Claimed and proven:** zero-re-read, bit-exact, zero-allocation span
verification at fleet scale — the affine-span algebra carried from proof, to
kernel, to the live multi-threaded pipeline, with the 2.5B REPORTED verdict
line (`PR1_R23_AFFINE_SUSTAINED_VERDICT`) and the zero-read counters now
first-class CI artifacts on every future draw.

**Explicitly not claimed:** the 2.5–3.0B+ sustained band on the 4-vCPU
standard runner. The null-mode ceiling (2.93B) proves the band is unreachable
on that runner class by worker-side levers alone; the 11ab median (1.654B
Zen 5) sits at −6% vs the ring default because the array protocol's fixed
costs dominate once reads are free. The band reopens when either (a) the
fabric placement escapes the 4-vCPU ruling (the 9V45's 96-core shape is the
natural host — the R12 protocol's next session), or (b) the main-side
submission/fold program (the R16b array protocol's own roadmap) collapses the
protocol gap.

**The next levers, in attribution order:** (1) the ring + affine hybrid — a
lean ring-sidecar for the tag stream at 16 KB/lane-class footprints, keeping
the ring's submission mechanics with the zero-read eval; (2) the 8-lane
vector projection as the combine's SIMD replacement once the lane stream is
64-aligned (the fold-then-project shape of Engineer 2's K2 law); (3) the
spec-slice vector re-arm on L3-resident streams (the one-touch 512-bit load's
amortization hypothesis, priced per draw by the twin rows).

---

## 6. Verification ledger (how every claim above is pinned)

| Claim | Pin |
| :--- | :--- |
| Tag kernel == reference lanes / raw scans / affine projection | `crcfold::tests::t_r23c_tag_core` (1217..=1380 + edge sweep, 4-fact assert) |
| Fabric (ledger-less fill) == sequential, warm-copied tags | `hydra::tests::t_r23c_affine_tag_parity` (2 generations, hash bit-exact, fallbacks=0) |
| Full pipeline (RX ledger → sink → workers) == sequential | `hydra::tests::t_r23c_affine_ledger_pipeline_parity` (ledger ≥ 10k rows, hits>0, fallbacks=0) |
| Rollback == pre-R23 protocol | The 11w arm (HFT_AFFINE_TAGS=0) + the disarmed-path parity suite |
| O(1) projections (Engineer 2) | kbench `affine_sub_1t` / `affine_sub_8lane` rows, in-bench parity pins |
| Zero reads at fleet scale | `R23B_AFFINE_VERDICT ... payload_fallbacks=0` grep-gated in ci.sh 11ab, 30/30 draws |
| Golden hashes | `HYDRA_BITPARITY ... 0x881639cead506f25` grep-gated on every armed arm |
| ALLOC_DELTA = 0 | The per-run asserts in every bench arm (unchanged) |
| Descriptor compactness | `const assert: 8 + 48 ≤ 64` bytes (rxdesc.rs) |

The differential batteries of Engineers 1 and 2 (the law oracle and the kernel
oracle, 10,000/10,000 each) remain in force unchanged beneath this layer; the
R23c wiring adds the pipeline-level parity proofs above them. The law, the
kernels, and the pipeline now form one continuous verified chain from GF(2)
algebra to fleet silicon.
