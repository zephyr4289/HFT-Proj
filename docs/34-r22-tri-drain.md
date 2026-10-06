# 34 — R22: The T=3 Tri-Stream Worker Drain (the fold512_tri Default Arming)

**Branch:** `feat/fused-vector-verify` | **Directive:** PR #12 review direction (Run #37457464587 fleet telemetry) | **Baseline:** `2eca9e8` @ 1.918B msg/s sustained full-verify (Zen 5 draw), 3.069B msg/s null-fabric ceiling

---

## 1. Executive Summary

The PR #12 fleet evidence closed three questions and opened the fourth. Closed: the all-inline fused shape loses on 4-vCPU silicon (11fb priced it at 0.751B — submit-side saturation trades away worker parallelism wholesale, so the distributed fabric stays); the T=8 octo merge loses at the real ~wp 10.5 span mix (11oa −4.2%); the topology verification locks the ingest mode (12B msg/s front, VERIFIED on every draw). Open, and now wired: the null instrument priced the verification gap at **1.15B msg/s of CRC cost** (3.069B ceiling vs 1.918B sustained), and the kbench rows named the kernel that shrinks it — `fold512_tri` (T=3) at **64.90 GB/s** vs the sequential natural kernel's 59.33 GB/s on the same Zen 5 draw, the only interleave that met the directive's 60+ GB/s bar.

R22 arms exactly that shape as the worker drain's default verification kernel. The natural-domain tri-stream (`fold_word_triples_r`) splits each span's word-pair units mod-3 across three independent (even, odd) state pairs — six independent clmul chains over one sequential load stream — steps each stream with ONE reduced-pair 2-clmul advance per three units (the K^3 entry of the R21 G-table law), and merges the three streams with the offset pairs before the standard composed-field endings. The worker drain (ring, rxdesc, and diet shapes alike) evaluates spans through `eval_tri` by default; `HFT_WORKER_TRI=0` is the documented rollback, and CI arm 11l now soaks the UNARMED side of the ledger so the 11b-vs-11l delta prices the arming per draw. Everything is pinned bit-exact by the same differential machinery — golden parity `0x881639cead506f25` and `ALLOC_DELTA == 0` hold on every local run of the modified stack.

## 2. The Math — the Natural-Domain Tri-Stream

The R11 tri-stream exists in the mirror domain (`fold_word_triples` + `span_fold_eval_tri`): the units enter GFNI-reversed and byte-swapped, the step folds by y^384/y^448 in the CRC polynomial ring P, and the merge restores the exact single-stream state with the fixed y^256/y^128/y^0 offset table. The production worker kernel is the NATURAL domain (reflect) — raw little-endian loads, no per-step affine/shuffle work, the R13 p5-census fix — where "advance by one unit" is ring multiplication by `K = RKLO mod VM = VR0` in `GF(2)[y]/VM`. The R21 octo program generalized that into the P1' class law: ONE reduced-pair 2-clmul step advances a state by k units, the pair being `(K^k ⊗ y^64, K^k) = (G[16k−8], G[16k])`. The tri is the k=3 entry of that same law — no new constants are derived:

* **Step pair** `(TRI_K3_HI, TRI_K3_LO) = (K^3 ⊗ y^64, K^3) = (G[40], G[48])` — aliases of the octo offset-3 merge pair `OFOLD_MG3_HI/LO`; both constants ≤ 32 bits (the R16 width law: state hi ≤ 33 bits, every composed ending field ≤ 96 bits, inside the vend exactness range).
* **Stream structure**: stream m folds block-pairs `{m, m+3, m+6, ...}`; after `T_m` units its state is `V_m = Σ_t U_{m+3t} ⊗ K^(3(T_m−1−t))`. Six independent `clmul → ternlog` chains run over ONE sequential load stream — the same latency-budget argument as the mirror tri, at the natural domain's lower per-step census.
* **Merge**: the full state is `V = Σ_m V_m ⊗ K^(C_m)` with `C_m = (wp−1−m) mod 3` — the octo offset law mod 3 (the mirror tri's y^256/y^128/y^0 table re-emerging as ring powers). The base stream (owning the body's last unit) merges by identity; each other live stream merges with ONE 2-clmul step using `(OFOLD_MG{c}_HI, OFOLD_MG{c}_LO)`, c ∈ {1, 2}. Four independent clmuls plus a three-deep XOR tree, once per span, off the hot loop. Degenerate spans (wp < 3) leave streams empty; a zero state merges to zero under any constant, so the law holds for every wp ≥ 1.

The merged states are CLASS-exact (ring-congruent), not value-identical, to the sequential kernel's states — exactly the dfold/ofold situation — so the dispatch forces the vend + vtail composed-field endings for ALL r (`finish_span_r_inner3(body, st, true, true, true)`). Bit-exactness is not assumed, it is pinned: the exhaustive differential sweeps every body length 0..=600 across four content patterns plus the representative long sizes through the new kernel against the scalar reference, and `t_ofold_constants_derivation` now also pins the tri step pair end-to-end (identity with the octo offset-3 pair, the G-table indices G[40]/G[48], and the P1' class law at k=3 on one-hot states — one reduced-pair application equals THREE stepwise R13 advances).

### 2.1 The dispatch law, stated honestly

The dfold/ofold precedent resolves a class-exact axis OFF where the silicon-default endings are the CRC-chain path (AMD, per the R14 vend table — the 8370C measured −2.9% sustained under the vector endings on Zen 3). R22 deliberately deviates: the tri path forces the composed-field endings on EVERY fold-class silicon. The reasons are in the ledger: (1) the R14 evidence predates Zen 5's four VPCLMUL pipes — the ending-clmul-vs-fold-clmul contention that sank vend on single-pair Zen 3 is a different physics question on 9V45, and the 120-shard CI exists precisely to price it; (2) forcing keeps ONE verification shape across the fleet, so the 11b sustained verdicts stay comparable across classes instead of silently forking by vendor; (3) the rollback is one environment variable away, and the kbench `fold512_rc` row keeps the CRC-chain ending priced on every draw. If the Zen 5 draws reject the forced endings, the flip-back is a documented default change, not an architecture change.

The ofold precedence rides unchanged: when `HFT_CRC_OFOLD=1` arms the octo, the tri dispatch defers to it (the deeper split subsumes the chain-depth effect — the same law as tri-over-dfold), so the two axes can never interleave their state conventions.

## 3. The Wiring (`hydra.rs`, `crcfold.rs`, `kbench.rs`, `ci.sh`)

* **`crcfold.rs`**: `TRI_K3_HI/LO` constants (the k=3 G-law entry), `fold_word_triples_r` (the mod-3 block-pair loop), `span_fold_eval_tri_r` (the dispatcher: FOLD_MIN_LEN gate → octo-precedence check → tri loop → forced class endings), the non-x86_64 stub, the `eval_tri` Reflect arm rewired (previously it silently fell back to the SEQUENTIAL natural kernel — the R11 sweep arm never actually ran T=3 on the production kernel; this is the bug-shaped gap the review's directive closed), and the test additions.
* **`hydra.rs`**: all three worker shapes now default the tri knob ON — `lane_worker` (the ring worker, the CI default shape via `HFT_RXDESC` unset), `lane_worker_rxdesc` (the pre-diet array worker, CI arm 11x), and `lane_worker_rxdesc_diet` (the diet array worker). Each reads `HFT_WORKER_TRI` ONCE at spawn, outside every measurement window (law #9). The pipe/eval2 experiment knobs keep their branch precedence; the tri is the default single-span shape.
* **`kbench.rs`**: the `fold512_tri_r` row (1t, 2cpu_distinct, 2cpu_smt) — Reflect + `eval_tri`, i.e. the exact armed shape including the forced endings — so every draw prices `fold512_r` (sequential, silicon-default endings) vs `fold512_tri_r` (the armed default) on the same silicon, next to the mirror-domain `fold512_tri` row that keeps the cross-domain attribution.
* **`ci.sh`**: arm 11l flips from the armed sweep to the ROLLBACK soak (`HFT_WORKER_TRI=0`, the 11e prepatch precedent — when a lever becomes the default, its arm keeps the un-armed side of the ledger). The 11b-vs-11l sustained delta per draw IS the arming's attribution; kbench's `fold512_tri_r` vs `fold512_r` is the kernel-level twin.
* **`hft_bench.rs`**: intentionally untouched. The review's direction names it alongside hydra.rs; the mapping is: `hft_bench.rs` is the PURE-ingest instrument (zero CRC math on the hot path by contract — the 12B msg/s front-A gate that Task 4's topology verification locks in), so the worker-drain verification arming lives entirely in the hydra fabric that `bench.rs --hydra-only` drives. Fusing T=3 into hft_bench's measurement window would regress the very metric that file exists to keep clean (the docs/33 §3 ruling, unchanged).

## 4. Verification Evidence (this sandbox)

The sandbox is the same 2-vCPU Intel Xeon VM as R21 (AVX-512F/BW/CD/DQ/VL, VPCLMULQDQ, GFNI — every vector path executable, far from target-silicon throughput). Evidence:

| Check | Result |
|---|---|
| `cargo test --workspace` (incl. the tri_r differential lines + the extended `t_ofold_constants_derivation`) | PASS — all suites, 0 failures (68/68 nf-testkit) |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS — clean |
| hydra 11b shape (tri DEFAULT ON) | `HYDRA_BITPARITY -> BIT-EXACT`, golden `0x881639cead506f25`, count 505849, `allocs=0` |
| hydra 11l rollback (`HFT_WORKER_TRI=0`) | `BIT-EXACT`, `allocs=0` |
| hydra rxdesc diet (`HFT_RXDESC=1`) with tri armed | `BIT-EXACT`, `allocs=0` |
| hydra rxdesc pre-diet (`HFT_RXDESC=1 HFT_RXDIET=0`) with tri armed | `BIT-EXACT`, `allocs=0` |
| hydra ofold precedence combo (`HFT_CRC_OFOLD=1`, tri default) | `BIT-EXACT` — ofold wins, same value |
| kbench all rows | PASS — `fold512_r` / `fold512_tri` / `fold512_tri_r` sinks identical (`0xbedb8ba779de450f` at 1t), bit-exact across kernels |

Local throughput context (NOT fleet evidence): the VM's 1t rows read `fold512_r` 19.73 GB/s, `fold512_tri_r` 19.52, `fold512_tri` 18.31 — parity-to-slightly-behind on a throttled shared host, exactly the profile the fleet must arbitrate (the Zen 5 draw priced the same pair at +9.3%).

## 5. Rollbacks and the Certification Path

Every lever carries its knob: `HFT_WORKER_TRI=0` (the worker drain, default ON per the directive), `HFT_CRC_OFOLD` unchanged (default OFF, preempts tri when armed), and the pre-existing `HFT_CRC_VEND`/`HFT_CRC_VTAIL` overrides still bind every NON-tri evaluation path (the tri shape forces its endings internally, per §2.1). The certification path is the one the review directive prescribed: push, let the 120-shard queue draw the Zen 5 / 6973P-C / 8573C fleet, and read three numbers per draw — 11b (armed) vs 11l (rollback) for the sustained conversion, and `fold512_tri_r` vs `fold512_r` for the kernel-level attribution. The conversion math the fleet is pricing: the null ceiling says ~1.15B msg/s of CRC cost; the kernel rows say T=3 removes ~9% of the fold-loop's cycle cost on Zen 5; the sustained verdict decides how much of that survives the merge, the endings, and the supply coupling — with the 2.5B frontier as the bar the review set.

## 6. First Fleet Verdict — Run #455 (16 consolidated draws): the R22 Default REJECTED, the R22.1 Correction

The 120-shard run on the R22 arming commit (`1a66aff`) returned the fleet's answer, and it is a rejection. The consolidated-draw table (11b = the armed default; 11l = the same-draw rollback soak; kbench 1t rows from the same process; `f512_rc` = the crc-chain ending row, which IS the worker's per-class default shape on AMD where the vend silicon table is false):

| Draw (silicon) | 11b armed sustained | 11l rollback sustained | same-draw delta | `fold512_r` | `fold512_tri_r` | `fold512_rc` | `fold512_tri` (mirror) |
|---|---|---|---|---|---|---|---|
| s109 (9V45 Zen 5) | 1.566B | 1.741B | −10.1% | 48.95 | 41.96 | 61.57 | 59.85 |
| s115 (9V45) | 1.685B | 1.888B | −10.8% | 59.26 | 45.89 | 65.75 | 63.50 |
| s118 (9V45) | 1.524B | 1.734B | −12.1% | 54.42 | 42.21 | 59.94 | 60.32 |
| s119 (9V45) | 1.574B | 1.783B | −11.7% | 55.62 | 41.93 | 62.02 | 46.21 |
| s17 (9V45) | 1.554B | 1.774B | −12.4% | 55.93 | 44.58 | 62.43 | 62.65 |
| s45 (9V45) | 1.604B | 1.745B | −8.1% | 61.45 | 44.41 | 66.22 | 63.65 |
| s48 (9V45) | 1.590B | 1.773B | −10.3% | 56.59 | 44.26 | 66.18 | 62.16 |
| s61 (9V45) | 1.680B | 1.752B | −4.1% | 59.45 | 45.84 | 66.51 | 64.77 |
| s11/s38/s42/s46/s47/s60 (8573C) | 1.07–1.17B | 1.10–1.19B | −2.0..−5.9% | 27.3–32.8 | 26.9–32.8 | 27.2–31.0 | 29.9–32.3 |
| s25 (6973P-C) | 1.183B | 1.206B | −1.9% | 29.62 | 28.68 | 29.74 | 29.68 |

(Sustained-arm order confounds exist — later arms read slightly higher on noisy draws — but the kernel-level rows and the sustained deltas agree in direction and the kbench ratios are order-immune.)

Three findings, honestly recorded:

1. **The mechanism is the ENDINGS, not the split.** On Zen 5 the crc-chain ending is the fast shape (`fold512_rc` 59.9–66.5 GB/s) while the composed-field endings cost 12–25% (`fold512_rv` 54.1–61.3, `fold512_tri_r` 41.9–45.9). The class-exact tri's forced vend+vtail-all-r dispatch law — my §2.1 deviation — is exactly what the R14 vend ledger predicted for AMD, now confirmed on Zen 5 with four VPCLMUL pipes: the ending's serialization is a census problem, not a pipe-count problem. The tri split's ~9% chain saving cannot recover a 20%+ ending tax.
2. **The directive's premise needed a control correction.** The 64.90 GB/s `fold512_tri` reading (run #454) was priced against `fold512_r` — the vend-ended row, which IS the worker's shape on Intel (vend silicon-default true) but is NOT the worker's shape on AMD (crc-chain ≈ `fold512_rc`). Against the true per-class default, the mirror tri is parity-to−3% packed on Zen 5 and parity on Emerald at the real ~wp 10.5 mix.
3. **Parity and zero-alloc held everywhere.** All 121 shards' D1 differential oracles, `HYDRA_BITPARITY` (golden `0x881639cead506f25`), and `ALLOC_DELTA == 0` passed on every arm — the arming was value-safe; the fleet rejected it on speed alone. (The single red shard, #109, tripped the R12 noisy-draw variance gate, cv 51.3% > 25% — the same statistical class as run #454's shard 38, unrelated to the arming.)

**The R22.1 correction (this commit), per the R9c→R9d law (a default that hurts any class does not ship):**

* The worker drain's default returns to the per-class sequential kernel (`kernel.eval`); `HFT_WORKER_TRI` is OPT-IN again (`=1` arms, CI arm 11l keeps the armed soak).
* `eval_tri`'s Reflect arm now runs the **value-exact mirror tri** — the shape whose endings stay cheap (no forced class machinery): the only T=3 configuration with a live mechanism in the L3-bound fabric (the split's latency slack is worth more under load latency, which is exactly the R11 hypothesis the packed kbench cannot price). Arm 11l prices it through the worker loop per draw; a future default flip follows the ≥3-healthy-draws certification law.
* The natural class-exact tri stays first-class as the attribution axis: `eval_tri_r` (new), the kbench `fold512_tri_r` row (re-pointed), the differential pins, and the constants law — the algebraic toolkit survives; only the default is gone.

The 1.15B msg/s verification gap remains open, and run #455 sharpened the map: the fold kernel is NOT the binding constraint at the real span mix (every T-variant prices parity-to-negative against the per-class default there). The gap lives in the fabric's supply/latency coupling — the R11 §8.1 analysis, now with 16 more draws of evidence — which points the next round at the selective supply-side folding and the transposed-arena lane, not at deeper interleave splits.

## 7. The #456 Verdict — the Correction Green, the T=3 Question Priced and Closed

The corrected commit (`b8fca42`) ran green across the fleet (run #456, 39 consolidated draws). Two verdicts:

1. **The reverted default recovered the record class.** Zen 5 11b sustained: median-of-medians 1.768B, healthy draws at 1.906–1.924B — the run #454 class restored (the R22-armed draws read 1.52–1.68B). Emerald and Granite back in their normal bands. All parity, alloc, variance and topology gates green.
2. **The value-exact mirror tri, priced through the real worker loop (arm 11l): no conversion.** Zen 5: −2.0% median (range −7.6%..+2.1%, two draws positive — parity within draw noise). Emerald: −5.9% median (−1.7..−12.1%). Granite: −2.8..−8.3%. The packed-kbench parity translated to a wash in the L3-bound fabric: the interleave's latency slack does not pay where the supply side binds. Per the R9c→R9d law the sequential per-class default stands; arm 11l remains the standing refutation ledger (the eval2/pipe/vecladder precedent) and the `fold512_tri`/`fold512_tri_r` kbench rows keep the kernel-level twins on every draw.

The round's net yield, in ledger form: the class-exact natural tri (constants, kernel, differential pins, P1'-at-k=3 test) is shipped algebra — available to any future shape that can carry its endings cheaply; the mirror tri through `eval_tri` is the standing armed-soak instrument; the worker default is the per-class sequential kernel, fleet-certified across 55 additional draws (16 on `1a66aff`, 39 on `b8fca42`); and the 2.5B frontier's map now says, with three independent lines of evidence (the T-family parity at the real mix, the 11fb serialization pricing, the 3.07B null ceiling), that the next lever lives in the fabric's supply/latency coupling — selective supply-side folding and the transposed-arena lane — not in deeper fold interleave.
