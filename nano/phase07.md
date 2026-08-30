Audit first — and this time there's a new problem category, worse than missing evidence. Then the doc.

## The G6 audit: three findings, one of them structural

**1. You answered every question except the one I asked.** ED-06 item 4 required one line: *which deviation caused the impossible watermark — (a) split disabled in config, (b) S2 doesn't restart seq, (c) hash ignored seq?* Your report is silent, and it matters more than bookkeeping: each answer implies a different bug (config plumbing / fabricator violating doc 04 §6.5 / sink contract violation). Without it, the claimed E2E-1 re-run can't be evaluated — and note the re-run itself is claimed by implication only: no VERDICT line, no `final_wm == N−m+1` value, just a doc edit saying the criteria changed. A spec update is not a test result.

**2. The CI pin is verified by nothing.** "Committed locally (88094e6) without pushing" — so the ci.yml hash-check step you updated has *never executed*. A pinned hash no machine has ever checked is decoration. The fix is one command: `git push`. This is the fifth consecutive gate with zero run URLs, which leads to—

**3. The structural one: `12-gates.md` is becoming a liar.** Your report says G5/G6 entries were "updated" — presumably toward PASSED — while the evidence ledger sits empty. That file is the project's conscience; the moment it records narration as fact, every gate after it is theater and we've rebuilt the exact "unprovable SUCCESS" pathology this whole apparatus was designed to kill. **G6 must be recorded as `OPEN-EVIDENCE` with a closure checklist**, not PASSED. This is non-negotiable and it's the cheapest fix on the list.

**What genuinely progressed** — credit where due: C8 applied across five files, hash regenerated consistently with the changed fold, CF-1..6 with per-file specificity, mint sites reported with line numbers. That's the most internally consistent report so far. It's also completely unverifiable from here, which is the entire problem.

**The boundary, stated once:** doc 07 drops below because that's my half of the labor split. But G7's first acceptance item is `git push` + a run URL, and **if the next message in this thread arrives without a run URL, no doc 08 — we fix the pipeline instead.** Design debt I can absorb indefinitely; evidence debt compounds.

Save as `~/Hft-fund/docs/07-zeroalloc.md`:

````markdown
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
| __-__-__ | 1.0 | Initial: three layers, window definition, tier law + self-test, traps registry, two-sink law, PR-3 two-stage discharge. |
````

---

## ED-07 — What you build NOW (gate G7)

1. **`nf-engine/src/alloc.rs`** — counting allocator per §3-L1; `--alloc-window` + `--startup-probe` flags in the replay binary; non-zero exit on Δ≠0.
2. **Lint tiers** — `#![cfg_attr(not(test), deny(clippy::disallowed_types))]` in nf-protocol/nf-arbitrator lib + `nf-transport` split into `sched_types` (Tier S) / `render` (Tier F); Tier-M module attribute on the engine loop; `tests/lint_fixture/` with one violation per banned item.
3. **CI lanes** — the three jobs above; **and push the repo** so they actually exist.
4. **Run all lanes on Termux too** — this is your machine; L3 on aarch64 is where surprises live.

**G7 acceptance:** run URLs for all lanes (the push happens first) · `ALLOC_DELTA=0` Termux + CI · strace diff empty both · lint self-test log showing every violation fired · **plus the four G6 closure artifacts:** the (a)/(b)/(c) answer, the E2E-1 re-run VERDICT line verbatim, the raw mint-site grep, and `12-gates.md` corrected so G6 reads `OPEN-EVIDENCE` until those artifacts exist · say **freeze 07**.

Then **engineer 08** — the endgame: the fake retransmission server, the non-blocking TCP client, the SPSC mailbox, widen-and-supersede made real, and TEST-DUAL-DROP-GAPFILL — the moment we deliberately vanish a range from both feeds and watch your engine refuse to lie about it.
