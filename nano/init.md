Two downloads, two different sites, two different jobs:

| What | From | Why you need it |
|---|---|---|
| **2 spec PDFs** (MoldUDP64 + ITCH 5.0) | nasdaqtrader.com | Source of truth for docs `02` and `03` — every field offset gets copied from these, not from my summaries |
| **1 data file** (one trading day) | emi.nasdaq.com | Ground truth for the replay transport, golden test, and fuzz corpus |

Here's each one, step by step.

---

### 1. The two spec PDFs

Search Google for these exact titles and take the PDF hosted on **nasdaqtrader.com**:

- **"MoldUDP64 Downstream Packet Specification"** — this one is critical for us: it contains the 20-byte retransmission request packet format (`Session[10] | Seq[8] | Count[2]`) that our recovery engine speaks. It also defines the heartbeat (`count=0`) and end-of-session (`0xFFFF`) rules.
- **"Nasdaq TotalView-ITCH 5.0"** — the catalog of all 23 message types with field offsets. This is doc `03`'s bible.

Candidate direct URLs (nasdaqtrader occasionally moves files — if 404, just use the search above):
```
https://www.nasdaqtrader.com/content/technicalsupport/specifications/dataproducts/moldudp64.pdf
https://www.nasdaqtrader.com/content/technicalsupport/specifications/dataproducts/Nasdaq-TotalView-ITCH-5.0.pdf
```
They're a few hundred KB each. In Termux:
```bash
mkdir -p ~/nexus-feed/docs/specs && cd ~/nexus-feed/docs/specs
wget "URL_HERE"
```
Commit these to the repo — they're small and public, and your docs should cite them.

---

### 2. One trading day of real data

```bash
pkg install wget curl xxd

# Check you have ~2 GB free first
df -h $HOME

# Browse the archive (it's plain anonymous FTP, no login, no key)
curl ftp://emi.nasdaq.com/ITCH/
```

You'll see year directories. `cd` into the **earliest year listed** (older = smaller files — a 2024 day is 20+ GB and will kill your phone; an early-2010s day is roughly 0.5–1.5 GB):

```bash
curl ftp://emi.nasdaq.com/ITCH/2012/
```

That lists every trading day, filenames like `06042012.NASDAQ_ITCH50.gz` (MMDDYYYY pattern). **Pick the smallest one.** Download it with resume enabled (GB-scale over Wi-Fi, connections drop):

```bash
mkdir -p ~/nexus-feed/data/raw && cd ~/nexus-feed/data/raw
wget -c ftp://emi.nasdaq.com/ITCH/2012/<smallest-file>.gz
```

If FTP hangs on your network, try the same path over HTTPS in your phone's browser: `https://emi.nasdaq.com/ITCH/2012/` — it's usually browsable, and you can download from there.

---

### 3. Verify you got the right thing (30 seconds)

```bash
zcat <file>.gz | head -c 12 | xxd
```

Expected:
```
00000000: 5300 0000 00XX XXXX XXXX XXXX 4f  S...........O
```

- Byte 0 = `0x53` = `'S'` — System Event message (every ITCH day starts with one)
- Byte 11 = `0x4f` = `'O'` — "Start of Messages" event code
- Bonus check: byte 12 will almost certainly be `0x48` (`'H'`, Stock Directory) — NASDAQ dumps the full instrument directory right after the opening events

If you see that, you have a genuine raw ITCH 5.0 day. If you see garbage or an Ethernet header (`xx xx xx xx xx xx` MAC-looking bytes), you got a pcap from somewhere else — wrong file.

**Important, one more time:** these files are **raw concatenated ITCH messages, NOT pcap captures** — no MoldUDP64 envelopes, no UDP headers, no Feed A/B. That's exactly why doc `04` fabricates synthetic MoldUDP64 framing and a synthetic Feed B. The data source *forces* our replay design, which is actually a feature: we control packetization boundaries and can inject pathological splits deliberately.

---

### 4. Make your dev samples (don't keep decompressing the monster)

A gzipped day expands to 5–10x on disk. **Never `gunzip` the full file** — always stream through `zcat`:

```bash
cd ~/nexus-feed

# Dev sample: first 200 MB of messages (~4M messages, plenty for daily work)
zcat data/raw/<file>.gz | head -c 200000000 > data/sample-dev.itch

# Tiny sample that CAN live in the git repo (unit tests, CI)
zcat data/raw/<file>.gz | head -c 15000000 > data/tests/sample-mini.itch
```

The dev sample ends mid-message (truncation point is arbitrary) — that's fine, even useful for fuzzing; the protocol crate will cut clean boundaries later.

`.gitignore`:
```
data/raw/
data/sample-dev.itch
```

The 15 MB mini sample is the **only** data file that ever gets committed. CI runs on it; the full day is for final validation runs only.

---

### Gotchas recap

- Don't grab a recent year — 20 GB on a phone is a bad Tuesday
- Only ONE day needed, ever
- If `xxd` isn't found: `pkg install xxd`, or use `od -t x1` instead
- Files are MMDDYYYY-named, so "06042012" is June 4, 2012, not Feb 4 — don't let that confuse you when comparing sizes

Once `xxd` shows you the `53...4f` bytes, you're holding real market data — say **engineer 00** and we write the frozen spec doc with real artifacts on disk to point at.
