# R11 — Phase 4: The 2.0B Salvo (placement unlock + tri-stream ILP)

**Branch**: `r8-2b-1b` (continues docs/23). **Baseline**: the 1.109B gate-break
draw (run 37040724600, Intel Xeon Platinum 8573C, fold512, THP granted).
**Targets** (Phase 4): 2.0B msg/s sustained full verification (55.30 GB/s of
verified body bytes) and 6.0B msg/s pure ingest (sub-0.35 cyc/msg).

---

## 1. The gate-break draw, re-read: the placement ceiling

The 1.109B draw's telemetry (ci-logs, run 37040724600) decomposes exactly:

| line | value |
|---|---|
| `BENCH_CALIBRATION` | `freq_mhz=2300.00` (invariant TSC — fbench's 3600 is a model-string artifact) |
| 11b sustained | **1,109,130,234 msg/s**, `crc_demand_gb_s=30.67` |
| 11e prepatch armed | 1,116,370,100 (armed > unarmed on Intel too, +0.66%) |
| 11i slots=4 / 256 | 1,020,207,632 / 1,076,130,239 (default 64 wins on Intel) |
| 11j pipe (eval_pair) | 988,715,349 (**dead on Intel**, −11% — stays default OFF, now with both-class evidence) |
| 11g eval2 / 11f w3 | 978,810,629 / 917,912,795 (dead) |
| Front A pure ingest | 3,474,691,064 msg/s (0.661 cyc/msg median) |
| workers | `eval_ms` 98.6% / 98.8% busy, both pinned `cpu2`/`cpu3` |

**The find**: `kbench topo cpus=[0, 2, 1, 3] phys=2 smt=true` — the runner is
2 physical cores × SMT (sibling pairs 0–1 / 2–3). The R8 `fabric_placement`
puts main+RX on core 0's siblings, then round-robins the workers over core 1's
pool — **both workers on ONE physical core's two hyperthreads**. kbench on the
same draw measured the consequences directly:

| kbench row | GB/s |
|---|---|
| fold512, 1 thread | 32.84 |
| fold512, 2 threads **distinct** cores (0,2) | **61.74** |
| fold512, 2 threads **SMT pair** (0,1) | **32.83** |

Two folders sharing a physical core gain **nothing** over one (the VPCLMULQDQ
ports saturate; SMT only adds decode slack). The 1.109B record's workers
delivered 30.67 GB/s of the 32.83 GB/s SMT cap (**93.5% extraction**) while
the 61.74 GB/s distinct-core ceiling sat unused. The gate broke against the
*wrong* ceiling — the machine's real fold capacity is ~2× what the fabric is
drawing.

Per-window decomposition (11b): main `work_ms` 3213.7 (64%, includes the
lane-full backpressure spin — i.e. time main waits for the workers),
`wait_ms` 403, `fold_ms` 322 (assist), `reset_ms` 1000.8 (**20%** — the
per-pass handshake, 91 µs × 10,964 passes), RX `prod_ms` 3628.8 (72.6%).

## 2. The Phase 4 physics budget (honest)

At 2.3 GHz the machine has 4.6 Gcyc/s of issue. The current kernel's measured
density is 14.28 B/cyc (32.84 GB/s ÷ 2.3 GHz = 9 cyc per 128-byte step — the
length of one `clmul→xor→clmul→xor` chain plus loop overhead).

2.0B msg/s demands 55.30 GB/s of fold work = 3.87 Gcyc/s (**84% of the whole
machine**) at the current kernel density, *plus* the ingest pipeline (Front A
says pure ingest alone costs 0.661 cyc/msg = 1.32 Gcyc/s at 2B = another
29%), *plus* RX, the fold chain and the 20% reset. Total demand ≈ 2.8 cores
on a 2-core machine. **2.0B is not reachable with the current per-message
ingest cost and kernel density** — it requires the levers below, and even
then the budget closes only if several of them land near their optimistic
edge:

| lever | mechanism | budget effect |
|---|---|---|
| distinct placement (11k) | workers → 61.74 ceiling | fold capacity ~+89% headroom, minus main/RX SMT theft |
| tri-stream fold (11l) | chain slack per step 3× | kernel density 14.28 → 16+ B/cyc *if* chain-bound (issue-bound: parity) |
| 8-way vector ladder (R12) | 8 watermark checks per instruction group | ingest 0.661 → ~0.35 cyc/msg; frees main for assist |
| compact Desc (R12) | 8B descriptors, derived span ids | ring footprint halved; store-forwarding pressure down |
| transport double-buffer (R12) | bake pass N+1 off the critical path | kills the 20% reset |

## 3. Lever P — `HFT_FABRIC_PLACE=distinct` (the 11k arm)

`fabric_placement_distinct` (affinity.rs): workers take the topology order's
distinct-physical representatives first (on the draws: workers (0, 2)); main
and rx take the leftover SMT siblings (1, 3) — each scalar thread steals
issue slots from exactly one worker instead of stacking both folders on one
core. The mailbox handoff stays L3-local (single-L3 runner). Degenerate
hosts (no SMT / fewer cores) keep workers distinct and float main/rx —
behavior-only difference, the fabric's correctness never depends on
placement. Default stays `siblings` until the sweep lands (the R9c→R9d
flip lesson: no default changes without class evidence).

The trade is real and measured, not assumed: under `distinct`, main's assist
folding and the RX's 72.6% parse duty contend for the same physical cores as
the workers' clmul streams, and main loses its L1/L2-local sibling for the
mailbox. Whether the 61.74 ceiling minus that theft beats the 32.83 cap is
exactly what 11k decides per runner class.

## 4. Lever K — the tri-stream fold (`HFT_WORKER_TRI=1`, the 11l arm)

The two-stream kernel advances every 128 body bytes through ONE
`clmul(3) → xor(1) → clmul(3) → xor(1)` dependency per state register (two
registers = two chains). Measured 9 cyc/step ≈ that chain. The tri-stream
split gives the OoO engine **six chains over one sequential load stream**
(unlike eval2's two spans = two load streams, measured dead at −6% on the
8573C and −21% locally): each chain gets 3× the steps between dependencies
at unchanged per-byte issue cost. If the kernel is chain-bound the row jumps
toward ~21 B/cyc; if port-issue-bound it sits at parity and the merge's
fixed ~8 clmuls per span price it slightly below on short spans. kbench's
`fold512_tri` rows (1t / 2cpu_distinct / 2cpu_smt) attribute it per draw.

### 4.1 The math

Block-pair units split mod-3: stream m folds pairs {m, m+3, m+6, …}; between
its consecutive units sit two other streams' units, so its fold constant is
y^384 (hi half: y^448) — **not** y^128. After T_m units,
`V_m = Σ_t Ū_{m+3t} · y^(384·(T_m−1−t))`, and the exact single-stream state
is `V = Σ_m V_m ⊗ y^(C_m)` where C depends only on `wp mod 3`:

| wp % 3 | C_0 | C_1 | C_2 | roles (far@256, mid@128, base@0) |
|---|---|---|---|---|
| 0 | 256 | 128 | 0 | far=0, mid=1, base=2 |
| 1 | 0 | 256 | 128 | base=0, far=1, mid=2 |
| 2 | 128 | 0 | 256 | mid=0, base=1, far=2 |

Merge = 2 clmuls + 2 xors per register per non-base stream (C=128 reuses the
shipped KP192/KP128; C=256 uses KP320/KP256). Degenerate spans (wp < 3)
leave streams zero — a zero state merges to zero under any constant, so the
table holds for every wp ≥ 0. Constants derived by carry-less power-mod in
GF(2)[y]/P (P = 0x11EDC6F41), verified against the shipped KP192/KP128, and
**re-derived at test time** by `t_tri_constants_derivation` so a
transcription typo cannot survive the suite. Bit-parity is pinned by the
exhaustive differential sweep (every length 0..=600 × 4 patterns + long
sizes, all wp%3 residues) and D11.

### 4.2 Local verdict (SPR sandbox, 2 cores no-SMT, noisy)

fold512 20.28 vs tri 19.12 GB/s 1-thread; **30.04 vs 31.55 (+5%)
2cpu_distinct**; sinks identical (bit-exact in-kbench). The sandbox is
±15% noisy — the CI's dedicated 8573C rows are the real judge, and the
2cpu_smt tri row is the one that matters for the *default* placement while
2cpu_distinct prices the 11k world.

## 5. Validation battery on the final tree (local, all green)

`cargo test --workspace` green; `clippy -D warnings` clean; D1..D12 oracle
green (2109 bodies, incl. the tri path via the crcfold differential);
`HYDRA_BITPARITY` bit-exact and per-pass tuples exact on the sustained arm
under default, `HFT_FABRIC_PLACE=distinct`, and `HFT_WORKER_TRI=1`;
window_sweep green; 17/17 matrix cells at golden `0xF6EF154EFDE905D8`.

## 6. Standing decisions and the R12 queue

* **Default placement stays `siblings`** until 11k lands class evidence
  (same for `HFT_WORKER_TRI` — default OFF). One flip candidate already
  closed: **eval_pair is dead on both classes** (−11% Intel, neutral Zen3)
  — the 11j arm keeps running for variance data, but the lever is retired.
* **11e armed prepatch** won again on Intel (1,116.4M, 6/6 across classes)
  — the flip to default-on is now supported by both silicon classes;
  execute it in R12 with the sweep as the rollback.
* **R12 lever 1 — the 8-way AVX-512 watermark ladder** (Front A 6B +
  main-capacity for 2B): the steady scan's per-frame scalar ladder
  (first/load, last/w-derive, three compares, counter adds, SpanRec build)
  becomes one vector group per 8 entries — side arrays (firsts, ns, lens,
  feeds, ok-flags) written by the RX into the entry buffer, one zmm
  contiguity compare (`first[i+1] == first[i] + n[i]`, masked 0x7F) plus
  the scalar anchor `first[i] == w`, horizontal counter sums, branch-free
  SpanRec stores. Falls back to the scalar ladder on the first mismatch
  (cold/dup/gap). Needs a cpuid gate in nf-arbitrator (CI compiles
  x86-64-v3) and full parity-suite coverage.
* **R12 lever 2 — compact 8-byte Desc** (`offset:u32 | len:u16 | spare:u16`):
  span ids derived worker-side from the lane cursor (chunk ≡ lane mod W is
  the deterministic map; the fold-order assert becomes a derived-vs-expected
  check with identical fail-stop semantics). Halves the descriptor rings'
  L1 footprint (32→16 KB per lane).
* **R12 lever 3 — transport double-buffer**: two THP-backed blobs, bake pass
  N+1 into the inactive copy (bodies are pass-invariant; only the 10-byte
  headers change), swap at reset — the 91 µs/pass handshake leaves the
  critical path and the 20% reset collapses.
* **Target 3 (tail latency)** stays queued behind the throughput war.

## 7. First verdicts — the 8370C draw (run 37106713045, attempt 15)

The R11 push drew an **Intel Xeon Platinum 8370C (Ice Lake, 2793 MHz
invariant TSC)** after 14 silicon-filter re-rolls — a DIFFERENT class from
the 8573C record silicon, and a physics lesson in itself:

| kbench row | 8370C (2793 MHz) | 8573C (2300 MHz, record draw) |
|---|---|---|
| fold512 1t | 27.48 GB/s = **9.84 B/cyc** | 32.84 GB/s = **14.28 B/cyc** |
| fold512 2cpu_distinct | 52.26 | 61.74 |
| fold512 2cpu_smt | **30.40 (> 1t!)** | 32.83 (= 1t) |
| fold512_tri 1t / distinct / smt | 27.51 / 52.43 / 29.55 | — |
| Front A pure ingest | 2.574B msg/s | 3.474B msg/s |

The 8370C's VPCLMULQDQ pipeline is 31% less dense per cycle (single clmul
port — SMT pairs genuinely help it: 2cpu_smt 30.40 EXCEEDS the 1-thread
27.48), and its scalar side is weaker too (Front A −26%).

**Sweep verdicts (all bit-exact, allocs=0, per-pass tuples green):**

| arm | rate | note |
|---|---|---|
| 11b default | 901.6M | crc 24.93 GB/s = 82% of the SMT cap — **main-bound draw**, not worker-bound |
| 11e armed | **930.5M (+3.2%)** | armed-wins now **7/7** across Zen3/8573C/8370C on the R10 stack |
| 11k distinct | 895.1M (−0.7%) | workers DID land on (0,2); worker0's idle_iters jumped 64k→667k (main's SMT theft) — inconclusive on a main-bound draw |
| 11l tri | 893.4M (−0.9%) | **dead — parity everywhere** |
| 11i slots 4/256 | 750.0M / 907.3M | 64 stays default |
| 11j pipe / 11g eval2 / 11f w3 | 862.3M / 822.2M / 755.8M | retired/dead |

**Decisions executed:**
1. **Tri-stream is DEAD as a rate lever** — the kernel is port-issue-bound,
   not chain-bound: kbench parity on every placement (27.51 vs 27.48; 52.43
   vs 52.26), sustained parity (−0.9%). The user's Target 1a premise (hide
   the 3-cycle clmul latency) is refuted by measurement — the ports are the
   wall, on both microarchitectures. The code, kbench rows and knob stay as
   the documented refutation (the eval2 precedent).
2. **Prepatch default flipped ON** (`HFT_PREPATCH=0` is the rollback; the
   11e arm becomes the unarmed soak). The R9d reversal was measured on the
   pre-R10 equilibrium; on the R10 stack every class says armed wins. The
   mechanism the flip prices: the deep ring converts main's spin to CRC, so
   the reset handshake's synchronous bake is now critical-path — exactly
   what the prepatch removes.
3. **11k distinct stays an open experiment** — this draw was supply-limited
   (24.93 GB/s of a 30.40 cap; workers 99% busy but the fabric starved).
   The verdict that matters is an **8573C draw** (where the default
   placement runs the workers at 93.5% of their SMT cap and main has
   headroom). Keep fishing.
