# 08 — Recovery Engine: UDP MoldUDP64 Retransmission & Dual-Drop Endgame (v2.0)

```
Status:    FROZEN (v2.0 reaffirmed under C9)
Exit Gate: E2E-2a/b/c green on Termux AND CI; recovery-inclusive
           ALLOC_DELTA=0 + strace diff (PR-3 stage 2); R1..R14 green.
Evidence:  Run URLs; intent-sequence assertions output; server counters;
           alloc/strace lane logs covering the recovery window.
Authority: This doc owns UDP re-request client, non-blocking socket loop,
           intent→request protocol (C9), retry/SessionDead semantics,
           vt-grace determinism law, fake UDP server contract.
```

---

## 1. The Determinism Split (the load-bearing idea of this phase)

Two clocks, two responsibilities, one boundary:

| Concern | Clock | Deterministic? | Why |
|---|---|---|---|
| Emitted message bytes | virtual (vt) | **YES — byte-identical across runs** | L2 confluence: recovered packets enter the same ingest; arrival order is irrelevant to output |
| Intent emission + retry count | virtual (vt) | **YES — exact sequence** | AM-5 grace-stepping (§6): vt advances through silence in grace-sized steps; each step re-evaluates intents; exactly 4 steps → SessionDead |
| Event stream (GapOpened timing, gen) | mixed | NO (S-1) | observation telemetry; invariants only, never golden-compared |
| Socket mechanics (sendto, recvfrom) | real | NO — and irrelevant | Plumbing; affects latency, never bytes (row 1), never retries (row 2) |

---

## 2. Architecture (v2.0 UDP Retransmission per C9)

```
┌─ Thread H (hot path) ────────────────────────────────────────────┐
│ P-ORDER (poll order law):                                        │
│  1. poll UDP recovery socket -> ingest()                         │
│  2. poll UDP multicast / replay transport -> ingest()            │
│  3. recovery_intent(now_vt) -> sendto 20-byte request + retry cnt│
│  4. transport.poll advances vt under AM-5 clamp (§6)             │
└──────────────────────────────────┬───────────────────────────────┘
                                   │ UDP unicast (Request / Responses)
                                   ▼
┌─ Fake Retransmission Server (nf-testkit, harness thread) ────────┐
│ std::net allowed · serves ground truth · seeded fault injection  │
└──────────────────────────────────────────────────────────────────┘
```

## 3. Channels

**PacketMailbox (R → H), SPSC ring, 16 slots × 1500 B.** Cache-line-padded head/tail. R **parks** on full (spin + exponential backoff capped at 1 ms, never allocates, never drops — O-3). H drains every iteration (P-ORDER step 1), so a parked R is structurally impossible to deadlock *provided* vt only freezes under the grace law (§6).

**CmdChannel (H → R) — AM-4: latest-wins register, superseding the 8×24B ring of docs 00/01.** Justification (record in doc 01 changelog): intents are widen-only and monotone (N-2), so the newest intent subsumes every queued older one — a queue of stale intents is pure overhead with a failure mode (R acting on a superseded range). Spec: 64-B-aligned slot `{ epoch: u64 (odd=writing), intent: RecoveryIntent, session: [u8;10], status_word: u32 }`. H publishes: epoch++ (odd) → write payload → epoch++ (even). R reads: spin until even, copy, re-check epoch. Zero allocation, no locks, latest-wins by construction.

**Session register:** the `session` field rides CmdChannel payloads (not a separate channel). R stamps every request with the register value at send time.

**INV-R5 (stale-session guard — without this, a bug class lives here):** R **drops any response packet whose session ≠ current register value** (counted `stale_session_dropped`). Reason: a late retransmission response from a dead session, forwarded blindly, would hit the sequencer's S1 session dispatch and *flip the engine into a boundary* — our own plumbing artifact masquerading as venue truth. UDP can't have this problem (the venue sequences its own sessions); TCP can, because we request. Filter at the source.

## 4. Thread R Loop (normative pseudocode)

```
R::run():
    rx: [u8; 65536]   // startup-allocated (doc 01 §6)
    have = 0; scan = 0; state = Disconnected
    loop:
        if let Some(cmd) = cmd.take_latest(): apply(cmd)     // record range,
                                                             // connect/send
        match state:
          Disconnected => try_connect()       // libc socket, O_NONBLOCK,
                                              // IP literal — doc 07 traps
          Connecting   => poll_writable()     // POLLOUT via poll(2), no epoll
          Connected    => drain_socket()

drain_socket():
    n = recv(fd, rx[have..], MSG_DONTWAIT)
    n > 0  => have += n; parse_framed()
    n == 0 => status = ServerClosed; state = Disconnected     // VERIFY-4 path
    EAGAIN => {}
    else   => status = SocketError(errno); state = Disconnected

parse_framed():
    while have - scan >= 2:
        len = BE_u16(rx[scan..])
        if len > 1500:                       // oversize/hostile — mailbox
            malformed_stream++; go_disconnect(); return        // can't bound
        if have - scan < 2 + len: break                        // need more
        pkt = rx[scan+2 .. scan+2+len]
        if pkt.session == cmd.latest_session: packets.push_park(pkt)
        else: stale_session_dropped++
        scan += 2 + len
    memmove(rx, rx[scan..have]); have -= scan; scan = 0        // compact

apply(cmd):
    if state != Connected: return            // next loop iteration connects;
                                             // cmd persists in register
    count = min(cmd.to_excl - cmd.from, 65535)                // doc 02 §4 cap
    encode_request(&cmd.session, cmd.from, count, &tx20)      // raw 20B, no
    send(fd, &tx20, MSG_DONTWAIT)                             // prefix (V-3)
```

Notes that are law: **no `std::net`** (io::Error construction allocates on error paths; raw errno ints only — this **resolves doc 01 R-1** and is recorded as ADR-0006). **Resync on malformed stream is forbidden** — an oversize length means the stream is untrustworthy; disconnect and let intents re-drive. **Requests are never queued**: one outstanding logical range (the register's latest); superseded ranges are simply never sent. Duplicate service (two overlapping requests) is harmless — C-1 kills the overlap at ingest.

## 4a. Loop Termination Law (engine binary & test harness)

The engine loop — in the binary *and* across test harnesses — runs until:
$$\text{sequencer.state} \in \{\text{ENDED}, \text{DEAD}\}$$
plus (multi-session) schedule events remaining in transport.

ENDED-clean, ENDED-unclean (`final_wm < announced_next`), and DEAD are all *terminal outcomes asserted against expectations* — never conditions to spin past. One universal termination rule everywhere guarantees that harness semantics cannot diverge from engine semantics (P-ORDER's twin: the poll order law and the loop termination law).

## 5. Retry & SessionDead Semantics (deterministic — hot thread owns them)

- **Retry counting lives in Thread H**, not R. Counter `intent_emissions` resets when `pending_to` widens; each re-emission (resuggest window elapsed, no W progress) increments.
- **Trip:** `intent_emissions == 4` with `W < pending_to` → engine calls `seal(reason)`: reason = `TcpUnreachable` if R's status word shows unreachable/closed at trip time, else `RetryExhausted`.
- R's socket failures feed the *reason*, never the *count*. Determinism preserved: the count is pure vt arithmetic.

## 6. AM-5 — vt Grace Law (doc 04 §5 clock-law amendment; doc 04 changelog)

**Problem:** doc 04's clock law jumps vt to the next event. With a response in flight on real-time loopback, vt could outrun the response and reach EOS → spurious `GapUnresolved`. With vt frozen waiting, a silent server would freeze retries forever. Both failure modes are real; one law kills both.

**Law:** when the sequencer holds a pending intent, the clock jump target is:

```
jump_to = min(next_event_vt, last_intent_vt + grace_ns)
```

After every jump, H evaluates `recovery_intent(now)` (P-ORDER step 3). If the server answered, mailbox delivery unblocks normal flow. If silent, the resuggest window (== grace_ns; **align these constants — registry update**) has elapsed → re-emission → `last_intent_vt` advances → new ceiling. **vt walks through silence in exactly grace-sized steps: 4 steps, then SessionDead.** Retry sequences are therefore byte-derivable from config. Default `grace_ns = 10_000_000` (10 ms); a healthy loopback answers in microseconds, so grace steps only occur under injected faults — which is precisely when we want deterministic stepping.

## 7. Fake Retransmission Server (nf-testkit — harness, allocations allowed, determinism mandatory)

- `TcpListener::bind("127.0.0.1:0")` → ephemeral port reported to the engine config at construction. Accept loop; one thread per connection; **std::net allowed** (harness).
- Constructed with `Vec<SessionTruth>`: `{ session_id, seq_base_to_index }` mapping sequence → ground-truth message index (the fabricator knows its own split arithmetic; the server never guesses).
- Request handling: read exactly 20 bytes → `(session, from, count)` → locate SessionTruth (mismatch → fault counter, close) → serve messages `[from, from+count)` packed into response packets (fill ≤ 1400 B each: 20B header + blocks, sequence-ordered), each prefixed 2-byte BE length (VERIFY-3, responses only).
- **Fault injection (config enum, no RNG where avoidable):**

| Mode | Behavior |
|---|---|
| `Ok` | correct service |
| `DelayMs(u64)` | sleep before responding (real time — harmless, §6) |
| `CloseOnRequest(n)` | close connection on nth request (VERIFY-4 exercise) |
| `IgnoreRequest(n)` | silence on nth request (retry path) |
| `TruncateAfter(u16)` | serve only k messages of the range (partial fill → widening) |
| `WrongSession` | respond with foreign session (INV-R5 exercise) |
| `DuplicateFirst` | serve first packet twice (C-1 live exercise) |

- **Counters (asserted by tests):** `requests_seen`, `packets_served`, `connections`, `faults_injected`. Deterministic output for deterministic input.

## 8. Session Boundary During Recovery

On `SessionBoundary`: hot thread clears `pending_to` (doc 05 §8), publishes new session via CmdChannel; register overwrite makes R's next request speak the new session. In-flight old-session responses die at INV-R5. No explicit cancellation protocol exists — **the register IS the cancellation.** (Design note: a cancel message would be a second source of truth about liveness; the register is the only one.)

## 9. LiveFeedProof Interplay

Recovered packets fill the gap → drain emits with fresh proofs → `ReAnchored{gen}` closes the era. No special proof path exists for recovered data — it's contiguous emission like any other (doc 06 G-T4 covers it structurally). Test R9 asserts proof-gen continuity across a dual-drop repair.

## 10. Zero-Allocation — Stage 2 (PR-3 discharge completes at G8)

The measured window (doc 07 §2) now **includes Thread R, TCP, and the server harness boundary**: from WINDOW_BEGIN to final EOS, `ALLOC_DELTA == 0` for the *engine process* (the fake server runs in-process for E2E-2 — therefore the server must ALSO be zero-alloc in-window, OR run as a separate process). **Decision (record as ADR-0007): the fake server runs as a separate `nf-testkit` binary child process.** Rationale: keeps harness `std::net` convenience out of the engine's alloc window; process boundary is the cleanest firewall; CI spawns server first, passes port via argv. Engine-side obligations: R's buffers are startup-allocated; `send`/`recv` are direct syscalls; no io::Error; no DNS; park loops are pure spins. Strace lane (L3) runs the same pair and asserts the engine process's syscall diff is empty.

## 11. Memory Inventory Additions (doc 01 §6 amendment)

| Region | Size | Owner |
|---|---|---|
| R rx buffer | 64 KiB (already reserved) | R |
| R tx buffer | 20 B stack | R |
| PacketMailbox | 16 × 1500 B + indices | H↔R |
| CmdChannel register | 64 B | H→R |
| Fake server | separate process — uncounted | harness |

## 12. Test Matrix

| # | Test | Pass condition |
|---|---|---|
| R1 | CmdChannel: publish/take under concurrent hammering | latest-wins always observed; epoch protocol sound (no torn reads over 10⁶ ops) |
| R2 | PacketMailbox: full → park → drain | no drops, no overwrites, FIFO |
| R3 | INV-R5: server `WrongSession` | packets counted-dropped; sequencer session unchanged |
| R4 | Request encode from intent | exact 20B (TV-4 reuse); count cap 65535 applied |
| R5 | Partial fill (`TruncateAfter`) | widening intent sequence exactly [W, head_then_wider); eventual completion |
| R6 | `CloseOnRequest` ×4 | SessionDead{RetryExhausted}; `last_wm` correct; post-dead ingest ignored |
| R7 | `IgnoreRequest` ×4 | same as R6 via silence path |
| R8 | `DuplicateFirst` | zero duplicate emissions (C-1 live); hash unchanged |
| R9 | Dual-drop repair | GapOpened→ReAnchored pair; proof gen continuity; intent sequence matches config-derived expectation exactly |
| R10 | Session boundary during pending recovery | pending cleared; R speaks new session; stale responses dropped (INV-R5) |
| R11 | Oversize framed packet (>1500) | malformed_stream++; disconnect; recovery re-drives; no UB, no alloc |
| R12 | vt-grace stepping (silent server) | exactly 4 grace steps; intent emissions at exact step boundaries; deterministic across double-run |
| R13 | Retry counting determinism | same retry count across 3 runs with server DelayMs(50) — count is vt-driven, not wall-driven |
| R14 | Full transition coverage incl. Connected↔Disconnected under every fault mode | no unreachable states, no hangs (watchdog: test fails at 30 s) |
| **E2E-2a** | **TEST-DUAL-DROP-GAPFILL canonical:** mini, split OFF, scripted drop [X..Y] on both feeds, `guarantee_coverage=false`, server `Ok` | hash == golden(mini); count == N; watermark == N+1; dup emissions == 0; ALLOC_DELTA == 0 (recovery-inclusive); server counters match config |
| E2E-2b | Split ON **and** dual-drop spanning the boundary | boundary + recovery interact lawfully; watermark == N−m+1 (see A-1) |
| E2E-2c | E2E-2a double-run | hash identical; retry/intent sequences identical; events satisfy invariants (not exact-match) |

## 13. Constants Registry Updates (doc 00 §10 changelog)

| Constant | Value | Change |
|---|---|---|
| `grace_ns` | 10 ms | NEW — AM-5; **aliased to resuggest window** (doc 05 §10) |
| CmdMailbox 8×24B | superseded | AM-4 → CmdChannel latest-wins register |

## 14. Amendment / ADR Register (paste into owning docs)

- **AM-4** (doc 01): CmdChannel latest-wins register replaces command ring — rationale §3.
- **AM-5** (doc 04 §5): grace-clamped clock jumps — law §6.
- **ADR-0006**: raw libc sockets on Thread R (io::Error allocation hazard); resolves doc 01 R-1.
- **ADR-0007**: fake server as separate process (alloc-window firewall) — §10.
- **Doc 00 changelog v1.4**: PR-3 stage-2 discharge scheduled at G8; constants registry updated.

## Changelog

| Date | Version | Entry |
|---|---|---|
| 2026-08-30 | 1.0 | Initial: determinism split, INV-R5, grace law, latest-wins cmd channel, fake server contract + fault modes, R1..R14, E2E-2a/b/c, stage-2 zero-alloc. |
