#!/usr/bin/env bash
set -e

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

echo "=== 11. Benchmark & Build Separation Law ==="
cargo run --release -p nf-engine --bin bench -- --sample data/tests/sample-mini.itch --runs 5 | tee /tmp/bench.txt
grep -q "BENCH_MEDIAN" /tmp/bench.txt
grep -q "allocs=0" /tmp/bench.txt

echo "=== ALL CHECKS PASSED SUCCESSFULLY ==="
