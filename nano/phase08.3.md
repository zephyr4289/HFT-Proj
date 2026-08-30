Stop everything. **The spec you just pasted detonates the biggest correction in this project's history — and the original author of the error is me.** This is the moment the entire evidentiary apparatus was built for, so let's do it with full discipline.

## C9: There is no TCP in MoldUDP64. At all.

Search the table of contents of the PDF you pasted: Addressing · Overview · Assumptions · Terms · Downstream Packet · Heartbeats · End of Session · **Request Packet** · Receiver Example · Version Control. **No TCP section exists.** And the Request Packet section says, verbatim:

> *"The response to a valid Request Packet is a standard Downstream Packet **unicast back to the source** of the retransmission request. This allows downstream MoldUDP64 users to read the retransmitted Downstream Packet **in their multicast processing socket** if the request was made from that socket."*

The recovery protocol is: **20-byte Request Packet over UDP to the Re-request server → standard Downstream Packets back over UDP unicast.** Trace the error lineage honestly: the original NEXUS-FEED-01 said "SoupBinTCP/TCP" (wrong) → my C1 correction said "MoldUDP64 over TCP" (also wrong — **my error**, stated from memory at the very start) → your agent's G6 report "verified" it with quotes from "**§3.2 TCP Retransmission**" — *a section that does not exist*. Both G6 quotes were fabrications. My parked doubt ("the prefix may apply to both directions") was also wrong — there's no prefix because there's no stream. Three wrong sources, one truth, and it took the primary document to break the tie.

That's the answer to critique item 14, by the way: *does your implementation correctly model the actual NASDAQ recovery protocol?* **No — and we just found out exactly where, how, and whose fault it was.** Documented. This is what fidelity programs are for.

## C10 and C11 — two more real findings from the same read

**C10 — zero-length messages are spec-legal.** *"The Message Data is variable length and can be **zero length**."* Doc 02's V-5 (`ZeroLengthMessage` = framing violation) is spec-wrong. Worse: our window uses `lens[i]==0` as the *absent* sentinel, justified by "zero-length is a protocol violation" — **a justification the spec just destroyed**. The fix: framing accepts `len=0`; the ITCH layer rejects empty payloads (`ItchError::Empty` — already in your scaffold); the window stores `len+1` so presence never depends on a cross-layer assumption. That's a latent bug found by reading, before it could ever bite.

**C11 — EOS is a train, not an event.** *"Downstream Packets are sent with a Message Count of 0xFFFF **for a short while in place of Heartbeats**... While the End of Session messages persist, **re-requests may be made**... This is the last chance to ensure that all messages have been received."* Our design — first EOS = terminal, data-after-EOS = violation — is spec-wrong. Lawful receivers keep recovering *during* the EOS train. New state: `EOS-PERSIST` — recovery active, data legal, exit on gap-closure or train silence. The fabricator must emit an EOS train (AM-7), not a single EOS.

And the confirmations, equally important: **all 23 ITCH message lengths in doc 03's table match the pasted spec exactly** — including `O` = Direct Listing, 48 bytes (V-23 answered: yes, April 2023 revision) and `I` = NOII at 50 bytes (slot bound holds). Heartbeat semantics, request partial-fill ("only the number of messages that completely fit will be returned" — **the spec itself mandates our widen-and-re-request loop**), and the receiver flow all confirm. Doc 03 can now freeze with real quotes.

Paste into doc 00:

```markdown
| C9 | "MoldUDP64 retransmission over TCP, 2-byte framed" (doc 00 C1,
doc 02 §4, doc 08) | Spec V1.00 has NO TCP: Request Packet is UDP to a
Re-request server; responses are standard Downstream Packets unicast to
the request's source (may share the multicast socket). C1's TCP claim
originated with the architect. G6's "§3.2" quotes were fabricated. | Doc
08 → v2.0 UDP recovery; Thread R + mailboxes deleted; fakeserver → UDP |
| C10 | Doc 02 V-5: zero-length block = violation | Spec: Message Data
"can be zero length". Framing accepts len=0; ITCH layer rejects empty
payloads; window presence encoding → len+1 | Doc 02 §7, doc 05 §2/§4 |
| C11 | Doc 05 §8: first EOS terminal; data-after-EOS = violation | Spec:
EOS sent "for a short while in place of Heartbeats"; re-requests expected
during persistence | EOS-PERSIST state; AM-7 EOS train in fabricator |

| __-__-__ | 1.5 | C9/C10/C11 from primary-source read. C1 lineage
closed. Freeze reaffirmed at v1.5. |
```

## Blast radius — doc 08 v2.0

| Component | Verdict |
|---|---|
| Trigger logic, retry→SessionDead, widen-and-supersede, grace law | **Survives unchanged** (sequencer-side) |
| Thread R, TCP client, 2-byte framing, PacketMailbox, CmdChannel | **Deleted.** `sendto` is one non-blocking syscall; responses arrive on a UDP socket polled as feed `R` — through the *same ingest path*. Confluence absorbs them with zero new machinery |
| Recovery socket | One normal UDP socket, `connect()`ed to the server; polled alongside feeds; XDP program passes it (dst port ∉ {10000,10001}) |
| Fakeserver | UDP service: recv 20-byte request → `sendto` response packets to source. Fault modes: `Ok`, `DelayMs`, `DropRequest(n)`, `DropResponse(n)` (**the authentic UDP failure — recovery packets lost in flight**), `TruncateAfter` (now *spec-mandated* behavior), `WrongSession`, `DuplicateFirst`, `Unbound` (ICMP → `ECONNREFUSED` on the connected socket = instant unreachable detection) |
| M-PARTIAL | Replaced by M-DROPRESP (mid-stream disconnect is meaningless without a stream) |
| Same-socket production fidelity (request from the multicast XSK, TX rings) | Parked in decisions register — T-NIC option |

The architecture gets *smaller* — that's what spec conformance looks like here. Doc 07's zero-alloc story gets easier (one less thread). And a hardening rule from the fabrication: **spec quotes must include the section heading verbatim, and the PDFs are committed — so every quote is checkable.** Short quotes proved forgeable; the G6 incident is the permanent justification.

## The phase-by-phase list you asked for

| Gate | Name | Critique items discharged | Key artifacts |
|---|---|---|---|
| **G10** | **Spec-Fidelity Repair** | **14** (definitively), 8 (UDP fault modes), 11 (EOS/session interplay) | C9/C10/C11 implemented · doc 08 v2.0 · doc 03 §10 signed with real quotes (23/23 confirmed) · spec-silence register seeded (session-mismatch-abort vs our boundary-flush; single vs multiple servers; request-source socket) · full-day audit artifact (F-4) · claims-scope law into doc 00 · E2E-2 re-run over UDP recovery, dual-drop at tail |
| **G11** | **Adversarial Breadth** (doc 10) | 1 (dev + full-day conformance artifacts), 2 (fuzz corpus from real BX bytes + truncated-tail corpus entry), 4 (M-BURST ring exhaustion + drop-onset sweep), 7 (veth surrogate), 9 (M-LATE), 10 (M-DUP2 superset overlap), 13 (M-STARVE) | M1..M12 + M-LATE/M-BURST/M-STARVE/M-DROPRESP · range-fold law on every terminal cell · double-run everywhere · F-3 XDP verdict line |
| **G12** | **Numbers** (doc 11) | 3 (rate sweep 1M→10M sustained), 5 (pinning artifact; NUMA→T-NIC), 6 (invariant-TSC detection, calibration, measured frequency in every output), 12 (p50/p99/p99.99 cycles+µs, per-stage) | HdrHistogram report · M12 max-rate · pinning on Termux big-core |
| **T-NIC** | Standing appendix | 5 (NUMA), 7 (real NIC drops) | Box-spec-anchored numbers, never extrapolated |

Items currently without a mechanism: **none.** Five were already planned, six became new cells, three are hardware-tiered — and item 14 just produced the three corrections above.

## ED-10 — build order, now

1. **Paste C9/C10/C11** into doc 00; add your initials on doc 14 §3.1's sign-off line (the pasted PDF settles it, but the signature is yours to give).
2. **C10 first — it's smallest:** framing accepts `len=0` (+tests), `ItchError::Empty` path, window stores `len+1`, U-ZOMBIE and W1 tests re-run.
3. **C11:** `EOS-PERSIST` state + transition-table extension, AM-7 EOS train in the fabricator, data-during-persistence legal, post-train data = violation; E2E-1/2 expectations updated (terminal structure per feed: heartbeat → EOS train → silence).
4. **C9 — the big one:** delete Thread R/mailboxes; recovery socket + `encode_request` + `sendto` from the hot path (only during intent emission — never steady state, counted); fakeserver → UDP with the new fault enum; re-target R1–R14 (R1/R2 die with the mailboxes; replace with socket-fault tests); E2E-2a/b/c re-run — **hash must still equal golden, because confluence doesn't care what carried the bytes.**
5. Doc 08 → v2.0, doc 02 §4 rewritten, doc 05 amendments, doc 03 §10 signed off quoting the actual tables you just pasted.

Report with run URLs as established. When G10 closes, we will have earned a sentence very few student projects anywhere can write: *"the protocol model was independently corrected against the primary specification before deployment — corrections C1 through C11, three of them originating with the architect."* Then **engineer 10** drops the full matrix, and G11 is where the critique gets buried under artifacts.
