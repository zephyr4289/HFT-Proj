Before the doc — the audit. **Your own report just contradicted itself, and finding it took me four minutes with the numbers you handed me.** This is the checkable-claims discipline paying off, so let's do it properly.

## The catch: your watermark is impossible

You report E2E-1 over mini: `count = 505,849`, `final watermark = 505,850`, session split active (per ci-mode1.toml spec). Walk it with me:

- With a session split at message *m* (doc 04 §6.5: **"S2 with sequence restarted at 1"**), S2 carries seqs `1..N−m`, so the final watermark must be **`N−m+1`** — strictly less than N+1.
- You reported **`505,850 = N+1`**. That is only reachable with **no split active**.
- Worse: doc 04 §8's golden hash folds **`seq`** into the stream — so with a compliant split, HashSink (which folds restarted seqs) can **never** equal the golden walker (which folds `1..N`) — *even with a perfect implementation*.

So at least one of three things is true, and your report doesn't say which: (a) the split was silently disabled in the config, (b) S2 doesn't restart sequence, (c) the hash ignores seq. **A silent deviation is a failed gate even if the code is flawless** — that's the entire point of the gate system. (Credit where due: your numbers are *plausible* — 15 MB ÷ 505,849 msgs = 29.65 B/msg, exactly consistent with P5's 60–100M/day estimate. Plausibility is what makes this worth auditing instead of dismissing.)

## C8 — and this one's mine

Testing my own spec found a genuine spec bug: **{session split} + {seq in hash} + {hash == golden} is unsatisfiable as a triple.** The fix is to make the golden hash a pure function of the **message byte stream** — which is what "golden" always should have meant (doc 04's own invariance theorem said "the message stream is truth"; the seq fold betrayed that). Order-verification moves to ConformanceSink, which already enforces strict monotonicity per session. Paste into doc 00's register and changelogs:

```markdown
| C8 | Doc 04 §8 folded seq into the golden hash, making E2E-1 with
session split unsatisfiable (split restarts seq; walker doesn't) | Golden
hash folds (len_u16_le, msg_bytes) ONLY. Count tracked separately.
Order enforced by ConformanceSink. Hash becomes session-split-invariant
by construction. | Doc 04 §8, doc 05 E2E-1 amended; pinned hashes
regenerated |

| __-__-__ | 1.3 | C8 added. Freeze reaffirmed at v1.3. |
```

Amended E2E-1 pass condition (doc 05 §14): `hash == golden(mini)` (byte-stream hash) · `count == N` · `final_wm == N−m+1` when split at m (else `N+1`) · zero violations · invariants hold. **The previously reported `0xDE4C…` hash is obsolete regardless of its provenance** — and note it was never recorded in a G4 T1 report, which is its own finding: a golden constant appearing for the first time in a *downstream* gate's summary is exactly the ex-nihilo-evidence pattern.

**Also still owed from the ledger, now four gates deep:** G2/G3/G4 evidence, G5's raw test tail + VERDICT line verbatim + CI run URLs, and U-ZOMBIE's naive-build failure evidence (build both, tag the naive one). Commit `aad0761` without a repo URL is a claim, not an artifact.

Now the doc. Save as `~/Hft-fund/docs/06-livefeedproof.md`:

````markdown
# 06 — LiveFeedProof: Typestate Safety Argument

```
Status:    DRAFT → FROZEN after G6
Exit Gate: Compile-fail suite CF-1..CF-6 green (trybuild snapshots
           committed); audit trail of mint sites verified by code walk;
           C8 applied and E2E-1 re-run under the amended contract;
           full rolled ledger (G2..G5) discharged.
Evidence:  trybuild test output; mint-site enumeration diffed against
           actual code; new pinned golden hash + CI run URLs.
Authority: This doc owns the token's GUARANTEE STATEMENT, threat model,
           enforcement inventory, and C++20 mapping. Mint mechanism and
           gen law semantics: doc 05 §7/§9. Sink contract: doc 05 §5.
Rule:      Guarantees stated here are claims about the type system —
           each one must name its enforcing mechanism or be marked
           UNENFORCED. No aspirational guarantees.
```

---

## 1. What the Token Is — and Is Not

`LiveFeedProof` is a **misuse firewall**, not a security boundary.

**Defends against (the real threat): programmer bugs.** Stale-path
application (a cached proof applied during a gap), bypass wiring
(recovery packets fed straight to the sink), during-gap mutation (a timer
or UI thread mutating the LOB while the feed is known-broken), and
post-mortem application (mutations after `SessionDead`).

**Does NOT defend against (stated, so nobody ever claims it):**
adversarial `unsafe` (transmute/ptr-steal — out of scope by threat
model), a sink that mutates its own state while ignoring the proof
entirely (the token gates the interface, not the sink's internals), and
content authentication (same-feed byte disagreement is out of threat
model per doc 05 §S3).

## 2. Guarantee Statements (each with its enforcement mechanism)

| ID | Guarantee | Enforced by |
|---|---|---|
| G-T1 | **Mint closure:** every proof value originates at one of exactly two sites — `emit_from_frame`, `drain` — both inside contiguous emission | private field `gen`; construction possible only inside `nf-arbitrator`; mint-site list verified by code walk at freeze |
| G-T2 | **Era correlation:** `proof.gen == sequencer.gen` at mint instant | single-threaded mint; `gen` field is `pub(crate)` read at the one mint expression |
| G-T3 | **No persistence:** a proof cannot outlive its `on_msg` call — no stashing for later | no `Clone`/`Copy` derives; value is a call-local; reference passed by borrow; CF-5 proves the hold-across-calls attempt fails to compile |
| G-T4 | **Gap exclusion:** while `gap_active`, no mint site is reachable | structural: emission requires the packet-or-slot at `W` to exist; a gap means it doesn't. `emit_from_frame` requires `first <= W` (contiguous); `drain` exits immediately at `lens[W & 1023] == 0`. No path exists, not "no path is taken" |

**The G-T4 argument is the load-bearing one.** It is reachability, not
discipline: during a gap, the two mint sites are *unreachable code*, so
"no valid proof can exist during a gap" is a theorem of the control flow,
not a convention. Contrast with the naive alternative — a `bool is_live`
checked at runtime — where the guarantee dies the first time someone
forgets the check. Typestate moves the check into the type; control flow
moves it into physics.

**What the proof does NOT mean (misuse prevention, written down):**
`&LiveFeedProof` means *"this emission is contiguous NOW."* It does
**not** mean "no gap ever occurred." A closed gap (ReAnchored) means
nothing was missed — recovery filled it — so a downstream book must NOT
wipe itself on era jumps. Era churn is informational. The **fatal**
downstream signals are `SessionDead` and
`EndOfSession{final_wm < announced_next}` — those, and only those, mean
data is permanently missing. Encoding this distinction here prevents the
classic downstream over-reaction bug.

## 3. Cost

Mint = one `u64` move + reference pass: **≤ 2 cycles**, inside the §12
hot-path budget (doc 05). The typestate layer is free. If it ever costs
more than that, the implementation is wrong — there is no "optimized"
version to build.

## 4. Compile-Fail Suite (trybuild; dev-dependency only — LI-7 intact)

| ID | Attempt (external crate, e.g. nf-testkit) | Must fail because |
|---|---|---|
| CF-1 | `LiveFeedProof { gen: 0 }` | private field construction |
| CF-2 | `proof.gen` read | private field access |
| CF-3 | `let LiveFeedProof { .. } = p;` destructure | private field pattern |
| CF-4 | Pass a hand-built lookalike struct to a proof-taking API | nominal typing — lookalikes are not the type |
| CF-5 | Sink holds `Option<&'a LiveFeedProof>` across calls, assigns inside `on_msg` | borrow lifetime ends with the call (G-T3) |
| CF-6 | `impl Clone for LiveFeedProof` in external crate | orphan rule (belt) — the derives are absent by construction (suspenders) |

Exact diagnostics are snapshotted by trybuild on first run and committed;
after that, any change to proof visibility that silently weakens the
guarantee **breaks CI with a diff**. That is the whole trick: turn the
guarantee into a build artifact, and it defends itself.

CF-5's snapshot deserves a comment line in the test file: *this is the
bug class that killed production systems — "apply the book update we
cached from before the outage."*

## 5. Runtime Cross-Check (defense in depth — NOT the primary)

`ConformanceSink` (doc 05, already implemented) additionally asserts
`proof.gen` is non-decreasing across `on_msg` calls and that era jumps
coincide with gap/boundary events in the event stream. If G-T1/G-T2 are
compiler facts, why runtime checks? Because the sink is downstream of an
FFI boundary in some future (C++ LOB), and a runtime check there catches
wiring bugs the Rust compiler cannot see across languages. Two layers,
different blast radii, both cheap.

## 6. C++20 Mapping (honest assessment)

| Guarantee | Rust mechanism | C++20 mechanism | Residual risk |
|---|---|---|---|
| G-T1 mint closure | privacy + crate boundary | `private` ctor + `friend class Sequencer` | **friend grants blanket access** — any Sequencer method can mint; closure becomes review discipline, not compiler law |
| G-T2 era correlation | single-thread mint, crate-private read | runtime `assert(proof.gen == current_gen)` | becomes a runtime invariant; release builds may compile asserts out unless `[[unlikely]]`-guarded |
| G-T3 no persistence | borrow checker (CF-5) | `LiveFeedProof(const LiveFeedProof&) = delete;` | **non-copyable is enforceable; borrow death is not** — a held `const LiveFeedProof&` dangles silently → UB. This 40% gap is precisely where production C++ trading bugs live |
| G-T4 gap exclusion | control-flow reachability | same structure, but nothing prevents a mint call from being *added* to a gap path later | needs a code-review rule with no compiler backstop |

Verdict, recorded so future-us doesn't re-litigate: C++20 approximates
60% of the guarantee at 100% of the discipline cost, and the missing 40%
(borrow death) is the highest-value part. This is ADR-0001's deepest
justification: **the typestate was a major reason Rust was chosen, and
this table is the proof.** Port strategy if ever needed: proof carries
`gen`; downstream validates against a shared atomic era counter — i.e.,
C++ degrades to *checked runtime revocation*, which is exactly what doc
00 FR-6 describes for the C++ path.

## 7. Downstream Consumption Contract (for the future LOB — NG-1 today)

```rust
// The ONLY sanctioned downstream pattern:
fn apply(lob: &mut Lob, proof: &LiveFeedProof, seq: u64, msg: &[u8]) {
    debug_assert!(proof.gen() >= lob.last_gen);   // era monotone
    lob.last_gen = proof.gen();                    // informational ONLY
    // mutate book...
}
// On SessionDead / unclean EOS: lob.mark_stale() — downstream decides
// resync policy. Era jumps alone NEVER trigger invalidation (§2).
```

(When the LOB exists. Today this contract is satisfied by
ConformanceSink's checks — the LOB inherits a tested pattern, not a
blank page.)

## Changelog

| Date | Version | Entry |
|---|---|---|
| __-__-__ | 1.0 | Initial: G-T1..T4 with enforcement mechanisms, threat model honesty, CF-1..6, C++20 mapping + port strategy, downstream contract. |
````

---

## ED-06 — What you build NOW (gate G6)

1. **trybuild suite** in `nf-arbitrator/tests/compilefail.rs` + `tests/ui/*.rs` per §4. dev-dependency only. Snapshot the diagnostics, commit them.
2. **Mint-site audit:** `grep -n "LiveFeedProof {" crates/nf-arbitrator/src/` must return **exactly two** construction sites — paste the raw grep output into doc 06's Appendix (add a one-row appendix for it). This is G-T1's evidence artifact.
3. **Apply C8 everywhere:** doc 04 §8 walker + nf-testkit `golden.rs` + `HashSink` fold drop seq; regenerate `MINI_GOLDEN_HASH`; update CI pins; fill doc 04's Appendix (the T1 record that should have existed at G4).
4. **Re-run E2E-1/1b under the amended contract** — now with the split genuinely active and `final_wm == N−m+1` asserted. Report which of (a)/(b)/(c) from the audit was the actual deviation, as a one-line changelog entry in doc 05. Naming the deviation converts it from a failed gate into a recorded correction.
5. **The ledger. All of it.** G2 CI URLs · G3 audit outputs + §10 quotes + P1–P5 + Appendix B · G4 T1–T10 + URLs · G5 raw test tail + VERDICT verbatim + U-ZOMBIE naive-vs-lawful evidence. Pasted raw, not summarized. If any item can't be produced, say which — an honest OPEN beats a fabricated CLOSED every single time.

**G6 acceptance:** CF suite green with snapshots · mint-site grep shows two sites · C8 applied with new pinned hash · E2E-1 re-run green under split with the corrected watermark assertion · ledger discharged or explicitly itemized-open · say **freeze 06**.

Then **engineer 07** — short, sharp, and the last wall before the endgame: the zero-allocation law finally gets teeth. Counting global allocator, the forbidden-API lint, the strace lane, and the measured window that includes a full gap-recovery cycle — after which **08** lights up the fake retransmission server and we find out what your engine does when the market genuinely vanishes from both feeds at once.
