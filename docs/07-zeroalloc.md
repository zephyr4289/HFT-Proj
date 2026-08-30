# 07 — Zero-Allocation Enforcement

```
Status:    DRAFT → FROZEN after G7
Exit Gate: ALLOC_DELTA=0 on the mode-1 mini window (Termux AND CI);
           strace delta == 0 both environments; lint tiers active and
           lint self-test proves the tripwire fires; CI URLs exist.
Evidence:  Run URLs for all four lanes; raw ALLOC_DELTA + strace diff
           output pasted into Appendix A; lint self-test log.
Authority: This doc owns the three enforcement layers, the measured
           window definition, the lint tier law, and the two-sink law.
           Hot-path design: docs 02/03/05. Recovery-thread obligations:
           doc 08. Bench methodology: doc 11.
Rule:      Zero-allocation is enforced by MACHINERY, not review.
           PR-3 is discharged in two stages: G7 = UDP-path window;
           G8 = recovery-inclusive window (doc 00 changelog records
           this sequencing).
```

---

## 1. Scope: what "zero-alloc" binds

**Bound (PR-3):** from WINDOW_BEGIN to WINDOW_END (§2), the process
performs zero heap allocations and zero heap frees. All lanes run the
identical mode-1 chaos schedule (mini sample) used by E2E-1.

**Not bound:** startup (schedule build, mmap, arena, sink construction —
O-6 permits one allocating phase), the verdict/status printing *after*
WINDOW_END, testkit correctness harnesses (§5), and the audit tool.

**Two-stage discharge:** G7's window includes reorder gaps, single-feed
loss recovery via the alternate feed, staging, drain, session boundary —
the complete arbitrator machinery. TCP gap-fill enters the window at G8,
where Thread R and its client must uphold the same law (obligations
pre-declared in §4 so doc 08 inherits them, not discovers them).

## 2. Measured Window (normative)

```
WINDOW_BEGIN: schedule built · mmap mapped · render arena allocated ·
              sink constructed · counters snapshotted
    …replay loop: poll → ingest → sink (HashSink) → intent eval…
WINDOW_END:   final EOS processed for both feeds
assertion:    allocs(BEGIN..END) == 0  ∧  deallocs(BEGIN..END) == 0
```

The binary gains `--alloc-window`: prints `WINDOW_BEGIN` /
`WINDOW_END` / `ALLOC_DELTA=<n>` and **exits non-zero if Δ≠0**. CI greps
`ALLOC_DELTA=0`. No human judgment in the loop.

## 3. Three Enforcement Layers

### L1 — Counting global allocator (behavioral truth)

`nf-engine/src/alloc.rs`: `#[global_allocator]` wrapping `System`, two
`AtomicU64` counters (`allocs`, `deallocs`), **Relaxed** ordering —
correct because the assertion is read at window boundaries, not
racing. The wrapper itself never allocates. Always-on in engine
binaries: on a truly zero-alloc path the counters are never touched —
the layer costs nothing it doesn't catch. Getter exposed for the
`--alloc-window` instrumentation.

### L2 — Lint tier law (structural prevention)

| Tier | Crates/modules | Law |
|---|---|---|
| **F (full)** | `nf-protocol` lib, `nf-arbitrator` lib, `nf-transport::render` | `deny(clippy::disallowed_types)`: Vec, String, Box, Rc, Arc, PathBuf, HashMap/HashSet, BTreeMap/BTreeSet — these crates have zero legitimate container use |
| **M (module)** | `nf-engine` hot-loop module | `deny(clippy::disallowed_methods)`: format, to_string, to_owned, collect, Vec::push/insert/with_capacity/reserve |
| **S (startup)** | `nf-transport::sched_types`, engine startup/config, testkit | No structural ban — L1 owns behavior |

Applied via crate/module attributes gated `cfg_attr(not(test), …)` so
unit tests keep their conveniences. **`nf-transport` splits into
`sched_types` (owns the startup `Vec<SchedEvent>`) and `render`
(container-free)** — module structure enforces the tier boundary.

**Lint self-test (the tripwire test):** a committed fixture crate
containing one violation of every banned type and method. CI runs
`clippy` on it and **expects failure** (`! cargo clippy …`) — if any
violation passes silently, the lane exits 0 on the negation and fails
CI. A lint configuration you have never negative-tested is a security
blanket, not a control. Verify exact attribute syntax against current
clippy docs during implementation; the self-test is what proves you got
it right.

### L3 — Syscall lane (kernel's testimony)

```
build release
strace -f -e trace=mmap,brk,munmap -o base.txt  ./replay --startup-probe
strace -f -e trace=mmap,brk,munmap -o full.txt  ./replay --config ci-mode1.toml
diff base.txt full.txt   # must be EMPTY
```

`--startup-probe`: constructs everything (schedule, mmap, arena, sink),
prints startup counters, exits before the loop. Same startup path as
the full run — so the diff isolates exactly the measured window's
syscall behavior. **Delta is asserted exactly zero.** Noise is
investigated, never absorbed into an allowance. Run on CI **and**
Termux (`pkg install strace`). (strace the binary directly, never
`cargo run`, or the build system's mmaps pollute the trace.)

## 4. Traps Registry (normative enumeration — each is a past production bug somewhere)

| Trap | Law here |
|---|---|
| `format!`/`println!` on rare paths | Tier-M ban; verdict printing is post-WINDOW_END |
| `Box<dyn Error>` on error paths | Tier-F ban on Box; engine run-fn returns a status enum |
| Panic message formatting (allocates before abort) | Unreachability is VR-4's proof obligation; release `panic=abort` limits blast |
| DNS resolution (`std::net` hostnames) | **Doc 08 pre-obligation:** R connects by IP literal only |
| Thread spawn (allocates closure + parker) | **Doc 08 pre-obligation:** R spawned once in startup phase, never in window |
| `std::net::TcpStream::connect` internals | **Doc 08:** raw `libc` sockets if the std path allocates (doc 01 R-1 resolves here) |
| Harness conveniences in the sink | §5 two-sink law |

## 5. Two-Sink Law

The alloc lane and the correctness lane run **different sinks by
design**:

- **HashSink** — O(1) state, zero-alloc by construction. THE sink of
  the alloc window. Its state is two u64s; it cannot allocate.
- **ConformanceSink** — correctness runs only; MAY allocate (event
  ledgers, checkers). Its checks (gen law, pairing, monotonicity) are
  what the alloc lane forgoes — deliberately, because measurement
  windows must not be polluted by harness convenience, and harness
  correctness must not be crippled for measurement.

Same schedule, same config, two lanes, two verdicts. Both must pass.

## 6. CI Lanes (ci.yml additions)

```yaml
  alloc-lane:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo build --release -p nf-engine
      - run: ./target/release/replay --config ci-mode1.toml --alloc-window
  strace-lane:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: sudo apt-get update && sudo apt-get install -y strace
      - run: cargo build --release -p nf-engine
      - run: |
          strace -f -e trace=mmap,brk,munmap -o base.txt ./target/release/replay --startup-probe
          strace -f -e trace=mmap,brk,munmap -o full.txt ./target/release/replay --config ci-mode1.toml
          diff base.txt full.txt
  lint-lane:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: '! cargo clippy --manifest-path tests/lint_fixture/Cargo.toml -- -D warnings'
```

## Appendix A — Evidence (fill at G7)

```
(paste: ALLOC_DELTA lines Termux+CI · strace diff (empty) · lint
self-test output showing each violation fired · run URLs)
```

## Changelog

| Date | Version | Entry |
|---|---|---|
| 2026-08-30 | 1.0 | Initial: three layers, window definition, tier law + self-test, traps registry, two-sink law, PR-3 two-stage discharge. |
