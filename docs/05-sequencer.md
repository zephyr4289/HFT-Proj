# 05 — Sequencer: Window, Drain, Emission & Proof Lifecycle

```
Status:    DRAFT → FROZEN after G5 (first end-to-end conformance)
Exit Gate: U1..U14 + E2E-1/1b green on Termux AND CI; W1 property test
           clean over 10⁶ random op sequences; HashSink(golden) ==
           golden(ground truth) on mini chaos; full rolled ledger
           (G2/G3/G4 debt) discharged. See ED-05.
Evidence:  Test output + run URLs; determinism double-run hashes;
           event-invariant checker output from ConformanceSink.
Authority: This doc owns the ingest algorithm (NORMATIVE pseudocode),
           window layout, W1 + proof, drain, gap/event grammar, gen law,
           session lifecycle, recovery_intent MECHANISM, counters,
           cycle budgets. Trigger CONSTANTS + TCP client: doc 08.
           Proof typestate safety argument: doc 06. Zero-alloc
           enforcement: doc 07.
Rule:      The pseudocode in §5 is law. Deviations require an ADR.
```

---

## 1. Role

The sequencer is the confluence engine: the single-threaded fold that turns
a chaotic multiset of frames into one strictly monotonic byte stream. It
knows sequences, sessions, and a 64 KiB window. It does not know sockets,
threads, feeds (beyond a counter tag), or time-of-day — time arrives as a
u64 parameter (O-4), recovery arrives as a return value (N-1).

## 2. State Inventory

```rust
#[repr(align(64))]
pub struct Sequencer {
    // ── line 0 ── written once per packet ──────────────────────
    w: u64,                      // watermark: next expected seq

    // ── line 1 ── written per event ────────────────────────────
    gen: u64,                    // proof era counter (§7)
    session: [u8; 10],
    state: State,                // §3 (u8-tagged)
    gap_active: bool,
    evidence_hwm: u64,           // highest seq KNOWN transmitted (gap-era)
    max_staged: u64,             // max staged seq (0 = none)
    staged_count: u32,
    progress_vt: u64,            // last W advance (or anchor) vt
    hb_seq: u64, hb_vt: u64,     // last heartbeat evidence > W
    pending_to: Option<u64>,     // outstanding intent, exclusive end (§10)

    // ── lines 2..3 ──
    counters: Counters,          // §11, Copy

    // ── lines 4..19 ── presence bitmap ────────────────────────
    lens: [u8; 1024],            // slot i: 0 = absent, else msg length

    // ── lines 20..1043 ── arena (64 KiB) ──────────────────────
    arena: [u8; 64 * 1024],      // slot i at byte offset i << 6
}
```

Total: 65 KiB + ~100 B. One startup allocation (O-6). `repr(align(64))`;
2 MiB/hugepage alignment is an engine-allocation concern (doc 09/11), not
a sequencer concern — no overclaim here.

## 3. Macro State Machine (total — no unreachable states)

```
INIT ──first Data──▶ CONTIG ──stage (first>W)──▶ GAP ──W≥evidence_hwm──▶ CONTIG
  │                    │                            │
  │                    ├──HB(seq>W)──▶ GAP          ├──SessionChange──▶ INIT
  │                    ├──EOS──▶ ENDED              └──seal()──▶ DEAD
  ├──HB────────(evidence only)                      │
  ├──EOS──▶ ENDED                                    │
  └──seal()──▶ DEAD        ENDED ──SessionChange──▶ INIT
                           ENDED ──seal()──▶ DEAD
Any state ──SessionChange──▶ INIT (after boundary flush)
```

Transition table — every (state, event) cell defined; "violation" means
counted + dropped, never a panic:

| State \ Event | Data pkt | Heartbeat | EOS | Other session | seal() |
|---|---|---|---|---|---|
| INIT | anchor W=seq; apply → CONTIG/GAP | record hb evidence | Ended(clean-by-anchor) | n/a (is anchor) | DEAD |
| CONTIG | §5 apply | hb>W → GAP open; else count | Ended | boundary → INIT | DEAD |
| GAP | §5 apply (stage or fill) | extend evidence_hwm | Ended+GapUnresolved | boundary → INIT (gap closes via boundary) | DEAD |
| ENDED | violation (data-after-EOS) | count | count (EOS dedup) | boundary → INIT | DEAD |
| DEAD | count ignored | count ignored | count ignored | count ignored | — |

ENDED is per-session: a session boundary from ENDED re-enters INIT for the
new session. DEAD is global-terminal: every further ingest is counted as
`ignored_after_dead` and returns.

## 4. The Window Invariant W1 — and the Zombie Slot Hazard

**W1 (quiescent form).** At every quiescent point (entry/exit of
`ingest`): every staged message S satisfies `W < S < W + 1024`, the
staging map `S ↦ S & 1023` is injective, and `staged_count` equals the
number of nonzero `lens`.

**Why injectivity is free.** Two staged S₁ ≠ S₂ sharing a slot differ by
≥ 1024; but both lie in an open interval of width 1024 — contradiction.
No generation tags, no ABA tags, no seq-per-slot bookkeeping. **But this
proof has a hole, and the hole is a real bug:**

### 4.1 The Zombie Slot Hazard (found by writing this proof)

Failing trace on the NAIVE design (advance-without-clear):

```
W=100. P1 delivers [200..210] → staged (gap opens, head=200).
P2 delivers [100..205] → in-order branch: emit 100..205 from P2's bytes,
       W := 206.   Slots 200..205 now hold STALE copies (P1's bytes) of
       messages ALREADY EMITTED (P2's bytes) — zombie slots.
Drain stops at 206 (hole). Zombies 200..205 linger undetected.
Hours of traffic pass. W climbs to 1224.  1224 & 1023 == 200.
Drain reads lens[200] != 0 → emits ARENA[200..] as message 1224 —
       64 BYTES OF WRONG DATA, then 1225, 1226… through the zombie run.
```

Silent downstream corruption, zero crashes, confluence violated in the
worst way: late, rare, and reproducible only under a specific arithmetic
coincidence (`W ≡ zombie (mod 1024)`). A fuzz suite would likely never
hit it; a golden conformance run over a full day might — or might not.

### 4.2 Clear-on-Advance Law (the fix — W1 is true BECAUSE of this)

1. **Drain self-clears:** each slot it emits is zeroed before W advances
   past it.
2. **Frame-advance clears:** when the in-order branch jumps W
   (`W_new = last+1 > W_old+1`), the engine clears every slot in
   `[W_old, min(W_new, W_old+1024))` — **guarded by
   `if staged_count != 0`**, so the steady-state in-order path pays ONE
   compare, not a walk. Steady state (SP-1) never touches the window;
   the walk runs only when staging exists, bounded by packet span.

With the law, zombies cannot exist, and W1's quiescent form holds. The
doc-01 claim "no generation tags, no ABA" survives — via this law and
only via this law. Test U-ZOMBIE (§14) replays the exact trace above and
must fail on the naive build / pass on the lawful build.

**W1 corollaries:** slot collisions impossible (above) · messages beyond
window never staged (clamp, §5 stage C) · drain is bounded by 1024
emissions per call · `emit_from_frame` is bounded by frame bytes ÷ 14.

## 5. Ingest Algorithm (NORMATIVE pseudocode)

```
ingest(frame, feed, now_ns, sink):

  S0 FRAMING HEADER
     if frame.len() < 20 → viol(Truncated); return
     hdr = header(frame)                          # infallible past 20B
     if state == DEAD → cnt.ignored_after_dead++; return

  S1 SESSION DISPATCH
     if session != hdr.session → session_boundary(hdr, sink)   # §8
         # flush window+gap+hb+pending; gen++; emit SessionBoundary;
         # re-enter INIT anchored by THIS packet (fall through)

  S2 KIND CLASSIFY
     if count == 0      → heartbeat(hdr.seq, feed, now_ns); return
     if count == 0xFFFF → end_of_session(hdr, sink); return

  S3 SPAN + DUPLICATE FAST PATH        # runs BEFORE full validation
     (first,last) = span(hdr)  checked; overflow → viol(SeqOverflow)
     if last < W → per-feed dup++; return           # ~15 cycles, done.
     # Sound: these seqs were already validated+emitted on first
     # arrival. Content-disagreement between feeds is out of threat
     # model (same venue); policy is first-wins, never re-emitted.

  S4 FULL VALIDATION (atomic, P-2)
     pkt = validate_frame(frame)                    # doc 03 packet.rs:
     else viol(e) → return                          # framing + every
                                                     # block's ITCH len

  S5 APPLY
     if first <= W:                                 # in-order / overlap
         old_w = W
         emit_from_frame(pkt, first_seq=W, sink)    # zero-copy slices
         W = last + 1
         if staged_count != 0: clear_slots(old_w, W)   # §4.2 LAW
         progress_vt = now_ns
         drain(sink)                                # §6
         check_gap_close(sink)                      # §7
     else:                                          # ahead of W
         gap_evidence(first, sink)                  # §7: maybe GapOpened
         stage_clamped(pkt):                        # §5A
             for S in [first..=last]:
                 if S < W + 1024: stage(S)          # copy ≤64B, lens set
                 else: cnt.beyond_window++          # NOT staged — cache
             max_staged = max(max_staged, last)
         drain(sink); check_gap_close(sink)
         # note: no progress_vt update unless W advanced (drain did it)
```

**Steady state** (in-order, first == W, empty window): S0→S3 header
parse, two compares, S4 walk, S5 emit, one `staged_count != 0` false
compare. The window is never written. SP-1 preserved.

## 6. Drain

```
drain(sink):
    while lens[W & 1023] != 0:
        emit arena slot (W & 1023) as message W   # slice, zero-copy
        lens[W & 1023] = 0; staged_count--
        W += 1; progress_vt = now (caller's)
    if staged_count == 0: max_staged = 0
```

Bounded: ≤ 1024 emissions per call (W1). Each emitted slot cleared
before W passes it — drain can never re-read its own output.

## 7. Gap Lifecycle & Event Grammar

**Evidence.** A gap is *known* — never guessed — when the engine holds
evidence that seqs `[W, X)` were transmitted but not received. Evidence
sources: (a) any staged message at seq > W (transmission is ordered);
(b) a heartbeat announcing `next_seq > W`; (c) a packet's `last` beyond
the window clamp (its dropped tail is evidence).

```
gap_evidence(x):                       # x = highest KNOWN-transmitted+1
    if !gap_active and x > W:
        gap_active = true; gen++
        emit GapOpened{from: W, ahead: Some(x), gen}
        evidence_hwm = x
    else if gap_active:
        evidence_hwm = max(evidence_hwm, x)

check_gap_close():                     # after any W advance
    if gap_active and W >= evidence_hwm:
        gap_active = false
        emit ReAnchored{gen, at: W}
        evidence_hwm = 0
```

**Gen law (G-INV).** `gen` increments at exactly two sites: GapOpened,
SessionBoundary. Every emitted message's proof carries the gen current
at emission; any event carrying a higher gen retroactively invalidates
all lower-gen proofs. ReAnchored reports the closing gap's gen without
incrementing. Consequence: messages before a gap and after it are
distinguishable downstream by gen — the "stale token" story without any
runtime revocation machinery.

**Expected telemetry:** under seeded jitter, gaps open/close constantly
(one pair per reorder). This is correct behavior, not noise: each
reorder event is a true transient gap. VR-1 asserts gap_pairs ≈ reorder
count as a sanity check, not a fixed number.

**Pairing invariant (AM-2).** Every GapOpened is eventually paired with
ReAnchored, SessionBoundary, EndOfSession(GapUnresolved), or SessionDead.
ConformanceSink enforces this — no orphans.

## 8. Session Lifecycle

**Anchor law (A-1).** W anchors at the first data packet's `seq` of a
session. Messages before the anchor are *unknowable* (no evidence they
exist) — not gaps, not fabricated, documented limitation. Heartbeats
before the first data packet record evidence only.

**Boundary.** Session-field change: flush window (count
`window_flushed`), close any open gap WITHOUT ReAnchored (the boundary
closes it — pairing invariant above), clear hb/pending/evidence, `gen++`,
emit `SessionBoundary{prev, next, gen}`, re-enter INIT anchored by the
triggering packet.

**EOS law.** First EOS per session: if `W == announced_next` → clean;
else emit EndOfSession carrying `final_wm = W` AND `announced_next` —
**AM-2 schema change:** the doc-01 enum lacked `announced_next`, without
which a sink cannot distinguish clean from unclean ends. Unclean ⇒ the
GapUnresolved report IS the event itself (`final_wm < announced_next`).
State → ENDED. Subsequent EOS for the same session: counted, no event
(EOS dedup — discharges the doc 04 §7 flag). Data after EOS, same
session: violation, counted, dropped.

## 9. LiveFeedProof Minting (mechanism; full safety argument → doc 06)

Exactly **two** mint sites exist, and both are inside contiguous
emission: `emit_from_frame` and `drain`. Both call
`sink.on_msg(&proof, seq, msg)` with a proof minted *per message*
carrying current gen. Gap-open, boundary, seal, and dead states are
structurally unreachable from these sites. Compile-fail tests (doc 06)
prove no other construction path exists; U10 proves gen correlation.

## 10. recovery_intent Mechanism (constants live in doc 08)

```
recovery_intent(now_ns) -> Option<Intent{from: W, to_excl}>:
    # trigger inputs (evaluated in order):
    T-HWM:   max_staged - W >= 512                    → target = max_staged
    T-TIME:  staged_count > 0 and now - progress_vt >= 250µs
                                                     → target = max_staged
    T-HB:    hb_seq > W and now - hb_vt >= 250µs      → target = hb_seq
    (none)   → if pending_to exists and now - last_intent_vt >= resuggest
               window → re-emit [W, pending_to)      # client counts retries
               else → None
    target = min(target, W + 65535)                   # wire cap, doc 02 §4
    if pending_to is None or target > pending_to:
        pending_to = target; last_intent_vt = now
        return Some([W, target))
    return None
    # pending cleared when W >= pending_to (gap closed through range)
```

Properties: intents always start at current W; W is monotone ⇒ ranges
extend rightward only ⇒ widen-and-supersede is total (N-2); every round
either advances W (iterative liveness — each round fills up to 65535
messages) or exhausts retries → SessionDead via doc 08. `seal(reason)`:
state → DEAD, emit `SessionDead{reason, last_wm: W}`; engine invokes it
when the recovery client's retry cap trips.

## 11. Counters (Copy struct; no strings, no maps)

Per-feed: `packets`, `dups`, `bytes`. Global: `violations` (by FrameError
class — fixed 6 + 1 ITCH class), `staged_msgs`, `dup_msgs`,
`beyond_window_dropped`, `window_flushed`, `gap_opens`, `reanchors`,
`heartbeats`, `eos_seen`, `eos_dup`, `sessions`, `data_after_eos`,
`ignored_after_dead`, `intents_issued`.

## 12. Cycle Budgets (design budgets — PR-4; gating is PR-1/2/3)

| Branch | Budget |
|---|---|
| Pure duplicate (S3 exit) | ≤ 15 cycles |
| In-order msg (S0–S5, excl. sink) | ≤ 10 + validate-walk amortized |
| Staged msg (copy + later drain emit) | ≤ 20 + sink |
| HashSink (fnv over ~28 B avg msg) | ~40 cycles |

Throughput math: 10M msg/s @ 3.5 GHz = **350 cycles/msg total budget**;
worst realistic path (validate + emit + hash) ≈ 150. ≥ 2× headroom. The
sequencer will not be the bottleneck; if PR-1 fails, look at the sink
first, the sequencer last.

## 13. Layout Diagram

```
┌─ line 0 ────────────────────────────────┐
│ W (u64)                                 │ ← written once per packet
├─ line 1 ────────────────────────────────┤
│ gen │ session[10] │ state │ gap flags   │ ← written per event
├─ lines 2–3 ─────────────────────────────┤
│ Counters (Copy)                         │
├─ lines 4–19 ────────────────────────────┤
│ lens[1024]  (u8 × 1024)                 │ ← 16 lines, read/write on
│                                         │   disorder paths only
├─ lines 20–1043 ─────────────────────────┤
│ arena[64 KiB]  slot i @ i<<6            │ ← 64 B/slot, whole window
│                                         │   spans 1025 lines
└─────────────────────────────────────────┘
```

## 14. Test Matrix

| # | Test | Pass condition |
|---|---|---|
| U1 | Dup fast path (B re-delivers A's range) | per-feed dup counters; zero emission |
| U2 | Partial overlap (packet straddles W) | only `[W, last]` emitted, once |
| U3 | Reorder (P2 before P1) | staged then drained; strict monotonicity |
| U4 | Window clamp (span beyond W+1024) | prefix staged, tail counted `beyond_window`, evidence extended |
| **U-ZOMBIE** | **The §4.1 trace, verbatim, extended to W=1224** | **naive build fails (wrong bytes); lawful build byte-identical** |
| U5 | W1 property: 10⁶ seeded random (packet, drop, advance) ops | invariant holds at every quiescent point; `staged_count` == nonzero lens count |
| U6 | Session change mid-staging | flush counted, re-anchor, gen++, boundary event |
| U7 | Heartbeat-opened gap (nothing staged) + data closure | GapOpened→ReAnchored pair |
| U8 | Clean EOS vs unclean EOS | final_wm ==/≠ announced_next; no second event |
| U9 | EOS-then-data; double EOS | violation / counted-dup, no events |
| U10 | Gen law | strictly increasing across event stream; proof gen correlation |
| U11 | seal() | terminal; all later ingest counted-ignored |
| U12 | recovery_intent: all three triggers (synthetic `now_ns` — time is a parameter, no clock mocking), widen rule, pending clear on W advance | exact intents as sequences |
| U13 | Transition table totality | every (state,event) cell exercised ≥ once |
| U14 | Emitted-vs-staged byte identity | staged copies byte-equal to source frames |
| **E2E-1** | **Mode-1 chaos (guarantee_coverage) over mini: both feeds, jitter, reorder, single-feed loss, session split** | **HashSink hash == golden(mini); count == N; final W == N+1; zero violations; ConformanceSink invariants hold (gen law, gap pairing, no-orphan events)** |
| E2E-1b | Same schedule, run twice | byte-identical sink state both runs |
| E2E-1c (rec.) | Dev sample (200 MB) mode-1 | same contract; Termux timing recorded, not gated |

## 15. Doc 01 Amendments (record in 01's changelog)

- **AM-2:** Event grammar closure set for gap pairing includes
  SessionBoundary; `EndOfSession` gains `announced_next: u64`;
  `Sequencer::new()` takes no args (session discovered on first packet —
  scaffold signature retired).
- **AM-3:** The doc-01 invariant "no generation tags / no ABA" is
  affirmed WITH the Clear-on-Advance Law (§4.2) as its enabling
  mechanism — recorded so the claim's dependency is explicit.

## Changelog

| Date | Version | Entry |
|---|---|---|
| 2026-08-30 | 1.0 | Initial: normative ingest, W1 + zombie hazard + clear-on-advance law, event grammar, gen law, session lifecycle, intent mechanism, U1..U14, E2E-1. |
