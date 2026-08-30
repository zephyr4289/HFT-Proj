Division of labor locked: **I architect, you build.** One status note first — `00` is still DRAFT until you run Appendix A (magic bytes + hashes) on that BX file when the download lands; don't lose that thread. `01` below is written to be saved verbatim, then ED-01 tells you exactly what code to scaffold tonight.

Save as `~/Hft-fund/docs/01-architecture.md`:

````markdown
# 01 — System Architecture

```
Status:    DRAFT → FROZEN after comprehension gate (§11) passes
Exit Gate: A reader answers all five §11 questions unaided;
           ED-01 scaffold (G1) compiles on Termux + CI.
Evidence:  cargo build/clippy output attached to G1 in 12-gates;
           ADR-0001/0005 entries exist in 13-journal.
Authority: This doc owns STRUCTURE and PROOF. Field layouts: 02/03.
           Data-structure internals: 05. Policies: 08.
Rule:      Any change here after freeze = ADR entry in 13-journal.
```

---

## 1. System Shape

The engine is a **single-writer commutative fold over a set-union**.
Everything else — windowing, mailboxes, TCP recovery — is plumbing around
that one idea.

```
            ┌──────────────────────── Thread H (hot) ────────────────────────┐
            │                                                                │
 Feed A ──▶ │ poll(A) ──┐                                                   │
 (UDP/XDP)  │           ├──▶ ingest(frame) ──▶ [dup? drop] ──▶ [contiguous?  │
 Feed B ──▶ │ poll(B) ──┘        │              emit, zero-copy]             │
 (UDP/XDP)  │                    └─[ahead? stage 64B slot]─┐                 │
            │                                                 ▼             │
            │  poll(mailbox) ──▶ ingest(frame)  ◀── identical path ──┤       │
            │                                     (recovery TCP)     ▼       │
            │  recovery_intent(now) ──▶ CmdMailbox ──┐        Sink (LOB stub)│
            └────────────────────────────────────────┼───────────────────────┘
                                                     ▼
            ┌──────────────────────── Thread R ──────────────────────────────┐
            │ encode MoldUDP64 request → non-blocking TCP send → parse       │
            │ response frames → push WHOLE packets into PacketMailbox        │
            │ (SPSC 16×1500B; if full: park. Never drop. Never allocate.)    │
            └────────────────────────────────────────────────────────────────┘
```

**Steady-state property (SP-1):** in the lossless steady state the reorder
window is never written, the mailbox is empty, and one loop iteration is:
`poll → parse 20-byte header → one watermark compare → emit slice into frame →
recycle frame`. The entire disorder machinery is *off* the steady path.
Every design decision below exists to protect SP-1.

**Why one hot thread (vs. the enterprise sharded pipeline):** at the target
rate there is nothing to parallelize *within* one session — sequence numbers
are a total order, so arbitration is inherently sequential. Parallelism buys
nothing here but coherence traffic. Peak TotalView ≈ 1.3M msg/s; single-core
budget is 10M msg/s (PR-1) = 7× headroom. Multi-core multi-venue aggregation
is NG-4 and out of scope. The incumbent's 32-thread feed handler is solving
a problem this architecture refuses to have.

---

## 2. The Confluence Lemma (the load-bearing wall)

### 2.1 Definitions

**D1 (Delivery universe).** For a session anchored at sequence `a`, `U ⊆ ℕ`
is the set of message sequence numbers delivered to `ingest()` by ANY
transport — Feed A, Feed B, or TCP recovery. Deliveries are ranges
`[s, s+c-1]` (MoldUDP64 packet, `c ≥ 1`).

**D2 (Coverage).** `cov(U) = [a, e)` — the maximal contiguous prefix of `U`
starting at the anchor. If `a ∉ U`, coverage is empty.

### 2.2 Lemmas

**L1 (Watermark purity).** At every quiescent point (after any `ingest` +
drain), the watermark equals the coverage boundary:

    W = e  where cov(U_processed) = [a, e)

*Proof.* W only advances via the drain rule "advance while the next sequence
is present," and W is initialized to the anchor. By induction: if
`W = e(U)` holds and a delivery adds range R, then either R ∩ [W,∞) = ∅
(duplicate — W unchanged, still `e(U)`, since U's prefix is unchanged) or
R extends the contiguous prefix, and drain advances W exactly to the new
`e(U ∪ R)`. W can never skip a missing sequence, so it can never exceed
coverage. ∎

**L2 (Confluence).** The emitted message stream is the ordered enumeration of
`cov(U_processed)` at every quiescent point. Therefore, for any two delivery
schedules σ₁, σ₂ (any interleaving, any feed assignment, any loss + recovery
pattern) with the same final universe `U_final`, the final emitted streams
are **byte-identical**.

*Proof.* Emission happens only inside the contiguous branch, only at
sequence W, followed by `W += 1` (FR-2). Combined with L1, the emitted
prefix after processing any D is exactly `cov(U_D)` enumerated in order — a
pure function of `U_D`, containing no schedule, feed, or timing term. ∎

### 2.3 Corollaries (each one deletes a subsystem)

| # | Corollary | Subsystem deleted |
|---|---|---|
| C-1 | Duplicate suppression and idempotent recovery injection are the *same compare* (`last < W`) | dedup ledger, "have I applied this recovery packet" bookkeeping |
| C-2 | The golden byte-identical test (VR-5) is the empirical form of L2 | whole classes of order-sensitivity tests |
| C-3 | Feed arbitration is emergent — union, not choice. The faster copy wins by physics; there is no "feed selector" to tune or get wrong | A/B decision logic, feed health scoring |
| C-4 | Recovery splicing needs no merge logic — recovered packets enter the identical ingest path | splice/merge coordinator |

### 2.4 Scope limits (state these or the lemma is a lie)

- **S-1 — Events are NOT confluent.** `GapOpened` timing, `gen` values, and
  gap extents depend on arrival order. They are *observation telemetry*, not
  data. Golden tests compare the **message stream** byte-for-byte and check
  events only against invariants (§7). Anyone who conflates these two is
  selling something.
- **S-2 — `staged` is a cache, not truth.** Messages delivered while
  beyond-window are dropped from staging; `staged ⊆ U ∩ [W, W+1024)` may be
  an under-approximation. A cache miss is repaired by recovery re-delivery
  (C-1 makes that free). L1/L2 depend only on `U`, never on `staged`.
- **S-3 — Per-session.** L2 is stated per session. Cross-session output is
  the concatenation under Assumption **A-2**: no session-k+1 deliveries begin
  before session-k's universe is complete. Real feeds are time-ordered; the
  replay fabricator enforces it.
- **A-1 (anchor agreement):** both feeds agree on the session's starting
  sequence. NASDAQ guarantee; fabricator respects it.

### 2.5 Harness contract TH-1 (liveness of tests)

Every test schedule MUST terminate with: heartbeat announcing final seq on
both feeds, then EOS on both feeds. This guarantees every schedule's `U`
closes, so L2 is checkable at end-of-session. (Real exchanges heartbeat at
1 Hz — same mechanism.)

---

## 3. Threading & Ownership Model

| Thread | Owns | Touches sequencer state? | Syscalls in steady state |
|---|---|---|---|
| **H (hot)** | sequencer, transports, sink, both mailboxes (consumer/producer sides) | YES — sole writer | zero (XDP rings are memory; replay is mmap) |
| **R (recovery)** | TCP fd, request encoder, response parser buffers | NO — never | send/recv only when a gap exists |

**Ownership rules:**

- **O-1:** Every byte of sequencer state is written by exactly one thread
  (H). No locks, no atomics on the data path. R communicates only through
  the two SPSC mailboxes.
- **O-2:** Frame lifetime contract: a `FrameView` returned by
  `Transport::poll` is valid **until the next `poll()` on that transport**.
  The sequencer must consume-or-stage before the engine re-polls. This is
  the entire frame-recycle protocol (EN-3) — one sentence, enforced by
  construction in the engine loop.
- **O-3:** Mailbox full ⇒ R parks (spin + backoff). Never drops, never
  grows, never allocates. Mailbox drain is H's responsibility.
- **O-4:** Time discipline: H reads the clock **once per loop iteration**
  into `now_ns`, threads it through every call. No clock reads per packet.
  R keeps its own clock (event timing only — see S-1).
- **O-5:** Panic policy: `panic = "abort"` in release. A panic on the hot
  path is a bug whose unreachability is proven by VR-4, not caught at
  runtime.
- **O-6:** No thread spawn after `start()`. Threads, buffers, rings, and the
  window are all created in the one allocating phase: **startup**.

---

## 4. Crate Decomposition & Layer Laws

```
nexus-feed/
├── crates/
│   ├── nf-protocol/     # MoldUDP64 + ITCH5 codecs. Pure. #![forbid(unsafe_code)]
│   ├── nf-transport/    # Transport trait, FrameBatch, ReplayTransport, (xdp feature)
│   ├── nf-arbitrator/   # Sequencer, window, LiveFeedProof, events. Pure. forbid(unsafe)
│   ├── nf-recovery/     # TCP client, SPSC mailboxes, trigger *execution*
│   ├── nf-engine/       # Thread H loop, wiring, config; `nf-replay` + `nf-bench` bins
│   └── nf-testkit/      # chaos scheduler, fake retransmit server, golden hash, harnesses
```

```
             ┌───────────┐
             │ nf-engine │  (bins: replay, bench)
             └─┬──┬──┬──┘
       ┌───────┘  │  └────────┐
       ▼          ▼           ▼
 nf-transport  nf-arbitrator  nf-recovery
       └────┬───────┘    │         │
            ▼            │         │
        nf-protocol ◀────┴─────────┘
             ▲
        nf-testkit (dev-dependency of everything; depends on all)
```

**Layer laws:**

- **LI-1:** `nf-arbitrator` must compile with `nf-transport`, `nf-recovery`,
  `nf-engine` deleted from the workspace. It knows frames and sequences —
  not feeds, sockets, or threads. (This is what makes C-3 literally true.)
- **LI-2:** `nf-protocol` has zero dependencies (not even `libc`). All
  parsing is safe code over `&[u8]` — no struct transmutes, ever; field
  reads are `from_be_bytes` on checked slices.
- **LI-3:** `nf-transport` knows frames and memory, not sequences. No
  MoldUDP64 semantics above `nf-protocol`.
- **LI-4:** `nf-engine` is the only crate that may name every other crate.
  Release binaries live only in `nf-engine`.
- **LI-5:** `nf-testkit` is a dev-dependency only. If a production crate
  imports it, the build is wrong.
- **LI-6 (unsafe budget):** `unsafe` exists in exactly two crates —
  `nf-transport` (mmap, later UMEM) and `nf-engine` (thread pinning). Both
  auditable in one sitting. `nf-protocol` and `nf-arbitrator` carry
  `#![forbid(unsafe_code)]`.
- **LI-7 (dependency budget):** zero external runtime crates. `libc` only,
  and only in the two unsafe-allowed crates. No serde, no tokio, no
  memmap2. Hand-roll mmap. This is what makes PR-3 provable instead of
  hoped-for.

---

## 5. Interface Contracts (compile-time law)

```rust
// ── nf-transport ─────────────────────────────────────────────
pub type FeedId = u8;

/// SAFETY CONTRACT: `ptr` is valid for `len` bytes until the next
/// `poll()` call on the owning transport (O-2).
pub struct FrameView { pub(crate) ptr: *const u8, pub len: u16, pub feed: FeedId }
impl FrameView { pub fn bytes(&self) -> &[u8]; }

pub struct FrameBatch { /* [FrameView; 256] + len, owned by engine, reused */ }
impl FrameBatch { pub fn clear(&mut self); pub fn capacity() -> usize; }

pub trait Transport {
    /// Fill `batch`; return frame count. Zero allocations. Never blocks.
    fn poll(&mut self, batch: &mut FrameBatch) -> usize;
}
```

```rust
// ── nf-arbitrator ────────────────────────────────────────────
pub struct Sequencer { /* W, window (64 KiB), session, timestamps, counters */ }

/// Constructible ONLY inside nf-arbitrator, ONLY in the contiguous branch.
pub struct LiveFeedProof { gen: u64 }   // private fields = the typestate

pub enum Event {
    GapOpened      { from: u64, ahead: Option<u64>, gen: u64 },
    ReAnchored     { gen: u64, at: u64 },
    SessionBoundary{ prev: [u8; 10], next: [u8; 10], gen: u64 },
    EndOfSession   { session: [u8; 10], final_wm: u64 },
    SessionDead    { reason: DeadReason, last_wm: u64 },
}

pub trait Sink {
    fn on_msg(&mut self, proof: &LiveFeedProof, seq: u64, msg: &[u8]);
    fn on_event(&mut self, ev: Event);
}

pub struct RecoveryIntent { pub from: u64, pub to_incl: u64 }

impl Sequencer {
    pub fn new(anchor_session: [u8; 10]) -> Self;
    /// Generic (monomorphized) in the engine; dyn only in testkit.
    pub fn ingest<S: Sink>(&mut self, frame: &[u8], feed: FeedId,
                           now_ns: u64, sink: &mut S);
    /// Pure decision fn of state + now. Engine executes; sequencer
    /// never performs I/O.
    pub fn recovery_intent(&mut self, now_ns: u64) -> Option<RecoveryIntent>;
    pub fn watermark(&self) -> u64;
    pub fn counters(&self) -> Counters;   // Copy struct, per-feed + global
}
```

```rust
// ── nf-recovery ──────────────────────────────────────────────
pub mod spsc {
    pub struct PacketMailbox;  // 16 × 1500B, cache-line-padded idx (spec: 08)
    pub struct CmdMailbox;     // 8 × 24B
}
pub struct RecoveryClient;     // Thread R's entire world (spec: 08)
```

Notes that are law:

- **N-1:** `recovery_intent` returning a value (not performing I/O) is what
  keeps LI-1 true and the arbitrator unit-testable without sockets.
- **N-2:** Intent ranges always start at W and W is monotone ⇒ pending ranges
  only extend rightward. Widen-and-supersede is therefore total; no queue of
  intents is needed. Details in 08.
- **N-3:** The sink default is a **streaming hash sink** (wyhash/FNV over
  seq‖len‖bytes). Golden comparison at dev-sample scale = hash equality,
  not 200 MB of diffing. Byte-compare is done on the mini sample only.

---

## 6. Memory Inventory

Everything below is allocated at startup, once. Nothing grows thereafter
(PR-5).

| Region | Size | Owner | Alignment | Notes |
|---|---|---|---|---|
| Sequencer window | 64 KiB + 1 KiB meta | H | 2 MiB (hugepage-friendly) | layout detail: doc 05 |
| FrameBatch ×2 | 4 KiB each | H | 64 B | reused, never reallocated |
| PacketMailbox | ~24.2 KiB | H↔R | 64 B on indices | SPSC, single-writer each side |
| CmdMailbox | ~200 B | H→R | 64 B on indices | |
| ReplayTransport mmap | file size | H | page | MAP_PRIVATE read-only |
| Hash sink state | ~100 B | H | — | N-3 |
| TCP buffers (R) | 64 KiB rx + 4 KiB tx | R | 64 B | preallocated; zero-alloc target incl. Thread R (risk R-1: if `std::net` allocates under the hood, drop to raw `libc` sockets — ADR required) |
| XDP UMEM | 2048 × 4 KiB | transport | page | feature-gated; doc 09 |

Total hot-path static footprint: **< 100 KiB.** The working set (window +
batch + mailbox heads) fits in L2 permanently.

---

## 7. Determinism Scope (what "deterministic" means here — precisely)

| Artifact | Deterministic? | Enforced by |
|---|---|---|
| Emitted message stream | YES — byte-identical across schedules (L2) | VR-5 |
| Event stream | NO (by design, S-1); must satisfy: gen strictly increasing; every GapOpened eventually paired with ReAnchored or SessionDead/EndOfSession; no event after SessionDead | VR-2/VR-5 invariant checks |
| Timing / latency | NO — measured, reported, never golden-compared | doc 11 |

The engine contains **zero sources of nondeterminism**: no RNG anywhere
(all randomness lives in testkit behind explicit seeds — EN-6), no hashmaps
on any path (iteration order), no wall-clock in data decisions (O-4; the
only clock use is trigger *timing*, which by S-1 cannot touch the message
stream), no allocator (post-startup).

---

## 8. Liveness (why the engine cannot stall while data exists)

L2 says nothing about *progress*. Separate argument, by stall taxonomy:

| Stall state | Detected by | Unblocked by |
|---|---|---|
| Gap, staged head exists | HWM ≥ 512 or 250 µs no-progress | request [W, head) → server serves → drain |
| Gap, nothing staged, heartbeat seen | heartbeat seq H > W, 250 µs silence | request [W, H) → drain |
| Gap, nothing staged, no heartbeat | not detectable — nothing observable | data or heartbeat arrival re-arms the above |
| Request outstanding, server silent | retry counter (4) | SessionDead hard event — halt, never silently restart |

Completeness argument: whenever `W < e(U)` and UDP delivery has ceased, the
harness (TH-1) guarantees a heartbeat or staged head exists, so row 1 or 2
fires. Row 3 stalls only when *nothing at all* is arriving, which is not a
liveness bug — it's a paused market. Row 4 converts unbounded waiting into
a finite, explicit, reported death.

---

## 9. Non-Goals → Architectural Rationale

| NG | What it protects |
|---|---|
| NG-1 no LOB | the sink trait keeps L2 provable end-to-end; an LOB drags in its own correctness burden |
| NG-2 no DPDK/netmap | one kernel-bypass surface (AF_XDP) max; two = double the env matrix, zero perf delta at 10M msg/s |
| NG-3 no FPGA | different sport; hardware gates make software numbers unfalsifiable |
| NG-4 no multi-core | C-3: arbitration of one session is inherently sequential; threads would only add coherence traffic |
| NG-5 no io_uring | zero steady-state syscalls means nothing to submit |
| NG-6 no SoupBinTCP | C1 (corrections, doc 00): wrong protocol; recovery is MoldUDP64-over-TCP |
| NG-7 no live connectivity | golden truth requires controlled U; live feeds make L2 untestable in v1 |
| NG-8 no persistence | journals re-open crash-recovery questions this architecture deliberately does not need |
| NG-9 no dashboards | telemetry = counters + histogram structs, snapshot on demand |
| NG-10 no unmeasured claims | the project's entire identity |

---

## 10. Document Map

| Doc | Owns the detail deferred here |
|---|---|
| 02 | MoldUDP64 field map, request packet grammar, heartbeat/EOS semantics |
| 03 | ITCH 5.0 type table, LENGTH[256], 64-byte slot justification |
| 04 | replay fabricator, synthetic framing, Feed-B model, golden hash |
| 05 | window layout, drain algorithm, W1 proof, cycle budgets |
| 06 | LiveFeedProof construction sites, C++20 mapping |
| 07 | zero-alloc enforcement (counter, lint, strace) |
| 08 | trigger constants, mailbox spec, widen rule, retry policy |
| 09 | UMEM, rings, XSKMAP, NUMA/IRQ |
| 10 | test matrix incl. chaos scheduler + fake retransmit server |
| 11 | bench methodology |
| 12 | gate ledger |
| 13 | ADR log (0001 language, 0002 window, 0005 build env due before Phase 0 exits) |

## 11. Reader Comprehension Gate

Answer unaided, one line each:

1. State L2 and name the one property of the emission rule that makes it true.
2. Why are `GapOpened` events *not* covered by confluence, and why is that acceptable?
3. Why is `staged` a cache rather than truth, and what repairs a cache miss?
4. A gap exists, nothing is staged, no heartbeat has ever arrived. What does the engine do, and why is that correct rather than a bug?
5. Why does the sequencer return `RecoveryIntent` instead of performing recovery itself?

## Changelog

| Date | Version | Entry |
|---|---|---|
| ____-__-__ | 1.0 | Initial architecture. L1/L2 proven; C-1..C-4; scope limits S-1..S-3; layer laws LI-1..7. |
````

---

## ED-01 — What you build NOW (gate G1)

Signatures only. **No logic.** Bodies are `todo!()`. The moment you write
`if` inside `ingest` before doc 05 exists, you're designing in the editor —
that's how the last project got 56 files.

```bash
mkdir -p ~/Hft-fund && cd ~/Hft-fund
cargo init --workspace --name nexus-feed
cargo new --lib crates/nf-protocol   && \
cargo new --lib crates/nf-transport  && \
cargo new --lib crates/nf-arbitrator && \
cargo new --lib crates/nf-recovery   && \
cargo new --lib crates/nf-engine     && \
cargo new --lib crates/nf-testkit
mkdir -p docs .github/workflows
```

Root `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2021"

[profile.release]
panic = "abort"
lto = true
codegen-units = 1
```

`crates/nf-protocol/src/lib.rs`:

```ru
Here's the tail end again, verbatim — from §11 to the end of ED-01. Pick up where your copy stopped.

---

````markdown
## 11. Reader Comprehension Gate

Answer unaided, one line each:

1. State L2 and name the one property of the emission rule that makes it true.
2. Why are `GapOpened` events *not* covered by confluence, and why is that acceptable?
3. Why is `staged` a cache rather than truth, and what repairs a cache miss?
4. A gap exists, nothing is staged, no heartbeat has ever arrived. What does the engine do, and why is that correct rather than a bug?
5. Why does the sequencer return `RecoveryIntent` instead of performing recovery itself?

## Changelog

| Date | Version | Entry |
|---|---|---|
| ____-__-__ | 1.0 | Initial architecture. L1/L2 proven; C-1..C-4; scope limits S-1..S-3; layer laws LI-1..7. |
````

---

## ED-01 — What you build NOW (gate G1)

Signatures only. **No logic.** Bodies are `todo!()`. The moment you write
`if` inside `ingest` before doc 05 exists, you're designing in the editor —
that's how the last project got 56 files.

```bash
mkdir -p ~/Hft-fund && cd ~/Hft-fund
cargo init --workspace --name nexus-feed
cargo new --lib crates/nf-protocol   && \
cargo new --lib crates/nf-transport  && \
cargo new --lib crates/nf-arbitrator && \
cargo new --lib crates/nf-recovery   && \
cargo new --lib crates/nf-engine     && \
cargo new --lib crates/nf-testkit
mkdir -p docs .github/workflows
```

Root `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2021"

[profile.release]
panic = "abort"
lto = true
codegen-units = 1
```

`crates/nf-protocol/src/lib.rs`:

```rust
#![forbid(unsafe_code)]

pub mod moldudp64;
pub mod itch5;

pub const MAX_MSG_LEN: usize = 64;
```

`crates/nf-protocol/src/moldudp64.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub session: [u8; 10],
    pub seq: u64,
    pub count: u16,
}

#[derive(Debug)]
pub enum FrameError {
    Truncated { need: usize, got: usize },
}

pub fn parse_header(buf: &[u8]) -> Result<Header, FrameError> {
    todo!("doc 02")
}
```

`crates/nf-protocol/src/itch5.rs`:

```rust
/// Total length of an ITCH 5.0 message by type byte, or None if unknown.
/// Backed by a const table; largest legal message is 50 bytes (doc 03).
pub fn msg_len(type_byte: u8) -> Option<u8> {
    todo!("doc 03")
}
```

`crates/nf-transport/src/lib.rs`:

```rust
pub type FeedId = u8;

pub struct FrameView { pub(crate) ptr: *const u8, pub len: u16, pub feed: FeedId }

impl FrameView {
    /// SAFETY CONTRACT (doc 01 §5, O-2): valid until next poll() on the
    /// owning transport.
    pub fn bytes(&self) -> &[u8] { todo!("doc 09 / 04") }
}

pub struct FrameBatch { slots: [FrameView; 256], len: usize }

impl FrameBatch {
    pub fn clear(&mut self) { todo!() }
    pub const fn capacity() -> usize { 256 }
    pub fn frames(&self) -> &[FrameView] { todo!() }
}

pub trait Transport {
    fn poll(&mut self, batch: &mut FrameBatch) -> usize;
}
```

`crates/nf-arbitrator/src/lib.rs`:

```rust
#![forbid(unsafe_code)]

pub type FeedId = u8;

pub struct LiveFeedProof { pub(crate) gen: u64 }

#[derive(Debug)]
pub enum Event {
    GapOpened       { from: u64, ahead: Option<u64>, gen: u64 },
    ReAnchored      { gen: u64, at: u64 },
    SessionBoundary { prev: [u8; 10], next: [u8; 10], gen: u64 },
    EndOfSession    { session: [u8; 10], final_wm: u64 },
    SessionDead     { reason: DeadReason, last_wm: u64 },
}

#[derive(Debug)]
pub enum DeadReason { RetryExhausted, TcpUnreachable }

#[derive(Debug, Clone, Copy)]
pub struct RecoveryIntent { pub from: u64, pub to_incl: u64 }

pub trait Sink {
    fn on_msg(&mut self, proof: &LiveFeedProof, seq: u64, msg: &[u8]);
    fn on_event(&mut self, ev: Event);
}

pub struct Sequencer { /* doc 05 defines every field */ }

impl Sequencer {
    pub fn new(anchor_session: [u8; 10]) -> Self { todo!("doc 05") }
    pub fn ingest<S: Sink>(&mut self, frame: &[u8], feed: FeedId,
                           now_ns: u64, sink: &mut S) { todo!("doc 05") }
    pub fn recovery_intent(&mut self, now_ns: u64) -> Option<RecoveryIntent> {
        todo!("doc 08")
    }
    pub fn watermark(&self) -> u64 { todo!() }
}
```

`crates/nf-recovery/src/lib.rs` and `nf-engine/src/lib.rs` / `nf-testkit/src/lib.rs`: empty module docs + `todo!()` stubs mirroring doc 01 §5. Engine gets `src/bin/replay.rs` with an empty `fn main() {}`.

`.github/workflows/ci.yml` (compact):

```yaml
name: ci
on: [push, pull_request]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: clippy }
      - run: cargo build --workspace --release
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test  --workspace
```

**G1 acceptance — report all of these:**

1. `cargo build --workspace --release` green **on Termux (aarch64)** — this is your dev box; it must build where you live
2. Same green on CI (link the run)
3. `cargo clippy --workspace --all-targets -- -D warnings` clean
4. `cargo tree -p nf-arbitrator` shows **only** nf-protocol — LI-1 physically verified
5. Two ADR entries in `13-journal.md`: ADR-0001 (Rust; one-paragraph rationale referencing doc 00 §2), ADR-0005 (Termux dev + GH Actions CI + deferred real-Linux XDP box)
6. And when the BX download finishes: run doc 00 Appendix A, paste hexdump + hashes into 00's changelog, say **freeze 00**

When G1 is green, next prompt is **engineer 02** — MoldUDP64 down to the byte, written straight off the 126 KB PDF sitting in your specs folder. That one you can implement the same night.

