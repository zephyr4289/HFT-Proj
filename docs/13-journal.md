# 13 — Architecture Decision Records (ADR Log)

## ADR-0001: Implementation Language Selection (Rust)

- **Status**: Accepted
- **Context**: NEXUS-FEED-01 requires zero-allocation steady-state processing, compile-time typestate enforcement of valid feed state transitions (`LiveFeedProof`), deterministic confluence across redundant feeds, and high single-core throughput (≥ 10M msg/s).
- **Decision**: Rust is selected as the primary implementation language.
- **Rationale**: Rust provides affine types to enforce the `LiveFeedProof` token lifecycle at compile time (FR-6), zero-cost abstractions without garbage collection, strong standard concurrency primitives without hidden runtime allocations (PR-3), and integrated tooling (`cargo-fuzz`, `Miri`) for hostile-frame verification (VR-4). A C++20 typestate mapping is maintained for portability reference (doc 06).

---

## ADR-0005: Multi-Tier Execution & Validation Environment Strategy

- **Status**: Accepted
- **Context**: The feed arbitrator must be developed and validated in a low-latency environment, with fast local iteration on ARM64 and rigorous continuous integration on x86_64.
- **Decision**: Adopt a three-tier environment strategy:
  1. **Tier 1 (Local Dev)**: Termux / aarch64 for local iterative development, unit testing, and sample replay (`ReplayTransport`).
  2. **Tier 2 (CI / Verification)**: GitHub Actions `ubuntu-latest` (x86_64) for automated PR builds, clippy checks, and golden confluence tests on `sample-mini.itch`.
  3. **Tier 3 (Production Baseline)**: Dedicated bare-metal Linux hardware with AF_XDP / zero-copy kernel bypass for Phase 3 real-world hardware verification.

---

## Architecture Register: VERIFY-3 (TCP Request Framing Uncertainty)

- **Status**: Parked for adjudication before Doc 08 freeze
- **Item**: MoldUDP64 §3.2 specifies a 2-byte BE length prefix on TCP downstream response packets. Whether client retransmission request packets on the wire also carry a 2-byte BE prefix or are sent as raw 20 bytes is isolated behind a single constant `REQUEST_FRAMING` in the recovery client.
- **Impact**: Isolated to a single TX formatting function in `nf-recovery`. Zero impact on arbitrator, protocol parser, or steady-state hot loop.
