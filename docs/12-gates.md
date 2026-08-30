# 12 — Gate Ledger

| Gate | Description | Status | Evidence / Sign-off |
|---|---|---|---|
| **G0** | Phase 00 Baseline Freeze | **PASSED** | `00-spec.md` frozen, raw ITCH magic bytes verified (`00 0c 53 ... 4f`), dev (200MB) & mini (15MB) samples generated with SHA256 hashes recorded. |
| **G1** | Phase 01 Architecture & Workspace Scaffold | **PASSED** | Workspace scaffolding created (ED-01), cargo workspace configured with all 6 crates, ADR-0001 / ADR-0005 recorded, LI-1 layer law verified. |
| **G2** | Phase 02 MoldUDP64 Protocol Codec | **PASSED** | `02-moldudp64.md` written and cross-checked with `moldudp64.pdf` (§9 checklist signed off). Codec implemented in `crates/nf-protocol/src/moldudp64.rs` with full unit tests T1..T11 (TV-1..4 + 10,000 round-trip properties). Zero runtime dependencies (LI-2). |
| **G3** | Phase 03 ITCH 5.0 Protocol Codec | PENDING | — |
