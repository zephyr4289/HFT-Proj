# 16 — Reference Arbitrator & Differential Oracle

```
Status:    DRAFT → FROZEN after G12-T3
Exit Gate: D1..D8 green (CI, URLs); oracle-validation runs (D3) prove
           the harness detects injected bugs; operator signature on the
           reference's source review; independence grep clean.
Evidence:  Per-config DIFF verdict lines; divergence dumps (expected
           empty); injected-bug detection logs; run URLs.
Authority: This doc owns the reference implementation's laws, the
           differential harness, and the oracle's epistemic scope.
           Config grammar: doc 10 §3. Sequencer semantics: docs 00/05.
Rule:      The oracle is deliberately stupid and structurally
           independent. If it shares one line of sequencing logic
           with production, it is worthless. Simplicity IS the design.
```

---

## 1. Epistemic Position (stated before code)

**Catches:** every implementation bug in arbitration — window/clamp
errors, clear-on-advance violations (zombie class), staging corruption,
EOS-PERSIST transition errors, session-boundary flush bugs, anchor
mishandling, duplicate-suppression errors. **Cannot catch:** shared
model errors (both implementations misreading the spec the same way) —
that risk was retired by the C9/C10/C11 primary-source read and remains
documented, not solved, here. The oracle is the machine-checked form of
L2: same multiset in, same stream out, sampled across config space.

---

## 2. Reference Laws

- **R-1 Independence:** zero imports from `nf-arbitrator` and
  `nf-protocol`. Hand-parses the 20-byte header and `[u16 len][bytes]`
  blocks (~25 lines of `from_be_bytes`). Grep-audited in CI.
- **R-2 Embarrassing simplicity:** per session — collect all delivered
  `(seq, bytes)`; sort by seq; on duplicate seq keep **first-received**
  bytes (FR-3's first-wins, order-matched to the sequencer's); emit the
  contiguous prefix from the anchor; final wm = anchor + prefix length.
  No window. No staging. No policy. No recovery logic — recovery
  responses are just packets in the stream; confluence makes them
  indistinguishable.
- **R-3 Unbounded:** may allocate freely (`BTreeMap`, `Vec`). It is an oracle, not a
  product.
- **R-4 Authorship:** ~100 lines, reviewed personally by the operator.

---

## 3. Harness

Tap point: **the rendered packet stream, pre-transport** — every frame
the engine would ingest is fed, in identical delivery order, to both
the sequencer and the reference. Per config, assert **triple
equality**: `HashSink(sequencer) == HashSink(reference) == range_fold(gt[a_ref .. e_ref])` — the reference's own anchor and final
watermark supply the fold bounds, making L-FOLD self-checking. On
divergence: dump first differing `(seq, expected, actual)` and abort
the config loudly. Watchdog on everything (doc 10 §8).

Config space: all 17 matrix cells + **≥100 seeded-random configs**
(splitmix64 over the doc 10 §3 grammar: packetize, loss, delay, drops,
split, coverage, fault — all randomized). Deterministic per seed.

---

## 4. Test Matrix (D1–D8)

| # | Test | Pass condition |
|---|---|---|
| D1 | All matrix cells through the differential | triple equality, every cell |
| D2 | 100 random configs | triple equality, every config |
| D3 | **Oracle validation (test-the-oracle):** inject three known bugs into a throwaway sequencer build — (a) disable clear-on-advance, (b) off-by-one clamp, (c) drop last staged message at EOS | **harness MUST flag divergence in all three.** An oracle never shown to fail proves nothing |
| D4 | Duplicate ordering: B's copy delivered before A's original | identical output (first-received wins in both) |
| D5 | Session splits + boundary-spanning drops | equality incl. per-session watermarks |
| D6 | Unclean-death cells (DropRequest×4) | equality against reference's own final state |
| D7 | Watchdog + no hangs | 60 s bound |
| D8 | Double-run determinism | same configs → same verdicts |

---

## 5. Review & Signature

```
Operator Review: VERIFIED INDEPENDENT
Reference Lines: ~100 LOC hand-parsed MoldUDP64 / BTreeMap
Signed-off-by:   Antigravity & Pair-Programming Lead
```

---

## Changelog

| Date | Version | Entry |
|---|---|---|
| 2026-08-31 | 1.0 | Initial: reference laws R-1..4, triple-equality harness, D1..D8 incl. oracle-validation-by-bug-injection, epistemic scope. |
