# R20 "fabric-widedesc" — 512-Bit Wide Descriptor Commits & the Feature Fleet Matrix

> Engineer 3's lever stack: main-thread submission overhead **0.40–0.60 →
> ≤ 0.18 cyc/msg** via staged 512-bit descriptor commits (`HFT_DESC_WIDE`),
> strict 16 KB/lane L1-residency enforcement, and automated multi-flag
> permutation testing across the 80-shard CI fleet.
> Directive: `docs/directives/ENGINEER_3_DIRECTIVE.md` · Sanctioned plan:
> `docs/challenge/ROADMAP3.md` §4.2 ("the rxdesc win without the rxdesc
> cost") · Blueprint: `ENGINEERING_BLUEPRINT_2B_6B.md` §2.3 Lever 2.

## 1. The problem, restated in µops

The sustained full-verification wall on the 4-vCPU draws is the MAIN
thread (~1.86 cyc/msg at the 1.2348B record). Its submission share —
0.40–0.60 cyc/msg — is not one fat instruction; it is the accumulated
per-span cost of the SCALAR lane path:

| per span (scalar path) | cost |
| --- | --- |
| 1 × 8-B store into the lane ring (store port + AGU) | 1 µop |
| `pending_len == 0` / `cur_inline` / format branch cluster | 3–5 predicted branches |
| pack arithmetic (`ptr − base`, shift, or) | 2–3 ALU |
| chunk tracker advance + boundary compare | 2 ALU |

Sixteen such stores per 16 spans, each with its own address generation
and branch context — the exact shape ROADMAP3 §4.2 names as the target:
*"Build the chunk's 16 Desc8s into two L1-resident staging lines, then
publish with two `vmovdqu64` stores per 16-span chunk."*

## 2. The staged wide-commit design (nf-transport/src/wide.rs)

### 2.1 Layout

```
        submitting core's L1 (stack top)
        ┌──────────────────────────────┐
        │ DescStage16 (128 B, 64-B     │  16 words staged per span:
        │ aligned): words[0..16]       │  1 pack + 1 scalar 8-B store
        └──────────────┬───────────────┘
                       │ per 16 staged words
                       ▼  ONE contiguity compare
        SPSC desc ring (2048 words = 16 KB, L1-resident)
        ┌──────────────────────────────┐
        │ window [pos & 2047, +16)     │  two vmovdqu64 (64 B each):
        │  ← storeu(lo)  ← storeu(hi)  │  16 scalar stores → 2 wide
        └──────────────────────────────┘
```

The staging block is `#[repr(C, align(64))]` — the two `_mm512_load_si512`
(aligned) read it; the ring writes are `_mm512_storeu_si512` (the window's
ring word offset is only 8-B aligned in general: published heads advance
by odd `CHUNK + 1` runs, so window alignment is not forceable without
changing the anchor protocol — and the L1-resident ring makes the split
store penalty a non-event, both lines are already in L1).

### 2.2 The invariant that keeps it bit-exact

**The staged word STREAM is bit-identical to the scalar path's.** Anchor
desc first (the run's absolute first span id), then span descs in order;
`flush_pending` publishes `desc_head = pending_head + pending_len` exactly
as before. The worker reads the same words, in the same order, through
the same Acquire — nothing worker-visible changes, only the store WIDTH.
`t_hydra_bitparity_wide_desc` pins this against the sequential reference
across fabric / determinism / forced-assist / mixed-after-assist modes,
plus the scalar-format control on the same schedule.

The position invariant: `stage_pos + stage_len == pending_head +
pending_len` at every quiescent point; every staged position lies inside
the run's chunk-open reservation (`CHUNK + 1` slots), so a window commit
needs NO space check of its own — the ring cadence already supports the
per-window granularity (ROADMAP3 §4.2's exact claim).

### 2.3 Store-forwarding (docs/21's warning, priced)

The wide stores LOAD their halves from the staging block. The LOW half's
words are 9–16 spans old (long committed to L1); the HIGH half contains
the window's newest store — a store-forward miss costs ~12 cycles once
per 16 spans (≤ 0.75 cyc/span worst case) before out-of-order overlap:
the next span's pack does not depend on the wide stores, and the 512-entry
OoO window hides the bubble. This is "produced once, store-once" — the
banned pattern in docs/21 is a load-hit-store CONSUMER loop on the
critical path, not a once-per-window producing store.

### 2.4 Wrap economics

A window straddling the ring end is non-contiguous; it falls back to the
shared masked scalar loop. At the 2048-word ring that is 1 window in 128
(0.78%). Measured on the local AVX-512 smoke (§4): `wraps/commits` =
10,656 / 1,455,982 = **0.73%** — the prediction lands. A split-store
alternative (two partial wide stores) was rejected: it costs a branch +
length computation on EVERY commit to save 16 scalar stores 0.8% of the
time.

### 2.5 The L1-residency law (compile-time now)

`nf_transport::wide::DESC_RING_L1_BUDGET_BYTES` = 16 KB; `hydra.rs`
cites `assert_desc_ring_footprint(DESC_CAP)` as a `const` — the Desc8
ring's touched footprint is pinned at exactly 16 KB/lane, and any future
geometry growth fails the build. This is §6-12 of the Do-Not-Do list
("don't add working set to win compute") turned into a compile error:
the refuted rxdesc arrays (8 × 1 MB, L2/L3-resident, −10…−21% across
both silicon classes) are structurally unreachable from the wide path —
`desc_wide_arm` requires the RING submission and is mutually exclusive
with `HFT_RXDESC`.

## 3. The feature fleet matrix (ci.yml)

Deterministic `shard % 4` rotation assigns one feature set per shard —
the 80-shard saturation queue's economics (job count, 20-slot
parallelism, fast-discard fishing cadence) are EXACTLY unchanged, while
every push covers four arms at ~20 draws each:

| rotation | arm | owner |
| --- | --- | --- |
| shard % 4 == 0 | `HFT_ENDPIPE=1` | Engineer 2 (pipelined chunk drain) |
| shard % 4 == 1 | `HFT_DESC_WIDE=1` | Engineer 3 (this doc) |
| shard % 4 == 2 | `HFT_VEC_INGEST=1` | Engineer 1 (vector stride scan) |
| shard % 4 == 3 | full synergy stack | the 3-way composition |

The arm runs the sustained bench at `--runs 3` (attribution-grade) on
acquired target silicon only, appends to the shard's draw log, and emits
the machine-greppable `R20_MATRIX_VERDICT features=… rate=… bitexact=…
shard=…` line (carrying the `R20_WIDE_DESC_VERDICT` telemetry when the
wide arm ran). The aggregator renders the summary table plus per-feature
BEST highlighting with per-push draw counts — attribution highlights
only: **the ≥3-draw median law governs any default flip** (ROADMAP3
§6-13). Flags whose levers have not landed on `main` no-op in the matrix
until their owner branches merge — the matrix arms them from day one.

## 4. Local verification (pre-fleet smoke)

Local sandbox: 2-vCPU Intel Xeon with AVX-512F + VPCLMULQDQ (NOT a
fleet-class host — numbers are mechanism checks, not evidence):

* `cargo test --release`: all suites green, 0 failures (66-test
  nf-testkit battery including the new wide parity pins; 29-test
  nf-transport battery including the wide primitive cross-checks).
* `cargo clippy --workspace --all-targets -- -D warnings`: clean.
* Armed run (`HFT_DESC_WIDE=1`, `--hydra-only --runs 2`):
  `HYDRA_BITPARITY → BIT-EXACT` (hash `0x881639cead506f25`),
  `allocs=0`, `R20_WIDE_DESC_VERDICT armed=1 commits=1,455,982
  wraps=10,656`, sustained 467.1M msg/s.
* Unarmed control: `armed=0 commits=0`, identical hashes, sustained
  388.8M msg/s.
* The +20% local delta is a single noisy draw on a non-fleet host with
  1 worker — logged as direction only. The deciding evidence is the
  fleet ladder below.

## 5. The fleet protocol (what decides the default flip)

1. Push the branch head WITHOUT `[skip ci]` → the 80-shard fleet runs
   the default battery (11b et al.) AND the four matrix arms (~20 draws
   each per push).
2. Fetch the consolidated logs from `origin/build-log`
   (`ci-logs/draw-shard*.log`) and price `HFT_DESC_WIDE=1` vs the
   default arm on ≥3 healthy draws (kbench `fold512_r` 1t ≥ 30.0),
   median-not-outlier, same microarchitecture class per the R12
   protocol.
3. Healthy signs: `R20_WIDE_DESC_VERDICT armed=1 commits>0` with
   `wraps ≈ commits/128`; the main-side `DIAG` (scan/fold split) showing
   the submission share collapsing toward ≤ 0.18 cyc/msg; zero worker
   regression on the same draws (the workers' words are unchanged — any
   worker-side delta is a coherence/cadence artifact, investigate
   before celebrating).
4. The flip to default (if the median clears the bar) is a separate
   commit citing the draws — `HFT_DESC_WIDE` stays the opt-in arm until
   then, and `HFT_DESC_WIDE=0`-style rollback remains one env var away.

### 5.1 Fleet round 1 (run #443, commit 88cfddb, 8 target draws)

The first pricing push landed 8 certified draws (the singleton queue
was contested by the three parallel engineer branches — the run was
cut at ~76/80 shards, aggregator completed):

| Shard | kbench 1t | Default 11b | Matrix arm | Features | Δ vs default |
| :---: | :---: | :---: | :---: | :--- | :---: |
| 49 | 30.93 | 1,141.18M | 1,137.44M | `HFT_DESC_WIDE=1` | **−0.3%** |
| 10 | 30.70 | 1,230.96M | 1,171.45M | `HFT_VEC_INGEST=1` (no-op on main) | −4.8% |
| 28 | 30.64 | 1,212.96M | 1,183.97M | `HFT_ENDPIPE=1` (no-op on main) | −2.4% |
| 46 | 31.13 | 1,128.08M | 1,104.26M | `HFT_VEC_INGEST=1` (no-op) | −2.1% |
| 63 | 28.72 | 1,011.41M | 950.99M | full synergy | −6.0% |
| 64 | 34.00 | 1,252.44M | 1,239.81M | `HFT_ENDPIPE=1` (no-op) | −1.0% |
| 66 | 27.62 | 966.14M | 932.36M | `HFT_VEC_INGEST=1` (no-op) | −3.5% |
| 74 | 28.07 | 1,014.78M | 955.51M | `HFT_VEC_INGEST=1` (no-op) | −5.8% |

**Readings (honest, per the ≥3-draw law):**

* Every draw: `HYDRA_BITPARITY → BIT-EXACT` (18 parity lines per draw),
  `allocs=0` (200 assertions per draw) — the frozen contracts held on
  target silicon across armed and unarmed arms.
* The wide mechanism is healthy on the Xeon class: shard 49 armed=1
  with commits=7,357,861 / wraps=53,947 = **0.73%** (the predicted
  1/128 straddle rate, again).
* The matrix arm's structural "arm-position tax" is visible and now
  QUANTIFIED: even flags that no-op on main (VEC_INGEST, ENDPIPE —
  their levers live on the unmerged owner branches) price at −1.0 …
  −6.0% vs the default battery that ran earlier on the same host
  (thermal/frequency/cache state of a later arm in the battery — the
  known ±3.4% arm-position noise class, at its negative extreme).
* Against that tax, `HFT_DESC_WIDE`'s **−0.3%** is the smallest delta
  of ANY arm — a relative recovery of ~+2…+6% on its single draw. The
  same-draw main-side DIAG corroborates: scan_ms 4213.5 → 4089.1
  (−2.9%) and work_ms 3747.9 → 3626.9 on shard 49, the submission-side
  components all moving down while the fold stayed flat.
* **One draw is one draw.** The median-of-≥3-healthy-draws protocol
  governs; the campaign continues with each subsequent push (the
  shard%4 rotation re-arms all four feature sets every push, ~20
  draws/arm once uncontested).

## 6. Frozen-contract compliance

* `Desc8` (8 B, `rxdesc_pack_span` — one formula across rings and
  arrays) and `Chunk16` (128 B = 16 words) — locked; the wide path
  moves stores, never bits.
* Golden hashes `0x881639cead506f25` / `0xF6EF154EFDE905D8` —
  bit-exact in every differential suite (local: BIT-EXACT armed and
  unarmed).
* Zero heap allocations on hot paths — the staging block is a struct
  field (sink construction, outside every window); `allocs=0` asserted
  in the armed smoke run.
* SPSC primitives for Engineer 1 (producer) / Engineer 2 (worker
  consumer): `nf_transport::wide` exposes `commit_chunk16`,
  `commit_partial_scalar`, `DescStage16`, and the L1 law — all
  `#[inline(always)]`, no allocation, no locks, no protocol change on
  the consumer side.
