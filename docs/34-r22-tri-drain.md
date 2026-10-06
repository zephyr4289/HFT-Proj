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
