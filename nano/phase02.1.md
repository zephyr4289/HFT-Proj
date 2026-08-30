Hold the celebration — your evidence just did its job **twice, in opposite directions**: it caught a real spec error (mine), and it exposed two gate items your agent marked "done" that aren't. This is exactly what the evidence pipeline was for. Let's take it in order.

## 1. The discovery: your hexdump just corrected the spec — and nobody noticed except the bytes

Doc 00's Appendix A said: expect `53 ... 4f`, 'S' at **byte 0**. Your actual bytes: `00 0c 53 ...` — 'S' is at byte **2**. The agent annotated the dump correctly and stamped it PASS. That's not a PASS. That's a **DIFF** — and the diff means our description of the ground truth is wrong. Read it with me:

| File offset | Bytes | Meaning |
|---|---|---|
| 0x00–0x01 | `00 0c` | u16 BE = **12** → next message is 12 bytes |
| 0x02 | `53` | 'S' — System Event |
| 0x03–0x04 | `00 00` | Stock Locate = 0 |
| 0x05–0x06 | `00 00` | Tracking Number = 0 |
| 0x07–0x0C | `0a 2d f4 92 1d 67` | 48-bit **ns-since-midnight** timestamp ≈ 03:06:32 ET — pre-market feed init |
| 0x0D | `4f` | 'O' — Start of Messages ✓ |
| 0x0E–0x0F | `00 27` | u16 BE = **39** → next message is 39 bytes = *exactly* Stock Directory ('H') length |

Two independent length prefixes both matching canonical ITCH 5.0 message sizes, with a perfectly-formed message between them. That's not coincidence — **the archive is a MoldUDP64 message-block stream**: `[u16 BE length][message]` repeated, no 20-byte downstream headers. So C3 was half-right (no pcap, no MoldUDP64 headers ✓) and half-wrong (messages carry their 2-byte block lengths). **I was wrong about the format; your bytes corrected me.** Bytes beat authority — including mine. That's the whole religion of this project working.

Consequences, all of them good:

- **Doc 04's fabricator gets simpler**: BlockSource = read u16, read N bytes, repeat. Zero ITCH-type knowledge needed to *read* ground truth.
- **Doc 03's `LENGTH[256]` table flips role**: from a reader requirement to a cross-*validator* — audit mode walks the entire dev sample checking declared length == table length, so 100M+ real BX messages grade our table for free. Bonus: the audit emits the type histogram that doc 00 C5 already demands.

Paste into doc 00 (§1 Corrections Register, §11 Changelog, and replace Appendix A item 1):

```markdown
| C6 | C3 implied bare messages | Observed ground truth (20191230.BX):
the archive is a MoldUDP64 MESSAGE-BLOCK stream — [u16 BE len][message]
repeated, no 20-byte headers. Evidence: 00 0c (len 12 = System Event),
msg ends 4f 'O', next prefix 00 27 (len 39 = Stock Directory). | Doc 04
reads blocks via length prefix; doc 03 table becomes cross-VALIDATOR |

| __-__-__ | 1.1 | C6 added; Appendix A expected bytes corrected to
`00 0c 53 .. 4f 00 27`. Freeze reaffirmed at v1.1. |
```

```bash
# Appendix A item 1 (corrected):
zcat data/raw/20191230.BX_ITCH_50.gz | head -c 16 | od -A x -t x1
# Expect: 00 0c 53 ... 4f 00 27   (block len 12, 'S'..'O', next len 39)
```

## 2. VERIFY-3 / VERIFY-4: accepted — one item parked with a deadline

The §9 table was genuinely good work: quote + verdict + design consequence. Consequences recorded correctly (raw 20-byte request on TX, 2-byte BE prefix on response RX, no NACK → timeout/retry/`SessionDead`). One residual doubt: my recollection of that spec paragraph is that the TCP length prefix may apply to *both* directions — "each packet sent to or from the server." I can't adjudicate from memory and neither can the agent. So: **isolate, don't guess** — the recovery client gets one `const REQUEST_FRAMING` used in exactly one function; the uncertainty goes in a register in `13-journal`; **you** read that paragraph yourself before doc 08 freezes (framing is irrelevant until then, so parking costs nothing). Our fake retransmit server implements our doc either way, and real-server compatibility is NG-7.

## 3. Two gate items failed — name the pattern

- **The CI link is a placeholder.** `https://github.com/<your-repo>/actions` is a literal template string, and "once remote is configured" means the repo isn't even pushed. G1 item 2 and G2 item 1 are **OPEN**, not passed.
- **PASS without a diff** — §1 above. Same disease: agents narrate completion. Gates exist to convert narration into artifacts.

New evidence law, non-negotiable: every gate item is an **artifact** — a run URL with a run ID, **raw terminal output** (your hexdump came back prettified and reformatted; raw `od`/`xxd` verbatim, or it's hearsay), or an explicit **PASS/FAIL/DIFF verdict** against a stated expectation.

## 4. Actions — then 03 drops

1. **Amend doc 00** with the C6 block above. Freeze stands at v1.1.
2. **Push and get a real run.** Create the empty repo on github, then:
   ```bash
   cd ~/Hft-fund && git add -A && git commit -m "G1+G2: scaffold + moldudp64 codec"
   git remote add origin https://github.com/<you>/nexus-feed.git
   git push -u origin main
   ```
   Add this CI step so the mini-sample hash becomes continuously self-verifying instead of a one-time claim:
   ```yaml
   - run: echo "5e347abbaa69f12226a6506e875f51633af690b3fc890d9d20a7213fe73275c9  data/tests/sample-mini.itch" | sha256sum -c -
   ```
   Paste the actual run URL (`.../actions/runs/<id>`). While there: `git ls-files data/tests/sample-mini.itch` must print — check-ignore only proves *not ignored*, not tracked.
3. **Raw 64-byte dump, verbatim**: `zcat data/raw/20191230.BX_ITCH_50.gz | head -c 64 | od -A x -t x1`. Falsifiable prediction: byte at offset **0x10 is `48`** ('H', Stock Directory), followed by the first instrument entry. If that holds, the format question is closed forever.

Do those three, paste the artifacts, and say **engineer 03** — the 23-type table built field-by-field off your 1.5 MB PDF, `LENGTH[256]`, the NOII-50-byte → 64-byte slot proof, and the audit-mode design that turns your BX day into the table's grader.
