//! kbench — R8 full-verify kernel-ceiling microbenchmark (diagnostics only).
//!
//! Purpose: measure the ACTUAL CRC-verification throughput ceilings of the
//! CI runner's silicon, per kernel and per topology, so the full-verify
//! fabric design is driven by measured physics instead of folklore:
//!
//! * `scalar8lane` — the canonical 8× `crc32` chain (`span_crc32c_8lane`),
//!   the only option on 128-bit-only silicon (Zen3 7763).
//! * `fold512` — the VPCLMULQDQ mirror-domain fold (AVX-512 + GFNI class).
//! * `pclmul128_raw` — raw PCLMULQDQ XMM independent-op issue rate (is the
//!   128-bit carry-less pipe a SECOND throughput resource on this chip?).
//! * `crc32_pclmul_mix` — 1:1 interleave of `crc32` chains and PCLMULQDQ
//!   chains: if the two share execution ports, combined GB/s stays at the
//!   single-kernel ceiling; if they issue on disjoint ports, the mix beats
//!   both (the Zen3 hybrid-kernel question).
//! * multi-thread placements — distinct physical cores and SMT sibling
//!   pairs — because the fabric's CRC capacity is per-CORE, not per-thread.
//!
//! Every mode's `gb_s` counts NOMINAL VERIFIED BYTES: the instruction mix
//! is sized at the canonical 8 bytes per carry-less instruction, so raw
//! issue-rate probes are directly comparable with the real kernels.
//!
//! This binary is a TOOL: it allocates, prints freely, and is never gated.
//! It runs as a CI telemetry step (scripts/ci.sh) so every run's log carries
//! the machine's measured ceilings next to the fabric's achieved numbers.

// Diagnostic tool, not an engine binary: no measurement windows, no
// invariants — the workspace's Tier-F zero-allocation law does not apply.
#![allow(clippy::all)]
#![allow(warnings)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use nf_testkit::affinity;
use nf_testkit::crcfold::{
    fold512_available, install_spec_slice_vec, span_crc32c_affine_sub,
    span_crc32c_8lane_affine_sub, transpose_arena_slot, CrcKernel,
};
use nf_testkit::sink::span_crc32c_8lane;

/// Working-set size per thread (fits L3 on runner silicon; we are measuring
/// instruction throughput, not DRAM).
const BUF_BYTES: usize = 8 << 20;
/// R17/I-1: the supply row's working set — ~14.3 MB (7x L2, L3-resident on
/// the runner class): each draw's actual L3 streaming ceiling for 1.4 KB
/// bodies, the 2B supply gate's gauge (ROADMAP2 §5.1b; CHECKLIST I-1/D-1).
const SUPPLY_TARGET_BYTES: usize = 14_996_224;
/// Minimum measurement window per sample.
const MIN_MS: u64 = 300;
/// Span quantum: ~21 cache lines, the fabric's real per-span body size.
const SPAN: usize = 1344;

fn main() {
    // Capture before any pinning (the mask-pollution trap).
    let _ = affinity::capture_topology();
    let topo = affinity::cpu_order();
    let n_phys = affinity::physical_core_count();
    let smt = topo.len() > n_phys;
    println!(
        "KBENCH topo cpus={:?} phys={n_phys} smt={smt} fold512={}",
        topo,
        fold512_available()
    );
    if topo.is_empty() {
        eprintln!("KBENCH: no topology; aborting");
        std::process::exit(1);
    }

    // Distinct-physical-core pair and an SMT-sibling pair (if any).
    let mut phys_pair: Option<(usize, usize)> = None;
    let mut smt_pair: Option<(usize, usize)> = None;
    'outer: for (i, &a) in topo.iter().enumerate() {
        for &b in topo.iter().skip(i + 1) {
            if read_siblings(a).contains(&b) {
                if smt_pair.is_none() {
                    smt_pair = Some((a, b));
                }
            } else if phys_pair.is_none() {
                phys_pair = Some((a, b));
            }
            if phys_pair.is_some() && smt_pair.is_some() {
                break 'outer;
            }
        }
    }
    let c0 = topo[0];

    // R23b: install the speculative slicer's AVX-512 table builder (the
    // unsafe-boundary hook from nf-testkit's crcfold; idempotent, one-shot).
    let spec_vec = install_spec_slice_vec();
    println!("KBENCH spec_slice_vec={spec_vec}");

    // ── single-core ceilings ──────────────────────────────────────────────
    bench_1t("scalar8lane", c0, mode_scalar);
    bench_1t("pclmul128_raw", c0, mode_pclmul_raw);
    bench_1t("crc32_pclmul_mix", c0, mode_mix);
    if fold512_available() {
        bench_1t("fold512", c0, mode_fold512);
        bench_1t("fold512_r", c0, mode_fold512_r);
        bench_1t("fold512_rv", c0, mode_fold512_rv);
        bench_1t("fold512_rc", c0, mode_fold512_rc);
        bench_1t("fold512_rd", c0, mode_fold512_rd);
        bench_1t("fold512_ro", c0, mode_fold512_ro);
        bench_1t("fold512_r_pair", c0, mode_fold512_r_pair);
        bench_1t("fold512_eval2", c0, mode_fold512_eval2);
        bench_1t("fold512_pair", c0, mode_fold512_pair);
        bench_1t("fold512_tri", c0, mode_fold512_tri);
        bench_1t("fold512_tri_r", c0, mode_fold512_tri_r);
        bench_1t("fold512_pclmul_mix", c0, mode_fold512_pclmul_mix);
        // R17 Phase I instruments (CHECKLIST I-1; ROADMAP2 §5.1b) + the
        // Route T kill test (CHECKLIST T-1; ROADMAP2 §5.2). All 1t rows on
        // the packed corpus; `fold512_t` vs `fold512_r` on the SAME draw
        // is the Route T decision (>= +8% builds it, < +8% kills it).
        bench_1t("fold512_noend", c0, mode_fold512_noend);
        bench_1t("fold512_pre", c0, mode_fold512_pre);
        bench_1t("fold512_t", c0, mode_fold512_t);
        let supply_bytes = (SUPPLY_TARGET_BYTES / SPAN) * SPAN;
        bench_1t_sz("fold512_supply", c0, mode_fold512_r, supply_bytes);
    }

    // ── R23b: the O(1) affine span projections + the speculative slicer ──
    // (rows run on EVERY class — the kernels carry their own scalar
    // fallbacks; the GB/s column counts NOMINAL VERIFIED BYTES, one SPAN
    // per projection, the same unit as the fold rows, so the ceilings
    // are directly comparable. msgs_s = the ITCH-average 32 B/msg
    // convention: SPAN/32 verified messages per projection — the
    // zero-re-read frontier number.)
    bench_affine("affine_sub_1t", c0, mode_affine_sub_1t);
    bench_affine("affine_sub_8lane", c0, mode_affine_sub_8lane);
    bench_spec_slice(c0, "spec_slice_512", false);
    // The attribution twin: the same corpus driven through the EXPLICIT
    // vector composition (builder + register-table walk) — the per-draw
    // re-arm evidence (R22.1's precedent: the default follows the MEASURED
    // verdict; this row keeps the evaluation one flag away).
    if nf_protocol::moldudp64::spec_vec_installed() {
        bench_spec_slice(c0, "spec_slice_512_vec", true);
    }

    // ── multi-core ceilings ───────────────────────────────────────────────
    if let Some((a, b)) = phys_pair {
        bench_2t("scalar8lane", "2cpu_distinct", a, b, mode_scalar);
        if fold512_available() {
            bench_2t("fold512", "2cpu_distinct", a, b, mode_fold512);
            bench_2t("fold512_r", "2cpu_distinct", a, b, mode_fold512_r);
            bench_2t("fold512_rv", "2cpu_distinct", a, b, mode_fold512_rv);
            bench_2t("fold512_rc", "2cpu_distinct", a, b, mode_fold512_rc);
            bench_2t("fold512_rd", "2cpu_distinct", a, b, mode_fold512_rd);
            bench_2t("fold512_ro", "2cpu_distinct", a, b, mode_fold512_ro);
            bench_2t("fold512_tri", "2cpu_distinct", a, b, mode_fold512_tri);
            bench_2t("fold512_tri_r", "2cpu_distinct", a, b, mode_fold512_tri_r);
        }
    }
    if let Some((a, b)) = smt_pair {
        bench_2t("scalar8lane", "2cpu_smt", a, b, mode_scalar);
        if fold512_available() {
            bench_2t("fold512", "2cpu_smt", a, b, mode_fold512);
            bench_2t("fold512_r", "2cpu_smt", a, b, mode_fold512_r);
            bench_2t("fold512_rv", "2cpu_smt", a, b, mode_fold512_rv);
            bench_2t("fold512_rc", "2cpu_smt", a, b, mode_fold512_rc);
            bench_2t("fold512_rd", "2cpu_smt", a, b, mode_fold512_rd);
            bench_2t("fold512_ro", "2cpu_smt", a, b, mode_fold512_ro);
            bench_2t("fold512_tri", "2cpu_smt", a, b, mode_fold512_tri);
            bench_2t("fold512_tri_r", "2cpu_smt", a, b, mode_fold512_tri_r);
        }
    }
    println!("KBENCH done");
}

fn read_siblings(cpu: usize) -> Vec<usize> {
    std::fs::read_to_string(format!(
        "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
    ))
    .ok()
    .map(|s| parse_cpu_list(&s))
    .unwrap_or_else(|| vec![cpu])
}

fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.trim().split(',') {
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                out.extend(a..=b);
            }
        } else if let Ok(v) = part.parse::<usize>() {
            out.push(v);
        }
    }
    out
}

/// Deterministic non-zero fill (defeats any zero-page shortcuts).
fn fill(buf: &mut [u8], seed: u64) {
    let mut x = seed;
    for chunk in buf.chunks_mut(8) {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
}

// ── kernels under test ─────────────────────────────────────────────────────

/// The canonical scalar kernel over span quanta; returns verified bytes.
#[inline(always)]
fn mode_scalar(buf: &[u8], sink: &mut u64) -> usize {
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + SPAN <= buf.len() {
        acc ^= span_crc32c_8lane(&buf[off..off + SPAN]);
        off += SPAN;
    }
    *sink = acc;
    off
}

#[cfg(target_arch = "x86_64")]
fn mode_pclmul_raw(buf: &[u8], sink: &mut u64) -> usize {
    // SAFETY: the x86_64 baseline for this crate includes sse2; pclmulqdq
    // is guarded per-call below via runtime detection in main()'s caller
    // chain (every x86_64 CPU in the runner pool has PCLMULQDQ — it is a
    // 2010-era Westmere feature; the probe aborts otherwise).
    if !std::arch::is_x86_feature_detected!("pclmulqdq") {
        *sink = 0;
        return 0;
    }
    unsafe { pclmul_raw_inner(buf, sink) }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2,sse4.1,pclmulqdq")]
unsafe fn pclmul_raw_inner(buf: &[u8], sink: &mut u64) -> usize {
    use std::arch::x86_64::*;
    // Independent PCLMULQDQ chains over register data + one stream load per
    // chain per line group: 512 clmuls per 4096 advanced bytes = the fold's
    // canonical 8-bytes-per-clmul instruction mix, measuring pure carry-less
    // issue throughput on the 128-bit pipe.
    let mut off = 0usize;
    let mut x = [_mm_set_epi64x(0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321u64 as i64); 8];
    let k = _mm_set_epi64x(
        0x9e37_79b9_7f4a_7c15u64 as i64,
        0xc2b2_ae3d_27d4_eb4fu64 as i64,
    );
    while off + 4096 <= buf.len() {
        unsafe {
            for j in 0..256 {
                let d = (buf.as_ptr() as *const __m128i).add(off / 16 + j);
                let v = _mm_loadu_si128(d);
                let lane = j & 7;
                x[lane] = _mm_xor_si128(_mm_clmulepi64_si128(x[lane], k, 0x00), v);
                x[lane ^ 1] = _mm_xor_si128(_mm_clmulepi64_si128(x[lane ^ 1], k, 0x01), v);
            }
        }
        off += 4096;
    }
    unsafe {
        let mut a = 0u64;
        for v in x {
            a ^= std::mem::transmute::<__m128i, [u64; 2]>(v)[0];
        }
        *sink = a;
    }
    off
}

#[cfg(not(target_arch = "x86_64"))]
fn mode_pclmul_raw(buf: &[u8], sink: &mut u64) -> usize {
    let _ = buf;
    *sink = 0;
    0
}

#[cfg(target_arch = "x86_64")]
fn mode_mix(buf: &[u8], sink: &mut u64) -> usize {
    if !std::arch::is_x86_feature_detected!("pclmulqdq")
        || !std::arch::is_x86_feature_detected!("sse4.2")
    {
        *sink = 0;
        return 0;
    }
    unsafe { mix_inner(buf, sink) }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2,sse4.2,pclmulqdq")]
unsafe fn mix_inner(buf: &[u8], sink: &mut u64) -> usize {
    use std::arch::x86_64::*;
    // 1:1 crc32 : pclmulqdq interleave — the port-sharing probe. Per 64B
    // line: 4 crc32 (32 verified bytes) + 4 clmul (32 nominal bytes) = 64
    // nominal bytes per 8 instructions: identical nominal mix to both pure
    // modes. If crc32 and PCLMULQDQ issue on disjoint ports, this beats both
    // pure ceilings; if they contend, it lands between them.
    let mut off = 0usize;
    let mut c = [0u64; 4];
    let mut x = [_mm_set_epi64x(0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321u64 as i64); 4];
    let k = _mm_set_epi64x(
        0x9e37_79b9_7f4a_7c15u64 as i64,
        0xc2b2_ae3d_27d4_eb4fu64 as i64,
    );
    while off + 2048 <= buf.len() {
        unsafe {
            for j in 0..32 {
                let base = buf.as_ptr().add(off + j * 64);
                let p = base as *const u64;
                for lane in 0..4 {
                    c[lane] = _mm_crc32_u64(c[lane], p.add(lane).read_unaligned());
                }
                let d = base as *const __m128i;
                for lane in 0..4 {
                    let v = _mm_loadu_si128(d.add(lane));
                    x[lane] = _mm_xor_si128(_mm_clmulepi64_si128(x[lane], k, 0x01), v);
                }
            }
        }
        off += 2048;
    }
    let mut a = c[0] ^ c[1] ^ c[2] ^ c[3];
    unsafe {
        for v in x {
            a ^= std::mem::transmute::<__m128i, [u64; 2]>(v)[1];
        }
    }
    *sink = a;
    off
}

#[cfg(not(target_arch = "x86_64"))]
fn mode_mix(buf: &[u8], sink: &mut u64) -> usize {
    let _ = buf;
    *sink = 0;
    0
}

fn mode_fold512(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Fold512;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        acc ^= unsafe { kernel.eval(&buf[off..off + SPAN]) };
        off += SPAN;
    }
    *sink = acc;
    off
}

/// R13: the natural-domain (reflected) fold on the same packed-SPAN
/// corpus — the kernel-level attribution of the p5 fix (fold512_r vs
/// fold512 on the SAME runner isolates the removed GFNI/pshufb pair from
/// all fabric effects).
fn mode_fold512_r(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Reflect;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        // R14/R15: forced vend ON, vtail OFF — the R14 ending shape (the
        // controlled twin of mode_fold512_rc (crc-chain) and
        // mode_fold512_rv (vend + the R15 vectorized tail)); the triple
        // stays comparable on every silicon class regardless of the
        // CPUID-conditional defaults (the fabric arms carry the default's
        // per-class behavior).
        acc ^= unsafe { kernel.eval_rpath3(&buf[off..off + SPAN], true, false) };
        off += SPAN;
    }
    *sink = acc;
    off
}

/// R15: the reflect kernel with vend + the VECTORIZED TAIL forced ON —
/// the new-production attribution row (vs `fold512_r` (the R14 shape)
/// and `fold512_rc` (the R13 crc-chain) on the same draw). The GB/s
/// delta IS the tail vectorization's diet, per runner class.
fn mode_fold512_rv(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Reflect;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        acc ^= unsafe { kernel.eval_rpath3(&buf[off..off + SPAN], true, true) };
        off += SPAN;
    }
    *sink = acc;
    off
}

/// R16: the reflect kernel with the DUAL-STREAM FOLD (dfold) forced ON —
/// the T=2 block-parity shape (forced vend + vtail-all-r, the class
/// endings). fold512_rd vs fold512_rv on the same draw IS the chain-depth
/// effect at IDENTICAL census (4 VPCLMULQDQ + 2 VPUNPCK + 2 VPTERNLOG per
/// 128 B; 4 chains instead of 2): if the kernel is latency-bound the row
/// jumps toward the p5 floor; if supply-bound it stays put — the fleet
/// decides per draw (the fold512_tri precedent).
fn mode_fold512_rd(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Reflect;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        acc ^= unsafe { kernel.eval_rpath4(&buf[off..off + SPAN], true, true, true) };
        off += SPAN;
    }
    *sink = acc;
    off
}

/// R21: the reflect kernel with the OCTO-STREAM FOLD (ofold) forced ON —
/// the T=8 block-parity shape (forced vend + vtail-all-r, the class
/// endings; scripts/r21_ofold_derive.py). fold512_ro vs fold512_rv on the
/// same draw IS the chain-depth effect at T=8 (16 independent clmul
/// chains vs 2; the directive Task 1's 8-accumulator shape): if the
/// kernel is latency-bound on wide OoO silicon the row jumps toward the
/// p5 floor; the seed+merge overhead (14 extra clmuls per span) prices
/// against it at small wp — the fleet decides per draw (the fold512_rd
/// precedent).
fn mode_fold512_ro(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Reflect;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        acc ^= unsafe { kernel.eval_rpath5(&buf[off..off + SPAN], true, true, false, true) };
        off += SPAN;
    }
    *sink = acc;
    off
}

/// R13: the production pair path on the natural-domain kernel.
fn mode_fold512_r_pair(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Reflect;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + 2 * SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        let (a, b) =
            unsafe { kernel.eval_pair(&buf[off..off + SPAN], &buf[off + SPAN..off + 2 * SPAN]) };
        acc ^= a ^ b;
        off += 2 * SPAN;
    }
    *sink = acc;
    off
}

/// R14: the reflect kernel with the R13 crc-chain ENDING forced ON — the
/// attribution twin of `mode_fold512_r` (which runs the HFT_CRC_VEND
/// default, i.e. the vector Barrett ending). Same corpus, same value (the
/// sweeps pin equality); the GB/s delta IS the ending diet, per runner
/// class, in one process on one draw.
fn mode_fold512_rc(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Reflect;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        acc ^= unsafe { kernel.eval_rpath3(&buf[off..off + SPAN], false, false) };
        off += SPAN;
    }
    *sink = acc;
    off
}

fn mode_fold512_eval2(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Fold512;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + 2 * SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        let (a, b) =
            unsafe { kernel.eval2(&buf[off..off + SPAN], &buf[off + SPAN..off + 2 * SPAN]) };
        acc ^= a ^ b;
        off += 2 * SPAN;
    }
    *sink = acc;
    off
}

/// R10: the sequential-load pair — the pipelined-tail schedule on the same
/// packed-SPAN corpus as mode_fold512. fold512_pair vs fold512 is the
/// kernel-level attribution of the deferred-ending mechanism (the fabric
/// effect additionally carries the real layout + ring mechanics).
fn mode_fold512_pair(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Fold512;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + 2 * SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        let (a, b) =
            unsafe { kernel.eval_pair(&buf[off..off + SPAN], &buf[off + SPAN..off + 2 * SPAN]) };
        acc ^= a ^ b;
        off += 2 * SPAN;
    }
    *sink = acc;
    off
}

/// R11: the tri-stream fold on the same packed-SPAN corpus — the kernel-
/// level attribution of the three-way interleave (chain slack vs issue
/// cost). fold512_tri vs fold512 on the SAME runner is the clean
/// experiment: if the kernel is dependency-bound the row jumps; if it is
/// port-issue-bound it sits at parity (and the merge's fixed ~8 clmuls
/// per span price it slightly below on short spans).
fn mode_fold512_tri(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Fold512;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        acc ^= unsafe { kernel.eval_tri(&buf[off..off + SPAN]) };
        off += SPAN;
    }
    *sink = acc;
    off
}

/// R22: the natural-domain TRI-STREAM fold (the class-exact K^3 shape).
/// R22.1: the fleet verdict (run #455, 16 draws) rejected the class-exact
/// shape as a worker default — the forced composed-field endings cost
/// 12–25% on Zen 5 (tri_r/fold512_rc = 0.67–0.86 on the AMD draws). This
/// row STAYS as the per-draw attribution instrument for that verdict (via
/// `eval_tri_r`, split from `eval_tri`); the worker-loop armed soak (11l)
/// prices the VALUE-EXACT mirror tri instead.
fn mode_fold512_tri_r(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Reflect;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        acc ^= unsafe { kernel.eval_tri_r(&buf[off..off + SPAN]) };
        off += SPAN;
    }
    *sink = acc;
    off
}

#[cfg(target_arch = "x86_64")]
fn mode_fold512_pclmul_mix(buf: &[u8], sink: &mut u64) -> usize {
    // SAFETY: guarded identically to fold512_available() by the caller.
    unsafe { fold512_pclmul_mix_inner(buf, sink) }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2,sse4.2,pclmulqdq,avx512f,avx512bw,vpclmulqdq,gfni")]
unsafe fn fold512_pclmul_mix_inner(buf: &[u8], sink: &mut u64) -> usize {
    use std::arch::x86_64::*;
    // fold512 on the 512-bit pipes + ~1 XMM clmul per 64B of span on the
    // side: if gb_s stays at the pure fold512 ceiling, the 128-bit unit was
    // free capacity on this silicon.
    let kernel = CrcKernel::Fold512;
    let mut off = 0usize;
    let mut acc = 0u64;
    let mut x = _mm_set_epi64x(0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321u64 as i64);
    let k = _mm_set_epi64x(
        0x9e37_79b9_7f4a_7c15u64 as i64,
        0xc2b2_ae3d_27d4_eb4fu64 as i64,
    );
    while off + SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        acc ^= unsafe { kernel.eval(&buf[off..off + SPAN]) };
        unsafe {
            for _ in 0..21 {
                x = _mm_xor_si128(_mm_clmulepi64_si128(x, k, 0x00), k);
            }
        }
        off += SPAN;
    }
    unsafe {
        *sink = acc ^ std::mem::transmute::<__m128i, [u64; 2]>(x)[0];
    }
    off
}

#[cfg(not(target_arch = "x86_64"))]
fn mode_fold512_pclmul_mix(buf: &[u8], sink: &mut u64) -> usize {
    let _ = buf;
    *sink = 0;
    0
}

// ── R17: Phase I instruments + the Route T kill test ─────────────────────

/// R17/I-1: the fold-loop-only floor row — the ending stack stubbed to a
/// state sum (`eval_rpath_noend`). fold512_r minus this row = the ending +
/// lane-0-continuation diet, per draw (ROADMAP2 §5.1b). The sink is a
/// STATE SUM, not a CRC — attribution telemetry only.
fn mode_fold512_noend(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Reflect;
    let mut off = 0usize;
    let mut acc = 0u64;
    while off + SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available().
        acc ^= unsafe { kernel.eval_rpath_noend(&buf[off..off + SPAN]) };
        off += SPAN;
    }
    *sink = acc;
    off
}

/// R17/I-1: the spray twin — the worker's ahead-of-cursor prefetch shape
/// (hydra.rs: next span's lines, T0) over the same packed corpus. The
/// roadmap's `fold512_nopre` resolved as THIS PAIR: the kbench baseline
/// never carried a spray to turn off, so fold512_r (unsprayed) vs
/// fold512_pre (sprayed) prices the spray's kernel-level cost.
#[cfg(target_arch = "x86_64")]
fn mode_fold512_pre(buf: &[u8], sink: &mut u64) -> usize {
    use std::arch::x86_64::_mm_prefetch;
    use std::arch::x86_64::_MM_HINT_T0;
    let kernel = CrcKernel::Reflect;
    let mut off = 0usize;
    let mut acc = 0u64;
    let lines = SPAN / 64;
    while off + SPAN <= buf.len() {
        // Spray the NEXT span's lines (wraps to span 0 at the tail — the
        // corpus is visited repeatedly, same as the worker's blob).
        let nxt = if off + 2 * SPAN <= buf.len() {
            off + SPAN
        } else {
            0
        };
        for l in 0..lines {
            unsafe {
                _mm_prefetch(buf.as_ptr().add(nxt + 64 * l) as *const i8, _MM_HINT_T0)
            };
        }
        // SAFETY: main() only dispatches here when fold512_available().
        acc ^= unsafe { kernel.eval_rpath3(&buf[off..off + SPAN], true, false) };
        off += SPAN;
    }
    *sink = acc;
    off
}

#[cfg(not(target_arch = "x86_64"))]
fn mode_fold512_pre(buf: &[u8], sink: &mut u64) -> usize {
    let _ = buf;
    *sink = 0;
    0
}

/// R17/T-1: the TRANSPOSED-arena kill-test row (`fold512_t`) — Route T,
/// ROADMAP2 §5.2. The arena is built ONCE, untimed (the deterministic
/// packed corpus — same fill seed on every bench_1t call), then each span
/// folds through the no-unpck loop + the UNCHANGED ending stack (tail
/// reads the ORIGINAL wire corpus). The sink MUST equal fold512_r's sink
/// on the same corpus — the structural bit-exactness proof prints on
/// every run. Decision rule: fold512_t vs fold512_r at 1t on a healthy
/// draw — >= +8% builds Route T; < +8% kills it (the kernel program ends
/// with a measurement).
fn mode_fold512_t(buf: &[u8], sink: &mut u64) -> usize {
    static ARENA: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    // Full 128 B units per span (the trailing partial unit stays on the
    // wire corpus — the ending reads it there).
    const ARENA_SPAN: usize = 128 * (SPAN / 128);
    let arena = ARENA.get_or_init(|| {
        let n = buf.len() / SPAN;
        let mut a = vec![0u8; n * ARENA_SPAN];
        for i in 0..n {
            transpose_arena_slot(
                &buf[i * SPAN..i * SPAN + SPAN],
                &mut a[i * ARENA_SPAN..(i + 1) * ARENA_SPAN],
            );
        }
        a
    });
    debug_assert_eq!(buf.len() / SPAN * ARENA_SPAN, arena.len());
    let kernel = CrcKernel::Reflect;
    let mut off = 0usize;
    let mut acc = 0u64;
    let mut i = 0usize;
    while off + SPAN <= buf.len() {
        // SAFETY: main() only dispatches here when fold512_available();
        // the slot holds this span's transposed image (built above).
        acc ^= unsafe {
            kernel.eval_rpath_t(
                &buf[off..off + SPAN],
                arena[i * ARENA_SPAN..].as_ptr(),
                true,
                false,
            )
        };
        off += SPAN;
        i += 1;
    }
    *sink = acc;
    off
}

// ── R23b: the O(1) affine span rows + the speculative slicer row ──────────

/// One qword of CRC32C advance: the hardware chain on x86_64 (SSE4.2 —
/// the standing runner contract), the portable bitwise scan elsewhere
/// (the corpus builder is untimed; only the projections are measured).
#[cfg(target_arch = "x86_64")]
fn crc32c_qword(c: u32, w: u64) -> u32 {
    // SAFETY: SSE4.2 is baseline on every x86_64 runner (kbench's own
    // probe aborts otherwise).
    unsafe { std::arch::x86_64::_mm_crc32_u64(c as u64, w) as u32 }
}

#[cfg(not(target_arch = "x86_64"))]
fn crc32c_qword(mut c: u32, w: u64) -> u32 {
    for b in w.to_le_bytes() {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x82F6_3B78
            } else {
                c >> 1
            };
        }
    }
    c
}

/// The ingest-snapshot corpora for the affine rows, built ONCE untimed
/// (the snapshot PRODUCTION is the ingest core's job — priced by the
/// fold rows; these rows isolate the O(1) PROJECTION kernels): the
/// single-register (prefix, cumulative) pairs and the 8-lane snapshots
/// at every SPAN boundary, plus in-bench bit-exact parity pins (span 1
/// projected through both kernels vs the direct scans).
struct AffineCorpus {
    pairs: Vec<(u32, u32)>,
    lanes: Vec<([u32; 8], [u32; 8])>,
}

fn affine_corpus(buf: &[u8]) -> AffineCorpus {
    let n_spans = buf.len() / SPAN;
    let mut pairs = Vec::with_capacity(n_spans);
    let mut lanes = Vec::with_capacity(n_spans);
    let mut reg = 0u32;
    let mut ln = [0u32; 8];
    for s in 0..n_spans {
        pairs.push((reg, reg)); // prefix snapshot (filled below)
        lanes.push((ln, ln));
        let base = s * SPAN;
        for q in 0..(SPAN / 8) {
            let w = u64::from_le_bytes(
                buf[base + 8 * q..base + 8 * q + 8].try_into().unwrap(),
            );
            reg = crc32c_qword(reg, w);
        }
        for b in 0..(SPAN / 64) {
            let blk = base + 64 * b;
            for k in 0..8usize {
                let w = u64::from_le_bytes(
                    buf[blk + 8 * k..blk + 8 * k + 8].try_into().unwrap(),
                );
                ln[k] = crc32c_qword(ln[k], w);
            }
        }
        pairs[s].1 = reg; // cumulative snapshot
        lanes[s].1 = ln;
    }
    // In-bench bit-exact parity pins (span 1 — a non-trivial prefix):
    // the O(1) projections MUST equal the direct scans of the span body.
    {
        let (p1, c1) = pairs[1];
        let mut direct = 0u32;
        for q in 0..(SPAN / 8) {
            let w = u64::from_le_bytes(
                buf[SPAN + 8 * q..SPAN + 8 * q + 8].try_into().unwrap(),
            );
            direct = crc32c_qword(direct, w);
        }
        // SAFETY: width law — raw registers and C(SPAN) ≤ 32 bits.
        let proj = unsafe { span_crc32c_affine_sub(c1, p1, SPAN) };
        assert_eq!(proj, direct, "affine_sub_1t in-bench parity pin");
    }
    {
        let (p1, c1) = (lanes[1].0, lanes[1].1);
        let direct = span_crc32c_8lane(&buf[SPAN..2 * SPAN]);
        let proj = span_crc32c_8lane_affine_sub(&c1, &p1, SPAN);
        assert_eq!(proj, direct, "affine_sub_8lane in-bench parity pin");
    }
    AffineCorpus { pairs, lanes }
}

/// R23b Task 1 (scalar row): the O(1) single-register projection over
/// the packed-SPAN corpus — the C(L) composition + the projection
/// multiply per span, zero payload reads. vs `scalar8lane` on the same
/// corpus: the re-read ceiling vs the O(1) frontier.
fn mode_affine_sub_1t(buf: &[u8], sink: &mut u64) -> usize {
    static CORPUS: std::sync::OnceLock<AffineCorpus> = std::sync::OnceLock::new();
    let corpus = CORPUS.get_or_init(|| affine_corpus(buf));
    let mut acc = 0u64;
    for &(p, c) in &corpus.pairs {
        // SAFETY: width law — raw registers and C(SPAN) ≤ 32 bits.
        acc ^= unsafe { span_crc32c_affine_sub(c, p, SPAN) } as u64;
    }
    *sink = acc;
    corpus.pairs.len() * SPAN
}

/// R23b Task 1 (vector row): the 8-lane O(1) projection + the FNV-1a-64
/// golden combine over the packed-SPAN corpus — the EXACT
/// span_crc32c_8lane value per span without reading a span byte. On
/// AVX-512 class: 2 × VPCLMULQDQ projection + 2 × plain-Barrett
/// reduction + the combine; scalar fallback elsewhere (bit-identical).
fn mode_affine_sub_8lane(buf: &[u8], sink: &mut u64) -> usize {
    static CORPUS: std::sync::OnceLock<AffineCorpus> = std::sync::OnceLock::new();
    let corpus = CORPUS.get_or_init(|| affine_corpus(buf));
    let mut acc = 0u64;
    for (p, c) in &corpus.lanes {
        acc ^= span_crc32c_8lane_affine_sub(c, p, SPAN);
    }
    *sink = acc;
    corpus.lanes.len() * SPAN
}

/// The single-core printer for the affine rows: GB/s in NOMINAL VERIFIED
/// BYTES (one SPAN per projection — the fold rows' unit) plus the
/// per-span latency and the msgs/s frontier (the ITCH-average 32 B/msg
/// convention: SPAN/32 verified messages per projection).
fn bench_affine(name: &str, cpu: usize, mode: Mode) {
    let mut buf = vec![0u8; BUF_BYTES];
    fill(&mut buf, 0x243f_6a88_85a3_08d3);
    let pinned = affinity::pin_current_to(cpu);
    // Warmup: page faults, caches, and the corpus build + parity pins.
    let mut sink = 0u64;
    mode(&buf, &mut sink);
    let mut bytes = 0usize;
    let mut iters = 0u32;
    let t0 = Instant::now();
    while t0.elapsed().as_millis() < MIN_MS as u128 {
        bytes += mode(&buf, &mut sink);
        iters += 1;
    }
    let dt = t0.elapsed().as_secs_f64();
    let gb_s = bytes as f64 / dt / 1e9;
    let spans = (bytes / SPAN) as f64;
    let ns_per_span = dt * 1e9 / spans;
    let msgs_s = gb_s * 1e9 / 32.0;
    println!(
        "KBENCH mode={name} threads=1 cpu={cpu} pinned={pinned} gb_s={gb_s:.2} ns_per_span={ns_per_span:.2} msgs_s={msgs_s:.0} bytes={bytes} iters={iters} sink={sink:#x}"
    );
}

/// R23b Task 2: the speculative 512-bit ingest slicer rows. The corpus is
/// a VALID message-block stream (ITCH-like lens 8..=32 derived from the
/// deterministic fill); the hot loop is the INGEST SHAPE — the direct
/// 64-byte window pipeline (the chunk primitive per window, the batch
/// fold anti-DCE), not the per-item iterator. Every pass pins the count.
/// `force_vec` drives the EXPLICIT vector composition (builder +
/// register-table walk) for the attribution twin row.
fn bench_spec_slice(c0: usize, name: &str, force_vec: bool) {
    static CORPUS: std::sync::OnceLock<(Vec<u8>, u64)> = std::sync::OnceLock::new();
    let mut buf = vec![0u8; BUF_BYTES];
    fill(&mut buf, 0x243f_6a88_85a3_08d3);
    let (corpus, expect_msgs) = CORPUS.get_or_init(|| {
        let mut out: Vec<u8> = Vec::with_capacity(BUF_BYTES);
        let mut msgs = 0u64;
        let mut i = 0usize;
        while i + 32 <= buf.len() {
            let l = 8 + (buf[i] % 25) as usize; // ITCH-like 8..=32
            if out.len() + 2 + l > BUF_BYTES {
                break;
            }
            out.extend_from_slice(&(l as u16).to_be_bytes());
            out.extend_from_slice(&buf[i..i + l]);
            i += l;
            msgs += 1;
        }
        (out, msgs)
    });
    let pinned = affinity::pin_current_to(c0);
    let build = if force_vec {
        nf_protocol::moldudp64::spec_vec_builder()
    } else {
        None
    };
    // The ingest-shaped driver: windows in, descriptors out, lens folded.
    let mut batch = [nf_protocol::moldudp64::SpecMsg { off: 0, len: 0 }; 16];
    let mut drive = |sink: &mut u64| -> u64 {
        let mut pos = 0usize;
        let mut msgs = 0u64;
        let mut acc = *sink;
        let len = corpus.len();
        let mut e_tab = [0u16; 32];
        let mut o_tab = [0u16; 32];
        while pos < len {
            let rem = len - pos;
            let wlen = rem.min(64);
            let (n, pf) = if let Some(build) = build {
                build(&corpus[pos..pos + wlen], &mut e_tab, &mut o_tab);
                nf_protocol::moldudp64::spec_slice_walk(
                    &e_tab,
                    &o_tab,
                    wlen,
                    0,
                    rem,
                    &mut batch,
                )
            } else {
                nf_protocol::moldudp64::spec_slice_512(
                    &corpus[pos..pos + wlen],
                    0,
                    rem,
                    &mut batch,
                )
            };
            msgs += n as u64;
            for m in batch[..n].iter() {
                acc = acc.rotate_left(7) ^ m.len as u64;
            }
            pos += pf;
            if pf == 0 {
                break;
            }
        }
        *sink = acc;
        msgs
    };
    // Warmup + the every-pass count parity pin (the count must be exact).
    let mut sink = 0u64;
    let got = drive(&mut sink);
    assert_eq!(got, *expect_msgs, "{name} count parity");
    let mut bytes = 0usize;
    let mut msgs_total = 0u64;
    let mut iters = 0u32;
    let t0 = Instant::now();
    while t0.elapsed().as_millis() < MIN_MS as u128 {
        let m = drive(&mut sink);
        assert_eq!(m, *expect_msgs, "{name} count parity");
        msgs_total += m;
        bytes += corpus.len();
        iters += 1;
    }
    let dt = t0.elapsed().as_secs_f64();
    let gb_s = bytes as f64 / dt / 1e9;
    let msgs_s = msgs_total as f64 / dt;
    let ns_per_msg = dt * 1e9 / msgs_total as f64;
    println!(
        "KBENCH mode={name} threads=1 cpu={c0} pinned={pinned} gb_s={gb_s:.2} msgs_s={msgs_s:.0} ns_per_msg={ns_per_msg:.3} msgs={msgs_total} iters={iters} sink={sink:#x}"
    );
}

// ── harness ────────────────────────────────────────────────────────────────

type Mode = fn(&[u8], &mut u64) -> usize;

fn bench_1t(name: &str, cpu: usize, mode: Mode) {
    bench_1t_sz(name, cpu, mode, BUF_BYTES);
}

/// R17/I-1: the size-parameterized single-core harness (the supply row's
/// ~14.3 MB working set; every other row keeps the 8 MB default).
fn bench_1t_sz(name: &str, cpu: usize, mode: Mode, buf_bytes: usize) {
    let mut buf = vec![0u8; buf_bytes];
    fill(&mut buf, 0x243f_6a88_85a3_08d3);
    let pinned = affinity::pin_current_to(cpu);
    // Warmup pass (page faults, caches, branch predictors).
    let mut sink = 0u64;
    mode(&buf, &mut sink);
    let mut bytes = 0usize;
    let t0 = Instant::now();
    let mut iters = 0u32;
    while t0.elapsed().as_millis() < MIN_MS as u128 {
        bytes += mode(&buf, &mut sink);
        iters += 1;
    }
    let dt = t0.elapsed().as_secs_f64();
    let gb_s = bytes as f64 / dt / 1e9;
    println!(
        "KBENCH mode={name} threads=1 cpu={cpu} pinned={pinned} gb_s={gb_s:.2} bytes={bytes} iters={iters} sink={sink:#x}"
    );
}

fn bench_2t(name: &str, placement: &str, a: usize, b: usize, mode: Mode) {
    struct Res {
        bytes: usize,
        dt: f64,
        pinned: bool,
        sink: u64,
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for &cpu in [a, b].iter() {
        let stop = stop.clone();
        handles.push(std::thread::spawn(move || {
            let mut buf = vec![0u8; BUF_BYTES];
            fill(&mut buf, 0x243f_6a88_85a3_08d3 ^ (cpu as u64 + 1));
            let pinned = affinity::pin_current_to(cpu);
            let mut sink = 0u64;
            mode(&buf, &mut sink); // warmup
            let mut bytes = 0usize;
            let t0 = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                bytes += mode(&buf, &mut sink);
                if t0.elapsed().as_millis() > 20 * MIN_MS as u128 {
                    break; // safety valve
                }
            }
            Res {
                bytes,
                dt: t0.elapsed().as_secs_f64(),
                pinned,
                sink,
            }
        }));
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    std::thread::sleep(std::time::Duration::from_millis(MIN_MS));
    stop.store(true, Ordering::Relaxed);
    let mut total = 0usize;
    let mut dts = Vec::new();
    let mut sinks = 0u64;
    let mut all_pinned = true;
    for h in handles {
        let r = h.join().expect("kbench worker panicked");
        total += r.bytes;
        dts.push(r.dt);
        sinks ^= r.sink;
        all_pinned &= r.pinned;
    }
    // Each thread self-times its own window; the aggregate uses the mean.
    let dt = dts.iter().sum::<f64>() / dts.len() as f64;
    let gb_s = total as f64 / dt / 1e9;
    println!(
        "KBENCH mode={name} threads=2 placement={placement} cpus={a},{b} pinned={all_pinned} gb_s={gb_s:.2} bytes={total} sink={sinks:#x}"
    );
}
