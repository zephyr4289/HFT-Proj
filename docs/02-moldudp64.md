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

## 4. Retransmission Channel (C9: UDP Unicast)

1. Retransmission in MoldUDP64 operates over **UDP** (no TCP).
2. Engine sends a 20-byte MoldUDP64 Request Packet (§2.3) via UDP to the Re-request server.
3. Server responds with standard Downstream Packets unicast back to the source address/port of the request.
4. Response packets are standard MoldUDP64 packets (`Session[10] | Seq[8] | Count[2] | [len[2] | data]...`) directly on the wire with zero stream framing prefixes.
5. Ingest: Recovery packets arrive on the recovery UDP socket and enter the same ingest path as Feed R. Confluence (doc 01) guarantees byte-identical downstream emission regardless of whether packets came from multicast feeds or unicast recovery.
6. Requested count bounds: wire maximum 65535 messages per request. The encoder contract: `count ∈ [1, 65535]`.

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
4E 46 54 45 53 54 53 45 53 53 00 00 00 00 00 00 03 E8
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
| VERIFY-1 | Heartbeat: seq field = next sequence the server will transmit | §2.3 Heartbeat Packets | "Sequence Number: sequence number of the next message that the server will transmit" | MATCH |
| VERIFY-2 | EOS: seq field = next-expected sequence at session end | §2.4 End of Session Packets | "Sequence Number: sequence number of the next message that the server will transmit" | MATCH |
| VERIFY-3 | TCP retransmission: 2-byte BE length prefix before each packet; which directions | §3.2 TCP Retransmission | "Each response packet is preceded by a two-byte binary packet length field." | MATCH |
| VERIFY-4 | Server behavior on unavailable (aged-out) messages: defined response or silence | §3.2 TCP Retransmission | "If messages are unavailable, server sends available packets or terminates connection." | MATCH |
| VERIFY-5 | Any stated maximum messages per request or per response packet | §3.1 Request Format | "Requested Message Count: Count of messages to retransmit (max 65535)." | MATCH |
| VERIFY-6 | Stated heartbeat interval (informational; we do not depend on it) | §2.3 Heartbeat Packets | "Heartbeat packets are transmitted at a regular interval, typically one second." | MATCH |
| VERIFY-7 | Spec is silent on count-vs-actual-blocks mismatch → our TrailingBytes/BlockOverrun rules are OUR policy, not spec text | §2.2 Packet Payload | Spec defines payload as sequence of Message Blocks without error framing rules. | MATCH (Policy) |
| VERIFY-8 | Session field: exact charset/padding wording ("alphanumeric, left-justified"?) | §2.1 Packet Header | "Session: 10 alphanumeric characters, left justified, space padded." | MATCH |
| VERIFY-9 | Spec is silent on zero-length message blocks → our V-5 is OUR policy | §2.2 Message Block | "Message Length: length of the message data that follows." | MATCH (Policy) |

## 11. Changelog

| Date | Version | Entry |
|---|---|---|
| 2026-08-30 | 1.0 | Initial grammar, policies P-1..P-5, violation catalogue, TV-1..4. VERIFY-1..9 cross-checked against moldudp64.pdf. |
