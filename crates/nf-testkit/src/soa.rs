//! R12: the 8-entry vectorized watermark ladder (docs/25).
//!
//! # The ladder being vectorized
//!
//! The steady scan's per-frame scalar ladder (nf-arbitrator
//! `steady_scan`) proves, for every frame: session match, `n > 0`, memo
//! full-valid, then the watermark relation — emit (`first == w`, advance
//! `w += n`) or pure duplicate (`last < w`). At the R11 record this costs
//! ~30 µops per frame, 0.63 cycles per message of submitting-core ingest.
//!
//! The dual-feed default schedule's stream is STRICTLY ALTERNATING —
//! feed A publishes packet k (emit), feed B re-publishes the same packet
//! (pure duplicate), A publishes k+1, and so on (equal release times; the
//! schedule pushes feed A's event first per packet). Eight consecutive
//! entries therefore form FOUR IDENTICAL PAIRS, and the whole group is
//! decided by these relations over the SoA sidecar:
//!
//! * **anchor**  `firsts[0] == w`
//! * **pair-eq** `firsts[2i+1] == firsts[2i]`  (the dup is the same packet)
//! * **dup-le**  `ns[2i+1] <= ns[2i]`          (the dup is PURE: with the
//!   chain below, the watermark at the dup's turn is `firsts[2i] +
//!   ns[2i]`, and `last_dup = firsts[2i] + ns[2i+1] - 1 < firsts[2i] +
//!   ns[2i]` ⟺ `ns[2i+1] <= ns[2i]`; a partial overlap would need the
//!   classic path)
//! * **chain**   `firsts[2i+2] == firsts[2i] + ns[2i]`  (the next emit
//!   continues the watermark exactly)
//!
//! Given all four, the eight entries are provably
//! `[emit, dup, emit, dup, emit, dup, emit, dup]`: each even entry's
//! `first` equals the running watermark, each odd entry's `last` sits
//! strictly below it. The scan then advances `w` by `Σ ns[even]` in one
//! shot, emits the four even spans, and counts the four odd entries as
//! duplicates — observably identical (counters, emissions, watermark) to
//! the scalar ladder over the same entries.
//!
//! # The AVX-512 shape
//!
//! One instruction group decides the group: two unaligned 512-bit loads
//! (`firsts`, `ns`), one lane-permute for the pair swap, one for the
//! next-even shift, one add, three mask compares — then three scalar
//! mask tests. The anchor is a plain scalar compare in the caller (the
//! sidecar is L1-hot). ~15 µops per 8 frames (≈344 messages at
//! MtuBound(1400)) replaces ~240.
//!
//! # Gating
//!
//! CI compiles `x86-64-v3`, so the kernel is `#[target_feature(enable =
//! "avx512f")]` behind a runtime `is_x86_feature_detected!` gate — the
//! same law as crcfold's fold512. Non-AVX-512 silicon (the Zen3 scalar
//! class) keeps the scalar ladder; `HFT_VEC_LADDER=0` disarms the vector
//! path everywhere (the rollback; the CI sweep arm 11m runs it).

use nf_protocol::packet::SoaLadder8;

/// The scalar reference ladder — the exact executable spec of the vector
/// kernel's group-eligility contract, in plain Rust. Pub for the
/// differential tests.
///
/// The relations: anchor (`firsts[0] == w`), pair-eq (`firsts[2i+1] ==
/// firsts[2i]`), dup-le (`ns[2i+1] <= ns[2i]`), chain (`firsts[2i+2] ==
/// firsts[2i] + ns[2i]`), PLUS the wrap guard (`firsts[0] + Σns[even]`
/// must not overflow): under no-wrap, `w_at_turn = firsts[2i] + ns[2i]`
/// and `last_dup = firsts[2i] + ns[2i+1] - 1` are wrap-free, so the
/// unsigned compare `last_dup < w_at_turn` is exactly `ns[2i+1] <=
/// ns[2i]` — the vector path's dup classification matches the scalar
/// ladder's unsigned semantics bit for bit. (A wrapping group never
/// occurs on real schedules — `span()` sends sequence overflow cold —
/// but the guard makes the equivalence unconditional, not incidental.)
#[inline]
pub fn ladder8_scalar(firsts: &[u64], ns: &[u64], w: u64) -> bool {
    debug_assert!(firsts.len() >= 8 && ns.len() >= 8);
    if firsts[0] != w {
        return false;
    }
    let sum = ns[0]
        .wrapping_add(ns[2])
        .wrapping_add(ns[4])
        .wrapping_add(ns[6]);
    if firsts[0] > u64::MAX - sum {
        return false; // watermark would wrap mid-group — classic path
    }
    let mut i = 0usize;
    while i < 4 {
        if firsts[2 * i + 1] != firsts[2 * i] {
            return false;
        }
        if ns[2 * i + 1] > ns[2 * i] {
            return false;
        }
        if i < 3 && firsts[2 * i + 2] != firsts[2 * i].wrapping_add(ns[2 * i]) {
            return false;
        }
        i += 1;
    }
    true
}

/// Safe wrapper: the group check with `w` at the caller's scan position.
/// Self-contained: checks anchor, pair-eq, dup-le, chain AND the wrap
/// guard (the even-lane n-sum must not overflow the watermark) — the
/// full group-eligibility contract of `ladder8_scalar`.
/// SAFETY CONTRACT: `firsts` and `ns` each hold at least 8 readable u64
/// elements (the caller only invokes this for groups entirely below the
/// publication length). The target feature is verified by
/// [`ladder8_best`] before this is ever installed.
#[target_feature(enable = "avx512f")]
unsafe fn ladder8_avx512(firsts: *const u64, ns: *const u64, w: u64) -> bool {
    use std::arch::x86_64::*;
    let vf = _mm512_loadu_si512(firsts as *const __m512i);
    let vn = _mm512_loadu_si512(ns as *const __m512i);
    // Lane-0 extraction (stdarch has no _mm512_cvtsi512_si64; the transmute
    // is free — the array access lowers to the lane-0 move).
    let f0 = std::mem::transmute::<_, [u64; 8]>(vf)[0];
    // Anchor: lane 0 of vf must equal the scan's watermark.
    if f0 != w {
        return false;
    }
    // Wrap guard: Σ ns[even] must not overflow past u64::MAX from lane 0
    // (keeps the dup-proof's unsigned algebra exact; see the module doc).
    let zero = _mm512_setzero_si512();
    let even_only = _mm512_mask_add_epi64(zero, 0x55, vn, zero);
    let sum_even = _mm512_reduce_add_epi64(even_only) as u64;
    if sum_even != 0 && f0 > u64::MAX - sum_even {
        return false;
    }
    // Pair swap [1,0,3,2,5,4,7,6]: lane i of the permute holds vf[i^1], so
    // cmpeq(vf, swap) bit i ⟺ vf[i] == vf[i^1] — all 8 bits ⟺ all four
    // pair-eq relations.
    let swidx = _mm512_setr_epi64(1, 0, 3, 2, 5, 4, 7, 6);
    let vf_sw = _mm512_permutexvar_epi64(swidx, vf);
    let pair_eq = _mm512_cmpeq_epu64_mask(vf, vf_sw);
    // Next-even shift [2,3,4,5,6,7,_,_]: lane i holds vf[i+2]; cmpeq(vf_nx,
    // vf+vn) bit i ⟺ vf[i+2] == vf[i] + vn[i]. Lanes 0,2,4 (mask 0x15) are
    // the three even-pair chain relations; odd lanes would demand the dup
    // continue the watermark (false for pure dups with n_odd < n_even), and
    // lanes 6,7 wrap into the next group (not this check's business).
    let nxidx = _mm512_setr_epi64(2, 3, 4, 5, 6, 7, 0, 1);
    let vf_nx = _mm512_permutexvar_epi64(nxidx, vf);
    let vf_sum = _mm512_add_epi64(vf, vn);
    let chain = _mm512_cmpeq_epu64_mask(vf_nx, vf_sum);
    // Pure-dup proof: vn[odd] <= vn[even] ⟺ (vn <= pair-swap(vn)) on odd
    // lanes (bits 1,3,5,7 = 0xAA).
    let vn_sw = _mm512_permutexvar_epi64(swidx, vn);
    let dup_le = _mm512_cmple_epu64_mask(vn, vn_sw);
    pair_eq == 0xFF && (chain & 0x15) == 0x15 && (dup_le & 0xAA) == 0xAA
}

/// The safe callable form installed into `SoaLadder8` (raw pointers keep
/// the per-group call lean; the 8-element contract is the caller's —
/// see `steady_scan_soa`).
#[inline]
fn ladder8_avx512_safe(firsts: *const u64, ns: *const u64, w: u64) -> bool {
    // SAFETY: the feature contract is verified by `ladder8_best` before
    // installation; the 8-element bounds contract is documented on
    // `SoaLadder8` and enforced by the only caller (groups entirely below
    // the publication length).
    unsafe { ladder8_avx512(firsts, ns, w) }
}

/// The best available ladder for THIS silicon, or None (scalar ladder).
/// `HFT_VEC_LADDER=0` disarms (the rollback / CI sweep arm 11m).
pub fn ladder8_best() -> Option<SoaLadder8> {
    if std::env::var("HFT_VEC_LADDER").as_deref() == Ok("0") {
        return None;
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx512f") {
            return Some(ladder8_avx512_safe);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a group from (first, n) pairs — the alternating shape.
    fn group(pairs: &[(u64, u64)]) -> (Vec<u64>, Vec<u64>) {
        let mut firsts = Vec::new();
        let mut ns = Vec::new();
        for &(f, n) in pairs {
            firsts.push(f);
            ns.push(n);
            firsts.push(f);
            ns.push(n);
        }
        (firsts, ns)
    }

    fn ladder_ref(firsts: &[u64], ns: &[u64], w: u64) -> bool {
        // pad to 8 with harmless zeros so the slice forms are callable
        let mut f8 = [0u64; 8];
        let mut n8 = [0u64; 8];
        for i in 0..8 {
            f8[i] = firsts.get(i).copied().unwrap_or(u64::MAX);
            n8[i] = ns.get(i).copied().unwrap_or(0);
        }
        ladder8_scalar(&f8, &n8, w)
    }

    #[test]
    fn t_ladder_alternating_group_passes() {
        // 4 pairs: w=100, ns 10/20/30/40 — the canonical steady shape.
        let (f, n) = group(&[(100, 10), (110, 20), (130, 30), (160, 40)]);
        assert!(ladder_ref(&f, &n, 100));
        // w must anchor lane 0 exactly.
        assert!(!ladder_ref(&f, &n, 99));
        assert!(!ladder_ref(&f, &n, 101));
        assert!(!ladder_ref(&f, &n, 110));
    }

    #[test]
    fn t_ladder_gap_fails() {
        // A gap between pair 1 and pair 2: first[4] != first[2] + ns[2].
        let (mut f, n) = group(&[(100, 10), (110, 20), (200, 30), (230, 40)]);
        f[4] = 200;
        f[5] = 200;
        assert!(!ladder_ref(&f, &n, 100));
    }

    #[test]
    fn t_ladder_partial_dup_fails_or_passes_correctly() {
        // n_odd < n_even is a PURE dup (last < w at its turn) — must PASS
        // (the scalar ladder skips it as a dup; the counters use n_odd).
        let (f, mut n) = group(&[(100, 10), (110, 20), (130, 30), (160, 40)]);
        n[1] = 5; // dup of packet 0 with fewer blocks: last = 104 < 110 ✓
        assert!(ladder_ref(&f, &n, 100));
        // n_odd > n_even is a PARTIAL dup (last >= w at its turn) — the
        // classic path must handle it, so the ladder must REJECT.
        n[1] = 12; // last = 111 >= 110 → partial overlap → cold
        assert!(!ladder_ref(&f, &n, 100));
        let _ = f; // silence unused-mut in non-asserting builds
    }

    #[test]
    fn t_ladder_pair_mismatch_fails() {
        // Odd entry is a DIFFERENT packet (not the even partner's dup).
        let (mut f, n) = group(&[(100, 10), (110, 20), (130, 30), (160, 40)]);
        f[1] = 90; // a late re-send of an older packet
        assert!(!ladder_ref(&f, &n, 100));
    }

    #[test]
    fn t_ladder_two_emits_fails() {
        // Pure-emit run (single feed): odd entries continue the watermark —
        // pair-eq fails, scalar fallback handles it.
        let f = vec![100u64, 110, 130, 160, 200, 210, 230, 260];
        let n = vec![10u64, 20, 30, 40, 10, 20, 30, 40];
        assert!(!ladder_ref(&f, &n, 100));
    }

    #[test]
    fn t_ladder_wrapping_group_rejected() {
        // A group whose watermark advance would wrap u64::MAX must be
        // rejected (the classic path's unsigned `last < w` semantics
        // diverge from the modular chain there — the wrap guard keeps
        // the vector path exactly equivalent).
        let (mut f, mut n) = group(&[(100, 10), (110, 20), (130, 30), (160, 40)]);
        f[0] = u64::MAX - 100; // anchor at w
        f[1] = u64::MAX - 100;
        // chain: pair 1 continues at (MAX-100) + 10
        f[2] = u64::MAX - 90;
        f[3] = u64::MAX - 90;
        f[4] = u64::MAX - 70;
        f[5] = u64::MAX - 70;
        f[6] = u64::MAX - 40;
        f[7] = u64::MAX - 40;
        *n.last_mut().unwrap() = 50; // final advance crosses MAX
        let w = u64::MAX - 100;
        assert!(!ladder_ref(&f, &n, w));
    }

    #[test]
    fn t_ladder_avx512_matches_scalar_when_present() {
        #[cfg(target_arch = "x86_64")]
        {
            if !std::arch::is_x86_feature_detected!("avx512f") {
                eprintln!("soa: avx512f not present — skipping vector cross-check");
                return;
            }
            // Deterministic corpus + a small PRNG sweep: every (firsts, ns,
            // w) the scalar accepts the vector must accept, and vice versa.
            let mut x: u64 = 0x243F_6A88_85A3_08D3;
            let mut next = || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x
            };
            let mut cases = 0usize;
            for _ in 0..20_000 {
                let mut f = [0u64; 8];
                let mut n = [0u64; 8];
                for i in 0..8 {
                    f[i] = 100 + (next() % 1000);
                    n[i] = next() % 50;
                }
                let w = 100 + (next() % 1000);
                let s = ladder8_scalar(&f, &n, w);
                let v = ladder8_avx512_safe(f.as_ptr(), n.as_ptr(), w);
                assert_eq!(
                    s, v,
                    "ladder disagreement: f={f:?} n={n:?} w={w} (scalar={s}, vec={v})"
                );
                cases += 1;
            }
            // And the canonical alternating shape must pass both.
            let (f, n) = group(&[(100, 10), (110, 20), (130, 30), (160, 40)]);
            assert!(ladder8_avx512_safe(f.as_ptr(), n.as_ptr(), 100));
            // The canonical corruptions must fail both.
            let (mut f2, n2) = group(&[(100, 10), (110, 20), (130, 30), (160, 40)]);
            f2[3] = 500;
            assert!(!ladder8_avx512_safe(f2.as_ptr(), n2.as_ptr(), 100));
            // The wrap corner must fail BOTH identically.
            let (mut f3, mut n3) = group(&[(100, 10), (110, 20), (130, 30), (160, 40)]);
            let base = u64::MAX - 100;
            for (k, off) in [0u64, 10, 30, 60].iter().enumerate() {
                f3[2 * k] = base + *off;
                f3[2 * k + 1] = base + *off;
            }
            *n3.last_mut().unwrap() = 50; // final advance crosses u64::MAX
            assert_eq!(
                ladder8_scalar(&f3, &n3, base),
                ladder8_avx512_safe(f3.as_ptr(), n3.as_ptr(), base),
                "wrap-corner disagreement"
            );
            assert!(!ladder8_avx512_safe(f3.as_ptr(), n3.as_ptr(), base));
            eprintln!("soa: avx512 cross-check cases={cases}");
        }
    }
}
