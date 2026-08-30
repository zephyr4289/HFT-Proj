# 04 — Replay Fabricator, Chaos Models & Golden Contract

```
Status:    FROZEN (v1.0)
Exit Gate: T1..T10 green on Termux + CI; determinism hashes recorded
           (§9 Appendix); golden hash of mini sample committed and
           CI-pinned; docs 00/01/03 evidence debt discharged.
Evidence:  Test output with run URLs; recorded schedule hashes;
           Appendix A filled.
Authority: This doc owns the SCHEDULE format, chaos models, virtual
           clock law, and the canonical golden hash. Framing law: doc 02.
           Payload law: doc 03. Sequencer consumption: doc 05. Fake
           retransmit server: doc 10.
Rule:      No new wire-format claims here (all framing inherited from 02).
           Every model is a pure function of (ground truth, config, seeds).
```

---

## 1. Role & Scope

The archive (C6) is a bare message-block stream: `[u16 BE len][msg]`
repeated. No packets, no UDP, no Feed A/B. The fabricator manufactures
everything the real venue would have sent — and everything the network
would have done to it:

```
ground truth (.itch mmap)
      │
      ▼  [nf-testkit::sched]  packetize → per-feed loss → per-feed delay
      │                        → heartbeats → EOS → sort by release order
      ▼
ReplaySchedule (pure data, deterministic)
      │
      ▼  [nf-transport::replay]  render packets on demand from mmap
      │                          via forward cursors; virtual clock
      ▼
FrameBatch (frames tagged feed A/B) ──▶ engine poll loop (doc 05)
```

**Design consequence of C-3 (doc 01):** one `ReplayTransport` multiplexes
BOTH feeds — frames carry a `FeedId` tag, the trait doesn't care, and the
sequencer is feed-agnostic anyway. One queue trivially preserves global
vt order.

---

## 2. Doc 01 Amendment

**AM-1:** `Transport` trait gains `fn now_ns(&self) -> u64`. Live
transports derive it from the kernel clock (doc 09); the replay transport
returns the **virtual clock** (§5).

---

## 3. Layer Split

| Lives in | Contains | May NOT know about |
|---|---|---|
| `nf-transport::replay` | `ReplaySchedule` types, renderer, cursors, virtual clock, `Transport` impl | loss models, jitter, seeds, ground-truth semantics beyond block walking |
| `nf-testkit::sched` | schedule builder, packetization, loss/delay models, TH-1 generation, golden walker, PRNG | rendering, FrameBatch, Transport |

---

## 4. ReplaySchedule

```rust
pub struct ReplaySchedule {
    pub events: Vec<SchedEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedEvent {
    pub release_vt: u64,          // ns on the virtual clock
    pub feed: FeedId,             // 0 = A, 1 = B
    pub kind: SchedKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedKind {
    Packet { first_seq: u64, first_msg: u64, count: u16 },
    Heartbeat { next_seq: u64 },
    EndOfSession { next_seq: u64 },
}
```

**Total order law (determinism):** events sort by
`(release_vt, feed, kind_rank, tiebreak)` where kind_rank is
`Packet=0 < Heartbeat=1 < EndOfSession=2` and tiebreak is `first_seq`
(Packet) / `next_seq` (HB, EOS).

---

## 5. Renderer & Virtual Clock

**Rendering:** Static render arena (256 × 1500 B = 384 KiB, single startup allocation).
`render(event)` formats `[20B Header][u16 len | msg bytes] × count` using forward cursors.

**Virtual clock law:**
1. Clock starts at the first event's `release_vt`, monotone thereafter.
2. `poll()`: deliver every event with `release_vt <= clock`, in schedule order, until the batch is full or events run dry.
3. If nothing is deliverable and events remain: `clock` jumps to the next event's `release_vt`, then deliver.
4. `now_ns()` returns the clock.

---

## 6. Chaos Models

- **PRNG**: SplitMix64 (exact power-of-two uniform floats).
- **Packetization**: `Fixed(k)`, `MtuBound(b)`, `SeededRange { min, max }`.
- **Loss**: `None`, `Bernoulli { p_pm }`, `GilbertElliott { ... }`, scripted drops.
- **Two-Mode Testing (`guarantee_coverage`)**:
  - `true`: Pure arbitration testing; stochastic dual-drops are force-delivered on one feed.
  - `false`: Dual-drops permitted; gapfill tested.
- **Delay (Irwin-Hall-12)**:
  $z = \left(\sum_{i=1}^{12} u_i\right) - 6 \in [-6, 6]$.
  $\text{delay\_ns} = \text{round}(\text{mean\_ns} + z \cdot \sigma)$.
  100% IEEE-deterministic across all hardware platforms (eliminates libm Box-Muller platform drift).
- **Session Split**: S1 carrying messages $1..m$ with $\text{EOS}(\text{next\_seq}=m+1)$, S2 restarting at sequence 1.

---

## 7. TH-1 Harness Termination

Builder appends rolling heartbeats, terminal heartbeat ($N+1$), and EOS ($N+1$) per feed.

---

## 8. Golden Hash Contract (C8 Updated)

Canonical FNV-1a-64 over $(len\_le \parallel msg\_bytes)$ (split-invariant by construction):
```rust
h0 = 0xcbf29ce484222325
for msg in messages:
    h = fnv2(h, len.to_le_bytes())
    h = fnvN(h, msg_bytes)
```

---

## 9. Appendix A — Golden Reference Hashes (C8 Pinned)

| Artifact | Messages | Canonical Golden Hash (FNV-1a-64) | Status |
|---|---|---|---|
| `data/tests/sample-mini.itch` | 505,849 | `0xF6EF_154E_FDE9_05D8` | Pinned (v1.1 / C8) |

---

## 10. Changelog

| Date | Version | Entry |
|---|---|---|
| 2026-08-30 | 1.0 | Initial replay fabricator, Irwin-Hall determinism model, ReplayTransport arena. |
| 2026-08-30 | 1.1 | C8 applied: golden hash folds (len_le \|\| msg_bytes) only, dropping seq. Pinned mini hash updated to `0xF6EF_154E_FDE9_05D8`. |
