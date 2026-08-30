Stop — before anything else. **My prediction failed.** I said byte `0x10` would be `0x48` ('H', Stock Directory). Your dump shows `0x52` — **'R' is Stock Directory in ITCH 5.0; 'H' is Stock Trading Action.** I conflated the two type codes, and nobody — including me — caught it until the bytes arrived. That's correction **C7**, and it's the second time raw data has overruled authority in this project. First the archive format (C6), now my own recall. This is the pipeline working *on its own author*, which is the only honest way it can work.

Meanwhile the **structure** of the prediction held five ways: second length prefix `00 27` ✓, a 39-byte directory message ✓, locate 1 ✓, symbol `"A       "` ✓, locate 2 follows ✓. The format question is closed forever. Paste into doc 00:

```markdown
| C7 | Prior doc claimed 'H' = Stock Directory | ITCH 5.0 Stock Directory
is 'R' (0x52, 39B); 'H' (0x48, 25B) is Stock Trading Action. Evidence:
0x52 at file offset 0x10, declared len 0x27=39, locate 1, stock "A".
(Also corrects the "byte 12 = 0x48" expectation from the download guide.) | Doc 03 table |

| __-__-__ | 1.2 | C7 added. Freeze reaffirmed at v1.2. |
```

Ledger: C6 amendment paste + real CI run URL are **still open** — they're now items in gate G3 below. No phase after 03 drops until they exist.

Now the doc. Save as `~/Hft-fund/docs/03-itch5.md`:

````markdown
# 03 — ITCH 5.0 Payload Layer

```
Status:    DRAFT → FROZEN after §10 PDF sign-off + audit evidence + G3
Exit Gate: Every LENGTH row verified against the TotalView-ITCH 5.0 PDF;
           full-day audit reports violations == 0; predictions P1–P5
           recorded with verdicts (§8); G3 green.
Evidence:  Audit output (mini + dev + full day) in Appendix B;
           TV-IT1/IT2 decoded from REAL archive bytes; CI diff of the
           golden histogram.
Authority: This doc owns the LENGTH table (validation law + slot bound),
           common header, and order-flow overlays. Framing: doc 02.
           Replay: doc 04. Sequencer use of these tables: doc 05.
Rule:      Byte-level claims are law. Two oracles grade this table:
           the PDF (authoritative) and the BX day (empirical).
           A disagreement between them is a correction entry, not a vote.
```

---

## 1. Role & Scope

ITCH 5.0 is the payload layer. To MoldUDP64 framing (doc 02) a message is
opaque bytes; to the future LOB it is typed semantics. This crate serves
**three consumers with three different depths**:

| Consumer | Gets | Depth |
|---|---|---|
| Hot path (arbitrator, doc 05) | `LENGTH[256]` + `validate()` | O(1) lookup — **nothing else** |
| Staging bound (doc 05) | max legal length = 50 < 64-byte slot | the number only |
| Future LOB / tests (cold path) | typed overlays for order-flow types | field accessors |

**Opacity law (OP-1):** the hot path never decodes payload semantics. The
arbitrator stages and emits *bytes*. Overlays exist for tests and the future
LOB consumer (NG-1 keeps the LOB itself out of v1). Wiring overlay parsing
into `ingest` is a layer violation — it buys nothing and costs cycles.

**Raw-integers law (OP-2):** all accessors return raw integers. Timestamp is
u64 nanoseconds-since-midnight. Price is u32 scaled ×10⁴. Scaling to float
is presentation-layer, nowhere in this crate. No floats in the protocol
layer, period.

---

## 2. Ground-Truth Corrections (C7) and Real Golden Vectors

The first two messages of the 2019-12-30 BX day, verbatim from the raw
dump — these are **TV-IT1 and TV-IT2**. Real bytes, not synthetic.

**TV-IT1 — System Event ('S'), 12 bytes:**

```
53 00 00 00 00 0A 2D F4 92 1D 67 4F
│  └──┬──┘ └──┬──┘ └──────┬──────┘ │
'S'  locate  tracking    timestamp  event 'O'
0x52  = 0     = 0        = 11,192,493,022,567 ns   (Start of Messages)
                          ≈ 03:06:32.493 ET
```

**TV-IT2 — Stock Directory ('R'), 39 bytes:**

```
52 00 01 00 00 0A 66 A0 E0 DC 44 41 20 20 20 20
20 20 20 20 4E 20 00 00 00 64 4E 43 5A 20 50 4E
20 31 4E 00 00 00 00 4E
```

| Offset (msg-relative) | Bytes | Field | Value |
|---|---|---|---|
| 0 | `52` | type | 'R' Stock Directory |
| 1–2 | `00 01` | stock locate | 1 |
| 3–4 | `00 00` | tracking number | 0 |
| 5–10 | `0A 66 A0 E0 DC 44` | timestamp | 11,435,902,032,964 ns ≈ 03:10:35.902 ET |
| 11–18 | `41 20×7` | stock | `"A       "` (Agilent, NYSE) |
| 19 | `4E` | market category | 'N' (NYSE) |
| 20 | `20` | financial status | ' ' (normal) |
| 21–24 | `00 00 00 64` | round lot size | 100 |
| 25 | `4E` | round lots only | 'N' |
| 26 | `43` | issue classification | 'C' (common stock) |
| 27–28 | `5A 20` | issue sub-type | "Z " (VERIFY semantics) |
| 29 | `50` | authenticity | 'P' |
| 30 | `4E` | short-sale threshold | 'N' |
| 31 | `20` | IPO flag | ' ' |
| 32 | `31` | LULD tier | '1' |
| 33 | `4E` | ETP flag | 'N' |
| 34–37 | `00 00 00 00` | ETP leverage | 0 |
| 38 | `4E` | inverse indicator | 'N' |

Tests assert these decodes exactly — the archive itself is the test vector.

---

## 3. Common Header (all 23 types)

Every ITCH 5.0 message begins:

| Offset | Width | Field | Type | Notes |
|---|---|---|---|---|
| 0 | 1 | Message Type | u8 | indexes LENGTH |
| 1 | 2 | Stock Locate | u16 BE | 0 = not symbol-associated (system-wide) |
| 3 | 2 | Tracking Number | u16 BE | market-data group tag |
| 5 | 6 | Timestamp | u48 BE | ns since midnight ET |

Symbol-associated messages continue with Stock (8 bytes, space-padded) at
offset 11, ending the 19-byte "extended header."

---

## 4. Message Type Table

Lengths are my recall — **every row requires PDF sign-off (§10) before
freeze**. The "2019 BX?" column is filled empirically by audit (§7).

| # | Type | Hex | Message | Len | One-line semantics | 2019 BX? |
|---|---|---|---|---|---|---|
| 1 | S | 0x53 | System Event | 12 | session lifecycle: O/S/Q/M/E/C codes | |
| 2 | R | 0x52 | Stock Directory | 39 | locate ↔ symbol registration | ✓ (TV-IT2) |
| 3 | H | 0x48 | Stock Trading Action | 25 | halt/resume + 4-char reason | |
| 4 | Y | 0x59 | Reg SHO Restriction | 20 | short-sale restriction toggle | |
| 5 | L | 0x4C | Market Participant Position | 26 | MPID registration | |
| 6 | V | 0x56 | MWCB Decline Level | 35 | circuit-breaker level values | |
| 7 | W | 0x57 | MWCB Status | 12 | breach status | |
| 8 | K | 0x4B | IPO Quoting Period | 28 | IPO release time/price | |
| 9 | J | 0x4A | LULD Auction Collar | 35 | limit-up/down collar bounds | |
| 10 | h | **0x68** | Operational Halt | 21 | **lowercase** — venue operational halt | |
| 11 | A | 0x41 | Add Order (no MPID) | 36 | new visible order | |
| 12 | F | 0x46 | Add Order (MPID) | 40 | new order + attribution | |
| 13 | E | 0x45 | Order Executed | 31 | fill: shares + match number | |
| 14 | C | 0x43 | Order Executed w/ Price | 36 | fill at differing price | |
| 15 | X | 0x58 | Order Cancel | 23 | partial cancel | |
| 16 | D | 0x44 | Order Delete | 19 | full removal | |
| 17 | U | 0x55 | Order Replace | 35 | atomic cancel+re-add | |
| 18 | P | 0x50 | Trade (non-cross) | 44 | off-book print | |
| 19 | Q | 0x51 | Cross Trade | 40 | auction print | |
| 20 | B | 0x42 | Broken Trade | 19 | trade bust | |
| 21 | I | 0x49 | NOII | **50** | auction imbalance snapshot | |
| 22 | N | 0x4E | Retail Price Improvement | 20 | RPI flag | |
| 23 | O | 0x4F | Direct Listing PD | 48 | **revision-dependent — VERIFY; add only if your PDF lists it** | |

**Case trap (law):** `H` (0x48, 25B) ≠ `h` (0x68, 21B). The table is
indexed by raw byte — case is data, not style. A decoder that uppercases
type bytes corrupts both.

**Slot bound chain:** max legal length = 50 (I). Any block failing
`validate` is a violation (FR-12), never staged. Therefore every staged
message fits a 64-byte slot. QED — the doc 05 arena bound hangs off this
table.

---

## 5. LENGTH[256] Contract

```rust
pub const LENGTH: [u8; 256];   // 0 = unknown type
```

- **Sentinel safety:** 0 is never a legal length (min message = 12B).
- **Hot-path cost:** one load, one zero-compare, one length-compare. That
  is the *entire* ITCH footprint of the ingest loop.
- **Unknown type = loud violation** (FR-12): packet dropped, counted. If a
  venue adds a type our table lacks, the engine fails visibly and recovery
  retries → `SessionDead` — a correct, loud death. We never silently skip
  bytes we don't understand. Spec drift is discovered by the audit tool
  (§7) reporting unknown types, not by the hot path guessing.

---

## 6. Order-Flow Overlays (cold path)

Field offsets, msg-relative. All integers BE. `price_raw` = u32 ×10⁻⁴.

| Type | Len | 11–18 | 19–22 | 19 | 20–23 | 23–30 | 24–31 | 31 | 32–35 | 36–39 |
|---|---|---|---|---|---|---|---|---|---|---|
| A | 36 | order ref u64 | — | side | shares u32 | — | stock 8B | — | price u32 | — |
| F | 40 | order ref u64 | — | side | shares u32 | — | stock 8B | — | price u32 | MPID 4B |
| E | 31 | order ref u64 | exec shares u32 | — | — | match u64 | — | — | — | — |
| C | 36 | order ref u64 | exec shares u32 | — | — | match u64 | — | printable u8 | price u32 | — |
| X | 23 | order ref u64 | cancel shares u32 | — | — | — | — | — | — | — |
| D | 19 | order ref u64 | — | — | — | — | — | — | — | — |
| U | 35 | orig ref u64 | — | — | new ref u64 [19–26] | — | — | shares [27–30] | price [31–34] | — |

(U is irregular: `19–26` new order ref, `27–30` shares, `31–34` price.)
'S' overlay: event code at offset 10. Side: `b'B'`/`b'S'`. Printable:
`b'Y'`/`b'N'`.

Overlays are `parse(&[u8]) -> Result<Self, ItchError>` (checks type + exact
length) with zero-copy getter methods. **Allocations forbidden** — these are
borrowed views, not owned structs.

---

## 7. Audit Mode — the table's second oracle

A standalone tool (`nf-engine` bin `audit`) that walks a block stream and
grades the LENGTH table against reality:

```
loop over input (file path or "-" for stdin):
    remaining < 2          → clean EOF, or tail garbage (violation)
    len  = BE u16          → remaining < 2+len → TRUNCATED TAIL (expected
                             for byte-cut samples; flag, stop — not a violation)
    msg  = next len bytes
    check LENGTH[msg[0]] == len  else violation (count + first-64 report)
    hist[msg[0]]++
```

- **Allocations allowed here** — the audit tool is not the hot path (PR-3
  scope: ingestion/arbitration/dispatch). Do not contort it.
- **Deterministic output** (EN-6): no timing, no environment, pure function
  of input bytes:
  ```
  AUDIT file=<path> bytes=<N> blocks=<N> violations=<N> truncated_tail=<bool>
  HIST 0x41 A len=36 count=<c> pct=<p.pp>
  … (ascending by type byte)
  ```
- **Golden artifact:** first audit of the mini sample is committed as
  `data/tests/mini-histogram.txt`; CI diffs every subsequent run against
  it. The histogram is a fingerprint of the ground truth — tamper-evidence
  for the data file, for free.
- **Full-day run:** `zcat data/raw/20191230.BX_ITCH_50.gz | audit -` —
  no 2 GB intermediate file on the phone. This run is doc 03's exit
  evidence and discharges doc 00 C5 (type histogram over the actual day).

---

## 8. Falsifiable Predictions (record verdicts in Appendix B)

| # | Prediction | Verdict |
|---|---|---|
| P1 | Full-day audit: violations == 0 | |
| P2 | 'R' count == max stock locate observed (symbol count, low thousands) | |
| P3 | Order-flow types (A F E C X D U) > 90% of message count | |
| P4 | First message 'S'/event 'O'; last message 'S'/event 'C' | |
| P5 | Total blocks in the 60–100M range (≈2.2 GB ÷ ~28 B avg) | |

A failed prediction is a correction entry, never a quiet edit.

---

## 9. Test Vectors

- **TV-IT1 / TV-IT2:** §2 — real bytes, exact decodes (locate, tracking,
  timestamps as u64, symbol bytes, event codes).
- **TV-IT3 (case trap):** `LENGTH[b'H']==25`, `LENGTH[b'h']==21`;
  a 21-byte message with type 0x48 fails; a 25-byte message with type 0x68
  fails; each passes only at its own length.
- **TV-IT4 (synthetic 'A'):** built by test helper from field values,
  accessor round-trip must return identical values (order ref 42, side 'B',
  shares 100, stock `"AAPL    "`, price_raw 1,234,500).

## 10. PDF Verification Checklist (exit gate)

Every row: quote ≤ 15 words + section number. Corrections → changelog + body.

| # | Claim | Quote | § | Verdict |
|---|---|---|---|---|
| V-1..22 | Each type's **length** in §4 | | | |
| V-23 | 'O' Direct Listing present in your PDF revision? | | | |
| V-24 | 'H' Reason field is 4 bytes | | | |
| V-25 | Timestamp = ns since midnight; u48 | | | |
| V-26 | Price fields ×10⁴ (4 decimals) | | | |
| V-27 | Stock Locate 0 = system-wide messages | | | |
| V-28 | System Event codes O/S/Q/M/E/C meanings | | | |
| V-29 | Market category 'N' = NYSE | | | |

## Appendix B — Audit Evidence (fill at G3)

```
(mini / dev / full-day summaries + histogram + P1..P5 verdicts)
```

## Changelog

| Date | Version | Entry |
|---|---|---|
| __-__-__ | 1.0 | Initial table (recall-basis), overlays, audit design, TV-IT1/2 from real bytes. PDF + audit sign-off pending. |
````

---

## ED-03 — What you build NOW (gate G3)

**1. `crates/nf-protocol/src/itch5.rs`** — full file:

```rust
//! ITCH 5.0 payload layer. Hot path uses ONLY LENGTH + validate() (OP-1).
//! Overlays are cold-path borrowed views; zero allocation anywhere.

/// Total wire length per type byte; 0 = unknown. Sentinel safe: minimum
/// legal message is 12B. EVERY NON-ZERO ROW REQUIRES PDF SIGN-OFF (doc 03
/// §10) BEFORE FREEZE — lengths below are recall-basis.
pub const LENGTH: [u8; 256] = {
    let mut t = [0u8; 256];
    t[b'S' as usize] = 12;  t[b'R' as usize] = 39;  t[b'H' as usize] = 25;
    t[b'Y' as usize] = 20;  t[b'L' as usize] = 26;  t[b'V' as usize] = 35;
    t[b'W' as usize] = 12;  t[b'K' as usize] = 28;  t[b'J' as usize] = 35;
    t[b'h' as usize] = 21;  t[b'A' as usize] = 36;  t[b'F' as usize] = 40;
    t[b'E' as usize] = 31;  t[b'C' as usize] = 36;  t[b'X' as usize] = 23;
    t[b'D' as usize] = 19;  t[b'U' as usize] = 35;  t[b'P' as usize] = 44;
    t[b'Q' as usize] = 40;  t[b'B' as usize] = 19;  t[b'I' as usize] = 50;
    t[b'N' as usize] = 20;
    // 'O' (48): add ONLY if §10 V-23 confirms your PDF revision lists it.
    t
};

pub fn msg_len(type_byte: u8) -> Option<u8> {
    match LENGTH[type_byte as usize] { 0 => None, l => Some(l) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItchError {
    Empty,
    UnknownType { t: u8 },
    LengthMismatch { expected: u8, got: usize },
}

/// O(1): one load, two compares. The ONLY ITCH call on the hot path.
pub fn validate(msg: &[u8]) -> Result<(), ItchError> { todo!() }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommonHeader { pub locate: u16, pub tracking: u16, pub timestamp_ns: u64 }

/// Infallible for validated messages (all known types ≥ 12B).
pub fn header(msg: &[u8]) -> CommonHeader { todo!("offsets 1,3,5 — u48 ts") }

pub struct SystemEvent<'a> { msg: &'a [u8] }
impl<'a> SystemEvent<'a> {
    pub fn parse(msg: &'a [u8]) -> Result<Self, ItchError> { todo!("type+len check") }
    pub fn event_code(&self) -> u8 { todo!("offset 10") }
}

pub struct Add<'a> { msg: &'a [u8] }
impl<'a> Add<'a> {
    pub fn parse(msg: &'a [u8]) -> Result<Self, ItchError> { todo!() }
    pub fn order_ref(&self) -> u64  { todo!("offset 11") }
    pub fn side(&self) -> u8        { todo!("offset 19") }
    pub fn shares(&self) -> u32     { todo!("offset 20") }
    pub fn stock(&self) -> &[u8]    { todo!("offset 24, 8B") }
    pub fn price_raw(&self) -> u32  { todo!("offset 32") }
}
// F(40) = A + attribution[36..40]; E(31); C(36); X(23); D(19); U(35:
// orig[11..19], new[19..27], shares[27..31], price[31..35]).
// Same borrowed-view pattern, offsets in doc 03 §6. Implement all six.
```

**2. `crates/nf-protocol/src/packet.rs`** — fused validation orchestrator:

```rust
//! MoldUDP64 framing + per-block ITCH validation in one pass. This is the
//! single entry the arbitrator (doc 05) and replay (doc 04) will call.
use crate::{itch5, moldudp64};

#[derive(Debug)]
pub enum PacketError { Framing(moldudp64::FrameError), Payload(itch5::ItchError) }

pub fn validate_frame(buf: &[u8]) -> Result<moldudp64::Parsed<'_>, PacketError> {
    todo!("moldudp64::parse, then itch5::validate on every block")
}
```

**3. `crates/nf-engine/src/bin/audit.rs`** — implement per doc 03 §7 pseudocode. File arg or `"-"` for stdin, `BufReader`, allocations allowed, deterministic stdout format exactly as specified. Export the core loop as a testable `fn audit_stream(r: &mut impl std::io::Read) -> AuditReport` so unit tests can feed in-memory block streams.

**4. Tests (T1–T8):** TV-IT1/IT2 exact decodes (assert the u64 timestamps 11192493022567 and 11435902032964); TV-IT3 case trap; TV-IT4 'A' round-trip; table totality (every nonzero LENGTH byte ↔ `msg_len`); error paths (empty / unknown 0x00 / wrong length); `validate_frame` on a synthetic framed packet; audit unit test (in-memory stream → zero violations + histogram + truncated-tail flag); seeded randomized block stream → audit counts match builder counts.

**5. CI addition:**

```yaml
      - run: cargo run --release -p nf-engine --bin audit -- data/tests/sample-mini.itch | tee /tmp/h.txt
      - run: diff /tmp/h.txt data/tests/mini-histogram.txt
```

(First run: generate the golden, commit it. Every run after: diff.)

**G3 acceptance — all artifacts, no narration:**

1. `cargo test -p nf-protocol` green on Termux **and** CI — real run URL this time
2. clippy `-D warnings` clean; `cargo tree -p nf-protocol` still zero deps
3. Mini audit: 0 violations; golden histogram committed; CI diff green
4. Dev-sample audit (200 MB): 0 violations; summary + top-10 histogram rows into Appendix B
5. **Full-day audit** `zcat … | audit -`: 0 violations; totals + full histogram into Appendix B; C5 discharged
6. §10 checklist filled with quotes; corrections applied; P1–P5 verdicts recorded; say **freeze 03**
7. **Still open from before:** doc 00 C6+C7 amendments pasted (blocks provided above), and the G2 CI run URL

When G3 lands, you'll have graded your protocol table against ~70 million real exchange messages — and then **engineer 04** builds the replay fabricator that turns that ground truth into lossy, jittered, dual-feed chaos.
