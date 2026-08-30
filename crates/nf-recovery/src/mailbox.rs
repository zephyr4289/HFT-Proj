//! SPSC PacketMailbox ring (doc 08 §3).
//! 16 slots × 1500 B with cache-line-padded head/tail.
//! Thread R parks on full (spin + exponential backoff); Thread H drains every iteration.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

pub const MAILBOX_SLOTS: usize = 16;
pub const MAILBOX_SLOT_SIZE: usize = 1500;

#[repr(align(64))]
struct CachePadded<T>(T);

pub struct PacketMailbox {
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
    slots: [[u8; MAILBOX_SLOT_SIZE]; MAILBOX_SLOTS],
    lens: [u16; MAILBOX_SLOTS],
}

// UnsafeCell isn't needed if we use internal unsafe or UnsafeCell for slots.
// Since SPSC guarantee ensures R only writes slot[tail%16] when tail-head < 16,
// and H only reads slot[head%16] when head < tail, slots are disjoint.
unsafe impl Sync for PacketMailbox {}
unsafe impl Send for PacketMailbox {}

impl PacketMailbox {
    pub const fn new() -> Self {
        Self {
            head: CachePadded(AtomicUsize::new(0)),
            tail: CachePadded(AtomicUsize::new(0)),
            slots: [[0u8; MAILBOX_SLOT_SIZE]; MAILBOX_SLOTS],
            lens: [0u16; MAILBOX_SLOTS],
        }
    }

    /// Pushes a packet into the mailbox. If full, parks with spin + exponential backoff (up to 1ms).
    pub fn push_park(&self, data: &[u8]) {
        assert!(
            data.len() <= MAILBOX_SLOT_SIZE,
            "Packet exceeds MAILBOX_SLOT_SIZE (1500)"
        );

        let mut backoff_spins = 1;
        loop {
            let tail = self.tail.0.load(Ordering::Relaxed);
            let head = self.head.0.load(Ordering::Acquire);

            if tail.wrapping_sub(head) < MAILBOX_SLOTS {
                let slot_idx = tail % MAILBOX_SLOTS;

                // Safe access to disjoint slot
                let slot_ptr = self.slots[slot_idx].as_ptr() as *mut u8;
                let lens_ptr = self.lens[slot_idx..].as_ptr() as *mut u16;

                unsafe {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), slot_ptr, data.len());
                    *lens_ptr = data.len() as u16;
                }

                self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
                return;
            }

            // Full: backoff
            if backoff_spins < 64 {
                for _ in 0..backoff_spins {
                    std::hint::spin_loop();
                }
                backoff_spins <<= 1;
            } else {
                thread::sleep(Duration::from_micros(50));
            }
        }
    }

    /// Attempts to push without parking. Returns true on success, false if full.
    pub fn try_push(&self, data: &[u8]) -> bool {
        if data.len() > MAILBOX_SLOT_SIZE {
            return false;
        }
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);

        if tail.wrapping_sub(head) >= MAILBOX_SLOTS {
            return false;
        }

        let slot_idx = tail % MAILBOX_SLOTS;
        let slot_ptr = self.slots[slot_idx].as_ptr() as *mut u8;
        let lens_ptr = self.lens[slot_idx..].as_ptr() as *mut u16;

        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), slot_ptr, data.len());
            *lens_ptr = data.len() as u16;
        }

        self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// Drains all available packets to the provided consumer callback.
    pub fn drain<F: FnMut(&[u8])>(&self, mut callback: F) -> usize {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);

        let count = tail.wrapping_sub(head);
        if count == 0 {
            return 0;
        }

        for i in 0..count {
            let slot_idx = head.wrapping_add(i) % MAILBOX_SLOTS;
            let len = self.lens[slot_idx] as usize;
            let slice = &self.slots[slot_idx][..len];
            callback(slice);
        }

        self.head.0.store(tail, Ordering::Release);
        count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head.0.load(Ordering::Relaxed) == self.tail.0.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn len(&self) -> usize {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }
}

impl Default for PacketMailbox {
    fn default() -> Self {
        Self::new()
    }
}
