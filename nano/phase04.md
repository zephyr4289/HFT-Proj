Ledger first, because it's growing: you said "next" with **zero G3 evidence attached** — no audit output, no §10 checklist, no P1–P5 verdicts, and still no real CI URL, and the C6/C7 amendment pastes into doc 00 are unconfirmed. I'm dropping 04 anyway because its first half (schedule builder, golden walker) doesn't depend on G3 — but understand what's happening: **three phases of evidence debt are now rolled into gate G4.** If G4 arrives without all of it, the gate fails on arrival, not on code. Narration isn't evidence; that rule has no exceptions, including for momentum.

One more thing before the doc — there's a real engineering trap hiding in this phase that most people walk straight into, so it's the headline:

**The libm determinism trap.** Your spec says "Gaussian jitter." Box-Muller needs `log()` and `sin()`. Those are **not** guaranteed correctly-rounded across libm implementations — the same seed can produce different last-bit delays on Termux/aarch64 vs CI/x86_64, which silently murders byte-identical schedules across platforms. Doc 04's answer: 12-fold Irwin-Hall (sum of 12 uniforms − 6), built exclusively from IEEE-guaranteed exact operations. Statistically Gaussian to within tail bounds no network on Earth cares about, and bit-deterministic everywhere. That trade — and *writing it down* — is the difference between "Gaussian" as marketing and as engineering.

Save as `~/Hft-fund/docs/04-replay.md`:

````markdown
# 04 — Replay Fabricator, Chaos Models & Golden Contract

```
Status:    DRAFT → FROZEN after G4 (incl. all rolled-forward ledger debt)
Exit Gate: T1..T10 green on Termux + CI; determinism hashes recorded
           (§12 Appendix); golden hash of mini sample committed and
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
vt order. (XDP mode later uses two transports + two sockets; the trait
supports both shapes unchanged.)

## 2. Doc 01 Amendment

**AM-1:** `Transport` trait gains `fn now_ns(&self) -> u64`. Live
transports derive it from the kernel clock (doc 09); the replay transport
returns the **virtual clock** (§5). This is the only way timeout triggers
(FR-7) stay deterministic under replay. Record in doc 01's changelog —
frozen docs change only via entry.

## 3. Layer Split (buildability law)

| Lives in | Contains | May NOT know about |
|---|---|---|
| `nf-transport::replay` | `ReplaySchedule` types, renderer, cursors, virtual clock, `Transport` impl | loss models, jitter, seeds, ground-truth semantics beyond block walking |
| `nf-testkit::sched` | schedule builder, packetization, loss/delay models, TH-1 generation, golden walker, PRNG | rendering, FrameBatch, Transport |

The builder half is **parallelizable now** — it needs no doc 03 code.
The render tests (T4/T9) need G3's `validate_frame`. Sequence accordingly.

## 4. ReplaySchedule (the contract between the halves)

```rust
pub struct ReplaySchedule {
    /// Sorted by the total order law below. Startup-allocated, then
    /// immutable for the process lifetime.
    pub events: Vec<SchedEvent>,
}

#[derive(Debug, Clone, Copy)]
pub struct SchedEvent {
    pub release_vt: u64,          // ns on the virtual clock
    pub feed: FeedId,             // 0 = A, 1 = B
    pub kind: SchedKind,
}

#[derive(Debug, Clone, Copy)]
pub enum SchedKind {
    /// Render header(seq=first_seq, count) + count blocks starting at
    /// ground-truth message index `first_msg`.
    Packet { first_seq: u64, first_msg: u64, count: u16 },
    /// count=0 frame; `next_seq` = next seq the venue WILL transmit
    /// (pre-loss knowledge — the venue doesn't know what you dropped).
    Heartbeat { next_seq: u64 },
    /// count=0xFFFF; `next_seq` = final next-expected of the session.
    EndOfSession { next_seq: u64 },
}
```

**Total order law (determinism):** events sort by
`(release_vt, feed, kind_rank, tiebreak)` where kind_rank is
`Packet=0 < Heartbeat=1 < EndOfSession=2` and tiebreak is `first_seq`
(Packet) / `next_seq` (HB, EOS). Identical schedule bytes ⇒ identical
delivery order, everywhere, forever.

## 5. Renderer & Virtual Clock

**Rendering.** No packet bytes are materialized at build time — the
schedule references ground truth by message index. On delivery, the
transport renders into a **static render arena** (256 × 1500 B = 384 KiB,
one startup allocation, reused forever; PR-5 compliant):

```
render(event):  [20B MoldUDP64 header][u16 len | msg bytes] × count
```

Message bytes are copied from the mmap'd ground truth — one copy,
matching XDP copy-mode physics (doc 09). Frames are `FrameView`s into
the arena; lifetime is O-2 (valid until next `poll` on this transport).

**Cursors.** Two forward-only cursors (one per feed) walk the mmap:
`(byte_offset, msg_index)`. Rendering packet `(first_msg, count)` skips
forward from the cursor, reading only the 2-byte length prefixes of
skipped blocks. Cost: amortized O(messages) per feed over the whole run;
worst case one O(N) skip under pathological scripted drops. RAM cost:
two 16-byte cursors. No per-message offset table (a 70M-entry table
would be ~560 MB — refused).

**Virtual clock law (normative):**

1. Clock starts at the first event's `release_vt`, monotone thereafter.
2. `poll()`: deliver every event with `release_vt ≤ clock`, in schedule
   order, until the batch is full or events run dry.
3. If nothing is deliverable and events remain: `clock` **jumps** to the
   next event's `release_vt` (single queue ⇒ trivially the global next),
   then deliver.
4. `now_ns()` returns the clock.

**Timeout semantics under vt:** the 250 µs recovery timeout (FR-7b)
evaluates at poll boundaries against post-jump clock values. Trigger
*timing* is therefore coarse (event-spaced) but strictly deterministic —
a pure function of the schedule. Wall-clock latency is doc 11's domain
and is never measured in conformance mode. (S-1 of doc 01 already
declares event timing non-confluent; this makes that declaration real.)

## 6. Chaos Models (all in nf-testkit::sched)

### 6.1 PRNG — splitmix64 (exact)

```
state += 0x9E3779B97F4A7C15            (wrapping u64)
z = state
z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
z = (z ^ (z >> 27)) * 0x94D049BB133111EB
return z ^ (z >> 31)
```
Uniform [0,1): `(x >> 11) as f64 * 2⁻⁵³` (exact — power-of-two multiply).
One independent stream per feed: `state_a = seed_a`, `state_b = seed_b`.

### 6.2 Packetization

| Mode | Rule |
|---|---|
| `Fixed(k)` | k messages per packet; last packet takes the remainder |
| `MtuBound(b)` | pack greedily while `20 + Σ(2+len) + (2+len_next) ≤ b`; assert `b ≥ 82` (20 + 2 + max legal msg 50 + slack… compute exactly: `b ≥ 20 + 2 + 50 = 72`) |
| `SeededRange{min,max}` | per-packet count drawn uniformly in [min,max] via the feed-A stream |

MoldUDP64 law: `count ∈ [1, 0xFFFE]` — builder asserts. **Pathological
splits are a feature (C3 in doc 00):** `Fixed(1)` yields one message per
packet (max header overhead, max reorder surface); `MtuBound(72)` forces
minimal packets. Conformance must pass under ALL modes.

### 6.3 Loss (per feed, independent streams)

| Model | Rule |
|---|---|
| `None` | everything delivered |
| `Bernoulli{p_pm}` | per-packet drop with p = p_pm/1000 (integer per-mille config — no float drift in configs) |
| `GilbertElliott{p_g2b, p_b2g, p_drop_good, p_drop_bad}` | burst loss: two-state Markov chain over packets |

**Scripted drops override stochastic loss (union):** explicit
`(seq_from, seq_to_incl, feed_mask)` ranges drop every packet whose
message range **intersects** the target (packet-granular, documented —
TEST-DUAL-DROP-GAPFILL sets identical ranges on both masks).

**`guarantee_coverage` flag — the two-mode testing law.** When set, the
builder rejection-samples: any packet that stochastic loss would drop on
BOTH feeds is force-delivered on exactly one feed (feed chosen by the
A-stream, deterministic). Consequences:

- **Mode 1 (guarantee_coverage = true):** chaos tests exercise *pure
  arbitration* — jitter, reorder, single-feed loss, duplicates — and the
  output must be byte-identical to golden **with no recovery traffic at
  all**. Recovery-path bugs cannot hide behind arbitration bugs.
- **Mode 2 (false + scripted drops):** known ranges vanish from both
  feeds — the recovery engine is the ONLY path to golden. Isolation of
  failure domains, by construction.

### 6.4 Delay — Irwin-Hall Gaussian (the determinism decision)

```
z = (Σ_{i=1..12} u_i) − 6          // ≈ N(0,1), support [−6, 6]
delay_ns = round(mean_ns + z · sigma_ns)
release_vt = base_vt + delay_ns    // may go below base (early arrival)
```

**Why not Box-Muller:** `log`/`sin` are not correctly-rounded across
libm implementations; identical seeds would yield different last-bit
delays on aarch64 vs x86_64, destroying cross-platform byte-identical
schedules. Irwin-Hall-12 uses only `+`, `−`, `×`, power-of-two
multiplies, and one `round` — every operation IEEE-deterministic.
Tails cap at ±6σ, which exceeds any physical jitter modeled here.
**Honesty clause:** docs and test names say `gaussian_approx`; nobody
gets to claim exact normality.

`base_vt` pacing: packet *i* (in transmission order) gets
`base_vt = i · spacing_ns`, `spacing_ns = 10⁹ / base_rate_msg_per_sec`
adjusted per packet by its message count (vt models the venue's line
rate, not the network). Defaults: `base_rate = 1M msg/s` of vt.

### 6.5 Synthetic Session Split (FR-9 testing)

`session_change_at_msg = m` splits the day: messages `[1..m]` ride
session S1, `[m+1..N]` ride session S2 with **sequence restarted at 1**;
an `EOS(S1, next_seq = m+1… careful: EOS next-expected for S1 = m+1 only
if S1 seqs are 1..m — use m+1` — wait: S1 sent m messages ⇒ next expected
= m+1? No: seqs 1..m ⇒ next = m+1. Record: `EOS{next_seq: m+1}` — hmm,
if seqs are 1..m then next-expected IS m+1. Confirmed.) is emitted at the
split, S2's first packet follows, and heartbeats after the split speak
S2's sequence space.

**Golden invariance theorem (tested as T7):** the golden hash depends
only on message bytes — session framing changes NOTHING. The
session-split schedule and the plain schedule must produce the same
golden hash. This is doc 01's confluence made visible at the test layer:
framing is not truth; the message stream is truth.

## 7. TH-1 Generation (harness termination, doc 01 §2.5)

The builder ALWAYS appends, per feed:

1. **Rolling heartbeats** every `heartbeat_interval_ns` of vt
   (default 1 s): `next_seq` = first seq scheduled after that vt on the
   schedule (pre-loss), else `N+1`.
2. **Terminal heartbeat** `next_seq = N+1` as the last pre-EOS event.
3. **EOS** `next_seq = N+1`, after every packet's release on that feed.

Therefore every schedule's delivery universe closes; every conformance
run ends with the sequencer's final watermark assertable as exactly
`N+1` (post-anchor arithmetic in doc 05). EOS dedup across feeds (A's
EOS then B's EOS — one event or two?) is a **doc 05 decision**, flagged
here so it isn't forgotten.

## 8. Golden Hash Contract (canonical — doc 05's sink MUST reuse this)

FNV-1a-64 over the ground truth in order, `seq` assigned 1..N:

```
h₀ = 0xcbf29ce484222325
fnv(h, byte): h ^= byte; h = h · 0x100000001b3 (wrapping u64)

for seq in 1..=N:
    h = fnv8(h, seq.to_le_bytes())          // 8 bytes, LE
    h = fnv2(h, (len as u16).to_le_bytes()) // 2 bytes, LE  (len = block prefix)
    h = fnvN(h, msg_bytes)                  // len bytes
golden = h ; golden_count = N
```

Defined HERE (not doc 05) because both the golden walker (testkit) and
the engine's recording sink (doc 05, N-3) must implement the identical
function. A divergence between the two implementations is a bug class
with a test dedicated to catching it (T1 cross-checks walker vs a
hand-rolled second implementation over the first 10K messages).

## 9. Schedule Invariants (proof obligations of the builder)

| # | Invariant | Checked by |
|---|---|---|
| I-1 | Accounting identity: `delivered_A ∪ delivered_B ∪ dropped_both = all packets`, and `dropped_both ⊆ scripted_drops ∪ (stochastic ∧ ¬guarantee_coverage)` | T5 |
| I-2 | Events totally ordered by §4 law; per-feed release may be non-monotone in seq (jitter reorder — intended) | T2 |
| I-3 | Every rendered Packet re-validates via `validate_frame` with matching seq/count, blocks byte-equal to ground truth slices | T4, T9 |
| I-4 | TH-1: per feed, terminal heartbeat(N+1) precedes EOS(N+1); EOS is the feed's final event | T6 |
| I-5 | Determinism: same (file, config, seeds) ⇒ identical event-stream hash; no libm dependence | T2, T8 |
| I-6 | Session split: S1/S2 partition messages; S2 seq restarts at 1; heartbeats/EOS speak their own session's space | T7 |
| I-7 | Builder memory = O(packets), never O(disorder) or O(messages) beyond the event vector itself | code review + budget table |

## 10. Memory Budget (startup allocations only — O-6)

| Component | mini (15 MB) | dev (200 MB) | full day (~2.2 GB) |
|---|---|---|---|
| Ground truth mmap | 15 MB (page cache, shared) | 200 MB | 2.2 GB |
| Events (24 B × pkts×2 + HBs) | < 1 MB | ~12 MB | ~100–200 MB |
| Render arena | 384 KiB | 384 KiB | 384 KiB |
| Cursors | 32 B | 32 B | 32 B |

**Deployment law:** phone runs mini + dev. Full-day conformance runs on
CI/dev-box only (runner RAM is not your phone's problem). Documented, not
discovered at 2 AM.

## 11. Config Surface (nf-testkit)

```rust
pub struct ReplayConfig {
    pub gt_path: String,
    pub seed_a: u64, pub seed_b: u64,
    pub msgs_per_packet: Packetize,            // Fixed(u16) | MtuBound(u16) | SeededRange{min,max}
    pub loss: [LossModel; 2],
    pub delay: [DelayModel; 2],                // None | GaussianApprox{mean_ns: i64, sigma_ns: u64}
    pub base_rate_msg_per_sec: u64,            // default 1_000_000
    pub heartbeat_interval_ns: u64,            // default 1_000_000_000
    pub guarantee_coverage: bool,
    pub scripted_drops: Vec<DropRange>,        // {seq_from, seq_to_incl, feed_mask}
    pub session_change_at_msg: Option<u64>,
    pub feeds_enabled: u8,                     // bitmask; MaxRate bench = one feed, loss=None, delay=None
}
```

## 12. Test Matrix

| # | Test | Pass condition |
|---|---|---|
| T1 | Golden walker over mini; cross-checked vs independent hand implementation over first 10K msgs; run twice | identical hash both times; hash + N recorded in Appendix |
| T2 | Fixed config → schedule hash; run twice; different seed → different hash | byte-stable; seed-sensitivity confirmed |
| T3 | Packetization: Fixed(k) exact except last; MtuBound never exceeds; SeededRange within bounds | all modes |
| T4 | Render round-trip: every packet → `validate_frame` → seq/count match → blocks byte-equal ground truth | over mini, all three packetization modes |
| T5 | I-1 accounting with Bernoulli(p=100‰) both feeds, guarantee_coverage on and off | identity holds; with flag, `dropped_both` empty |
| T6 | TH-1 structure per feed | terminal HB(N+1) then EOS(N+1) last |
| T7 | Session split at m: golden hash identical to plain schedule; S2 restarts at seq 1 | hash equality |
| T8 | Delay distribution sanity: seeded run, delays within [µ−6σ, µ+6σ], mean within ±σ/10 | bounds loose, seed pinned |
| T9 | Full mini chaos run: render every event, all validate | 0 violations |
| T10 | MaxRate mode: single feed, in-order, no loss — the PR-1 benchmark input contract | monotone delivery, T4 properties hold |

(Zero-alloc proof of the poll/render path is **deferred to doc 07's
counting-allocator lane** — no informal claims in the meantime.)

## Changelog

| Date | Version | Entry |
|---|---|---|
| __-__-__ | 1.0 | Initial: schedule contract, vt clock law, Irwin-Hall determinism decision, two-mode coverage law, golden hash spec, I-1..I-7. |
````

*(One correction to catch before your agent does: in §6.5 I left an
inline "careful/hmm" scratch note in the EOS arithmetic — that was me
showing the work. The settled law: S1 carries seqs 1..m, so
`EOS{next_seq: m+1}`. If m = 0 (split at start) there is no S1 and no
first EOS. Clean the paragraph when you paste it in; scratch reasoning
doesn't belong in frozen docs, but the arithmetic above is the final
word.)*

---

## ED-04 — What you build NOW (gate G4)

**1. `nf-transport/src/replay.rs`** — schedule types (§4 verbatim),
render arena, cursor pair, virtual clock (§5 law), `Transport` +
`now_ns()` impls. Unsafe only for the mmap (LI-6); rendering itself is
safe code over slices.

**2. `nf-transport/src/lib.rs`** — add `fn now_ns(&self) -> u64` to the
`Transport` trait; XDP stub returns 0 with a doc-09 TODO.

**3. `nf-testkit/src/sched.rs`** — splitmix64 (exact constants from §6.1),
packetization, loss models + `guarantee_coverage` rejection sampling,
Irwin-Hall delay, session split, TH-1 generation, sort by the total
order law. Builder returns `ReplaySchedule` (imported from nf-transport —
testkit depends on it, LI-5 satisfied: dev-dependency direction).

**4. `nf-testkit/src/golden.rs`** — the walker per §8, EXACTLY:
`fn golden(gt: &[u8]) -> (u64 /*hash*/, u64 /*count*/)` plus a second
independent implementation used only by T1's cross-check.

**5. Tests T1–T10** per §12, plus pinned constants:

```rust
// nf-testkit tests — update values after first run, then FROZEN:
const MINI_GOLDEN_HASH: u64 = 0x____;   // from T1, recorded in Appendix
const SCHED_HASH_SEED1: u64 = 0x____;   // from T2
```

**6. CI additions:**

```yaml
      - run: cargo test -p nf-testkit -p nf-transport -- --include-ignored
      - run: cargo run --release -p nf-engine --bin audit -- data/tests/sample-mini.itch | diff - data/tests/mini-histogram.txt
```

**G4 acceptance — the full debt comes due here:**

1. **Rolled ledger:** doc 00 C6+C7 pastes confirmed · doc 01 AM-1 changelog entry · G2 CI run URL · complete G3 evidence (mini/dev/full-day audit outputs with 0 violations, §10 quotes, P1–P5 verdicts, Appendix B filled) · **freeze 03**
2. T1–T10 green on Termux **and** CI — real run URLs, no placeholders
3. Mini golden hash + schedule determinism hash recorded in doc 04 Appendix, pinned in CI
4. clippy `-D warnings` clean; `cargo tree` layer check (transport: nf-protocol only; testkit: dev-only)
5. Say **freeze 04** — and Phase 0 is **closed**: protocol, replay, ground truth all frozen

Then the project changes character: **engineer 05** is the sequencer — the 64 KiB window, the W1 proof turned into code, the drain loop, `LiveFeedProof` minting, and the first end-to-end golden run where your engine eats a chaotic dual-feed replay of a real NASDAQ day and emits the byte-identical truth. The part everything so far was scaffolding for.
