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
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --runs 5 --study | tee /tmp/bench.txt
grep -q "STUDY_REPORT written to" /tmp/bench.txt
grep -q "allocs=0" /tmp/bench.txt

echo "=== 12. Reference Arbitrator & Differential Oracle (G12-T3 / D1..D8) ==="
# R-1 Independence Grep Audit
! grep -E "nf_arbitrator|nf_protocol" crates/nf-testkit/src/reference.rs || (echo "R-1 violation: reference arbitrator contains forbidden imports" && exit 1)
cargo run --release -p nf-testkit --bin diff_oracle | tee /tmp/diff_oracle.txt
grep -q "ALL D1..D8 DIFFERENTIAL ORACLE CHECKS PASSED SUCCESSFULLY" /tmp/diff_oracle.txt

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

echo "=== ALL CHECKS PASSED SUCCESSFULLY ==="
