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
use nf_testkit::crcfold::{fold512_available, CrcKernel};
use nf_testkit::sink::span_crc32c_8lane;

/// Working-set size per thread (fits L3 on runner silicon; we are measuring
/// instruction throughput, not DRAM).
const BUF_BYTES: usize = 8 << 20;
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
        bench_1t("fold512_r_pair", c0, mode_fold512_r_pair);
        bench_1t("fold512_eval2", c0, mode_fold512_eval2);
        bench_1t("fold512_pair", c0, mode_fold512_pair);
        bench_1t("fold512_tri", c0, mode_fold512_tri);
        bench_1t("fold512_pclmul_mix", c0, mode_fold512_pclmul_mix);
    }

    // ── multi-core ceilings ───────────────────────────────────────────────
    if let Some((a, b)) = phys_pair {
        bench_2t("scalar8lane", "2cpu_distinct", a, b, mode_scalar);
        if fold512_available() {
            bench_2t("fold512", "2cpu_distinct", a, b, mode_fold512);
            bench_2t("fold512_r", "2cpu_distinct", a, b, mode_fold512_r);
            bench_2t("fold512_tri", "2cpu_distinct", a, b, mode_fold512_tri);
        }
    }
    if let Some((a, b)) = smt_pair {
        bench_2t("scalar8lane", "2cpu_smt", a, b, mode_scalar);
        if fold512_available() {
            bench_2t("fold512", "2cpu_smt", a, b, mode_fold512);
            bench_2t("fold512_r", "2cpu_smt", a, b, mode_fold512_r);
            bench_2t("fold512_tri", "2cpu_smt", a, b, mode_fold512_tri);
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
        acc ^= unsafe { kernel.eval(&buf[off..off + SPAN]) };
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

// ── harness ────────────────────────────────────────────────────────────────

type Mode = fn(&[u8], &mut u64) -> usize;

fn bench_1t(name: &str, cpu: usize, mode: Mode) {
    let mut buf = vec![0u8; BUF_BYTES];
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
