//! CRC kernel micro-probe: measures the isolated cost of the span CRC
//! kernels over representative span bodies (scalar 8-lane vs vector fold).
//!
//! Run: cargo run -p nf-testkit --release --example crc_probe

use nf_testkit::crcfold::{fold512_available, CrcKernel};
use nf_testkit::sink::span_crc32c_8lane;
use std::time::Instant;

fn main() {
    let sizes = [1360usize, 1380, 680, 340, 44];
    let iters = 200_000u64;
    let kernel = CrcKernel::detect();
    println!(
        "crc_probe kernel={} fold512_available={} (HFT_CRC_KERNEL overrides)",
        kernel.name(),
        fold512_available()
    );
    for &sz in &sizes {
        let mut body = vec![0u8; sz];
        // Deterministic-ish content (avoid all-zero special cases in prefetch logic).
        for (i, b) in body.iter_mut().enumerate() {
            *b = (i * 31 + 7) as u8;
        }
        let mut body2 = vec![0u8; sz];
        for (i, b) in body2.iter_mut().enumerate() {
            *b = (i * 37 + 11) as u8;
        }
        // Warm + verify bit parity right here.
        let a = span_crc32c_8lane(&body);
        let b = unsafe { kernel.eval(&body) };
        assert_eq!(a, b, "kernel parity broken at size {}", sz);

        let mut acc = 0u64;
        for _ in 0..1000 {
            acc ^= span_crc32c_8lane(&body);
        }
        let t0 = Instant::now();
        for _ in 0..iters {
            acc ^= span_crc32c_8lane(&body);
        }
        let dt = t0.elapsed().as_secs_f64();
        let ns_scalar = dt / iters as f64 * 1e9;

        let mut acc2 = 0u64;
        for _ in 0..1000 {
            acc2 ^= unsafe { kernel.eval(&body) };
        }
        let t0 = Instant::now();
        for _ in 0..iters {
            acc2 ^= unsafe { kernel.eval(&body) };
        }
        let dt = t0.elapsed().as_secs_f64();
        let ns_kernel = dt / iters as f64 * 1e9;

        // eval2 (two interleaved spans)
        let mut acc3 = (0u64, 0u64);
        for _ in 0..1000 {
            acc3 = unsafe { kernel.eval2(&body, &body2) };
        }
        let t0 = Instant::now();
        for _ in 0..iters / 2 {
            acc3 = unsafe { kernel.eval2(&body, &body2) };
        }
        let dt = t0.elapsed().as_secs_f64();
        let ns_eval2 = dt / (iters / 2) as f64 * 1e9 / 2.0;

        // Assume ~3.2 GHz for the cycle figure on this box.
        let ghz = 3.2;
        std::hint::black_box((acc, acc2, acc3));
        println!(
            "CRC size={} scalar={:.1}ns kernel={:.1}ns eval2/span={:.1}ns | B/cyc: scalar={:.2} kernel={:.2} eval2={:.2} | speedup kernel={:.2}x eval2={:.2}x",
            sz,
            ns_scalar,
            ns_kernel,
            ns_eval2,
            sz as f64 / (ns_scalar * ghz),
            sz as f64 / (ns_kernel * ghz),
            sz as f64 / (ns_eval2 * ghz),
            ns_scalar / ns_kernel,
            ns_scalar / ns_eval2,
        );
    }
}
