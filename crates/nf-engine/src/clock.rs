//! Clock discipline, invariant TSC detection, Theil-Sen calibration, and serialized marks (doc 11 §2).

#![allow(unused)]

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockCalibration {
    pub has_invariant_tsc: bool,
    pub freq_hz: f64,
    pub freq_mhz: f64,
    pub overhead_cycles: u64,
}

#[inline(always)]
pub fn read_tsc_serialized_start() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_mm_lfence();
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        read_monotonic_raw_ns()
    }
}

#[inline(always)]
pub fn read_tsc_serialized_end() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let mut aux = 0u32;
        let tsc = core::arch::x86_64::__rdtscp(&mut aux);
        core::arch::x86_64::_mm_lfence();
        tsc
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        read_monotonic_raw_ns()
    }
}

#[inline(always)]
pub fn read_monotonic_raw_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

pub fn detect_invariant_tsc() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        unsafe {
            let cpuid = core::arch::x86_64::__cpuid(0x80000007);
            (cpuid.edx & (1 << 8)) != 0
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Theil-Sen median-of-slopes frequency calibration over 256 samples (doc 11 §2).
pub fn calibrate_clock() -> ClockCalibration {
    let has_invariant_tsc = detect_invariant_tsc();

    // 1. Collect 256 sample pairs (tsc, mono_raw) spaced over ~100ms
    const SAMPLES: usize = 256;
    let mut tsc_samples = [0u64; SAMPLES];
    let mut mono_samples = [0u64; SAMPLES];

    for i in 0..SAMPLES {
        tsc_samples[i] = read_tsc_serialized_start();
        mono_samples[i] = read_monotonic_raw_ns();
        // small busy wait
        let target = mono_samples[i] + 400_000; // ~400 µs
        while read_monotonic_raw_ns() < target {}
    }

    // 2. Theil-Sen estimator: median of pairwise slopes
    let mut slopes = Vec::with_capacity(SAMPLES * (SAMPLES - 1) / 2);
    for i in 0..SAMPLES {
        for j in (i + 1)..SAMPLES {
            let dt_ns = mono_samples[j].saturating_sub(mono_samples[i]);
            let dtsc = tsc_samples[j].saturating_sub(tsc_samples[i]);
            if dt_ns > 0 {
                let freq_ghz = (dtsc as f64) / (dt_ns as f64);
                slopes.push(freq_ghz * 1e9);
            }
        }
    }

    slopes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_freq_hz = if !slopes.is_empty() {
        slopes[slopes.len() / 2]
    } else {
        2.5e9
    };
    let freq_mhz = median_freq_hz / 1e6;

    // 3. Mark overhead measurement (10,000 back-to-back pairs)
    let mut overheads = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let t0 = read_tsc_serialized_start();
        let t1 = read_tsc_serialized_end();
        overheads.push(t1.saturating_sub(t0));
    }
    overheads.sort();
    let overhead_cycles = overheads[overheads.len() / 2];

    ClockCalibration {
        has_invariant_tsc,
        freq_hz: median_freq_hz,
        freq_mhz,
        overhead_cycles,
    }
}
