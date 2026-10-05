//! R20 "fabric-widedesc" — 512-bit wide descriptor chunk commits
//! (Engineer 3; docs/directives/ENGINEER_3_DIRECTIVE.md §4 Priority 1,
//! docs/challenge/ROADMAP3.md §4.2).
//!
//! # The physics this module exists for
//!
//! The sustained full-verification wall on the 4-vCPU draws is the MAIN
//! thread (~1.86 cyc/msg at the 1.2348B record): the ladder (~0.63
//! cyc/msg), the ordered fold (~0.15), and the per-span descriptor
//! submission into the per-lane SPSC rings (~0.4–0.6 — the desc store,
//! the chunk-open anchor write, the space checks, the backpressure
//! spin). The per-span lane submission itself is SIXTEEN+ scalar 8-byte
//! stores per 16 spans, each carrying its own store-port µop, its
//! address-generation µop and the branch cluster around it
//! (`pending_len == 0`, `cur_inline`, format check) — that is the
//! 0.40–0.60 cyc/msg tax this module removes.
//!
//! Desc8 words are 8 bytes, so a 16-desc span window packs into exactly
//! 128 B = two 64-B lines = two 512-bit `vmovdqu64` stores. The sink
//! stages the window's words into one L1-resident 128-B block (the
//! stack top — never scattered, per ROADMAP3 §4.2's "two L1-resident
//! staging lines"), then commits the whole window with ONE contiguity
//! check + two wide stores:
//!
//! * per span: one pack + one 8-B store into the staging block;
//! * per 16 spans: one contiguity compare + two `vmovdqu64`
//!   (≈0.15–0.18 cyc/msg amortized — the ≤0.18 cyc/msg target);
//! * ring-space boundary checks stay per-chunk (the lane's chunk-open
//!   already reserves `CHUNK + 1` slots — ROADMAP3 §4.2: "the space
//!   check becomes per-chunk, which the ring's chunk cadence already
//!   supports").
//!
//! # Why the wide store is SAFE for the worker-visible protocol
//!
//! The staged word STREAM is bit-identical to the scalar path's (anchor
//! desc first, span descs in order — see `HydraSpanSink::submit_span`);
//! only the store WIDTH changes. Slots below the published `desc_head`
//! are the only ones the worker reads, and publication still happens
//! through the single per-chunk `desc_head` Release store in
//! `flush_pending` — the anti-ping-pong cadence (docs/20), the chunk
//! alignment, the L1-resident desc stream, and every SPSC ownership
//! law are unchanged; the workers read the same lines they read today.
//! The wide stores are PLAIN stores into producer-owned slots (space
//! checked at chunk open), so no ordering obligation exists before the
//! publish Release.
//!
//! # Store-forwarding note (docs/21's warning, priced)
//!
//! The two wide stores each LOAD their 64-B half from the staging
//! block. The LOW half loads words staged 9–16 spans ago (long
//! committed to L1); the HIGH half contains the window's newest word
//! (one store potentially still in the store buffer). A store-forward
//! miss costs ~12 cycles ONCE per 16 spans (≈0.75 cyc/span WORST case,
//! before out-of-order overlap — the next span's pack work does not
//! depend on the wide stores, and the 512-entry OoO window hides the
//! bubble). This is the "produced once, store-once" shape ROADMAP3 §4.2
//! explicitly endorses, not the banned load-hit-store consumer pattern.
//!
//! # Ring wrap economics
//!
//! The commit window at absolute position `pos` maps to ring slots
//! `[pos & mask, +16)`. A window straddling the ring end is
//! non-contiguous; it falls back to the masked scalar loop (once per
//! `cap/16` windows — 1 in 128 at the 2048-word Desc8 ring, ≈0.8% of
//! windows, invisible in the amortized cost). Splitting into two
//! partial wide stores was rejected: the split path would cost a
//! branch + two length computations on EVERY commit to save 16 scalar
//! stores 0.8% of the time.
//!
//! # The L1-residency law (blueprint Lever 4 / directive Priority 2)
//!
//! At >50 GB/s the 15 MB ITCH corpus must stay LLC-resident; every
//! descriptor byte cycling through L2/L3 steals fold memory bandwidth
//! (ROADMAP3 §6-12: "don't add working set to win compute" — every
//! loss since the record traces to exactly that). The Desc8 ring's
//! touched footprint is pinned at [`DESC_RING_L1_BUDGET_BYTES`]
//! (16 KB/lane); multi-megabyte descriptor arrays (the rxdesc "diet"
//! era, refuted draws 10–15) are structurally rejected on the wide
//! path — [`assert_desc_ring_footprint`] is the compile-time guard the
//! lane constructor cites.

/// The frozen wide-commit unit: 16 Desc8 words = 128 B = two 512-bit
/// `vmovdqu64` stores (directive §3 "Chunk16 (128 bytes) interface
/// definitions are locked"). `Desc8` itself stays the 8-byte
/// `offset:u32 | len:u16 | flags:u16` word (`rxdesc_pack_span` — one
/// formula across rings and arrays; never duplicated here).
pub const WIDE_SPANS: usize = 16;

/// Chunk16 — the 128-byte staging window: 16 packed Desc8 words, the
/// frozen commit unit of the wide path. Bit layout per word is owned by
/// `nf_transport::rxdesc` (`rxdesc_pack_span` / anchors via the sink).
pub type Chunk16 = [u64; WIDE_SPANS];

/// The per-lane SPSC descriptor ring's L1-residency budget (the
/// blueprint's cache-footprint invariance law). The Desc8 ring's
/// TOUCHED footprint (ring words × 8 B) must not exceed this; the law
/// is enforced compile-time by [`assert_desc_ring_footprint`] at the
/// lane constructor. Multi-megabyte descriptor structures dilute this
/// budget out of L1 and were refuted by the fleet (rxdesc, draws 10–15,
/// −10…−21% sustained, supply-coupled residue — ROADMAP3 §6-6/12).
pub const DESC_RING_L1_BUDGET_BYTES: usize = 16 * 1024;

/// Compile-time footprint guard: a Desc8 ring of `cap_words` slots
/// touches `cap_words * 8` bytes of L1 per lane. Panics (const-eval) if
/// the budget is exceeded — call as `const _: () = assert_...;`.
#[inline(always)]
pub const fn assert_desc_ring_footprint(cap_words: usize) {
    assert!(
        cap_words * 8 <= DESC_RING_L1_BUDGET_BYTES,
        "desc ring footprint exceeds the 16 KB/lane L1 law"
    );
}

/// The kind of commit [`commit_chunk16`] performed (telemetry: `Wide`
/// windows vs `Scalar` wrap fallbacks — a wrap rate far above
/// `1/(cap/16)` would indicate a position-accounting regression).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommitKind {
    /// Two 512-bit `vmovdqu64` stores into a contiguous window.
    Wide,
    /// Masked scalar loop — the window straddled the ring end (rare:
    /// 1 in `cap/16` windows) or AVX-512 is unavailable on this host.
    Scalar,
}

/// AVX-512F availability, detected ONCE per process (the `OnceLock`
/// caches `is_x86_feature_detected!` — the per-commit gate is then a
/// relaxed load + predictable branch, never a CPUID trap on the hot
/// path). `vmovdqu64` needs exactly AVX512F; no VL/BW/VBMI variant is
/// required for full-width zmm stores.
pub fn wide_store_available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            std::arch::is_x86_feature_detected!("avx512f")
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    })
}

/// Commit one staged 16-word window into the SPSC ring at ABSOLUTE
/// word position `pos` (ring slot = `pos & (cap-1)`).
///
/// * `ring` — the lane's descriptor word array base (word 0).
/// * `cap` — the ring's slot capacity in WORDS (power of two; the
///   Desc8 ring is `cap = DESC_CAP = 2048` even though the backing
///   array is `DESC_CAP * 2` words — the Desc8 stream addresses only
///   the first `cap` words).
///
/// Contiguous windows commit with two 512-bit `vmovdqu64` stores
/// (`CommitKind::Wide`); straddling windows and non-AVX512 hosts fall
/// back to the masked scalar loop (`CommitKind::Scalar`).
///
/// # Safety
///
/// The caller PROVES slots `[pos, pos + 16)` (masked) are
/// producer-owned — for the hydra sink, the lane's chunk-open space
/// check reserved the whole chunk (`CHUNK + 1` slots) up front, and
/// every staged position lies inside the current pending run
/// `[pending_head, pending_head + pending_len)`. The words must have
/// been staged (the staging block's contents are read here).
#[inline(always)]
pub unsafe fn commit_chunk16(ring: *mut u64, cap: usize, pos: u64, stage: &Chunk16) -> CommitKind {
    debug_assert!(cap.is_power_of_two(), "ring cap must be a power of two");
    debug_assert!(cap >= WIDE_SPANS, "ring smaller than one wide window");
    let mask = (cap - 1) as u64;
    let start = (pos & mask) as usize;
    if start + WIDE_SPANS <= cap && wide_store_available() {
        store_wide_128(ring.add(start), stage.as_ptr());
        CommitKind::Wide
    } else {
        commit_partial_scalar(ring, cap, pos, stage, WIDE_SPANS);
        CommitKind::Scalar
    }
}

/// Commit the FIRST `n` staged words with the masked scalar loop —
/// the partial-window tail (chunk/pass boundaries flush a run of
/// `n < 16` staged words) and the wrap fallback share this path.
/// `n` must be `<= WIDE_SPANS` (the staging block's size).
///
/// # Safety
///
/// As [`commit_chunk16`]: slots `[pos, pos + n)` (masked) are
/// producer-owned.
#[inline(always)]
pub unsafe fn commit_partial_scalar(ring: *mut u64, cap: usize, pos: u64, words: &[u64], n: usize) {
    debug_assert!(n <= WIDE_SPANS);
    debug_assert!(words.len() >= n);
    let mask = (cap - 1) as u64;
    let mut p = pos;
    for &w in words.iter().take(n) {
        *ring.add((p & mask) as usize) = w;
        p = p.wrapping_add(1);
    }
}

/// The two `vmovdqu64` stores: 128 B from the staging block to the
/// ring's contiguous window. LOW half first (its words are the oldest
/// — long committed to L1), HIGH half second (contains the window's
/// newest store; the intervening store gives it more drain time before
/// its load — see the store-forwarding note in the module doc).
///
/// The staging block MUST be 64-B aligned (`_mm512_load_si512`); the
/// ring destination may be any 8-B-aligned address (`storeu`).
///
/// # Safety
///
/// `dst` must point to 16 producer-owned, CONTIGUOUS ring words
/// (caller-proven — see `commit_chunk16`); `stage` to 16 valid staged
/// words at a 64-B-aligned address. AVX512F must be available (the
/// caller's `wide_store_available()` gate).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn store_wide_128(dst: *mut u64, stage: *const u64) {
    use std::arch::x86_64::{_mm512_load_si512, _mm512_storeu_si512};
    let lo = _mm512_load_si512(stage as *const std::arch::x86_64::__m512i);
    _mm512_storeu_si512(dst as *mut std::arch::x86_64::__m512i, lo);
    let hi = _mm512_load_si512(stage.add(WIDE_SPANS / 2) as *const std::arch::x86_64::__m512i);
    _mm512_storeu_si512(
        dst.add(WIDE_SPANS / 2) as *mut std::arch::x86_64::__m512i,
        hi,
    );
}

/// Non-x86_64 build symmetry (the scalar fallback serves all hosts —
/// `commit_chunk16` never routes here when `wide_store_available()`
/// is false, which is constant on non-x86_64).
#[cfg(not(target_arch = "x86_64"))]
unsafe fn store_wide_128(_dst: *mut u64, _stage: *const u64) {
    unreachable!("store_wide_128 called without avx512f on a non-x86_64 host")
}

/// The 128-B staging block: one 64-B-aligned line pair, L1-resident on
/// the submitting core (stack top). Produced per-span (scalar packs),
/// consumed once per window by [`commit_chunk16`].
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct DescStage16 {
    pub words: Chunk16,
}

impl DescStage16 {
    pub const fn zeroed() -> Self {
        Self {
            words: [0u64; WIDE_SPANS],
        }
    }
}

/// The per-run `HFT_DESC_WIDE` arm decision (directive §4: "Gate under
/// `HFT_DESC_WIDE=1`"). Requires the Desc8 ring format (the legacy
/// 16-B desc world is the pre-R12 rollback and never stages), the RING
/// submission path (rxdesc arrays and wide staging are mutually
/// exclusive submission strategies — the R17 fleet verdict keeps the
/// ring as the default), the env arm, and AVX512F silicon. Read ONCE
/// at fabric spawn, outside every measurement window (law #9: env
/// parsing allocates — setup only).
pub fn desc_wide_arm(desc8: bool, rxdesc: bool) -> bool {
    desc8
        && !rxdesc
        && std::env::var("HFT_DESC_WIDE").as_deref() == Ok("1")
        && wide_store_available()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Desc8 ring geometry the hydra lane pins (2048 words): the
    /// L1 law must hold for the real constants, compile-time.
    #[test]
    fn t_desc_ring_footprint_law() {
        assert_desc_ring_footprint(2048);
    }

    /// A larger (multi-KB-over-budget) ring must fail the law — the
    /// guard exists to be cited by the lane constructor and by any
    /// future geometry change.
    #[test]
    #[should_panic]
    fn t_desc_ring_footprint_law_rejects_growth() {
        assert_desc_ring_footprint(2048 + 16);
    }

    /// Wide-commit vs scalar reference across ALL window residues,
    /// including the wrap windows (`start > cap - 16`): the ring
    /// contents must be bit-identical and the reported kind must match
    /// the contiguity shape. Exercises the AVX-512 path when the host
    /// has it (CI Xeons) and the scalar fallback otherwise.
    #[test]
    fn t_commit_chunk16_matches_scalar_all_residues() {
        const CAP: usize = 256; // small ring: wrap every 16 windows
        let avx = wide_store_available();
        for pos in 0..(CAP * 3) as u64 {
            let mut stage = DescStage16::zeroed();
            for (i, w) in stage.words.iter_mut().enumerate() {
                *w = pos * 100_000 + i as u64;
            }
            let mut ring_wide = [0u64; CAP];
            let mut ring_ref = [0u64; CAP];
            // SAFETY: mock rings, fully producer-owned, n = 16 within bounds.
            unsafe {
                let kind = commit_chunk16(ring_wide.as_mut_ptr(), CAP, pos, &stage.words);
                commit_partial_scalar(ring_ref.as_mut_ptr(), CAP, pos, &stage.words, WIDE_SPANS);
                let contiguous = (pos & (CAP as u64 - 1)) as usize + WIDE_SPANS <= CAP;
                let expect_wide = contiguous && avx;
                assert_eq!(
                    kind == CommitKind::Wide,
                    expect_wide,
                    "pos {pos}: kind {kind:?} vs contiguous={contiguous} avx={avx}"
                );
            }
            for i in 0..CAP {
                assert_eq!(ring_wide[i], ring_ref[i], "ring word {i} at pos {pos}");
            }
        }
    }

    /// The partial-window tail: `n < 16` words land exactly at their
    /// masked positions (chunk and pass boundaries flush such tails).
    #[test]
    fn t_commit_partial_scalar_tail() {
        const CAP: usize = 64;
        let words: [u64; WIDE_SPANS] = core::array::from_fn(|i| 0xAAAA_0000 + i as u64);
        let mut ring = [0u64; CAP];
        // SAFETY: mock ring, producer-owned.
        unsafe { commit_partial_scalar(ring.as_mut_ptr(), CAP, 61, &words, 7) };
        for (i, &w) in words.iter().enumerate().take(7) {
            let p = (61 + i) & (CAP - 1);
            assert_eq!(ring[p], w, "tail word {i}");
        }
        for (i, w) in ring.iter().enumerate() {
            if !(0..7).any(|k| (61 + k) & (CAP - 1) == i) {
                assert_eq!(*w, 0, "word {i} must be untouched");
            }
        }
    }

    /// The Desc8 ring's real geometry (2048-word cap, 4096-word backing
    /// array): the wide window at the LAST contiguous position and the
    /// first wrap position behave exactly as the law says.
    #[test]
    fn t_commit_real_ring_geometry() {
        const CAP: usize = 2048; // DESC_CAP (desc8 slots)
        let avx = wide_store_available();
        for &pos in &[2032u64, 2033, 2047, 2048, 4095, 65535] {
            let stage = DescStage16 {
                words: core::array::from_fn(|i| pos * 31 + i as u64),
            };
            let mut ring = [0u64; CAP];
            let mut ring_ref = [0u64; CAP];
            // SAFETY: mock rings, producer-owned.
            unsafe {
                let kind = commit_chunk16(ring.as_mut_ptr(), CAP, pos, &stage.words);
                commit_partial_scalar(ring_ref.as_mut_ptr(), CAP, pos, &stage.words, WIDE_SPANS);
                let contiguous = (pos & 2047) as usize + WIDE_SPANS <= CAP;
                assert_eq!(kind == CommitKind::Wide, contiguous && avx, "pos {pos}");
            }
            assert_eq!(ring, ring_ref, "pos {pos}");
        }
    }
}
