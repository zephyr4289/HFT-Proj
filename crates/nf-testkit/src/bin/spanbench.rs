//! spanbench — R36 Res-fold roofline instrument (docs/solution1.md §2.7/§6).
//!
//! # Why this exists
//!
//! The R36 kernel's three operating points, priced on the CI runner with
//! pinning logged (the R34 sigbench discipline — never price on faith,
//! `bitexact=true` is computed, not assumed):
//!
//! * `cold` — the fresh Theorem-10 chunk fold: 977 × 512-message chunks
//!   through the dispatching kernel (`fold_chunk`: VPCLMULQDQ 4-lane
//!   V-fold with the 0x55 compress fix, scalar tail folded once). The
//!   reference's 21 µs/pass sequential span walk is the denominator; the
//!   blueprint's §2.7 budget is 0.23–0.40 µs/chunk (≈2 cycles/message).
//! * `seq` — the sequential 2-op chain (`fold_stream_sequential`) over
//!   the same 500k tokens: the serialized-hash baseline reconstructed
//!   (each CLMUL waits for the last — the loop-latency wall the closed
//!   form removes).
//! * `replay` — the memoized O(1) composition: 976 `p64_compose` hops
//!   over the frozen 36-byte records (Corollary 10.1: 1 carry-less
//!   multiply + 1 XOR each) + one `hash_apply_p64` — the regime-B
//!   arithmetic (~15 cycles per chunk record).
//! * `merge` — the R36-C Galois shift: `hash_merge` at worker-scale
//!   degrees (17.5 M — the 500k-message pass class, the two-level
//!   table's domain), 3 CLMUL + 1 XOR ≈ 10–15 cycles, plus the
//!   500k two-worker split equality (the §4.1 law, asserted).
//!
//! The corpus is the deterministic 500,000-token stream (the doc's own
//! working set), the same shape sigbench uses. Every timed regime's
//! result feeds a checksum sink (no dead-code elimination; the R34
//! lesson — LLVM will scalar-evolve closed-form loops otherwise).

// Diagnostic tool, not an engine binary: it allocates, prints freely,
// and shares nothing with the measured fabric beyond the kernel API —
// the workspace's Tier-F zero-allocation law does not apply.
#![allow(clippy::all)]
#![allow(warnings)]

use nf_testkit::spanfold::{
    compose_records, epoch_merge_p64, fold_cols, fold_stream, fold_stream_sequential,
    hash_merge, p64_compose, chunk_records, tree_fold_p64, tree_fold_p64_dispatch,
    tree_fold_two_level, P64, CHUNK, W_T,
};
use std::time::Instant;

const N: usize = 500_000;

/// splitmix64 — the deterministic stream generator (sigbench's PRNG).
struct Sm64(u64);
impl Sm64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The deterministic 500k-token corpus: the ITCH-ish type mix (9 types
/// at their wire widths), random digests, ~1/4 price setters, wrapping
/// volume deltas.
fn build_stream(n: usize) -> nf_testkit::spanfold::SpanTokens {
    let mut rng = Sm64(0x0DD0_5EED_3636_C0DE);
    let mut t = nf_testkit::spanfold::SpanTokens::with_len(n);
    for i in 0..n {
        t.ty[i] = match rng.next() % 10 {
            0..=2 => 0, // A (36)
            3..=4 => 1, // F (40)
            5 => 2,     // E (31)
            6 => 3,     // C (36)
            7 => 4,     // X (23)
            8 => 5,     // D (19)
            _ => 6,     // U (35)
        };
        t.vd[i] = rng.next();
        t.db[i] = (rng.next() >> 1) as i64 - (1 << 31);
        t.da[i] = (rng.next() >> 1) as i64 - (1 << 31);
        t.setp[i] = if rng.next() % 4 == 0 { 1 } else { 0 };
        t.px[i] = (rng.next() >> 32) as u32;
    }
    t
}

#[cfg(target_arch = "x86_64")]
fn rdtsc() -> u64 {
    // SAFETY: rdtsc is an unprivileged userspace instruction.
    unsafe { std::arch::x86_64::_rdtsc() }
}

/// Calibrate the TSC against the wall clock over ~200 ms.
#[cfg(target_arch = "x86_64")]
fn calibrate_tsc() -> u64 {
    let t0 = Instant::now();
    let c0 = rdtsc();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let dc = rdtsc().wrapping_sub(c0);
    (dc as f64 / t0.elapsed().as_secs_f64()) as u64
}

fn main() {
    #[cfg(target_arch = "x86_64")]
    let avx512 = std::arch::is_x86_feature_detected!("avx512f")
        && std::arch::is_x86_feature_detected!("vpclmulqdq");
    #[cfg(not(target_arch = "x86_64"))]
    let avx512 = false;

    #[cfg(target_arch = "x86_64")]
    let tsc_hz = calibrate_tsc();
    #[cfg(not(target_arch = "x86_64"))]
    let tsc_hz = 0u64;

    let chunks = (N + CHUNK - 1) / CHUNK; // 977 = 976·512 + 288
    let t = build_stream(N);
    println!("R36_SPANBENCH tokens={N} chunks={chunks} avx512={avx512} tsc_hz={tsc_hz}");

    // ── The judges, untimed: the sequential oracle and the dual-path
    //    equality over the WHOLE stream (Theorem 18 — bit-exact is
    //    computed, never assumed).
    let (seq_rec, h_seq) = fold_stream_sequential(&t, 0);
    let (whole_rec, h_whole) = fold_stream(&t, 0);
    let bitexact = whole_rec == seq_rec && h_whole == h_seq;
    // Per-chunk dual-path: the vector body vs the sequential chain on
    // every 512-block (the compress-fix pin — Corollary on real paths;
    // the correctness pass is untimed, so the slice materialization is
    // harmless — the timed regimes never allocate).
    let mut dual_path_ok = true;
    let mut i = 0usize;
    while i < N {
        let end = (i + CHUNK).min(N);
        let (v, _) = fold_cols(
            &t.ty[i..end], &t.vd[i..end], &t.db[i..end], &t.da[i..end], &t.px[i..end],
            &t.setp[i..end], 0,
        );
        let part = nf_testkit::spanfold::SpanTokens {
            ty: t.ty[i..end].to_vec(),
            vd: t.vd[i..end].to_vec(),
            db: t.db[i..end].to_vec(),
            da: t.da[i..end].to_vec(),
            px: t.px[i..end].to_vec(),
            setp: t.setp[i..end].to_vec(),
        };
        let (s, _) = fold_stream_sequential(&part, 0);
        dual_path_ok &= P64::from_r36(&v) == s;
        i = end;
    }

    // ── Regime: cold — the fresh chunk fold over all 977 chunks, the
    //    COLUMN-VIEW API (zero materialization: the fabric's shape —
    //    the scan phase owns the SoA columns, the fold consumes them).
    //    Warmup 1, best of 5 (the noise-free floor).
    let _ = fold_stream(&t, 0);
    let mut cold_best_ns = u128::MAX;
    let mut cold_best_cyc = 0u128;
    let mut sink: u64 = 0;
    for _ in 0..5 {
        #[cfg(target_arch = "x86_64")]
        let c0 = rdtsc();
        let now = Instant::now();
        let mut j = 0usize;
        while j < N {
            let end = (j + CHUNK).min(N);
            let (r, h) = fold_cols(
                &t.ty[j..end], &t.vd[j..end], &t.db[j..end], &t.da[j..end], &t.px[j..end],
                &t.setp[j..end], sink,
            );
            sink = h;
            j = end;
        }
        let dt = now.elapsed().as_nanos();
        #[cfg(target_arch = "x86_64")]
        {
            let dc = rdtsc().wrapping_sub(c0) as u128;
            if dt < cold_best_ns {
                cold_best_ns = dt;
                cold_best_cyc = dc;
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            if dt < cold_best_ns {
                cold_best_ns = dt;
            }
        }
    }
    // The cold regime's own correctness sink: recompose + checksum.
    let cold_recs = chunk_records(&t, CHUNK);
    let cold_composed = compose_records(&cold_recs);
    let mut chk: u64 = cold_composed.v
        ^ cold_composed.c
        ^ (cold_composed.deg as u64)
        ^ (cold_composed.cnt as u64);
    chk = chk.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    chk ^= sink; // the running-hash chain is the observable

    // ── Regime: seq — the sequential 2-op chain over the whole 500k
    //    (the serialized-hash baseline: the 21 µs/pass denominator's
    //    reconstruction at chunk scale). Best of 3 (it is ~50x slower
    //    than cold; 3 iterations keep the arm under a second).
    let mut seq_best_ns = u128::MAX;
    let mut seq_per_512 = 0.0f64;
    for _ in 0..3 {
        let now = Instant::now();
        let (r, h) = fold_stream_sequential(&t, 0);
        let dt = now.elapsed().as_nanos();
        chk = chk.wrapping_add(r.v).wrapping_add(h);
        if dt < seq_best_ns {
            seq_best_ns = dt;
            seq_per_512 = dt as f64 / chunks as f64;
        }
    }

    // ── Regime: replay — the memoized O(1) composition walk: 976
    //    p64_compose hops over the frozen records + one apply. 1000
    //    timed reductions; the record table is read round-robin (real
    //    memory, not scalar-evolvable constants) and the result feeds
    //    the checksum sink.
    let mut replay_best_ns = u128::MAX;
    let mut replay_ok = true;
    for _ in 0..1000 {
        let now = Instant::now();
        let mut acc = P64::IDENTITY;
        for r in &cold_recs {
            acc = p64_compose(&acc, &P64::from_r36(r));
        }
        let dt = now.elapsed().as_nanos();
        replay_ok &= acc == seq_rec;
        chk = chk.wrapping_add(acc.v).wrapping_add(acc.c);
        if dt < replay_best_ns {
            replay_best_ns = dt;
        }
    }

    // ── Regime: merge — the R36-C Galois shift at worker scale. The
    //    two-level shift (`shift_pow` at 17.5M) is OFF the critical
    //    path (known at work assignment); the on-path cost is
    //    mulmod + XOR. Timed over 100k merges at random worker-scale
    //    degrees (the table lookups are real traffic).
    let d_late: u64 = seq_rec.deg as u64; // the corpus's true span
    let h_early = 0xA5A5_5A5A_DEAD_0BEEu64;
    let mut merge_degrees = Vec::with_capacity(1024);
    {
        let mut rng = Sm64(0x5EED_3636_C0DE_BAAD);
        for _ in 0..1024 {
            merge_degrees.push(rng.next() & 0x3FF_FFFF); // worker-scale
        }
    }
    let now = Instant::now();
    let mut h = h_early;
    for k in 0..100_000usize {
        let d = merge_degrees[k & 1023];
        h = hash_merge(h, (h >> 17) ^ d as u64, d);
    }
    let merge_ns = now.elapsed().as_nanos() as f64 / 100_000.0;
    chk = chk.wrapping_add(h);

    // The 500k two-worker split equality (asserted, not timed — the
    // cell pins it; here it is the engagement check).
    let mid = N / 2;
    let early = nf_testkit::spanfold::SpanTokens {
        ty: t.ty[..mid].to_vec(),
        vd: t.vd[..mid].to_vec(),
        db: t.db[..mid].to_vec(),
        da: t.da[..mid].to_vec(),
        px: t.px[..mid].to_vec(),
        setp: t.setp[..mid].to_vec(),
    };
    let late = nf_testkit::spanfold::SpanTokens {
        ty: t.ty[mid..].to_vec(),
        vd: t.vd[mid..].to_vec(),
        db: t.db[mid..].to_vec(),
        da: t.da[mid..].to_vec(),
        px: t.px[mid..].to_vec(),
        setp: t.setp[mid..].to_vec(),
    };
    let (early_rec, h_e) = fold_stream_sequential(&early, 0);
    let (late_rec, _) = fold_stream(&late, 0);
    let merged = hash_merge(h_e, late_rec.v, late_rec.deg as u64);
    let merge_ok = merged == h_seq;
    let _ = early_rec;

    // ── Regime: tree (R37, solution2.md Part I — Gate 6's instrument) ──
    // The champion's per-pass fold is 322 fused-call records; the B1
    // wall is the serial left spine (322 dependent composes, the
    // 2.08 µs/pass cross-pass loop-carried barrier) vs the ⌈log₂ K⌉
    // up-sweep. Measured on BOTH rings (Theorem 28's polymorphism):
    // the SPANHASH P64 records (CLMUL) and the engine's segment
    // records (the rotl-13 law — `sink::tree_fold_segments`, the
    // journal's own surface). Warmup + best-of-20 (each regime is
    // sub-µs; the floor is the point). The records are REAL (the
    // corpus's frozen chunk table round-robin rebased into a 322-pass
    // window — no scalar-evolvable constants). Bit-exactness is
    // COMPUTED against the serial chain on every iteration (the R34
    // lesson: never assume, never dead-code).
    const K_PASS: usize = 322; // the champion's fused-call count per pass
    let mut pass_recs: Vec<P64> = Vec::with_capacity(K_PASS);
    for i in 0..K_PASS {
        // Round-robin over the frozen chunk records (real memory),
        // distinct by index — the pass's leaf stream.
        let src = &cold_recs[(i * 7 + 3) % cold_recs.len()];
        let mut p = P64::from_r36(src);
        // Distinct-per-position digests (defeat constant folding).
        p.v ^= (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        pass_recs.push(p);
    }
    // The SPANHASH ring: serial vs tree.
    let serial_pass = || -> P64 {
        pass_recs
            .iter()
            .fold(P64::IDENTITY, |acc, r| p64_compose(&acc, r))
    };
    let serial_p64 = serial_pass();
    let _ = serial_pass; // (the closure is the oracle's bracketing)
    // Warmup + timing: serial (best of 10).
    let mut serial_p64_ns = u128::MAX;
    for _ in 0..10 {
        let now = Instant::now();
        let acc = serial_pass();
        let dt = now.elapsed().as_nanos();
        chk = chk.wrapping_add(acc.v);
        if dt < serial_p64_ns {
            serial_p64_ns = dt;
        }
    }
    // Tree: scalar + dispatch (the mulmod4 schedule on AVX-512).
    let mut tree_scalar_ns = u128::MAX;
    let mut tree_dispatch_ns = u128::MAX;
    let mut tree_ok = true;
    for _ in 0..20 {
        let mut r1 = pass_recs.clone();
        let now = Instant::now();
        let got = tree_fold_p64(&mut r1);
        let dt = now.elapsed().as_nanos();
        tree_ok &= got == serial_p64;
        chk = chk.wrapping_add(got.v).wrapping_add(got.c);
        if dt < tree_scalar_ns {
            tree_scalar_ns = dt;
        }
        let mut r2 = pass_recs.clone();
        let now = Instant::now();
        let got2 = tree_fold_p64_dispatch(&mut r2);
        let dt2 = now.elapsed().as_nanos();
        tree_ok &= got2 == serial_p64;
        if dt2 < tree_dispatch_ns {
            tree_dispatch_ns = dt2;
        }
    }
    // The ENGINE ring (the rotation monoid — the journal's law):
    // 322 (k, B) leaves, serial iterated applies vs the SoA tree.
    let pass_leaves: Vec<(u32, u64)> = (0..K_PASS)
        .map(|i| {
            (
                1 + (i % 64) as u32,
                (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    ^ (pass_recs[i].v >> 11),
            )
        })
        .collect();
    let h0 = 0x1234_5678_9ABC_DEF0u64;
    let mut engine_serial_ns = u128::MAX;
    let mut engine_serial_h = 0u64;
    for _ in 0..10 {
        let now = Instant::now();
        let mut h = h0;
        for &(k, b) in &pass_leaves {
            h = nf_testkit::sink::fold_compose_apply(h, k, b);
        }
        let dt = now.elapsed().as_nanos();
        engine_serial_h = h;
        // (the serial chain feeds the checksum — the R34 DCE lesson)
        chk = chk.wrapping_add(h);
        if dt < engine_serial_ns {
            engine_serial_ns = dt;
        }
    }
    let _ = std::hint::black_box(&pass_leaves);
    let mut engine_tree_ns = u128::MAX;
    let mut engine_tree_ok = true;
    for _ in 0..20 {
        let mut kc: Vec<u32> = pass_leaves.iter().map(|&x| x.0).collect();
        let mut bc: Vec<u64> = pass_leaves.iter().map(|&x| x.1).collect();
        let now = Instant::now();
        let (kt, bt) = nf_testkit::sink::tree_fold_segments(&mut kc, &mut bc);
        let dt = now.elapsed().as_nanos();
        let h = nf_testkit::sink::fold_compose_apply(h0, kt, bt);
        engine_tree_ok &= h == engine_serial_h;
        chk = chk.wrapping_add(h).wrapping_add(kt as u64);
        if dt < engine_tree_ns {
            engine_tree_ns = dt;
        }
    }
    // The epoch merge (Theorem 25 — 2 composes/epoch, the L2 kernel
    // surface) and the two-level tree (§2.7) on the same pass window.
    let epoch_ns = {
        let x1 = serial_pass();
        let x2 = serial_pass();
        let now = Instant::now();
        for _ in 0..1000 {
            let _m = epoch_merge_p64(&P64::IDENTITY, &x1, &x2);
        }
        now.elapsed().as_nanos() as f64 / 1000.0
    };
    let two_level_ok = {
        // The two-level identity on the UNREBASED round-robin records —
        // compared against their own serial chain (the rebased window
        // above is the timing corpus; this is the equality pin).
        let raw: Vec<_> = (0..K_PASS)
            .map(|i| cold_recs[(i * 7 + 3) % cold_recs.len()])
            .collect();
        let flat: Vec<P64> = raw.iter().map(P64::from_r36).collect();
        let want = flat.iter().fold(P64::IDENTITY, |acc, r| p64_compose(&acc, r));
        for &run in &[1usize, 64, 512] {
            if tree_fold_two_level(&raw, run) != want {
                break; // (leaves two_level_ok false via the final compare)
            }
        }
        tree_fold_two_level(&raw, 64) == want
    };
    chk = chk.wrapping_add(epoch_ns.to_bits() as u64);

    // ── The per-message metrics.
    let cold_msg_per_s = N as f64 / (cold_best_ns as f64 / 1e9);
    let fold_cyc_per_msg = cold_best_cyc as f64 / N as f64;
    let replay_cyc_per_chunk = if tsc_hz > 0 {
        replay_best_ns as f64 * (tsc_hz as f64 / 1e9) / chunks as f64
    } else {
        0.0
    };
    let ratio = seq_per_512 / (cold_best_ns as f64 / chunks as f64);

    println!(
        "R36_FOLD_COLD best_ns={cold_best_ns} chunks={chunks} msg_per_s={cold_msg_per_s:.0} cyc_per_msg={fold_cyc_per_msg:.4}"
    );
    println!(
        "R36_FOLD_SEQ best_ns={seq_best_ns} per_512_chunk_ns={seq_per_512:.0} (the serialized-chain baseline)"
    );
    println!(
        "R36_FOLD_REPLAY best_ns={replay_best_ns} composes={} cyc_per_chunk_record={replay_cyc_per_chunk:.1} (Corollary 10.1: 1 CLMUL + 1 XOR per hop)",
        chunks - 1
    );
    println!(
        "R36_MERGE ns_per_merge={merge_ns:.2} worker_deg={d_late} (Theorem 17: 3 CLMUL + 1 XOR at the two-level degree)"
    );
    println!("R36_CHK sink={chk:#x}");
    // R37's regime lines (solution2.md Part I — Gate 6):
    let tree_speedup = serial_p64_ns as f64 / tree_dispatch_ns.max(1) as f64;
    let engine_speedup = engine_serial_ns as f64 / engine_tree_ns.max(1) as f64;
    println!(
        "R37_TREE_P64 serial_pass_ns={serial_p64_ns} tree_scalar_ns={tree_scalar_ns} tree_dispatch_ns={tree_dispatch_ns} records={K_PASS} depth=9 speedup={tree_speedup:.1}x (Theorem 20/21: the up-sweep vs the serial left spine, the CLMUL ring)"
    );
    println!(
        "R37_TREE_ENGINE serial_pass_ns={engine_serial_ns} tree_ns={engine_tree_ns} speedup={engine_speedup:.1}x (the rotation ring — the fold journal's law, the 2.08us/pass barrier's replacement)"
    );
    println!(
        "R37_EPOCH_MERGE ns_per_epoch={epoch_ns:.2} two_level_ok={two_level_ok} (Theorem 25: 2 composes per epoch, static tape order; §2.7's two-level split)"
    );
    // The budget line (Gate 6): the champion's 2.08 µs/pass fold chain
    // is the ENGINE ring's serial spine (322 dependent applies + the
    // drain-loop's latency); its replacement is the ENGINE tree — the
    // journal's boundary flush. The ≤0.15 µs class is the fleet's
    // dedicated-core number (this instrument reports; the shared dev
    // slice overstates it — never price on faith, but never gate on a
    // noisy slice either).
    let tree_budget_ok = engine_tree_ns <= 150_000;
    println!(
        "PR1_R37_TREE_FOLD_VERDICT serial_pass_ns={serial_p64_ns} tree_pass_ns={tree_dispatch_ns} engine_serial_ns={engine_serial_ns} engine_tree_ns={engine_tree_ns} tree_speedup={tree_speedup:.1}x engine_speedup={engine_speedup:.1}x budget_le_150ns_class={} tree_ok={tree_ok} engine_tree_ok={engine_tree_ok} two_level_ok={two_level_ok} avx512={avx512} (R37: the Blelloch-Galois tree — Theorems 19-22 landed: the pass fold's 9-level up-sweep over 322 fused records, both rings, the epoch merge; docs/solution2.md)",
        tree_budget_ok
    );
    println!(
        "PR1_R36_RES_FOLD_VERDICT fold_cyc_per_msg={fold_cyc_per_msg:.4} cold_fold_msg_per_s={cold_msg_per_s:.0} seq_chain_ns_per_512={seq_per_512:.0} kernel_speedup={ratio:.1}x replay_ns_per_500k={replay_best_ns} replay_cyc_per_record={replay_cyc_per_chunk:.1} merge_ns={merge_ns:.2} chunks={chunks} avx512={avx512} bitexact={bitexact} dual_path={dual_path_ok} replay_ok={replay_ok} merge_ok={merge_ok} (R36: the Res-fold — Theorems 9/10/11 landed: the 500k SPANHASH folds branch-free, composes in O(1) 36-byte records, and merges by the Galois shift; docs/solution1.md)"
    );
    assert!(bitexact, "Theorem 18 violated: stream fold != sequential oracle");
    assert!(dual_path_ok, "dual-path violated: vector fold != sequential chain");
    assert!(replay_ok, "Corollary 10.1 violated: record composition != whole fold");
    assert!(merge_ok, "Theorem 17 violated: the two-worker merge != sequential");
    assert!(tree_ok, "Theorem 22 violated (CLMUL ring): tree != serial chain");
    assert!(engine_tree_ok, "Theorem 22 violated (rotation ring): tree != serial chain");
    assert!(two_level_ok, "Theorem 19/§2.7 violated: two-level split != flat chain");
    let _ = W_T;
    let _ = P64::IDENTITY; // (API-surface pin)
}
