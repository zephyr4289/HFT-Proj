//! Window arena layout, staging, drain, and Clear-on-Advance enforcement (doc 05 §4, §6).

use crate::types::{LiveFeedProof, Sink};

pub const WINDOW_SLOTS: usize = 1024;
pub const ARENA_SIZE: usize = 64 * 1024; // 64 KiB
pub const SLOT_SIZE: usize = 64;
pub const WINDOW_MASK: usize = WINDOW_SLOTS - 1; // 1023
pub const WINDOW_MASK_U64: u64 = (WINDOW_SLOTS as u64) - 1; // 1023

/// Stages a single validated message payload into the 64 KiB arena if within the window.
/// Returns true if staged, false if dropped beyond the window.
/// P2: single wrapping_sub + single compare (off-1 < 1023) vs two compares + add.
#[inline(always)]
pub fn stage_msg(
    lens: &mut [u8; WINDOW_SLOTS],
    arena: &mut [u8; ARENA_SIZE],
    staged_count: &mut u32,
    w: u64,
    seq: u64,
    data: &[u8],
) -> bool {
    // off = seq - w; want 1 <= off <= 1023  <=>  off-1 < 1023
    let off = seq.wrapping_sub(w);
    if off.wrapping_sub(1) < (WINDOW_SLOTS as u64) {
        let slot = (seq & WINDOW_MASK_U64) as usize;
        if lens[slot] == 0 {
            *staged_count += 1;
        }
        let len = data.len().min(SLOT_SIZE);
        // C10: Presence encoded as len + 1 (0 = absent, >0 = present with length stored - 1)
        lens[slot] = (len as u8) + 1;
        let start = slot << 6;
        if len > 0 {
            arena[start..start + len].copy_from_slice(&data[..len]);
        }
        true
    } else {
        false
    }
}

/// §4.2 Clear-on-Advance Law:
/// Clears every slot in `[old_w, min(new_w, old_w + 1024))` to eliminate zombie slots.
/// P2: split-range linear walk avoids per-iter `& 1023` (1c) — two tight loops, no modulo.
#[inline(always)]
pub fn clear_slots(
    lens: &mut [u8; WINDOW_SLOTS],
    staged_count: &mut u32,
    max_staged: &mut u64,
    old_w: u64,
    new_w: u64,
) {
    if *staged_count == 0 {
        return;
    }
    // old_w <= new_w invariant (contiguous advance); defensive saturating for overflow
    let limit = new_w.min(old_w.saturating_add(WINDOW_SLOTS as u64));
    if limit <= old_w {
        return;
    }
    let n = (limit - old_w) as usize;
    let start = (old_w & WINDOW_MASK_U64) as usize;
    // First linear chunk until end of ring (iter_mut form satisfies needless_range_loop, same codegen)
    let first_n = (WINDOW_SLOTS - start).min(n);
    for cell in lens.iter_mut().skip(start).take(first_n) {
        if *cell != 0 {
            *cell = 0;
            *staged_count -= 1;
            if *staged_count == 0 {
                *max_staged = 0;
                return;
            }
        }
    }
    // Wrapped remainder from slot 0
    let rem = n - first_n;
    for cell in lens.iter_mut().take(rem) {
        if *cell != 0 {
            *cell = 0;
            *staged_count -= 1;
            if *staged_count == 0 {
                *max_staged = 0;
                return;
            }
        }
    }
}

/// §6 Drain: emits staged messages starting at `w` as long as consecutive slots are populated.
/// P2: early-return when empty (saves 1 load/packet steady-state), single slot compute,
/// hoisted proof, register-local w.
#[inline(always)]
pub fn drain<S: Sink>(
    lens: &mut [u8; WINDOW_SLOTS],
    arena: &[u8; ARENA_SIZE],
    w: &mut u64,
    staged_count: &mut u32,
    max_staged: &mut u64,
    gen: u64,
    sink: &mut S,
) -> u64 {
    if *staged_count == 0 {
        return 0;
    }
    let mut emitted = 0u64;
    let mut wv = *w;
    let proof = LiveFeedProof { gen };
    loop {
        let slot = (wv & WINDOW_MASK_U64) as usize;
        let stored = lens[slot];
        if stored == 0 {
            break;
        }
        let len = (stored - 1) as usize;
        let start = slot << 6;
        let msg = if len > 0 {
            &arena[start..start + len]
        } else {
            &[]
        };
        sink.on_msg(&proof, wv, msg);
        emitted += 1;

        lens[slot] = 0;
        *staged_count -= 1;
        wv += 1;
        if *staged_count == 0 {
            break;
        }
    }
    *w = wv;

    if *staged_count == 0 {
        *max_staged = 0;
    }
    emitted
}
