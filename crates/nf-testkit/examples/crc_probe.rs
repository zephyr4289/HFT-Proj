//! CRC kernel micro-probe: measures the isolated cost of the canonical
//! 8-lane CRC32C span kernel (the HYDRA worker's inner loop) over
//! representative span bodies.
//!
//! Run: cargo run -p nf-testkit --release --example crc_probe

use nf_testkit::sink::span_crc32c_8lane;
use std::time::Instant;

fn main() {
    // Representative MtuBound(1400) span body: ~43 msgs * ~31.65 B ≈ 1360 B.
    let sizes = [1360usize, 1380, 680, 340, 44];
    let iters = 200_000u64;
    for &sz in &sizes {
        let mut body = vec![0u8; sz];
        // Deterministic-ish content (avoid all-zero special cases in prefetch logic).
        for (i, b) in body.iter_mut().enumerate() {
            *b = (i * 31 + 7) as u8;
        }
        // Warm.
        let mut acc = 0u64;
        for _ in 0..1000 {
            acc ^= span_crc32c_8lane(&body);
        }
        let t0 = Instant::now();
        for _ in 0..iters {
            acc ^= span_crc32c_8lane(&body);
        }
        let dt = t0.elapsed().as_secs_f64();
        let ns_per = dt / iters as f64 * 1e9;
        // Assume ~3.2 GHz for the cycle figure on this box.
        let ghz = 3.2;
        let cyc = ns_per * ghz;
        let bytes_per_cyc = sz as f64 / cyc;
        std::hint::black_box(acc);
        println!(
            "CRC8LANE size={} ns={:.2} cyc={:.1} B/cyc={:.2}",
            sz, ns_per, cyc, bytes_per_cyc
        );
    }
}
