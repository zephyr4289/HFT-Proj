# 00 — NEXUS-FEED-01 Specification (Baseline v1.2)

```
Status:      FROZEN (v1.2 reaffirmed)
Exit Gate:   Every requirement numbered and testable; traceability matrix
             complete (§9); corrections register closed (§1); changelog
             initialized.
Evidence:    Appendix A audit commands; §9 traceability; sign-off entry.
Authority:   MoldUDP64 + TotalView-ITCH 5.0 PDFs committed under docs/specs/.
             Field-level truth lives in docs 02/03, which cite them. This
             document binds BEHAVIOR only, not field layouts.
Rule:        Once frozen, requirement numbers are never renumbered or
             silently edited. All change via changelog entry only.
```

---

## 1. Corrections Register (deviations from original NEXUS-FEED-01 text)

| # | Original text | Correction | Consequence |
|---|---|---|---|
| C1 | "SoupBinTCP / TCP recovery" | Recovery is **MoldUDP64 retransmission request/response over TCP** (Request: `Session[10] \| Seq[8] \| Count[2]`). SoupBinTCP is an order-entry (OUCH) session protocol, not ITCH recovery. | FR-8; SoupBinTCP moved to Non-Goals |
| C2 | Ambiguous sequence semantics | MoldUDP64 sequence numbers count **messages**, not packets. A packet `seq=S, count=C` carries messages `S..S+C-1`. | FR-4 window is a message-sequence window |
| C3 | "Historical PCAP data" | The NASDAQ archive is **raw concatenated ITCH messages** — no pcap, no UDP headers, no Feed A/B. Replay must fabricate MoldUDP64 framing and a synthetic Feed B. | FR-11; this is a feature (we control packetization and can inject pathological splits) |
| C4 | "Sub-microsecond determinism" (unmeasurable) | Replaced with numeric gates: ≥10M msg/s/core sustained, p50 < 60 cycles, p99 < 150 cycles, zero allocations (§5). | PR-1..PR-3 |
| C5 | Implied TotalView ground truth | Downloaded day is **Nasdaq BX** (2019-12-30), same ITCH 5.0 grammar, smaller venue. Protocol table implements full TotalView 5.0 catalog; doc 03 must run a type histogram over the actual BX day and record observed types. | EN-4; doc 03 exit gate |
| C6 | C3 implied bare messages | Observed ground truth (20191230.BX): the archive is a **MoldUDP64 MESSAGE-BLOCK stream** — `[u16 BE len][message]` repeated, no 20-byte headers. Evidence: `00 0c` (len 12 = System Event), msg ends `4f` 'O', next prefix `00 27` (len 39 = Stock Directory). | Doc 04 reads blocks via length prefix; doc 03 table becomes cross-VALIDATOR |
| C7 | Prior doc claimed 'H' = Stock Directory | ITCH 5.0 Stock Directory is **'R'** (0x52, 39B); **'H'** (0x48, 25B) is Stock Trading Action. Evidence: `0x52` at file offset `0x10`, declared len `0x27=39`, locate 1, stock `"A       "`. | Doc 03 table |
| C8 | Doc 04 §8 folded seq into golden hash, making E2E-1 with session split unsatisfiable (split restarts seq; walker doesn't) | Golden hash folds `(len_u16_le, msg_bytes)` ONLY. Count tracked separately. Order enforced by ConformanceSink. Hash becomes session-split-invariant by construction. | Doc 04 §8, doc 05 E2E-1 amended; pinned hashes regenerated |

---

## 2. Purpose

A standalone, deterministic feed arbitrator: consume two redundant, lossy,
out-of-order MoldUDP64/ITCH 5.0 streams and emit ONE contiguous, strictly
monotonic message sequence downstream, recovering any range lost on BOTH feeds
via TCP retransmission — with zero heap allocation on the hot path and
byte-identical output regardless of arrival order, loss pattern, or recovery
interleaving.

Language: **Rust** (ADR-0001: typestate enforcement of FR-6, counting-allocator
ergonomics for PR-3, cargo-fuzz/Miri for VR-4). The C++20 typestate mapping is
documented in doc 06 for portability reference.

---

## 3. Definitions

- **W** — watermark: next expected message sequence number downstream.
- **Staged** — a received message with sequence > W held in the reorder window.
- **Gap** — a range `[W, head)` where `head` = lowest staged sequence, unresolved on both feeds.
- **Golden replay** — loss-free, in-order replay of the ground-truth day; defines the reference output bytes.
- **Confluence** — the property that downstream output is a pure function of the received message *multiset*, independent of arrival order, feed of origin, or transport (UDP vs recovery TCP). Formal treatment: doc 01.

---

## 4. Functional Requirements

**FR-1 Transport ingestion.** The engine ingests N ≥ 2 independent streams
(Feed A, Feed B) through one `Transport` trait with two implementations:
`XdpTransport` (AF_XDP; copy-mode mandatory, zero-copy optional flag) and
`ReplayTransport` (mmap of raw ITCH with synthetic MoldUDP64 framing). The
sequencer contains **zero feed-specific logic**; feed count is not a parameter
of correctness.

**FR-2 Strict monotonic emission.** Emitted sequence satisfies
`S_out(k+1) = S_out(k) + 1` for all k. During an unresolved gap, emission
halts entirely — no speculative emission, no skip-ahead, no reordering.
Monotonicity is a hard invariant, not best-effort.

**FR-3 Duplicate suppression.** Packets entirely below W (`last < W`) are
dropped in O(1) by watermark comparison — zero heap allocation, zero window
mutation, counters only. Partial overlaps emit only the novel suffix.
Idempotency of recovery injection follows from FR-3: a late TCP retransmission
of an already-delivered range is a pure duplicate and dies in the same compare.

**FR-4 Reorder window.** 1024 message slots, statically allocated,
cache-line-aligned, 64-byte slots. Invariant W1: every staged sequence S
satisfies `W < S < W + 1024` at all times. Consequences (proof in doc 05):
no slot collision, no generation tags, no ABA. Sequences beyond the window are
not staged; they trigger recovery.

**FR-5 Zero-copy dispatch.** In-order messages are dispatched as slices
directly into the receive frame (no copy). Staged messages are copied exactly
once into their arena slot. Emitted bytes must equal ground-truth bytes — the
byte-identical contract (VR-5).

**FR-6 LiveFeedProof (typestate).** Any LOB-mutating downstream call requires
an unrevoked `LiveFeedProof` token. Tokens are constructible only inside the
contiguous-emission branch of the sequencer. Gap detection invalidates the
token (emit `GapOpened{from, to, gen}`); full resolution re-mints it (emit
`ReAnchored{gen}`). `gen` strictly increases. Stale-token use must fail to
compile (Rust); private-ctor + friend discipline documented for C++20 (doc 06).

**FR-7 Gap detection triggers.** All three, all non-blocking, zero-alloc:
(a) HWM: staged span ≥ 512 ahead of W; (b) timeout: staged data, no progress
for 250 µs; (c) heartbeat foreknowledge: heartbeat announces `seq H > W`, no
data for 250 µs. Defaults; changes require an ADR entry.

**FR-8 Slow-path recovery.** Non-blocking TCP client speaking MoldUDP64
retransmission request/response. One outstanding request range at a time
(widening rule: an extended gap supersedes and extends the pending range).
Recovered packets enter the **same ingest path** as UDP (FR-3 idempotency —
no splice bookkeeping exists). Retry cap 4 → `SessionDead` hard event; never
silently restart. The recovery thread communicates with the hot thread only
via a pre-allocated SPSC mailbox (16 × 1500 B); the hot thread never blocks
on recovery.

**FR-9 Session lifecycle.** Session-field change → flush window, re-anchor W,
`gen++`, emit `SessionBoundary`. Heartbeat (`count=0`) carries next-expected
sequence: recorded, no emission. End-of-session (`count=0xFFFF`) → emit
`EndOfSession`; any still-unresolved gap is reported as `GapUnresolved` —
reported and halted, never fabricated.

**FR-10 Confluence (determinism).** Downstream output is a pure function of
the received message multiset. Empirical form: any adversarial replay
schedule — loss, jitter, dual-drop, recovery interleaving — must produce
output **byte-identical** to the golden replay (VR-5). This subsumes FR-2,
FR-3, and splice-without-double-apply as corollaries.

**FR-11 Synthetic framing (replay).** Replay fabricates MoldUDP64 envelopes
over raw ITCH ground truth. Packetization boundaries, loss model, and delay
model are all seeded and reproducible. Feed B is fabricated independently
from the same ground truth with an independent seed.

**FR-12 Hostile-input validation.** Malformed frames — bad length, unknown
type byte, truncation, wrong session — are counted as protocol violations and
dropped. Never a panic, never UB, never an allocation. Any message length > 64
or type byte absent from the static length table is a violation (justified in
doc 03: max ITCH 5.0 message = 50 B, NOII).

---

## 5. Performance Requirements

Reference hardware: CI x86_64 runner class (EN-2). Cycle figures are
architecture-specific and non-portable; the portable gates are PR-1 and PR-3.

**PR-1 Throughput.** ≥ 10,000,000 messages/sec sustained, single core,
arbitrator + replay transport + recording stub sink, over the dev sample.

**PR-2 Latency.** Receipt→dispatch (rdtscp, invariant-TSC validated,
200 ms calibration vs CLOCK_MONOTONIC_RAW): p50 < 60 cycles, p99 < 150
cycles. p99.99 reported, no gate in v1.

**PR-3 Zero allocation.** Zero heap allocations in
ingestion/arbitration/dispatch over a measured window that includes at least
one full gap + recovery cycle. Enforced by counting global allocator
(counter delta == 0) and belt-and-suspenders `strace -e mmap,brk` (clean).

**PR-4 Cycle budgets (design budgets, non-gating).** Pure-duplicate packet
≤ 15 cycles; in-order message ≤ 10 + sink; staged message ≤ 20 + sink.
Gating requirements are PR-1/2/3 only.

**PR-5 Bounded memory.** Total engine state is static: 64 KiB window + fixed
metadata. No growth with input duration, message count, or gap count.

---

## 6. Verification Requirements

| ID | Test | Pass condition |
|---|---|---|
| VR-1 | AB-ASYMMETRIC-RACE: Feed A delayed N(50 µs, 20 µs) seeded; Feed B bursting 128 pkts/ms; ≥ 100M messages | Output byte-identical to golden; W never regresses |
| VR-2 | DUAL-DROP-GAPFILL: same range lost on both feeds; fake retransmission server serves ground truth | `GapOpened` observed; gen bumped; recovery range == [W, head); final output byte-identical; zero duplicate emissions |
| VR-3 | ZERO-ALLOC-ENFORCEMENT | Allocator counter delta == 0 incl. gap+recovery cycle; strace clean |
| VR-4 | FUZZ-CORRUPTED-FRAMES: cargo-fuzz on ingest + parsers; ASan/UBSan (+ Miri on pure components); corpus seeded from the real day incl. corrupted variants | No panic, no UB, no leak. CI gate ≥ 1M execs; extended ≥ 1B local |
| VR-5 | GOLDEN CONFLUENCE (master test): randomized seeded chaos schedule over the full day | Byte-identical to golden — empirical proof of FR-10 |
| VR-6 | Traceability | Every FR/PR maps to ≥ 1 VR or Appendix A command; matrix lives in doc 10 |

---

## 7. Environment & Artifact Requirements

**EN-1** Dev environment is Termux/aarch64. Pure crates + ReplayTransport
compile and run on-device. AF_XDP is out of scope on-device.

**EN-2** CI is GitHub Actions ubuntu x86_64. Full suite green on the mini
sample. CI runner class = performance reference hardware for PR-1/PR-3.

**EN-3** AF_XDP zero-copy on real Linux hardware is Phase 3; copy-mode is the
CI baseline. Frame-recycle contract (consume-or-stage, then refill) is part of
the transport interface from day one.

**EN-4** Artifacts (paths are law):

| File | Size | Tracked |
|---|---|---|
| `data/raw/20191230.BX_ITCH_50.gz` | ~390 MB | NO |
| `data/sample-dev.itch` | 200 MB | NO |
| `data/tests/sample-mini.itch` | ≤ 15 MB | **YES — gitignore exception required** |
| `docs/specs/*.pdf` | ~1.6 MB | YES |

`.gitignore` must contain:
```
data/raw/
data/sample-dev.itch
data/tests/*
!data/tests/sample-mini.itch
```

**EN-5** Rust stable pinned for core crates; nightly only for fuzz/Miri lanes.

**EN-6** All randomness flows from explicit seeds recorded in test output. No
wall-clock dependence in replay determinism.

---

## 8. Non-Goals (this list is law)

- NG-1 No LOB implementation. Sink trait + recording stub only.
- NG-2 No DPDK, netmap, or proprietary kernel modules.
- NG-3 No FPGA.
- NG-4 No multi-core sharding or NUMA topology — one hot core.
- NG-5 No io_uring.
- NG-6 No SoupBinTCP/OUCH or any order-entry protocol.
- NG-7 No live exchange connectivity. Replay + loopback only.
- NG-8 No HA, failover, persistence, or crash-recovery journaling.
- NG-9 No dashboards, GUIs, or web telemetry.
- NG-10 No unmeasured performance claims. Nothing "beats" anything without
  numbers on the same hardware in the same run.

---

## 9. Traceability Map (summary; full matrix in doc 10)

| Requirement | Verified by |
|---|---|
| FR-1, FR-11 | VR-1, VR-5, Appendix A |
| FR-2, FR-3, FR-10 | VR-5 (subsumes), VR-1 |
| FR-4, FR-5 | VR-5 + doc 05 proofs |
| FR-6 | VR-2 (gen bump events) + compile-fail tests |
| FR-7, FR-8 | VR-2 |
| FR-9 | VR-5 + doc 08 state machine |
| FR-12 | VR-4 |
| PR-1, PR-3 | doc 11 bench harness |
| PR-2 | doc 11 |
| EN-4 | Appendix A |

## 10. Constants Registry (normative defaults)

| Constant | Value | Owner doc |
|---|---|---|
| Reorder window | 1024 slots × 64 B | 05 |
| HWM trigger | 512 staged ahead | 08 |
| Progress timeout | 250 µs | 08 |
| Recovery retries | 4 → SessionDead | 08 |
| SPSC mailbox | 16 × 1500 B | 08 |
| CmdChannel | 64 B latest-wins register (AM-4) | 08 |
| `grace_ns` | 10 ms (aliased to resuggest window) | 08 |
| Feeds (v1) | 2 (design is N-agnostic) | 05 |

## Appendix A — Artifact Audit Commands

```bash
# 1. Magic-byte verification (corrected for MoldUDP64 message-block stream):
zcat data/raw/20191230.BX_ITCH_50.gz | head -c 16 | od -A x -t x1
# Expect: 00 0c 53 ... 4f 00 27   (block len 12, 'S'..'O', next len 39)

# 2. Samples (NEVER gunzip the full file to disk)
zcat data/raw/20191230.BX_ITCH_50.gz | head -c 200000000 > data/sample-dev.itch
zcat data/raw/20191230.BX_ITCH_50.gz | head -c 15000000  > data/tests/sample-mini.itch

# 3. Mini sample actually tracked?
git check-ignore -v data/tests/sample-mini.itch   # must match negation rule !data/tests/sample-mini.itch

# 4. Record evidence hashes in Changelog
sha256sum data/sample-dev.itch data/tests/sample-mini.itch
```

## 11. Changelog

| Date | Version | Entry |
|---|---|---|
| 2026-08-30 | 1.0 | Initial baseline. Corrections C1–C5 applied to original NEXUS-FEED-01 text. Frozen on review sign-off. Sample hashes: `sample-dev.itch: e34c17b4ed6c3c5ab47a64159af00cade44328b92cfc2b35c674d7ee92971953`, `sample-mini.itch: 5e347abbaa69f12226a6506e875f51633af690b3fc890d9d20a7213fe73275c9`. |
| 2026-08-30 | 1.1 | C6 added; Appendix A expected bytes corrected to `00 0c 53 .. 4f 00 27`. Freeze reaffirmed at v1.1. |
| 2026-08-30 | 1.2 | C7 added: ITCH 5.0 Stock Directory is 'R' (0x52, 39B); 'H' (0x48, 25B) is Stock Trading Action. Freeze reaffirmed at v1.2. |
| 2026-08-30 | 1.3 | C8 added: Golden hash folds (len_u16_le, msg_bytes) only, dropping seq. Freeze reaffirmed at v1.3. |
| 2026-08-30 | 1.4 | G8 release: PR-3 stage-2 zero-allocation fully discharged; AM-4 latest-wins register, AM-5 vt-grace clock clamp, ADR-0006/0007 recorded. |
