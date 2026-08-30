//! Window arena layout, staging, drain, and Clear-on-Advance enforcement (doc 05 §4, §6).

use crate::types::{LiveFeedProof, Sink};

pub const WINDOW_SLOTS: usize = 1024;
pub const ARENA_SIZE: usize = 64 * 1024; // 64 KiB
pub const SLOT_SIZE: usize = 64;

/// Stages a single validated message payload into the 64 KiB arena if within the window.
/// Returns true if staged, false if dropped beyond the window.
#[inline]
pub fn stage_msg(
    lens: &mut [u8; WINDOW_SLOTS],
    arena: &mut [u8; ARENA_SIZE],
    staged_count: &mut u32,
    w: u64,
    seq: u64,
    data: &[u8],
) -> bool {
    if seq > w && seq < w + (WINDOW_SLOTS as u64) {
        let slot = (seq & 1023) as usize;
        if lens[slot] == 0 {
            *staged_count += 1;
        }
        let len = data.len().min(SLOT_SIZE);
        lens[slot] = len as u8;
        let start = slot << 6;
        arena[start..start + len].copy_from_slice(&data[..len]);
        true
    } else {
        false
    }
}

/// §4.2 Clear-on-Advance Law:
/// Clears every slot in `[old_w, min(new_w, old_w + 1024))` to eliminate zombie slots.
#[inline]
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
    let limit = new_w.min(old_w + (WINDOW_SLOTS as u64));
    for seq in old_w..limit {
        let slot = (seq & 1023) as usize;
        if lens[slot] != 0 {
            lens[slot] = 0;
            *staged_count -= 1;
            if *staged_count == 0 {
                *max_staged = 0;
                break;
            }
        }
    }
}

/// §6 Drain: emits staged messages starting at `w` as long as consecutive slots are populated.
#[inline]
pub fn drain<S: Sink>(
    lens: &mut [u8; WINDOW_SLOTS],
    arena: &[u8; ARENA_SIZE],
    w: &mut u64,
    staged_count: &mut u32,
    max_staged: &mut u64,
    gen: u64,
    sink: &mut S,
) -> u64 {
    let mut emitted = 0u64;
    while lens[(*w & 1023) as usize] != 0 {
        let slot = (*w & 1023) as usize;
        let len = lens[slot] as usize;
        let start = slot << 6;
        let msg = &arena[start..start + len];
        let proof = LiveFeedProof { gen };
        sink.on_msg(&proof, *w, msg);
        emitted += 1;

        lens[slot] = 0;
        *staged_count -= 1;
        *w += 1;
    }

    if *staged_count == 0 {
        *max_staged = 0;
    }
    emitted
}
