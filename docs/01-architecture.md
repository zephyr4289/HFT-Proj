# 01 — System Architecture

```
Status:    FROZEN (v1.1)
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

**Layer laws:**

- **LI-1:** `nf-arbitrator` must compile with `nf-transport`, `nf-recovery`,
  `nf-engine` deleted from the workspace.
- **LI-2:** `nf-protocol` has zero dependencies (not even `libc`).
- **LI-3:** `nf-transport` knows frames and memory, not sequences.
- **LI-4:** `nf-engine` is the only crate that may name every other crate.
- **LI-5:** `nf-testkit` is a dev-dependency only.
- **LI-6 (unsafe budget):** `unsafe` exists in exactly two crates — `nf-transport` (mmap, later UMEM) and `nf-engine` (thread pinning).
- **LI-7 (dependency budget):** zero external runtime crates.

---

## 5. Interface Contracts (compile-time law)

```rust
// ── nf-transport ─────────────────────────────────────────────
pub type FeedId = u8;

pub struct FrameView { pub(crate) ptr: *const u8, pub len: u16, pub feed: FeedId }
impl FrameView { pub fn bytes(&self) -> &[u8]; }

pub struct FrameBatch { /* [FrameView; 256] + len, owned by engine, reused */ }
impl FrameBatch { pub fn clear(&mut self); pub fn capacity() -> usize; }

pub trait Transport {
    /// Fill `batch`; return frame count. Zero allocations. Never blocks.
    fn poll(&mut self, batch: &mut FrameBatch) -> usize;
    /// Return current timestamp in nanoseconds (AM-1). Virtual clock under replay,
    /// kernel clock under live transports.
    fn now_ns(&self) -> u64;
}
```

```rust
// ── nf-arbitrator ────────────────────────────────────────────
pub struct Sequencer { /* W, window (64 KiB), session, timestamps, counters */ }

pub struct LiveFeedProof { gen: u64 }

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
    pub fn ingest<S: Sink>(&mut self, frame: &[u8], feed: FeedId,
                           now_ns: u64, sink: &mut S);
    pub fn recovery_intent(&mut self, now_ns: u64) -> Option<RecoveryIntent>;
    pub fn watermark(&self) -> u64;
    pub fn counters(&self) -> Counters;
}
```

---

## 6. Memory Inventory

| Region | Size | Owner | Alignment | Notes |
|---|---|---|---|---|
| Sequencer window | 64 KiB + 1 KiB meta | H | 2 MiB (hugepage-friendly) | layout detail: doc 05 |
| FrameBatch ×2 | 4 KiB each | H | 64 B | reused, never reallocated |
| PacketMailbox | ~24.2 KiB | H↔R | 64 B on indices | SPSC, single-writer each side |
| CmdMailbox | ~200 B | H→R | 64 B on indices | |
| ReplayTransport render arena | 384 KiB | H | 64 B | 256 × 1500 B |
| ReplayTransport mmap | file size | H | page | MAP_PRIVATE read-only |
| Hash sink state | ~100 B | H | — | N-3 |
| TCP buffers (R) | 64 KiB rx + 4 KiB tx | R | 64 B | preallocated |
| XDP UMEM | 2048 × 4 KiB | transport | page | feature-gated; doc 09 |

---

## 11. Reader Comprehension Gate

1. State L2 and name the one property of the emission rule that makes it true.
2. Why are `GapOpened` events *not* covered by confluence, and why is that acceptable?
3. Why is `staged` a cache rather than truth, and what repairs a cache miss?
4. A gap exists, nothing is staged, no heartbeat has ever arrived. What does the engine do, and why is that correct rather than a bug?
5. Why does the sequencer return `RecoveryIntent` instead of performing recovery itself?

## Changelog

| Date | Version | Entry |
|---|---|---|
| 2026-08-30 | 1.0 | Initial architecture. L1/L2 proven; C-1..C-4; scope limits S-1..S-3; layer laws LI-1..7. |
| 2026-08-30 | 1.1 | AM-1 added: Transport trait gains `fn now_ns(&self) -> u64`. Render arena 384 KiB added to memory inventory. |
| 2026-08-30 | 1.2 | AM-2: Gap pairing closure includes SessionBoundary; EndOfSession gains announced_next: u64; Sequencer::new() takes no args. AM-3: No-ABA claim affirmed with Clear-on-Advance Law (§4.2) mechanism. |
