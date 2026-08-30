//! L1: Counting global allocator for zero-allocation enforcement (doc 07 §3).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct CountingAllocator {
    allocs: AtomicU64,
    deallocs: AtomicU64,
}

#[global_allocator]
pub static GLOBAL: CountingAllocator = CountingAllocator::new();

impl CountingAllocator {
    pub const fn new() -> Self {
        Self {
            allocs: AtomicU64::new(0),
            deallocs: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn alloc_count(&self) -> u64 {
        self.allocs.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn dealloc_count(&self) -> u64 {
        self.deallocs.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn snapshot(&self) -> (u64, u64) {
        (self.alloc_count(), self.dealloc_count())
    }
}

impl Default for CountingAllocator {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.allocs.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.deallocs.fetch_add(1, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.allocs.fetch_add(1, Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.allocs.fetch_add(1, Ordering::Relaxed);
        self.deallocs.fetch_add(1, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}
