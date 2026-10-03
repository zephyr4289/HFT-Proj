# CHALLENGE R13 — "Break the Port-5 Wall"

**Audience:** the author of docs/19–25.
**Baseline to beat (same silicon, 8573C @ 2.3 GHz, 2 phys cores × SMT):**
- Sustained full verification: **1,234,801,472 msg/s** (34.15 GB/s verified)
- Pure ingest (Front A): **3,624,572,766 msg/s** (0.6346 cyc/msg)

**Target:** sustained full verification **≥ 1.80 B msg/s** (stretch 2.0 B). Front A must not regress.

---

## 1. The problem, in your own numbers

Your own docs fix the wall:

- docs/24 §2: kernel density is **14.28 B/cyc**, i.e. **9 cycles per 128-byte fold step**.
- docs/24 §7: `fold512_tri` showed parity everywhere, so you concluded "**port-issue-bound, not chain-bound**". Tri-stream, eval2 and eval_pair all died on that evidence.
- docs/24 §8.2: the frontier on this class is "~1.4–1.6 B", and 2.0 B "exceeds this machine's structural budget (fold 84% of issue + ingest 29%)".

That budget assumes **9 cycles/step is the kernel's physics**. It was never decomposed by port. Nothing in the docs shows *which* port, or *which uops*, make up those 9 cycles.

### The open question (a hypothesis to prove or kill, not a claim)

Per 128 B step, `fold_step` plus `prep_block`/`units_pair` in `crcfold.rs` issue roughly:

| uops | count | port (Golden-Cove-class, 512-bit) |
|---|---|---|
| `vpclmulqdq zmm` | 4 | p5 only |
| `vpunpck{l,h}qdq zmm` | 2 | p5 |
| `vpshufb zmm` (bswap) | 2 | p5 |
| `vgf2p8affineqb zmm` (bit-reverse) | 2 | p0 |
| xor / ternlog | ≥ 4–6 | p0 / p5 |
| loads | 2 | load ports |

On 512-bit paths p5 carries **≥ 8 uops per step**, and 9 cycles/step is almost exactly that. If true, then **clmul is not the wall. The mirror-domain plumbing around it is.** The clmul floor is 4 cycles per step (32 B/cyc, ~2.2× today's density). That unlocks a ceiling of ~65 GB/s per physical core on the SMT pair, versus today's ~33 GB/s.

If the wall is lifted, fold stops being the binding constraint at ~1.2 B. Main (0.63 cyc/msg ingest) and the RX co-wall become the limit, which sits above 1.8 B on this box.

This is the lever your tri-stream experiment was *trying* to reach by hiding latency. It was aimed at the wrong resource.

---

## 2. Step 0 — Measure first (your own law: "diagnostics first")

Before writing a kernel, produce the port decomposition on the 8573C draw:

1. `perf stat -e uops_dispatched.port_0,...port_5,...` (or `UOPS_DISPATCHED_PORT.*`) around the `kbench fold512 1t` row. Report per-port uops per 128 B step.
2. Same for `fold512_tri` and 2cpu_smt. Explain why SMT adds *nothing* (32.83 = 32.84) in port terms.
3. Check whether LLVM already fused the 3-way xor into `vpternlogq` in `fold_step`. Check whether `units_pair`'s unpack+shuffle can be folded into the load by changing the lane layout (strided `[u64; 8]`-per-64 B).

Kill criterion: if p5 is **not** ≥ ~85% busy, the hypothesis is dead. Write it up like the tri-stream refutation and pick the next suspect (see §5).

---

## 3. The build (if the hypothesis holds)

Candidate design directions. All are open; pick by measurement, not taste.

**A. Reflected-domain fold, no GFNI bit-reverse.**
`rev64_qwords` / `prep_block` exist only because the fold runs in the mirror domain. Re-derive the 8-lane fold directly in the reflected domain (the standard crc32 reflected folding form) over the existing strided lane layout. This removes 2 GFNI uops and likely both `vpshufb`s per step. Constants must be re-derived and re-proven (as you did for KP192/KP128 and `t_tri_constants_derivation`).

**B. Remove `units_pair` by transposing the *constants*, not the data.**
The unpack exists because a 128-bit unit of lane L is split across two 64 B blocks. Alternative: fold at 64-bit granularity per lane with two clmuls (hi32/lo32 split). Same clmul count, no unpack/shuffle. Check shift-port cost (zmm shifts are p0).

**C. Hybrid ports.**
Ice/Sapphire-class cores have SSE/AVX-256 clmul on more than p5. 256-bit VPCLMULQDQ may have higher aggregate throughput than 512-bit on this part. Test `ymm` fold with 2 chains vs `zmm`. (He already found the 8370C has a single clmul port; the 8573C may differ.)

**D. Offload the endings.**
docs/23 §5 attributes ~26% of a real span's cycles to the endings (16 chained `crc32`, FNV imul chain, store/reload). After A or B this share *grows*. Re-profile and attack it (e.g. move the lane FNV combine onto the main-side fold, since it is a pure function of eight u32 lane values).

---

## 4. Rules — pure work only (no cheating, no bypass)

The measurement must stay exactly as defined in docs/11 and `gates.rs`:

1. **Bit-exact.** Every pass reproduces `0x881639cead506f25` (and `0xF6EF154EFDE905D8` on the classic path). `HYDRA_BITPARITY` must say BIT-EXACT. D11 must pass with the new kernel against the scalar reference *and* the reference table-driven CRC32C.
2. **Same value definition.** `span_crc32c_8lane` semantics (8 strided lanes + tail into lane 0 + FNV-1a-64 combine + length) are fixed. A *new faster function* that merely resembles it does not count.
3. **Every emitted byte is read and CRC-verified in-window, every pass, on a worker or assist core.** Not on a previous pass.
4. **No memoization across passes.** The blob bodies are pass-invariant (only the 10 B session prefix changes). Caching per-span CRC values, hashing a "blob generation", or diffing against the previous pass is cheating. Your own HYDRA doc forbids it. Hold that line.
5. **No schedule/data changes.** Same `sample-mini.itch`, same `MtuBound(1400)` dual-feed schedule, 505,849 msgs/pass, same sha256.
6. **`ALLOC_DELTA = 0`, per-pass tuples asserted, `#![forbid(unsafe_code)]` stays on `nf-protocol` and `nf-arbitrator`.**
7. **No shrinking the verified byte count** (e.g. skipping duplicate-feed bytes that your alias map already dedups is *already* counted fairly; do not go further).
8. **No silicon shopping.** Report the draw's `kbench fold512 1t` GB/s next to the result so draw quality is visible. Use your draw-adjusted comparison honestly.
9. **Reproducibility.** Publish raw CI logs, and report **≥ 3 independent draws of the same class**. One lucky draw is not a record. (docs/doc1.md's reviewer asked for exactly this.)

Disallowed shortcuts, by name: a lower-strength checksum (CRC16/xor-sum), sampling a subset of bytes, validating "equal to previous pass", and counting a message's bytes verified without reading them.

---

## 5. If §1 is wrong — next suspects (same discipline)

Only after the Step 0 measurement kills the p5 hypothesis:

1. **Front-end / µop-cache**: the fold loop and `finish_span` footprint (you already hit this with the R9 DSB lesson).
2. **L3 → L1 supply**: the real-mix layout is ~14% below the packed kbench ceiling (docs/23); quantify per-span line-fetch stalls with `mem_load_retired.*`.
3. **RX per-frame entry build (R13 candidate named in docs/25 §7)**: the Front A co-wall at ≤0.635 cyc/msg.
4. **Span-ending cost** (§3-D).

---

## 6. Scoring

| Tier | Sustained verified | Meaning |
|---|---|---|
| Bronze | ≥ 1.40 B | Beats the record by > 13%, three draws |
| Silver | ≥ 1.60 B | Top of your own mapped frontier |
| Gold | ≥ 1.80 B | Breaks the "structural budget" claim of docs/24 §8.2 |
| Obsidian | ≥ 2.00 B (55.3 GB/s verified) | The original Phase 4 target |

A **negative result with full port attribution counts**: a clean refutation (like tri-stream, ladder, distinct placement) is a deliverable. A win that cannot be reproduced is not.

## 7. Deliverables

- `docs/26-r13-*.md` in your usual format: physics first, lever ledger, refutations kept, claim scope.
- Port-decomposition table (Step 0), before and after.
- New kernel with differential tests (exhaustive length sweep × patterns, as D11).
- CI arm entries (default + rollback knob) following the 11x convention.
