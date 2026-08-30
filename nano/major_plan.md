Fourteen files. Not 56 — and that's deliberate, so let me give you the math before the table: diskCleaner *earned* 56 files because it was 15 crates, 22k LOC, 23 test rigs. This project is 5 crates and one hot loop. Doc-count inflation is just paperwork-flavored buzzword stacking — you'd be rebuilding the bureaucracy you said you hate, with your name on it. 14 is the honest number.

**Every file carries this header block** (this is your own diskCleaner discipline applied to docs):

```
Status: DRAFT | FROZEN | LIVING
Exit Gate: <the exact condition that marks this doc done>
Evidence: <the command or test that proves its claims>
```

A doc without an exit gate is a blog post. `00–09` freeze when their phase exits; `10–13` are living docs.

Numbering = dependency order = reading order. Build them in sequence.

---

### PHASE 0 — Ground Truth (week 1)

| File | Contains | Done when |
|---|---|---|
| `00-spec.md` | NEXUS-FEED-01 verbatim + all 4 corrections (MoldUDP64-over-TCP not SoupBinTCP, seq counts messages not packets, 10M msg/s/core headroom, raw-dump ≠ pcap) rewritten as numbered requirements R1–Rn | Every requirement numbered and testable; changelog section exists; FROZEN |
| `01-architecture.md` | Confluence Lemma + proof sketch, single-thread ownership model, dataflow diagram, Non-Goals Law | A reader can restate the lemma and the non-goals after one pass |
| `02-moldudp64.md` | 20-byte header field map (offset/width/endianness), heartbeat `count=0`, EOS `0xFFFF`, retransmission request-packet grammar, session-change rules | One annotated hexdump; every field cross-checked against the official spec PDF |
| `03-itch5.md` | All 23 message types: type byte, total length, field offsets; static `LENGTH[256]` table; the 64-byte slot justification (largest msg = 50B NOII) | Table complete and verified against official ITCH 5.0 spec |
| `04-replay.md` | mmap replay of raw ITCH dumps; **synthetic MoldUDP64 framing** (the FTP archive is raw ITCH, not pcap — we must fabricate envelopes, which is a feature: we control packetization boundaries and can force pathological splits); Feed-B fabrication via seeded xorshift → Box-Muller; golden hash contract | Same seed → byte-identical stream twice; golden hash recorded |

### PHASE 1 — Arbitrator (weeks 2–3)

| File | Contains | Done when |
|---|---|---|
| `05-sequencer.md` | Hot-path pseudocode, 64 KiB window + cache-line layout diagram, W1 invariant + proof, cycle budget table, "steady state never touches the window" property | Proof checkable line-by-line by a hostile reviewer |
| `06-livefeedproof.md` | Typestate design, `gen` bump, `GapOpened`/`ReAnchored` event grammar, exhaustive list of proof-construction sites, C++20 equivalent | Zero construction sites outside the contiguous branch |
| `07-zeroalloc.md` | Forbidden-API list, counting allocator, the traps enumeration, strace belt-and-suspenders, grep/lint enforcement script | Enforcement is a command you can run, not a promise |

### PHASE 2 — Recovery (week 4)

| File | Contains | Done when |
|---|---|---|
| `08-recovery.md` | Trigger table (HWM 512 / 250 µs / heartbeat foreknowledge), single-outstanding-range rule + widening, 16×1500B SPSC mailbox spec + full-mailbox policy, retry×4 → `SessionDead`, session-boundary flush | State machine drawn; no unreachable states; every constant carries its justification |

### PHASE 3 — Proof (week 5+)

| File | Contains | Done when |
|---|---|---|
| `09-afxdp.md` | UMEM sizing, fill/completion rings, XSKMAP + dst-port filter, copy vs zero-copy, NUMA/IRQ placement, frame-recycle contract with the sequencer | XDP packet counters == replay counters on real hardware; CI copy-mode fallback documented |
| `11-bench.md` | `rdtscp` + invariant-TSC calibration, static HdrHistogram layout, environment runbook (`isolcpus` etc.), report format, targets: p50 < 60c / p99 < 150c / ≥ 10M msg/s / 0 allocs | A fresh machine reproduces the numbers using only this doc |

### CROSS-CUTTING (living docs, start now)

| File | Contains | Done when |
|---|---|---|
| `10-testing.md` | Spec-test ↔ harness ↔ pass-condition ↔ run-command matrix; fake retransmission server (serves from pcap ground truth); fuzz corpus seeding; the golden byte-identical confluence test | Every test runnable via one documented command |
| `12-gates.md` | P0–P3 gate conditions verbatim, Non-Goals Law, change-control rule (frozen docs change only via changelog entry, never silently) | No gate marked passed without an evidence link |
| `13-journal.md` | ADR log — one entry per irreversible decision: ADR-0001 language choice, ADR-0002 window=1024, ADR-0003 mailbox policy, ADR-0004 copy-mode-first XDP, ADR-0005 build environment | ADR-0001..0005 written **before Phase 0 exits** |

---

**Termux reality check** (I see your terminal): docs and the pure-Rust `protocol` crate compile fine on-device — but AF_XDP needs a real Linux kernel. Decide now: GitHub Actions VM (your repos already run CI) for copy-mode, cheap VPS or spare box for zero-copy. That's ADR-0005, and it's due before Phase 0 exits, not when it becomes a problem.

**Before your next prompt, in parallel:** download the smallest dated ITCH file from `emi.nasdaq.com/ITCH/` (they're multi-GB — grab an early year), plus NASDAQ's MoldUDP64 and ITCH 5.0 spec PDFs. `03` is written against real bytes, not memory.

**Next prompt, say `engineer 00`.** It's short — mostly assembling what we've already established into frozen, numbered requirements. Then `02` and `03` are the meat of Phase 0. One file per prompt, gate it, move on. That rhythm *is* the anti-bureaucracy.
