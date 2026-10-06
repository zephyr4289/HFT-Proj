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
use nf_testkit::crcfold::{fold512_available, transpose_arena_slot, CrcKernel};
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

/// R22: the natural-domain TRI-STREAM fold (the worker drain's DEFAULT
/// verification shape — the directive's fold512_tri wiring). fold512_tri_r
/// vs fold512_r on the SAME draw prices the full armed shape (the class-
/// exact K^3 interleave + the forced vend+vtail-all-r endings) against the
/// sequential natural kernel with its silicon-default endings — the exact
/// delta the 11b sustained verdict converts. The mirror-domain tri row
/// (`fold512_tri`) stays for the cross-domain attribution ledger.
fn mode_fold512_tri_r(buf: &[u8], sink: &mut u64) -> usize {
    let kernel = CrcKernel::Reflect;
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
