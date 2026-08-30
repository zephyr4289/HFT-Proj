Status check before the doc: your BX download should be done by now — **doc 00 is still DRAFT until you run Appendix A** (magic bytes, sample cuts, hashes into the changelog). Don't let that slip; 02's test vectors will eventually run against real bytes.

One workflow note for this doc: its exit gate is *"every field cross-checked against the official spec PDF."* You own the PDF — so this doc ships claims **plus a VERIFY checklist**. You open `moldudp64.pdf`, confirm each item against the cited section, sign off in the changelog, and only then does 02 freeze. If any VERIFY fails, we correct before freeze — that's the diskCleaner evidentiary habit applied to protocol docs: no claim without a checkable source.

Save as `~/Hft-fund/docs/02-moldudp64.md`:

````markdown
# 02 — MoldUDP64 Protocol Reference & Parsing Contract

```
Status:    DRAFT → FROZEN after §9 VERIFY checklist sign-off + gate G2
Exit Gate: All VERIFY-1..9 items confirmed against moldudp64.pdf with
           section citations; TV-1..4 machine-verified by unit tests;
           G2 green on Termux + CI.
Evidence:  cargo test -p nf-protocol output; §9 checklist completed
           in changelog; annotated hexdump §8.
Authority: This doc owns the MoldUDP64 WIRE GRAMMAR and the parsing
           contract. Session/gap POLICY is doc 05/08. ITCH payload
           shapes are doc 03. TCP client behavior is doc 08.
Rule:      Byte-level claims here are law. Changes = changelog entry.
```

---

## 1. Role & Scope

MoldUDP64 is the framing layer: it chops a sequential message stream
(ITCH 5.0 payloads) into UDP datagrams and defines the retransmission
request/response used to repair loss. It knows nothing about order books;
message data is **opaque bytes** at this layer.

Two channels, one grammar:

| Channel | Transport | Direction | Carries |
|---|---|---|---|
| Downstream | UDP multicast | venue → engine | data / heartbeat / end-of-session packets |
| Retransmission | TCP | engine ↔ retransmission server | requests (engine→server), data packets (server→engine) |

Recovered TCP packets use the **identical downstream packet grammar** —
which is why recovery injection needs no special handling (doc 01, C-4).

**Endianness law:** every multi-byte integer in MoldUDP64 is **big-endian**
(network byte order). No exceptions.

---

## 2. Wire Grammar

### 2.1 Downstream packet

| Offset | Width | Field | Type | Endian | Semantics |
|---|---|---|---|---|---|
| 0 | 10 | Session | byte[10] | — | opaque session token; exact byte compare only (§5) |
| 10 | 8 | Sequence Number | u64 | BE | sequence of the FIRST message in this packet |
| 18 | 2 | Message Count | u16 | BE | 0 = heartbeat · 0xFFFF = end-of-session · else = block count |
| 20 | … | Message Blocks | seq | — | exactly `count` blocks, back to back |

Consequences of the seq semantics (doc 00 C2): a packet
`seq=S, count=C` carries messages `S, S+1, …, S+C−1`. Legal data counts
are `1..=0xFFFE` (0 and 0xFFFF are reserved).

### 2.2 Message block

| Offset | Width | Field | Type | Endian | Semantics |
|---|---|---|---|---|---|
| +0 | 2 | Message Length | u16 | BE | length of the data that follows |
| +2 | len | Message Data | bytes | — | one ITCH 5.0 message; opaque here |

### 2.3 Retransmission request packet (engine → server)

| Offset | Width | Field | Type | Endian | Semantics |
|---|---|---|---|---|---|
| 0 | 10 | Session | byte[10] | — | must identify the target session |
| 10 | 8 | Sequence Number | u64 | BE | first message requested |
| 18 | 2 | Requested Message Count | u16 | BE | how many messages requested |

A request for messages `[from, from+count−1]`. Total 20 bytes — **the same
width as a downstream header by coincidence.** They are different protocols:
two separate constants, never one shared `const`. (Naming hazard, called out.)

---

## 3. Special Packets

### 3.1 Heartbeat (`count = 0`)

No message blocks. The **sequence number field carries the sequence of the
next message the server will transmit** — i.e., free loss foreknowledge. This
is the fuel for trigger FR-7(c): a heartbeat announcing `seq H > W` proves
messages `[W, H)` were sent but not received. (Exact spec wording: VERIFY-1.)

### 3.2 End-of-Session (`count = 0xFFFF`)

No message blocks. Sequence field carries the next-expected sequence at
session end — i.e., the final message count of the session. Engine behavior:
emit `EndOfSession{final_wm}` (FR-9); any still-unresolved gap is reported
`GapUnresolved`, never fabricated. (VERIFY-2.)

### 3.3 Sequence number origin

Sessions conventionally begin at sequence 1. The engine does **not** assume
this: it anchors on the first observed sequence (doc 05, A-1). No
enforcement in this layer.

---

## 4. Retransmission Channel

1. Engine connects TCP to the venue's retransmission server (doc 08 owns
   connection lifecycle, retries, backoff).
2. Engine sends a request packet (§2.3).
3. Server responds with one or more standard downstream packets carrying
   the requested messages, in sequence order, possibly split across
   multiple packets.
4. **TCP framing:** each MoldUDP64 packet on the TCP stream is preceded by
   a 2-byte binary length prefix (big-endian) giving the packet's total
   byte length. Applies to response packets; whether the request is also
   length-prefixed on the wire: VERIFY-3.
5. Server behavior when requested messages are unavailable (aged out):
   spec-defined response vs. silence — VERIFY-4. Our client is safe under
   either: silence hits the retry cap → `SessionDead` (doc 08).
6. Duplicate service is harmless: a re-served range already delivered dies
   in the watermark compare (doc 01, C-1). The server being stateless about
   our retries is a feature we exploit, not a problem we solve.

**Requested count bounds:** wire maximum 65535 messages per request. Our
client's request-sizing policy is doc 08. The encoder contract here:
`count ∈ [1, 65535]` — a zero-count request is meaningless and never sent.

---

## 5. Session Field Policy

- The spec describes the field as alphanumeric, left-justified, space- or
  zero-padded (exact wording: VERIFY-8). **We do not enforce this.**
  Rationale (P-3, §6): charset is a *producer obligation*. The consumer's
  grammar needs exactly one operation — byte equality — and enforcing
  producer promises creates false violations on feeds we don't control.
- Session change handling (flush, re-anchor, `gen++`) is **policy**, owned
  by doc 05/08. This layer only reports the 10 raw bytes.

---

## 6. Parsing Contract (Policies P-1..P-5)

**P-1 — Eager validation, infallible iteration.** `parse()` walks the whole
frame once and returns either an error or a fully-validated packet. The
block iterator handed to the sequencer is then infallible: every bounds
condition was proven during validation. Fuzz (VR-4) certifies that the
iterator's "unreachable" failure modes are unreachable.

**P-2 — Packet atomicity.** A packet is applied **entirely or dropped
entirely**. No salvage of a valid prefix from a corrupt packet. Rationale:
a corrupted length field mid-packet makes every subsequent block boundary
suspect; salvage would emit data whose interpretation depends on where
corruption landed. Atomicity keeps confluence trivially intact and pushes
repair to recovery, which exists precisely for this.

**P-3 — Consume obligations, don't enforce producer promises.** We validate
what the grammar needs (lengths, counts, exact consumption) and nothing
more. No charset checks, no timestamp plausibility, no "should be"
rules. Every rule we add is a rule that can false-fire on a real feed.

**P-4 — Checked span arithmetic.** `seq + count − 1` is computed with
checked arithmetic. Overflow (`seq` near `u64::MAX`) is a violation
(`SeqOverflow`), not a wrap. Costs one branch on a path that runs once per
packet; buys immunity to a hostile-frame edge that would otherwise silently
corrupt the watermark math in doc 05.

**P-5 — Exact consumption.** The message blocks must consume the frame to
the last byte. Trailing bytes after the last block = `TrailingBytes`
violation, even if the blocks themselves parse cleanly. Same for heartbeat /
EOS packets: frame length must be exactly 20.

### 6.1 Validation walk (normative pseudocode)

```
parse(buf) -> Result<Parsed, FrameError>:
    if buf.len() < 20:                      Err(Truncated{need: 20, got: buf.len()})
    hdr = parse_header(buf)                 # infallible past the length check
    if hdr.span_overflows():                Err(SeqOverflow)
    match hdr.count:
      0      => if buf.len() != 20: Err(TrailingBytes{extra: buf.len()-20})
                else Ok(Heartbeat(hdr))
      0xFFFF => if buf.len() != 20: Err(TrailingBytes{extra: buf.len()-20})
                else Ok(EndOfSession(hdr))
      _      =>
        rest = &buf[20..]
        repeat hdr.count times:
            if rest.len() < 2:                          Err(BlockOverrun)
            len = BE_u16(rest[0..2])
            if len == 0:                                Err(ZeroLengthMessage)
            if rest.len() < 2 + len:                    Err(BlockOverrun)
            rest = &rest[2+len..]
        if rest.len() != 0:                             Err(TrailingBytes{extra: rest.len()})
        Ok(Data{hdr, blocks: MessageBlocks::new(&buf[20..], hdr.seq, hdr.count)})
```

The iterator re-walks the same bytes (`pos += 2 + len` per block, `seq += 1`
per block) — safe because validation already proved every bound it touches.

---

## 7. Violation Catalogue

| ID | Condition | FrameError variant | Counted as |
|---|---|---|---|
| V-1 | frame < 20 bytes | `Truncated` | violation |
| V-2 | HB/EOS frame ≠ 20 bytes | `TrailingBytes` | violation |
| V-3 | blocks end early vs `count` | `BlockOverrun` | violation |
| V-4 | bytes left after last block | `TrailingBytes` | violation |
| V-5 | block length 0 | `ZeroLengthMessage` | violation |
| V-6 | `seq + count − 1` overflows u64 | `SeqOverflow` | violation |

Notes:
- Block-length cap (≤ 64) is **NOT checked here** — that's ITCH-layer
  knowledge and arrives with doc 03's orchestrator. Framing accepts any
  `len ∈ [1, 65535]`.
- Counters for violations live in the arbitrator (doc 05); this crate only
  classifies. Classification is total: every malformed input maps to exactly
  one variant, first-match-wins in the order above.
- None of V-1..V-6 may panic, allocate, or read out of bounds — the
  fuzz harness (VR-4) proves this empirically, P-1 proves it structurally.

---

## 8. Test Vectors (normative — tests must reproduce these bytes exactly)

### TV-1 — data packet, session "NFTESTSESS", seq 1000, 2 messages

Annotated hexdump (48 bytes total):

```
Offset  Hex bytes                                Meaning
------  ---------------------------------------  ------------------------------
0x00    4E 46 54 45 53 54 53 45 53 53            Session "NFTESTSESS"
0x0A    00 00 00 00 00 00 03 E8                  Sequence = 1000 (u64 BE)
0x12    00 02                                    Message Count = 2
0x14    00 0C                                    Block 1 length = 12
0x16    53 00 00 00 00 00 00 00 00 00 00 4F      Block 1: 'S' event msg, code 'O'
0x22    00 0C                                    Block 2 length = 12
0x24    53 00 00 00 00 00 00 00 00 00 00 43      Block 2: 'S' event msg, code 'C'
0x30    — end of packet —
```

(Block payloads are minimal System Event-shaped messages: type 'S',
stock-locate 0, tracking 0, timestamp 0, event code 'O'/'C'. Their ITCH
correctness is doc 03's business; here they are opaque 12-byte payloads.)

Byte constant for tests:

```
4E 46 54 45 53 54 53 45 53 53 00 00 00 00 00 00 00 00 03 E8
00 02 00 0C 53 00 00 00 00 00 00 00 00 00 00 4F
00 0C 53 00 00 00 00 00 00 00 00 00 00 43
```

### TV-2 — heartbeat, next sequence 1002 (20 bytes)

```
4E 46 54 45 53 54 53 45 53 53 00 00 00 00 00 00 03 EA 00 00
```

### TV-3 — end-of-session, final next-sequence 1002 (20 bytes)

```
4E 46 54 45 53 54 53 45 53 53 00 00 00 00 00 00 03 EA FF FF
```

### TV-4 — retransmission request: messages [990, 1009] (20 bytes)

```
4E 46 54 45 53 54 53 45 53 53 00 00 00 00 00 00 03 DE 00 14
```

---

## 9. PDF Verification Checklist (exit gate — fill before freeze)

Open `docs/specs/moldudp64.pdf`. For each row: find the section, quote the
relevant sentence (≤ 15 words), and mark MATCH / CORRECTION. Corrections go
into the changelog and the doc body before freeze.

| # | Claim in this doc | Spec section | Quote | Verdict |
|---|---|---|---|---|
| VERIFY-1 | Heartbeat: seq field = next sequence the server will transmit | | | |
| VERIFY-2 | EOS: seq field = next-expected sequence at session end | | | |
| VERIFY-3 | TCP retransmission: 2-byte BE length prefix before each packet; which directions | | | |
| VERIFY-4 | Server behavior on unavailable (aged-out) messages: defined response or silence | | | |
| VERIFY-5 | Any stated maximum messages per request or per response packet | | | |
| VERIFY-6 | Stated heartbeat interval (informational; we do not depend on it) | | | |
| VERIFY-7 | Spec is silent on count-vs-actual-blocks mismatch → our TrailingBytes/BlockOverrun rules are OUR policy, not spec text | | | |
| VERIFY-8 | Session field: exact charset/padding wording ("alphanumeric, left-justified"?) | | | |
| VERIFY-9 | Spec is silent on zero-length message blocks → our V-5 is OUR policy | | | |

## Changelog

| Date | Version | Entry |
|---|---|---|
| ____-__-__ | 1.0 | Initial grammar, policies P-1..P-5, violation catalogue, TV-1..4. VERIFY-1..9: ____ |
````

---

## ED-02 — What you build NOW (gate G2)

Everything lives in `crates/nf-protocol/src/moldudp64.rs`. Full signatures
with contracts — **you write the bodies** following §6.1 pseudocode exactly.

```rust
//! MoldUDP64 framing codec. Pure, zero-alloc, no panics on any input.
//! Grammar law: docs/02-moldudp64.md. Verify claims there before relying.

pub const HEADER_LEN: usize = 20;
pub const REQUEST_LEN: usize = 20; // same width as HEADER_LEN BY COINCIDENCE.
                                   // Different protocols. Never share the const.
pub const HEARTBEAT_COUNT: u16 = 0;
pub const EOS_COUNT: u16 = 0xFFFF;

pub type SessionId = [u8; 10];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub session: SessionId,
    pub seq: u64,
    pub count: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind { Data, Heartbeat, EndOfSession }

impl Header {
    /// Classification by count field (§3).
    pub fn kind(&self) -> Kind { todo!() }

    /// Inclusive message span [seq, seq+count-1] for data packets.
    /// None for heartbeat/EOS (no messages) and on u64 overflow (P-4).
    pub fn span(&self) -> Option<(u64, u64)> { todo!() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Kept from G1 scaffold — frame shorter than 20 bytes.
    Truncated { need: usize, got: usize },
    /// P-5: bytes remaining after last block (or after HB/EOS header).
    TrailingBytes { extra: usize },
    /// Blocks end before `count` blocks are present.
    BlockOverrun,
    /// Block with Message Length == 0 (our policy V-5).
    ZeroLengthMessage,
    /// seq + count - 1 overflows u64 (P-4).
    SeqOverflow,
}

#[derive(Debug, Clone, Copy)]
pub struct MessageBlock<'a> {
    /// Absolute message sequence number of this block.
    pub seq: u64,
    /// Message payload — a slice INTO the caller's frame (zero copy).
    pub data: &'a [u8],
}

/// Infallible iterator (P-1): only constructible from a validated packet.
/// Walks (pos += 2 + len, seq += 1) over bounds proven by `parse`.
#[derive(Debug, Clone)]
pub struct MessageBlocks<'a> {
    buf: &'a [u8],
    pos: usize,
    next_seq: u64,
    remaining: u16,
}

impl<'a> Iterator for MessageBlocks<'a> {
    type Item = MessageBlock<'a>;
    fn next(&mut self) -> Option<MessageBlock<'a>> { todo!() }
}
impl<'a> ExactSizeIterator for MessageBlocks<'a> {}

#[derive(Debug)]
pub enum Parsed<'a> {
    Data { header: Header, blocks: MessageBlocks<'a> },
    Heartbeat { header: Header },
    EndOfSession { header: Header },
}

/// Full eager validation (P-1, P-2, P-5) per doc 02 §6.1 — normative
/// pseudocode there. First-match-wins error order: V-1, V-6, V-2/V-3/V-4/V-5.
/// Never panics, never allocates, never reads OOB on ANY input slice.
pub fn parse(buf: &[u8]) -> Result<Parsed<'_>, FrameError> { todo!() }

/// Kept from G1 scaffold; now infallible once buf.len() >= 20 checked by
/// callers — internal helper for `parse`, public for tests.
pub fn parse_header(buf: &[u8]) -> Result<Header, FrameError> { todo!() }

/// Encode a retransmission request (§2.3) into `out`.
/// Contract: count >= 1 (debug_assert); exactly REQUEST_LEN bytes written.
/// Zero-alloc by construction.
pub fn encode_request(session: &SessionId, from: u64, count: u16,
                      out: &mut [u8; REQUEST_LEN]) { todo!() }
```

**Dependency wiring** (workspace `Cargo.toml` + crate manifests — the layer
laws from doc 01 §4 become physical now):

```toml
# nf-arbitrator/Cargo.toml
[dependencies] nf-protocol = { path = "../nf-protocol" }
# nf-transport, nf-recovery: same
# nf-engine: depends on all four
# nf-testkit: dev-dependency everywhere it's used; depends on all
```

**Unit tests** (`#[cfg(test)] mod tests` in `moldudp64.rs` — `Vec` is allowed
in tests, never in the library):

- **T1 (TV-1 golden):** test-only builder `fn build_packet(sess, seq, msgs: &[&[u8]]) -> Vec<u8>` → assert exact bytes equal the TV-1 constant → `parse` → assert kind Data, header fields, and that iteration yields `(1000, 12 bytes)`, `(1001, 12 bytes)` with `data` byte-equal to the two payloads.
- **T2/T3 (TV-2/3):** heartbeat and EOS parse to correct kinds, seq 1002, and 20-byte frames with one extra byte appended → `TrailingBytes`.
- **T4 (TV-4):** `encode_request` output == TV-4 bytes, byte for byte.
- **T5:** 19-byte input → `Truncated { need: 20, got: 19 }`.
- **T6:** `count=3`, two valid blocks present → `BlockOverrun`.
- **T7:** `count=1`, two blocks present → `TrailingBytes`.
- **T8:** block with `len=0` → `ZeroLengthMessage`.
- **T9:** `seq = u64::MAX, count = 2` → `SeqOverflow` from `parse` (P-4 enforced at packet level).
- **T10 (round-trip property):** seeded xorshift PRNG (EN-6 — record the seed in the assert message) builds 10,000 random packets (random count 1..40, random payload lens 1..64) → build → parse → blocks must reproduce the input messages exactly, `ExactSizeIterator::len()` == count.
- **T11:** `span()` — `(1000,2) → Some((1000,1001))`; heartbeat → `None`; `(u64::MAX, 2)` → `None`.

**G2 acceptance — report all:**

1. `cargo test -p nf-protocol` green on **Termux** and on **CI** (link the run)
2. `cargo clippy --workspace --all-targets -- -D warnings` clean
3. `cargo tree -p nf-protocol` — **zero dependencies** (LI-2 physically verified)
4. TV-1..4 tests green (the doc's hexdump is now machine-verified law)
5. §9 checklist filled in with quotes + section numbers; corrections (if any) applied to the doc body; changelog updated; say **freeze 02**
6. Still outstanding from before: doc 00 Appendix A run + **freeze 00**

When G2 is green, next prompt is **engineer 03** — the ITCH 5.0 message-length table (all 23 types, `LENGTH[256]` const table, the 64-byte slot proof) written against the 1.5 MB TotalView PDF, plus the `packet.rs` orchestrator that closes the loop between framing and ITCH validation. After that, 04 (replay fabricator) and you'll be feeding real BX bytes through your own parser within the week.
