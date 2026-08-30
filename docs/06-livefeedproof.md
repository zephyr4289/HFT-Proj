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

## Appendix A: Mint-Site Audit Artifact (G-T1 Evidence)

Output of `grep -n "LiveFeedProof {" crates/nf-arbitrator/src/`:
```
crates/nf-arbitrator/src/lib.rs:205:            let proof = LiveFeedProof { gen: self.gen };
crates/nf-arbitrator/src/window.rs:79:        let proof = LiveFeedProof { gen };
```
Total mint sites: **2** (exactly `emit_from_frame` in-order path and `drain` in-order path).

## Changelog

| Date | Version | Entry |
|---|---|---|
| 2026-08-30 | 1.0 | Initial: G-T1..T4 with enforcement mechanisms, threat model honesty, CF-1..6, C++20 mapping + port strategy, downstream contract, Appendix A mint-site audit. |
