//! CmdChannel latest-wins register (doc 08 §3, AM-4).
//! Supersedes the 8×24B ring. Lockless seqlock protocol with odd/even epoch counter.

use nf_arbitrator::types::RecoveryIntent;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdPayload {
    pub intent: RecoveryIntent,
    pub session: [u8; 10],
    pub valid: bool,
}

impl Default for CmdPayload {
    fn default() -> Self {
        Self {
            intent: RecoveryIntent {
                from: 0,
                to_excl: 0,
            },
            session: [0u8; 10],
            valid: false,
        }
    }
}

#[repr(align(64))]
pub struct CmdChannel {
    epoch: AtomicU64,
    payload: UnsafeCell<CmdPayload>,
    status_word: AtomicU32,
}

unsafe impl Sync for CmdChannel {}
unsafe impl Send for CmdChannel {}

impl CmdChannel {
    pub const fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            payload: UnsafeCell::new(CmdPayload {
                intent: RecoveryIntent {
                    from: 0,
                    to_excl: 0,
                },
                session: [0u8; 10],
                valid: false,
            }),
            status_word: AtomicU32::new(0),
        }
    }

    /// Publishes a new recovery intent and session from Thread H.
    /// Odd epoch indicates write in progress; even epoch indicates stable payload.
    pub fn publish(&self, intent: RecoveryIntent, session: [u8; 10]) {
        let e = self.epoch.fetch_add(1, Ordering::AcqRel); // Becomes odd
        debug_assert_eq!(e & 1, 0, "Concurrent publish violation on CmdChannel");

        let p = self.payload.get();
        unsafe {
            *p = CmdPayload {
                intent,
                session,
                valid: true,
            };
        }

        self.epoch.fetch_add(1, Ordering::Release); // Becomes even
    }

    /// Clears any pending intent (e.g. on session boundary).
    pub fn clear(&self, new_session: [u8; 10]) {
        let e = self.epoch.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(e & 1, 0);

        let p = self.payload.get();
        unsafe {
            *p = CmdPayload {
                intent: RecoveryIntent {
                    from: 0,
                    to_excl: 0,
                },
                session: new_session,
                valid: false,
            };
        }

        self.epoch.fetch_add(1, Ordering::Release);
    }

    /// Reads the latest published command if new since `last_epoch`.
    /// Returns `Some((payload, current_epoch))` if new, or `None` if no new updates.
    pub fn take_latest(&self, last_epoch: u64) -> Option<(CmdPayload, u64)> {
        let mut spins = 0;
        loop {
            let e1 = self.epoch.load(Ordering::Acquire);
            if (e1 & 1) != 0 {
                // Writer active, spin
                std::hint::spin_loop();
                spins += 1;
                if spins > 10000 {
                    std::thread::yield_now();
                }
                continue;
            }

            if e1 <= last_epoch {
                return None;
            }

            let p = unsafe { *self.payload.get() };

            std::sync::atomic::fence(Ordering::Acquire);
            let e2 = self.epoch.load(Ordering::Acquire);

            if e1 == e2 {
                return Some((p, e1));
            }
        }
    }

    /// Reads the latest payload unconditionally.
    pub fn read_current(&self) -> CmdPayload {
        loop {
            let e1 = self.epoch.load(Ordering::Acquire);
            if (e1 & 1) != 0 {
                std::hint::spin_loop();
                continue;
            }
            let p = unsafe { *self.payload.get() };
            std::sync::atomic::fence(Ordering::Acquire);
            let e2 = self.epoch.load(Ordering::Acquire);
            if e1 == e2 {
                return p;
            }
        }
    }

    #[inline]
    pub fn set_status(&self, status: u32) {
        self.status_word.store(status, Ordering::Release);
    }

    #[inline]
    pub fn get_status(&self) -> u32 {
        self.status_word.load(Ordering::Acquire)
    }
}

impl Default for CmdChannel {
    fn default() -> Self {
        Self::new()
    }
}
