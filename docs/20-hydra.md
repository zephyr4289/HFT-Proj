# 20 — HYDRA: Bit-Exact Multi-Core Span Conformance (R6)

> Status: ACTIVE (R6 program, post-TITAN)
> Authority: this document + `crates/nf-testkit/src/hydra.rs` + `gates.rs::PR1_HYDRA_MIN_MSG_PER_SEC`
> Scope law: PR-1 HYDRA claims are **multi-core fabric claims** and are labeled as such everywhere. Every TITAN-era single-core claim (235M–259M verified, 4.41c classic, 1.77c span) remains true and re-verified on every CI run, unchanged.

---

## 1. The Problem: Physics, Not Engineering

The TITAN program ended with the end-to-end verified arm at **235–259M msg/s**
(9.4–10.4 cyc/msg on the GH EPYC 7763 @ 2.445 GHz, single pinned core). The
tail-study decomposition of that number:

| Stage | cyc/msg (measured/derived) |
|---|---|
| Transport poll + framing dispatch + dup rejection | ~1.9 |
| Sequencer arbitration (span-mode, closed-form emission) | ~1.8 |
| **8-lane CRC32C over every emitted byte** | **~4.0** |
| FNV lane-combine (9 serial `imul` per span) + fold | ~1.0 |
| Loop/instrument slack | ~1.3 |

The dominant term is irreducible on one core. The `crc32` hardware
instruction executes at **1/cycle throughput** on every SSE4.2 x86_64 part in
the fleet (Zen 3 included); span bodies average **31.65 B/msg** (29.65 B
payload + 2 B length prefix — the span protocol hashes prefixes, by
construction of `Sink::on_span`). Therefore:

```
CRC floor   = 31.65 B/msg ÷ 8 B/cycle  ≈ 3.96 cyc/msg
Absolute single-core ceiling (zero sequencer, zero transport, zero fold)
             = 2.445 GHz ÷ 3.96 cyc/msg ≈ 617M msg/s
Realistic single-core ceiling (with the 1.8c sequencer + 1.9c transport)
             ≈ 450–550M msg/s
```

**No single-core optimization can reach 800M–1B msg/s while every emitted
byte is read and CRC32C-verified in-window.** That is an instruction-set
throughput bound, not a design deficiency. The measured micro-probe
(`crc_probe` example, 1360 B bodies): **8.32 B/cycle** — the kernel is
already at the port-throughput limit.

## 2. The Insight: What Is Serial vs. What Is Pure

Decompose the sink's work:

1. `v_i = span_crc32c_8lane(body_i)` — a **pure function of the span bytes**.
   It may execute on any hardware context, in any order, at any time, without
   changing the result (H2).
2. `h ← (rotl(h,13) ⊕ v_i) · K` — the running fold, **serial in emission
   order** (H3). Integer multiply does not distribute over XOR, so the fold
   cannot be parallelized (prefix-doubling is impossible in Z_{2^64} with
   XOR-mixing). It is, however, O(1) (~7 cycles/span ≈ 0.16 cyc/msg).
3. The invariant asserts (G-INV era monotonicity, sequence continuity, count)
   are ordering-sensitive but consume only data available at emission time.

**HYDRA** = evaluate (1) on worker cores, keep (2)+(3) on the main core:

```
main core:  poll → ingest_indexed → on_span ──(ptr,len,id)──▶ lane rings
            ...continues arbitrating immediately...
worker ×W:  dequeue → span_crc32c_8lane(body) → result value ──▶ result rings
main core:  fold_available()/finish(): h ← (rotl(h,13) ⊕ v_i)·K  in span-id order
```

**Bit-parity law (H3):** the final `(count, hash, msg_hash)` is bit-identical
to the sequential `SpanConformanceSink` for any schedule, because every
per-span value is the same pure function of the same bytes (the *same
function symbol* — the workers call the shared `pub` kernel
`sink::span_crc32c_8lane`), and the fold applies those values in the same
emission order. Worker timing influences *when* values become available,
never *what* they are or where they land in the fold sequence. Determinism is
timing-independent by construction.

**No-skipping law:** every emitted byte is still read and CRC32C-verified
*inside the measured window* — on worker cores. Nothing is memoized across
passes (R2 `FrameMemo` remains the only precompute, and it memoizes only
validation verdicts, never hash input/output). The wall-clock window spans
the full pipeline including the final blocking fold drain (`finish()`), so
the reported rate pays for 100% of the verification work.

## 3. The Fabric (H4): Chunked SPSC Lanes

A naive per-span SPSC handoff bounces 5–6 cache lines between the main core
and each worker **per span** (head cursor, tail cursor, 4 slot lines, result
cursor + result slots). At ~100–200c per cross-core line transfer that is
+500–1000c/span — **measured: a 3.2x REGRESSION** on the v1 fabric (138M vs
264M sequential). The fix is the chunked handoff protocol:

* Descriptors are assigned to lanes in contiguous **CHUNK=16-span runs**
  (`lane = (span_id / 16) mod W`).
* The main thread buffers 16 descriptors and publishes them with **one
  Release store**; workers consume whole batches (≤64) and publish result
  batches with **one Release store**; the fold drains whole batches with one
  cursor advance.
* Because `CHUNK` divides both ring capacities, full chunks are
  ring-boundary-aligned (never wrap); the pass tail publishes a partial run
  with per-slot masked indexing.
* Handoff traffic collapses to ~2 atomics + ~8 line transfers per 16 spans
  (~3,600 cycles of worker CRC work) — a >10x reduction in cross-core
  coordination per span.

**Flow control / deadlock freedom:** submission and folding both proceed in
chunk-round-robin over the same span-id prefix, so every lane's in-flight
balance stays within ±1 chunk of every other lane. The descriptor ring
(2048 slots) is strictly smaller than the result ring (4096 slots), so
`processed − folded` per lane can never exhaust the result ring. The main
thread only ever blocks (backpressure spin, which folds) on a lane that
still holds unprocessed work — the pipeline always makes progress.

**Fold order law:** a lane's result ring may hold several of its own chunks
(c, c+W, c+2W, …) whose span ids are not globally adjacent. The fold
therefore caps each lane-drain at the current chunk boundary, guaranteeing
strict emission order; a `span_id` assert on every folded result makes any
routing bug fail-stop rather than silently corrupting the hash.

**Division-free lane tracking (H6):** `lane = (span/16) mod W` contains a
runtime `idiv` (~20–40c). Called per span on both the submit and fold paths,
that is 40–80c/span of pure main-thread fat. The sink tracks the submit/fold
lane incrementally (add + compare + wrap), eliminating the division.

**Worker prefetch pipeline (H5):** span bodies are ~21 cache lines separated
by inter-frame gaps, so the hardware streamer restarts at every body and the
first lines stall on L3 latency. Workers software-prefetch the body starts
of the next four queued spans while CRC-ing the current one; the ~200c/span
CRC pass covers the prefetch lead time, converting body-start latency
stalls into overlapped L3 bandwidth (measured: 278M → 362M locally).

**Idle backoff (H4b):** an idle worker spinning in a tight PAUSE loop
re-loads `desc_head` and ping-pongs the line against the main thread while
burning shared execution resources on SMT/co-tenant silicon. Capped
exponential backoff (1–32 pauses) cuts idle traffic 32x with ~1K-cycle
worst-case wake latency.

## 4. Zero-Allocation Window

All rings (128 KiB/lane), worker stacks and threads are constructed at
`HydraFabric::spawn` time — startup, outside every measurement window. The
hot path performs no heap allocation; both HYDRA bench arms assert
`ALLOC_DELTA == 0` across the measured window (the same `GLOBAL` counting
allocator as every other arm).

## 5. Verification Matrix (what proves bit parity)

| Layer | Check | Where |
|---|---|---|
| Unit | fabric (1/2/3 workers) == inline == sequential `SpanConformanceSink`, default MtuBound dual-feed schedule, incl. double-run determinism | `t_hydra_bitparity_default_schedule` |
| Unit | chaos schedule (Bernoulli loss both feeds, Gaussian jitter, mid-stream session change) — exercises the `on_msg` gap/drain fallback + control-plane events | `t_hydra_bitparity_chaos_schedule` |
| Unit | `Fixed(1)` packetization — 505,849 spans, maximum ring churn | `t_hydra_bitparity_fixed1` |
| Unit | `SeededRange(3..61)` — mixed span lengths | `t_hydra_bitparity_seeded_range` |
| Unit | 1-worker ring wraparound (backpressure path) | `t_hydra_bitparity_ring_wraparound` |
| Unit | worker counts 1, 2, 3 | `t_hydra_bitparity_worker_counts` |
| Bench | **every** `--hydra-only` invocation: sequential reference pass vs hydra reference pass, then every measured run vs the hydra reference (count, hash, msg_hash) | `run_hydra_burst` |
| CI | `HYDRA_BITPARITY ... -> BIT-EXACT` grep + both verdict gates + `allocs=0` | `scripts/ci.sh` step 11b |

The classic paths are untouched: the 17-cell matrix, D1..D8 differential
oracle, golden hash `0xF6EF154EFDE905D8`, strace probes and the replay
conformance binary all run unchanged on every CI run.

## 6. Throughput Model (and the local-vs-runner gap)

```
worker cost/span  = kernel(1360 B) + ring/prefetch  ≈ 195–222c   (Zen 3: crc32 @ 1/c)
wall-clock        = max(main_path, worker_path/W)  +  amortized tail
main_path         ≈ 1.77c/msg (span ingest, measured on GH) + ~0.4c/msg (submit + fold + drain)
                   ≈ 2.2–2.5 cyc/msg
```

* **4 real vCPUs (3 workers):** worker path = 65–74c/span ≈ 1.6 c/msg < main
  → main-bound → **~0.95–1.1B msg/s** projected.
* **2 physical cores + SMT (4 vCPU):** aggregate CRC capacity degrades to
  ~10–12 B/c → worker-bound ≈ 3.0 cyc/msg → **~700–800M msg/s**.
* The development sandbox (2 vCPU, measured 1.55x max parallel scaling)
  cannot validate the 4-core projection; it validates **correctness**, the
  anti-ping-pong protocol (2.5x over the naive fabric), and the kernel
  (8.32 B/c). The CI gate is the runner-side arbiter.

Gate policy: `PR1_HYDRA_MIN_MSG_PER_SEC = 800_000_000` (gates.rs,
Gates-as-Code F-22). If the runner pool's silicon/topology changes, tune the
constant there — it is the single source consumed by the bench verdict lines
and the CI greps. `HFT_HYDRA_WORKERS` overrides the worker count
(`0` = inline/sequential mode). `HFT_HYDRA_NULL=1` is a *diagnostic only*
(workers skip the CRC kernel; parity asserts disabled loudly; never in CI).

## 7. API

```rust
// startup (outside every measurement window)
let fabric = HydraFabric::spawn(HydraFabric::default_workers()); // or explicit N

// per pass
let mut sink = HydraSpanSink::new(&fabric);       // fabric mode
// let mut sink = HydraSpanSink::new_inline();    // sequential-equivalent mode
while transport.poll(&mut batch) > 0 {
    for (pos, f) in batch.frames().iter().enumerate() {
        seq.ingest_auto(f.bytes(), f.feed, now, &mut sink,
                        transport.batch_blocks(pos), transport.batch_memo(pos));
    }
    sink.drain_ready();   // opportunistic ordered fold (amortizable)
}
sink.finish();            // blocking drain — MUST be inside the measured window
```

Invariants: `finish()` (or drop-with-drain) before transport reset/drop;
`reset()` only on a drained sink (asserted); the sink is single-threaded
(the fabric is the only cross-thread component).

## 8. What Is NOT Claimed

* HYDRA does not change any single-core number; it adds a new,
  explicitly-labeled multi-core class of claim.
* HYDRA does not skip, sample, or memoize any verification work — the fold
  consumes every span value, every value is recomputed from bytes in-window.
* HYDRA is not a live-NIC feature: the fabric exists so the *verification
  burden* of the deterministic replay harness can exceed one core's CRC
  throughput. The sequencer core remains the latency-critical path for
  trading semantics (single-threaded, zero-alloc, affine-token guarantees
  unchanged).
