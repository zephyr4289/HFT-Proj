**G8: accepted — with two findings your own green run just handed me.** Let's do this properly, because this report is the real thing: run URL, commit, every lane green, and E2E-2a with the drop range at `500,000..=505,000` — the exact tail-of-day worst case I demanded, passing under AM-6. Dual-drop recovery, byte-identical to golden, `ALLOC_DELTA=0` *including the TCP client* — PR-3 stage 2 is discharged. Every functional requirement of NEXUS-FEED-01 is now proven end-to-end. What remains (09–11) is the real-kernel path and the numbers. That's the milestone — own it.

Now the audit, because green runs still get read:

## F-1: Your flagship replay lost its session split — silently, again

Walk the arithmetic with me. Stage 7 prints `watermark=505850` — that's **N+1**. But per ED-05, `ci-mode1.toml` was specified with **session split ON**, and per C8's amended E2E-1, split-on requires `final_wm == N−m+1` — strictly less than N+1. Meanwhile `split_watermark.rs` passes `N−m+1`, proving the fabricator *can* do it. There is exactly one configuration in which both facts are true: **the split is OFF in ci-mode1.toml.** So the A-1 question you've been asked four times answers itself from your own CI output: it was **(a)** — config-level, not fabricator arithmetic. The builder got fixed; the flagship config silently kept the split disabled, and with it, E2E-1 lost session-boundary coverage in the one lane everyone sees first.

The fix is five minutes and it is *not* optional: **restore `session_change_at_msg` in ci-mode1.toml**, pin `m` (print it in the VERDICT line), and change CI's watermark assertion to `N−m+1`. Note the beautiful part: the golden hash pin **doesn't change** — C8 made the hash split-invariant, which is precisely why the config drift hid for two gates. Hash equality can no longer catch config drift; only watermark assertions can. That's why they exist.

## F-2: `try_push` — inspect what "non-blocking" actually bought you

"RecoveryClient::parse_framed uses non-blocking try_push to eliminate park deadlocks" — O-3 says the mailbox **never drops**. If `try_push` drops-on-full, you fixed a harness deadlock by violating the transport law, and the only reason tests still pass is that P-ORDER drains every iteration so the mailbox never actually fills in the two-thread engine. The correct shape: `try_push` returns `Result<_, Full>`; **Thread R wraps it with the park/backoff loop** (O-3 lives in the wrapper); **the single-threaded harness drains between `step()` calls** (the deadlock dies because the harness drains, not because packets vanish). Then R2 must exercise the *wrapper* — a real park-until-drain — or the park path is untested code. If it already works this way, show me R2's output proving a park occurred; if it drops on full, fix it. No third state.

Both findings close at G9. Neither unpastes G8 — the evidence standard was met; green with filed findings is what honest engineering looks like.

---

Now the doc that takes this project onto the real kernel. Save as `~/Hft-fund/docs/09-afxdp.md`:

````markdown
# 09 — AF_XDP Transport: Real Kernel Packet Path

```
Status:    DRAFT → FROZEN after G9
Exit Gate: X-lane green on CI (veth, copy mode): conformance hash ==
           golden; dual-drop + recovery over veth; XDP-path ALLOC_DELTA=0.
           Real-NIC zero-copy is APPENDIX-GRADE (env report, never
           claimed without numbers from the box it ran on).
Evidence:  CI run URLs for X-lanes; xdpstats counters; fakeserver
           counters; alloc/strace lane logs on the XDP build.
Authority: This doc owns the XDP transport, UMEM/ring contracts, the
           venue-sender simulator, and the BPF program. Transport trait
           law: doc 01 §5. Frame lifetime O-2 gets its XDP twin here.
           Replay/virtual clock: doc 04. Bench methodology: doc 11.
Rule:      We gate ONLY what CI can prove. Zero-copy-on-real-NIC is
           reported, not claimed (NG-10 applied to ourselves).
```

---

## 1. Two-Tier Reality (the honesty architecture)

| Tier | Environment | Mode | Status |
|---|---|---|---|
| T-CI | GitHub Actions ubuntu runner, veth pair | XDP copy mode | **GATED** — every X-test must be green here |
| T-NIC | real Linux box, real NIC, `XDP_ZEROCOPY` | zero-copy | **APPENDIX** — numbers + box spec, no gate, no claims without the box |

GH runners are VMs; veth + XDP generic/copy mode works there (this is how
libxdp's own CI runs). Zero-copy requires NIC driver support — if we
claimed it without the hardware, we'd be the buzzword stack again. The
XdpTransport code is identical between tiers; only the flag and the
environment differ.

## 2. Topology

```
┌─ venue-sender (nf-testkit bin) ─────────────────────────────┐
│ schedule (doc 04 models) → MoldUDP64 packets → raw UDP via   │
│ libc sendmmsg on 127.0.0.1-grade crafted IP/UDP headers      │
│ feed A → dst port 10000 · feed B → dst port 10001            │
└──────────────┬───────────────────────────────────────────────┘
               │ veth0 ───────────── veth1 (XDP attached)
               ▼
┌─ XDP program (veth1) ───────────────────────────────────────┐
│ eth → ipv4 → udp → dst port ∈ {10000,10001}                  │
│   → bpf_redirect_map(XSKMAP, socket_for_port)  else XDP_PASS │
└──────────────┬───────────────────────────────────────────────┘
               ▼ UMEM rx ring
┌─ XdpTransport (nf-transport, feature "xdp") ─────────────────┐
│ poll(): drain rx ring → FrameBatch (FrameViews INTO UMEM)    │
│ consume-or-stage → refill fill ring (O-2-X law, §5)          │
└──────────────────────────────────────────────────────────────┘
```

The engine loop, sequencer, recovery, fakeserver: **unchanged, one
config flip** (`transport = "xdp"`). This is doc 01's trait law paying
off — the whole stack below the trait was built without knowing which
transport would carry it.

## 3. The BPF Program (committed source, CI-built)

~35 lines of C, committed at `xdp/redirect.c`. Pseudocode-law:

```
parse eth (skip VLAN awareness v1 — veth carries none, documented)
verify ethertype == 0x0800, ip->protocol == 17, ihl sane
udp = ip + ihl*4; port = udp->dest (BE)
if port == 10000: return redirect_map(xsk_a)
if port == 10001: return redirect_map(xsk_b)
return XDP_PASS
```

No length validation here — the parser (doc 02) owns validation, and
duplicating it in BPF is two sources of truth. The program routes; Rust
judges. Build: `clang -target bpf -O2` in CI, artifact `.o` loaded at
runtime.

**ADR-0008 (loader):** load via `libbpf` (`libbpf-sys` crate, pinned),
NOT a hand-rolled ELF loader. Justification: BPF relocation + map
creation is high-risk, low-value to reimplement; libbpf is the kernel
ecosystem's battle-tested path. This is a scoped LI-7 exception behind
the `xdp` feature flag — the zero-dependency law stays intact for every
non-XDP build, and `cargo tree -p nf-arbitrator` remains clean. Record
the exception; own it.

## 4. UMEM & Rings

| Object | Spec |
|---|---|
| UMEM | 2048 × 4096 B frames, one shared region, 2 sockets |
| Fill ring | pre-populated with ALL 2048 frame descriptors at startup |
| RX rings | per-socket, kernel→user, descriptors only (copy mode: kernel copies into UMEM) |
| Comp rings | v1 unused (no TX); required by API — sized, parked |

**Frame lifecycle (O-2-X):** a UMEM frame's bytes are valid from rx-ring
pop **until the engine returns its descriptor to the fill ring**. The
sequencer must consume-or-stage (doc 05 §S5: emit-from-frame or 64-byte
stage copy) before `poll()` refills. Identical contract to replay
mmap frames — the trait abstracted it correctly, so the sequencer code
doesn't change. Budget check: 2048 in-flight frames ≫ QD of any sane
veth rate; starvation = refill bug, asserted by X4.

## 5. XdpTransport::poll (normative)

```
poll(batch):
    n = 0
    while n < batch.capacity() and rx ring nonempty:
        desc = rx_pop()
        batch.push(FrameView{ ptr: umem_base + desc.addr,
                              len: desc.len, feed: socket_feed })
        n++
    return n
```

`now_ns()`: `clock_gettime(CLOCK_MONOTONIC_RAW)` — same clock family as
doc 11's rdtscp calibration; recorded once per loop iteration (O-4).

## 6. Venue-Sender Simulator (`nf-testkit`, bin `venue`)

The mirror of the fabricator that drives it: loads the SAME doc-04
schedule format, renders packets (doc 04 §5 renderer, reused verbatim),
sends via **raw libc sockets + `sendmmsg`** on veth0 — crafted
eth/IP/UDP headers, **UDP checksum 0** (legal for IPv4; our path never
validated checksums — doc 04 said so from the start). Pacing: vt from
the schedule maps to inter-send delays — a `--realtime` factor
(default 1.0; bench lane uses max-rate). Heartbeats, EOS, scripted
drops, session split: all inherited from the schedule. **The sender is
a transport test harness — allocations allowed; determinism of the
SENT multiset is what matters** (same schedule ⇒ same packets).

## 7. Determinism Stance (X-lane)

The kernel path is not byte-order-deterministic — and doesn't need to
be. What must hold, and is asserted:

1. **Confluence absorbs kernel chaos.** With `guarantee_coverage=true`
   and no stochastic loss, the *sent* multiset is complete ⇒
   `U_final` is complete regardless of kernel reordering/batching ⇒
   **HashSink == golden, count == N, watermark == N+1 (or N−m+1 with
   split — see F-1: pinned per config)**. The X-lane is L2 tested
   against an adversary with no seed at all: the Linux kernel.
2. **Kernel drops are recovery's problem, and recovery is on.**
   fakeserver listens on localhost; any veth-induced loss opens a gap
   → TCP fill → golden still holds. The X-lane therefore tests the
   *entire* stack, not just the happy path. Assert `gap events ≥ 0`,
   never a fixed count (S-1).
3. Sender counters vs receiver counters reconciled after every run:
   `sent_packets == received + dups + kernel_dropped(unknown residual
   is gap-recovered)` — publish both sides' counters in the verdict.

## 8. Test Matrix (all on CI T-CI; veth setup in-job)

| # | Test | Pass condition |
|---|---|---|
| X1 | XDP smoke: sender → 100 pkts, XDP_PASS baseline vs redirect | redirect path receives exactly what PASS path loses |
| X2 | Conformance: full mini schedule, coverage guaranteed, recovery on | hash == golden(mini); count == N; watermark matches config-pinned expectation; zero violations |
| X3 | Scripted dual-drop (mid-day AND tail ranges) over veth | recovery fills; hash == golden; fakeserver counters match config |
| X4 | Fill-ring discipline: run X2 with rx drained in batches of 8 | no starvation abort; final fill-ring occupancy == 2048 |
| X5 | Alloc window on the XDP build | ALLOC_DELTA == 0 across X2 run (frames are pre-owned; ring ops are index math) |
| X6 | Malicious frames: sender corrupts lengths/ports (10% seeded) | violations counted; zero panics; golden for the clean remainder |

## 9. CI Additions

```yaml
  xdp-lane:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: sudo apt-get install -y clang libbpf-dev
      - run: cargo build --release -p nf-engine --features xdp
      - run: |
          sudo ip link add v0 type veth peer name v1
          sudo ip link set v0 up; sudo ip link set v1 up
          clang -target bpf -O2 -c xdp/redirect.c -o xdp/redirect.o
          sudo ./target/release/venue --config ci-xdp.toml &   # + fakeserver
          sudo ./target/release/replay --config ci-xdp.toml --transport xdp
```

`sudo` throughout: XSK + BPF load are privileged. Artifact: `xdpstats`
(counters from the XSK + BPF map) printed in the verdict block.

## Appendix T-NIC (report-only template — fill when a box exists)

```
BOX: <cpu/nic/kernel/driver> · MODE: zerocopy · driver support: <y/n>
RATE: pkts/s sustained · drops at RX: n · p50/p99 cycles receipt→dispatch
NOTE: numbers from THIS box only. No extrapolation. Ever.
```

## Changelog

| Date | Version | Entry |
|---|---|---|
| __-__-__ | 1.0 | Initial: two-tier honesty law, veth topology, BPF program law, O-2-X frame lifecycle, venue-sender, X1..X6, ADR-0008 loader decision, T-NIC appendix. |
````

---

## ED-09 — What you build NOW (gate G9)

1. **F-1 closure first, it's five minutes:** split restored in `ci-mode1.toml`, `m` pinned and printed in VERDICT, CI watermark assertion `N−m+1`, doc 05 changelog entry recording **"(a) — config-level; fabricator arithmetic was correct post-G8"**. The flagship regains boundary coverage.
2. **F-2 closure:** mailbox `try_push -> Result<_, Full>`, park wrapper in Thread R, harness drains between steps, R2 proves a real park occurred. Paste R2 output showing the park.
3. `xdp/redirect.c` + ADR-0008 recorded; `venue` sender bin; `XdpTransport` behind `xdp` feature (libbpf-sys pinned); veth setup script committed.
4. X1–X6 green on CI with URLs; X5 alloc lane on the XDP build.
5. Say **freeze 09**.

When X2 prints `hash == golden` over a real kernel socket path, the last
honesty gap closes: every layer from eBPF redirect to the sink will have
carried real NASDAQ bytes under real kernel scheduling — with confluence
absorbing all of it. Then **10** formalizes the chaos matrix into the
full adversarial suite (VR-1's asymmetric race at scale, fuzz corpus
seeding from real bytes), and **11** is the report: rdtscp-calibrated
latency histograms, the throughput table, and the exact hardware caveat
that makes the numbers believable. The last mile is where the numbers
finally exist.
