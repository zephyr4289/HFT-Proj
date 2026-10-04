#!/usr/bin/env bash
set -e

# P0 GH tuning: honor external RUSTFLAGS if set (ci.yml znver3), else fallback to native for local/Termux
# Keep RUSTFLAGS minimal (target-cpu only); profile controls lto/codegen/opt/strip.
if [ -z "${RUSTFLAGS:-}" ]; then
  export RUSTFLAGS="-C target-cpu=native"
  echo "RUSTFLAGS (auto-native): $RUSTFLAGS"
else
  echo "RUSTFLAGS (env): $RUSTFLAGS"
fi
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_RELEASE_LTO=fat
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1

echo "rustc: $(rustc -Vv 2>&1 | head -n 1)"
rustc --print cfg 2>&1 | grep -E "target_arch|target_cpu|target_feature" | head -n 20 || true

# P5 GH affinity: pin bench-critical steps to single core + warmup page/icache.
# taskset optional (fallback unpinned); nproc + cpuinfo logged for provenance.
echo "CPUS: $(nproc 2>&1 || echo unknown)"
grep -m1 "model name" /proc/cpuinfo 2>&1 || true
# R8: topology provenance — sibling groups + physical count (drives the
# fabric's thread sizing; see nf-testkit/src/affinity.rs).
for c in $(seq 0 $(( $(nproc) - 1 ))); do
  echo "cpu$c siblings: $(cat /sys/devices/system/cpu/cpu$c/topology/thread_siblings_list 2>/dev/null || echo n/a) L3: $(cat /sys/devices/system/cpu/cpu$c/cache/index3/shared_cpu_list 2>/dev/null || echo n/a)"
done
if command -v taskset >/dev/null 2>&1; then
  echo "taskset: $(taskset -pc $$ 2>&1 || true)"
  export HFT_TASKSET="taskset -c 1"
else
  echo "taskset: unavailable (unpinned fallback)"
  export HFT_TASKSET=""
fi

LOGDIR="${RUNNER_TEMP:-/tmp}/ci-logs"
mkdir -p "$LOGDIR"

echo "=== 1. Mini Sample SHA256 Check ==="
echo "5e347abbaa69f12226a6506e875f51633af690b3fc890d9d20a7213fe73275c9  data/tests/sample-mini.itch" | sha256sum -c -

echo "=== 2. Build Workspace Release ==="
cargo build --workspace --release

echo "=== 3. Clippy Workspace ==="
cargo clippy --workspace --all-targets -- -D warnings

echo "=== 4. Negative Lint Tripwire Test ==="
(! cargo clippy --manifest-path tests/lint_fixture/Cargo.toml -- -D warnings)

echo "=== 5. Unit & Conformance Tests ==="
# F-5 Deletion Grep Audit (Mailboxes & CmdChannel deleted per C9)
! grep -rnE "PacketMailbox|CmdChannel" crates/ || (echo "F-5 violation: Thread R mailboxes/channels still present in crates/" && exit 1)
TRYBUILD=overwrite cargo test --workspace -- --include-ignored

echo "=== 6. Full Day Audit & Histogram Diff ==="
cargo run --release -p nf-engine --bin audit -- data/tests/sample-mini.itch | tee /tmp/h.txt
diff /tmp/h.txt data/tests/mini-histogram.txt

echo "=== 7. Replay Conformance & Golden Hash Check ==="
cargo run --release -p nf-engine --bin replay -- --config ci-mode1.toml | tee /tmp/verdict.txt
grep -q "VERDICT hash=0xF6EF154EFDE905D8 count=505849 watermark=255850 violations=0" /tmp/verdict.txt

echo "=== 8. Zero-Allocation Window (ALLOC_DELTA=0) ==="
./target/release/replay --config ci-mode1.toml --alloc-window | tee /tmp/alloc.txt
grep -q "ALLOC_DELTA=0" /tmp/alloc.txt

echo "=== 9. Kernel Syscall Strace Diff Probe ==="
strace -e trace=mmap,brk,munmap -o /tmp/strace-base.raw ./target/release/replay --config ci-mode1.toml --startup-probe
strace -e trace=mmap,brk,munmap -o /tmp/strace-full.raw ./target/release/replay --config ci-mode1.toml
bash scripts/normalize_strace.sh /tmp/strace-base.raw /tmp/strace-base.txt
bash scripts/normalize_strace.sh /tmp/strace-full.raw /tmp/strace-full.txt
diff -u /tmp/strace-base.txt /tmp/strace-full.txt

echo "=== 10. Venue Sender & XDP Transport Smoke Check ==="
cargo run --release -p nf-testkit --bin venue -- --sample data/tests/sample-mini.itch

echo "=== 11. Benchmark & G12-T1 Tail Attribution Study ==="
# P5 warmup: single cold-arm run discards page-fault + freq-ramp noise (H1/H3), ~seconds.
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --runs 1 --arm cold > /dev/null 2>&1 || true
$HFT_TASKSET cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --runs 5 --study | tee /tmp/bench.txt
grep -q "STUDY_REPORT written to" /tmp/bench.txt
grep -q "allocs=0" /tmp/bench.txt
grep -q "PR2_PROD_VERDICT" /tmp/bench.txt
# R4: PR-1 TITAN gate — burst + sustained span-conformance arms must PASS (>= 100M msg/s)
grep -q "PR1_TITAN_VERDICT.*-> PASS" /tmp/bench.txt
grep -q "PR1_TITAN_SUSTAINED_VERDICT.*-> PASS" /tmp/bench.txt

echo "=== 11b. R6: PR-1 HYDRA Bit-Exact Multi-Core Span Conformance (UNPINNED) ==="
# R6: the HYDRA fabric spans the runner's vCPUs — MUST run unpinned (a
# single-core taskset would collapse it to inline mode). Warmup first
# (thread spawn + page warm + scheduler settling), then the gate arms.
# The arm itself asserts, on EVERY run: (a) hydra == sequential
# SpanConformanceSink bit parity, (b) measured == reference pass,
# (c) ALLOC_DELTA == 0. Gates-as-Code: PR1_HYDRA_MIN_MSG_PER_SEC (gates.rs)
# is the single threshold source — see docs/20-hydra.md for the topology
# guidance (4-vCPU runner; adjust the constant if the pool's silicon differs).
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --hydra-only --runs 1 > /dev/null 2>&1 || true
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --hydra-only --runs 7 2>&1 | tee /tmp/bench_hydra.txt
grep -q "HYDRA_BITPARITY.*-> BIT-EXACT" /tmp/bench_hydra.txt
grep -q "allocs=0" /tmp/bench_hydra.txt
grep -q "PR1_HYDRA_VERDICT" /tmp/bench_hydra.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_hydra.txt
# R8: the full-verify verdict line must be present and honestly evaluated
# (the 1B sustained target is OPEN — measured 288M on the Zen5 runner;
# enforcement lands when the fabric reaches it; see docs/22 §Physics).
grep -q "PR1_R8_FULL_VERIFY_VERDICT rate=" /tmp/bench_hydra.txt

# R10b: the blob's THP backing line is a first-class CI artifact — a draw
# that lost the hugepage grant must be VISIBLE, not silent (the 1.867B gate
# failure was unattributable before this line existed). Assert presence;
# grep the verdict per draw so the fish-history lands in the log summary.
grep -q "BLOB_BACKING" /tmp/bench_hydra.txt
grep "BLOB_BACKING" /tmp/bench_hydra.txt | head -1

echo "=== 11c. R8: Kernel-Ceiling Microbenchmark (fabric physics telemetry) ==="
# Diagnostics only — never gated. Prints the runner's measured CRC ceilings
# per kernel (scalar8lane / fold512 / pclmul128_raw / crc32:pclmul mix) and
# per placement (1 cpu / 2 distinct / 2 SMT), so the fabric's achieved
# numbers sit next to the machine's physics in every run log.
cargo run --release -p nf-engine --bin kbench | tee /tmp/kbench.txt
grep -q "KBENCH mode=scalar8lane threads=1" /tmp/kbench.txt
grep -q "KBENCH done" /tmp/kbench.txt

echo "=== 11d. R9: Fabric-Shape Kernel Ablation (layout attribution telemetry) ==="
# Diagnostics only — never gated. Decomposes the worker's real execution
# shape (P packed / K real-layout kernel-only / D +desc ring / R +res ring
# / F full replica) on the actual tape bodies, attributing per-span cycle
# costs to the layout, the handoff rings, and the kernel. Post-R9 the real
# blob is alias-deduplicated, so K tracks the packed P closely; any K-vs-P
# regression is a layout regression and must be investigated.
cargo run --release -p nf-testkit --bin fbench -- --stage all --workers 2 --ms 1000 | tee /tmp/fbench.txt
grep -q "FBENCH stage=P" /tmp/fbench.txt
grep -q "FBENCH stage=F" /tmp/fbench.txt
grep -q "FBENCH done" /tmp/fbench.txt

echo "=== 11e. R11: Unarmed Prepatch Soak (rollback evidence) ==="
# R11: the prepatch is DEFAULT ON (7/7 armed-wins on the R10 stack across
# Zen3 / 8573C / 8370C — see pipeline.rs R11 note); this arm keeps the
# UNARMED side of the ledger running on every push (HFT_PREPATCH=0 is the
# rollback). The default (armed) numbers are 11b above; the per-draw
# comparison lands in the log summary either way.
HFT_PREPATCH=0 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_prepatch.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_prepatch.txt
grep -q "allocs=0" /tmp/bench_prepatch.txt

echo "=== 11f. R9: Third-Worker Placement Sweep (RX-hyperthread scavenging) ==="
# The 3-worker shape (third lane on the RX's hyperthread) was only ever
# measured on AMD (-15% Zen3, +2% Zen5). Post-R9 the workers' delivery
# economics changed (aliasing + spray); this sweep measures the shape on
# EVERY runner draw the CI sees, with the assist containing any straggler
# lane by construction. Diagnostics only — never gated beyond the
# bit-exact asserts the sustained arm already enforces.
HFT_SUSTAINED_WORKERS=3 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_w3.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_w3.txt
grep -q "allocs=0" /tmp/bench_w3.txt

echo "=== 11g. R9: Worker eval2 Interleave Sweep (load-MLP experiment) ==="
# The kbench eval-vs-eval2 parity was measured on PACKED buffers; the
# post-aliasing real layout is L3-latency bound, where two concurrent
# span streams double the loads in flight. Diagnostics only.
HFT_WORKER_EVAL2=1 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_eval2.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_eval2.txt
grep -q "allocs=0" /tmp/bench_eval2.txt

echo "=== 11h. R9: Worker Prefetch Shape Sweep (deeper lead) ==="
# The spray default (2,22,24) was tuned on the shared-core sandbox; the
# dedicated worker pairs of the real runners may prefer a deeper lead.
# Diagnostics only.
HFT_PF_AHEAD=6 HFT_PF_LINES=32 HFT_PF_BURST=32 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_pf.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_pf.txt
grep -q "allocs=0" /tmp/bench_pf.txt

echo "=== 11i. R10: Assist-Ring Depth Sweep (the spin->CRC conversion budget) ==="
# The deep assist ring converts lane-full backpressure spins into
# in-window CRC on the submitting core. The depth bounds how far the
# submit point may run ahead of the fold before the conversion saturates:
# local sandbox 4/16/64/256 -> 266/343/422/442M. The 11b default arm
# carries slots=64; this sweep brackets the curve per runner class
# (shallow=4 reproduces the R8 equilibrium, deep=256 probes the lead).
# Diagnostics only — bit-exactness is asserted by every arm's per-pass
# tuple checks.
HFT_ASSIST_SLOTS=4 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_assist4.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_assist4.txt
grep -q "allocs=0" /tmp/bench_assist4.txt
HFT_ASSIST_SLOTS=256 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_assist256.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_assist256.txt
grep -q "allocs=0" /tmp/bench_assist256.txt

echo "=== 11j. R10: Worker Pipelined-Tail Sweep (sequential-load pair eval) ==="
# The deferred-ending schedule: consecutive span pairs evaluate through
# eval_pair (A's vector fold, B's vector fold, A's endings, B's endings —
# one sequential load stream, the per-span ending overhead hidden under
# the next span's clmul chains). Unlike eval2 (dead: -28%, interleaved
# loads thrash the streamer) the load order is unchanged. kbench's
# fold512_pair vs fold512 rows attribute the kernel-level effect on
# every fold512 draw. Diagnostics only.
HFT_WORKER_PIPE=1 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_pipe.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_pipe.txt
grep -q "allocs=0" /tmp/bench_pipe.txt

echo "=== 11k. R11: Distinct-Core Worker Placement Sweep (the SMT ceiling unlock) ==="
# THE PLACEMENT CEILING: on the 2-physical-core SMT draws the default
# (siblings) strategy stacks BOTH workers on one physical core's two
# hyperthreads — their combined fold is capped at the kbench 2cpu_smt
# ceiling (32.83 GB/s on the 1.109B draw) while 2cpu_distinct sat unused
# at 61.74 GB/s. HFT_FABRIC_PLACE=distinct gives each worker a physical
# core and demotes main+rx to the SMT siblings (each steals issue slots
# from exactly one worker). The worker DIAG lines echo the new pins;
# kbench's fold512 2cpu_distinct row prices the ceiling it chases.
# Diagnostics only — bit-exactness asserted by the arm's per-pass checks.
HFT_FABRIC_PLACE=distinct cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_place_distinct.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_place_distinct.txt
grep -q "allocs=0" /tmp/bench_place_distinct.txt

echo "=== 11l. R11: Tri-Stream Fold Sweep (3-way interleave ILP) ==="
# The fold kernel's two state chains run one clmul->xor->clmul->xor
# dependency per 128 body bytes; measured fold512 sits at ~9 cyc/step —
# near that chain's length. The tri-stream split (mod-3 block pairs,
# y^384 stream fold, fixed-power merge) gives the OoO engine six chains
# over ONE load stream at unchanged per-byte issue cost. kbench's
# fold512_tri vs fold512 rows (1t / 2cpu_distinct / 2cpu_smt) attribute
# the kernel effect; this sweep prices it through the worker loop on the
# real span mix. Diagnostics only — D11 + the crcfold differential sweep
# pin the values bit-exact.
HFT_WORKER_TRI=1 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_tri.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_tri.txt
grep -q "allocs=0" /tmp/bench_tri.txt

echo "=== 11m. R12: Vectorized Watermark Ladder ARMED Soak (the refuted experiment) ==="
# R12 verdict (the R9c->R9d law): the CI attribution measured the ladder
# at -5.3% sustained on the 8573C (11b 968.5M vs scalar 1,022.6M) and
# +1.0% on the 8370C — the classes disagree and the record class
# refutes, so the default is OFF and this arm runs the ARMED soak for
# the evidence ledger (the eval2/tri precedent). Front A recovers to
# ~R11 parity with it on; the sustained record gate is what it costs.
HFT_VEC_LADDER=1 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_vecladder_on.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_vecladder_on.txt
grep -q "allocs=0" /tmp/bench_vecladder_on.txt

echo "=== 11n. R12: Compact 8-Byte Span Descriptors OFF (rollback attribution) ==="
# The R12 Desc8 format: {offset:u32 | len:u16 | flags:u16} — 8 descs per
# 64B L1 line (vs 4), one u64 store per span, span ids derived worker-side
# from per-chunk anchor descs (robust to assist diversion; the fold-order
# assert pins the derivation). 11b runs it ON; this arm runs the legacy
# 16-byte descriptor format for per-draw attribution.
HFT_DESC8=0 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_desc8_off.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_desc8_off.txt
grep -q "allocs=0" /tmp/bench_desc8_off.txt

echo "=== 11r. R13: Mirror-Domain Fold Kernel Soak (rollback attribution) ==="
# The R13 natural-domain (reflected) fold: per 128B step the mirror kernel
# issues 8 port-5 uops (4 clmul + 2 unpck + 2 pshufb) — the measured wall
# (docs/26 §1: +1 p5 op = +0.95 cyc/step). The reflect kernel drops the
# GFNI bit-reverse AND both vpshufb bswaps (units enter as raw LE loads,
# constants = ISA-L's CRC32C fold_1x128b pair) -> 6 p5 uops/step, ~+44%
# step density measured locally. 11b runs reflect ON by default; this arm
# soaks the legacy mirror kernel for per-draw attribution (the 11m/11n
# precedent). kbench's fold512 vs fold512_r rows give the kernel-level
# split on the same draw.
HFT_CRC_KERNEL=fold512 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_mirror_kernel.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_mirror_kernel.txt
grep -q "allocs=0" /tmp/bench_mirror_kernel.txt

echo "=== 11s. R14: Vector Barrett Ending OFF (rollback attribution) ==="
# The R14 ending diet: the reflect kernel's per-span ending replaces the
# 16 chained crc32 u64 instructions + their store/reload round-trip with
# an in-register vector Barrett (per zmm: 5 clmul + 2 vpalignr — the
# cross-qword byte shifts 32/56/32 are the unique byte-aligned triple that
# closes exactly; constants VR0/VH64/VM/VMU derived + basis-exhaustively
# pinned by t_vend_constants_derivation, docs/27). The real-mix span pays
# ~93 cyc/span over the packed loop's ~91 (endings + supply + ring, the
# R13 record draw's worker telemetry) — this lever attacks the endings'
# share. 11b runs vend ON by default; this arm runs the R13 crc-chain
# ending for per-draw fabric attribution. kbench's fold512_r vs fold512_rc
# rows give the kernel-level split on the same draw.
HFT_CRC_VEND=0 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_vend_off.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_vend_off.txt
grep -q "allocs=0" /tmp/bench_vend_off.txt

echo "=== 11t. R15: Vectorized Tail (vtail) OFF (rollback attribution) ==="
# The R15 vtail: lane 0's post-loop tail (the extra fold units + r0 bytes,
# |R| = 8*(B%2)+tail bytes) is absorbed into the vend input field via the
# length-indexed y-power tables G/KH/AT (scripts/r15_tail_derive.py; the
# chain decomposition lane = Z_r(vend(V)) XOR rawCRC(R) with everything in
# the ring GF(2)[y]/VM). The serial extract -> fold_extra chain -> 2-3
# chained crc32 (~25-60 cyc for r >= 16) becomes 2 lift clmuls + <=9
# INDEPENDENT data clmuls + vend_xmm (~22 cyc) — the r >= 16 gate skips
# the cheap r <= 8 spans where the old path has no serial chain to
# eliminate (the packed-loop evidence). 11b runs vtail ON by default
# (SPR+); this arm runs the R14 tail shape for per-draw attribution (the
# 11r/11s precedent). kbench's fold512_rv vs fold512_r rows give the
# kernel-level split on the same draw.
HFT_CRC_VTAIL=0 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_vtail_off.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_vtail_off.txt
grep -q "allocs=0" /tmp/bench_vtail_off.txt

echo "=== 11u. R15: Worker Drain Granularity Sweep (the supply-side rebalance) ==="
# docs/27 §7's third queue item: with the R13/R14/R15 kernel gains the
# workers drain faster, and the batch shape that paced the result
# publications against main's ordered fold may want re-tuning per class.
# HFT_WORKER_BATCH (a multiple of the 64-span CHUNK, clamped to [64, 256])
# is read once per worker spawn; the default 128 is the R8 shape. Two
# soaks per draw price the direction; the evidence ledger records whatever
# the silicon says.
HFT_WORKER_BATCH=64 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_wbatch_64.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_wbatch_64.txt
grep -q "allocs=0" /tmp/bench_wbatch_64.txt
HFT_WORKER_BATCH=256 cargo run --release -p nf-engine --bin bench -- --hydra-only | tee /tmp/bench_wbatch_256.txt
grep -q "PR1_HYDRA_SUSTAINED_VERDICT" /tmp/bench_wbatch_256.txt
grep -q "allocs=0" /tmp/bench_wbatch_256.txt

echo "=== 12. Reference Arbitrator & Differential Oracle (G12-T3 / D1..D12) ==="
# R-1 Independence Grep Audit
! grep -E "nf_arbitrator|nf_protocol" crates/nf-testkit/src/reference.rs || (echo "R-1 violation: reference arbitrator contains forbidden imports" && exit 1)
cargo run --release -p nf-testkit --bin diff_oracle | tee /tmp/diff_oracle.txt
grep -q "ALL D1..D12 DIFFERENTIAL ORACLE CHECKS PASSED SUCCESSFULLY" /tmp/diff_oracle.txt

echo "=== 13. T2 Window Sweep & Full 17-Cell Matrix Confluence Campaign ==="
cargo run --release -p nf-testkit --bin window_sweep | tee /tmp/window_sweep.txt
grep -q "T2 WINDOW SWEEP COMPLETED SUCCESSFULLY" /tmp/window_sweep.txt
cargo run --release -p nf-testkit --bin matrix_sweep | tee /tmp/matrix_sweep.txt
grep -q "ALL 17 MATRIX CELLS VERIFIED 100% GREEN" /tmp/matrix_sweep.txt

echo "=== 14. VR-4 Hostile Frame & Fuzz Campaign (3 Harnesses) ==="
cargo run --release -p nf-testkit --bin fuzz_campaign | tee /tmp/fuzz_campaign.txt
grep -q "VR-4 FUZZ CAMPAIGN 100% COMPLETE AND VERIFIED" /tmp/fuzz_campaign.txt

echo "=== 15. Spec-Only Retransmission Server Clean-Room Validation (doc 14 §3.2 / F-31) ==="
cargo run --release -p nf-testkit --bin spec_server | tee /tmp/spec_server.txt
grep -q "SPEC-ONLY SERVER CLEAN-ROOM VALIDATION PASSED" /tmp/spec_server.txt

echo "=== 16. HFT-Verify Statistical Gate (nano/constr1.1.md §1: 30 runs + warmup 5 + JSON) ==="
# Musl static build attempt (constr1.1.md requires musl target); gnu fallback keeps gate running.
HFT_BIN="target/release/hft_bench"
if ! rustup target list --installed 2>/dev/null | grep -q "x86_64-unknown-linux-musl"; then
  rustup target add x86_64-unknown-linux-musl || echo "MUSL_TARGET_UNAVAILABLE fallback gnu"
fi
if ! command -v musl-gcc >/dev/null 2>&1; then
  sudo apt-get update -qq && sudo apt-get install -y -qq musl-tools || echo "MUSL_TOOLS_UNAVAILABLE fallback gnu"
fi
if cargo build --release --target x86_64-unknown-linux-musl -p nf-engine --bin hft_bench 2>/tmp/musl_build.log; then
  echo "MUSL_BUILD_OK static target"
  HFT_BIN="target/x86_64-unknown-linux-musl/release/hft_bench"
else
  echo "MUSL_BUILD_FAILED fallback gnu (tail):"
  tail -n 20 /tmp/musl_build.log || true
  echo "MUSL_BUILD_FAILED fallback gnu" > /tmp/musl_build.log
fi
# R8: hft_bench runs the RX-pipelined span arm (2 threads) — the external
# single-core taskset would timeslice them. The binary pins its own threads
# topology-aware (main -> first allowed cpu, RX -> second); the classic
# per-message arm runs on the pinned main thread (deterministic, as before).
"$HFT_BIN" --sample data/tests/sample-mini.itch --runs 30 --warmup 5 --output-format json | tee /tmp/bench_results.json
grep -q "median_cycles" /tmp/bench_results.json
python3 - <<'PYEOF'
import json, sys
with open('/tmp/bench_results.json') as f:
    r = json.load(f)
required = ['median_cycles', 'p95_cycles', 'p99_cycles', 'stddev', 'cv_percent',
            'span_median_cycles', 'span_rate_msg_per_sec']
for k in required:
    if k not in r:
        print(f'MISSING METRIC: {k}')
        sys.exit(1)
constraints = {
    'median_cycles': {'max': 25.0, 'unit': 'cycles/msg'},
    'p95_cycles': {'max': 35.0, 'unit': 'cycles/msg'},
    'p99_cycles': {'max': 50.0, 'unit': 'cycles/msg'},
    'stddev': {'max': 2.5, 'unit': 'cycles'},
    'cv_percent': {'max': 25.0, 'unit': '%'},
}
failed = []
# R4: PR-1 TITAN — span arm wall-rate (count sink, closed-form emission) >= 100M msg/s
if r['span_rate_msg_per_sec'] < 100_000_000:
    failed.append(f"span_rate_msg_per_sec: {r['span_rate_msg_per_sec']} < 100000000 msg/s (PR1_TITAN)")
# R8: PR-1 pure ingest — 2B msg/s on the RX-pipelined span arm (gates.rs
# single threshold source; the verdict field is computed in Rust from the
# same statistical median). ENFORCED since the R8 branch stabilized six
# consecutive runner passes (2.04-2.48B on Zen3, 4.08B on Zen5).
if r.get('r8_pure_ingest_verdict') != 'PASS':
    failed.append(f"r8_pure_ingest_verdict: {r.get('r8_pure_ingest_verdict')} (R8 pure ingest 2B gate — see gates.rs PR1_R8_PURE_INGEST_MIN_MSG_PER_SEC)")
for metric, rule in constraints.items():
    val = r[metric]
    if val > rule['max']:
        failed.append(f'{metric}: {val} > {rule["max"]} {rule["unit"]}')
if failed:
    print('CONSTRAINT VIOLATIONS:')
    for f in failed:
        print(f'  - {f}')
    sys.exit(1)
print('ALL CONSTRAINTS PASSED')
PYEOF

echo "=== ALL CHECKS PASSED SUCCESSFULLY ==="
