//! AF_XDP kernel transport implementation (doc 09).
//! Implements Transport trait over Linux AF_XDP (XSK) sockets with shared UMEM.

#![allow(non_camel_case_types)]
#![allow(dead_code)]

use crate::{FrameBatch, Transport};
use std::sync::atomic::{AtomicU32, Ordering};

pub const AF_XDP: i32 = 44;
pub const SOL_XDP: i32 = 283;
pub const XDP_MMAP_OFFSETS: i32 = 1;
pub const XDP_RX_RING: i32 = 2;
pub const XDP_TX_RING: i32 = 3;
pub const XDP_UMEM_REG: i32 = 4;
pub const XDP_UMEM_FILL_RING: i32 = 5;
pub const XDP_UMEM_COMPLETION_RING: i32 = 6;
pub const XDP_STATISTICS: i32 = 7;

pub const UMEM_NUM_FRAMES: usize = 2048;
pub const UMEM_FRAME_SIZE: usize = 4096;
pub const UMEM_TOTAL_SIZE: usize = UMEM_NUM_FRAMES * UMEM_FRAME_SIZE; // 8 MiB

pub const RING_SIZE: u32 = 2048;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct xdp_umem_reg {
    pub addr: u64,
    pub len: u64,
    pub chunk_size: u32,
    pub headroom: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct xdp_ring_offset {
    pub producer: u64,
    pub consumer: u64,
    pub desc: u64,
    pub flags: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct xdp_mmap_offsets {
    pub rx: xdp_ring_offset,
    pub tx: xdp_ring_offset,
    pub fr: xdp_ring_offset,
    pub cr: xdp_ring_offset,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct xdp_desc {
    pub addr: u64,
    pub len: u32,
    pub options: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct sockaddr_xdp {
    pub sxdp_family: u16,
    pub sxdp_flags: u16,
    pub sxdp_ifindex: u32,
    pub sxdp_queue_id: u32,
    pub sxdp_shared_umem_fd: u32,
}

pub struct XskRingRx {
    pub producer: *const AtomicU32,
    pub consumer: *mut AtomicU32,
    pub ring: *mut xdp_desc,
    pub size: u32,
}

pub struct XskRingFill {
    pub producer: *mut AtomicU32,
    pub consumer: *const AtomicU32,
    pub ring: *mut u64,
    pub size: u32,
}

pub struct XdpTransport {
    umem_area: *mut u8,
    fill_ring: XskRingFill,
    rx_ring_a: XskRingRx,
    rx_ring_b: Option<XskRingRx>,
    fd_a: i32,
    fd_b: i32,
    recycled_addrs: [u64; 256],
    recycled_count: usize,
}

unsafe impl Send for XdpTransport {}
unsafe impl Sync for XdpTransport {}

impl XdpTransport {
    /// Creates a mock / synthetic XdpTransport when running in non-privileged / non-XDP mode.
    pub fn new_mock() -> Self {
        let layout = std::alloc::Layout::from_size_align(UMEM_TOTAL_SIZE, 4096).unwrap();
        let umem_area = unsafe { std::alloc::alloc_zeroed(layout) };

        Self {
            umem_area,
            fill_ring: XskRingFill {
                producer: std::ptr::null_mut(),
                consumer: std::ptr::null(),
                ring: std::ptr::null_mut(),
                size: RING_SIZE,
            },
            rx_ring_a: XskRingRx {
                producer: std::ptr::null(),
                consumer: std::ptr::null_mut(),
                ring: std::ptr::null_mut(),
                size: RING_SIZE,
            },
            rx_ring_b: None,
            fd_a: -1,
            fd_b: -1,
            recycled_addrs: [0; 256],
            recycled_count: 0,
        }
    }

    /// Attaches to AF_XDP sockets on the given network interface index.
    pub fn bind(ifindex: u32) -> Result<Self, i32> {
        unsafe {
            // 1. Allocate UMEM area
            let umem_area = libc::mmap(
                std::ptr::null_mut(),
                UMEM_TOTAL_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
                -1,
                0,
            );
            if umem_area == libc::MAP_FAILED {
                return Err(*libc::__errno_location());
            }

            // 2. Create Socket A
            let fd_a = libc::socket(AF_XDP, libc::SOCK_RAW, 0);
            if fd_a < 0 {
                return Err(*libc::__errno_location());
            }

            // Register UMEM on Socket A
            let mr = xdp_umem_reg {
                addr: umem_area as u64,
                len: UMEM_TOTAL_SIZE as u64,
                chunk_size: UMEM_FRAME_SIZE as u32,
                headroom: 0,
                flags: 0,
            };
            if libc::setsockopt(
                fd_a,
                SOL_XDP,
                XDP_UMEM_REG,
                &mr as *const _ as *const libc::c_void,
                std::mem::size_of::<xdp_umem_reg>() as libc::socklen_t,
            ) < 0
            {
                return Err(*libc::__errno_location());
            }

            // Set Fill Ring size
            let ring_sz = RING_SIZE;
            if libc::setsockopt(
                fd_a,
                SOL_XDP,
                XDP_UMEM_FILL_RING,
                &ring_sz as *const _ as *const libc::c_void,
                std::mem::size_of::<u32>() as libc::socklen_t,
            ) < 0
            {
                return Err(*libc::__errno_location());
            }

            // Set RX Ring size
            if libc::setsockopt(
                fd_a,
                SOL_XDP,
                XDP_RX_RING,
                &ring_sz as *const _ as *const libc::c_void,
                std::mem::size_of::<u32>() as libc::socklen_t,
            ) < 0
            {
                return Err(*libc::__errno_location());
            }

            // Read offsets
            let mut off = xdp_mmap_offsets::default();
            let mut optlen = std::mem::size_of::<xdp_mmap_offsets>() as libc::socklen_t;
            if libc::getsockopt(
                fd_a,
                SOL_XDP,
                XDP_MMAP_OFFSETS,
                &mut off as *mut _ as *mut libc::c_void,
                &mut optlen,
            ) < 0
            {
                return Err(*libc::__errno_location());
            }

            // Map Fill Ring
            let fill_map = libc::mmap(
                std::ptr::null_mut(),
                (off.fr.desc + (RING_SIZE as u64) * 8) as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd_a,
                0x100000000, // XDP_UMEM_PGOFF_FILL_RING
            );
            if fill_map == libc::MAP_FAILED {
                return Err(*libc::__errno_location());
            }

            let fill_ring = XskRingFill {
                producer: (fill_map as usize + off.fr.producer as usize) as *mut AtomicU32,
                consumer: (fill_map as usize + off.fr.consumer as usize) as *const AtomicU32,
                ring: (fill_map as usize + off.fr.desc as usize) as *mut u64,
                size: RING_SIZE,
            };

            // Map RX Ring A
            let rx_map_a = libc::mmap(
                std::ptr::null_mut(),
                (off.rx.desc + (RING_SIZE as u64) * (std::mem::size_of::<xdp_desc>() as u64)) as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd_a,
                0, // XDP_PGOFF_RX_RING
            );
            if rx_map_a == libc::MAP_FAILED {
                return Err(*libc::__errno_location());
            }

            let rx_ring_a = XskRingRx {
                producer: (rx_map_a as usize + off.rx.producer as usize) as *const AtomicU32,
                consumer: (rx_map_a as usize + off.rx.consumer as usize) as *mut AtomicU32,
                ring: (rx_map_a as usize + off.rx.desc as usize) as *mut xdp_desc,
                size: RING_SIZE,
            };

            // Bind Socket A to ifindex
            let sxdp = sockaddr_xdp {
                sxdp_family: AF_XDP as u16,
                sxdp_flags: 0, // XDP_COPY or generic mode
                sxdp_ifindex: ifindex,
                sxdp_queue_id: 0,
                sxdp_shared_umem_fd: 0,
            };
            if libc::bind(
                fd_a,
                &sxdp as *const _ as *const libc::sockaddr,
                std::mem::size_of::<sockaddr_xdp>() as libc::socklen_t,
            ) < 0
            {
                return Err(*libc::__errno_location());
            }

            // Pre-populate Fill Ring with all 2048 frame addresses
            let prod = (*fill_ring.producer).load(Ordering::Relaxed);
            for i in 0..RING_SIZE {
                let addr = (i as u64) * (UMEM_FRAME_SIZE as u64);
                std::ptr::write(fill_ring.ring.add(((prod + i) & (RING_SIZE - 1)) as usize), addr);
            }
            (*fill_ring.producer).store(prod + RING_SIZE, Ordering::Release);

            Ok(Self {
                umem_area: umem_area as *mut u8,
                fill_ring,
                rx_ring_a,
                rx_ring_b: None,
                fd_a,
                fd_b: -1,
                recycled_addrs: [0; 256],
                recycled_count: 0,
            })
        }
    }
}

impl Transport for XdpTransport {
    fn poll(&mut self, batch: &mut FrameBatch) -> usize {
        batch.clear();
        if self.fd_a < 0 || self.rx_ring_a.producer.is_null() {
            return 0;
        }

        unsafe {
            // 1. Refill fill ring with any recycled descriptors from previous iteration (O-2-X law)
            if self.recycled_count > 0 && !self.fill_ring.producer.is_null() {
                let prod = (*self.fill_ring.producer).load(Ordering::Relaxed);
                for i in 0..self.recycled_count {
                    let addr = self.recycled_addrs[i];
                    std::ptr::write(
                        self.fill_ring.ring.add(((prod + (i as u32)) & (self.fill_ring.size - 1)) as usize),
                        addr,
                    );
                }
                (*self.fill_ring.producer).store(prod + (self.recycled_count as u32), Ordering::Release);
                self.recycled_count = 0;
            }

            // 2. Poll Socket A RX Ring
            let prod_a = (*self.rx_ring_a.producer).load(Ordering::Acquire);
            let cons_a = (*self.rx_ring_a.consumer).load(Ordering::Relaxed);
            let avail_a = prod_a.wrapping_sub(cons_a);

            let mut count = 0usize;
            let cap = FrameBatch::capacity();

            for i in 0..avail_a {
                if count >= cap {
                    break;
                }
                let desc = *self.rx_ring_a.ring.add(((cons_a + i) & (self.rx_ring_a.size - 1)) as usize);
                let ptr = self.umem_area.add(desc.addr as usize);

                batch.push_raw(ptr, desc.len as usize, 0);

                if self.recycled_count < self.recycled_addrs.len() {
                    self.recycled_addrs[self.recycled_count] = desc.addr;
                    self.recycled_count += 1;
                }
                count += 1;
            }

            if count > 0 {
                (*self.rx_ring_a.consumer).store(cons_a + (count as u32), Ordering::Release);
            }

            count
        }
    }

    #[inline]
    fn now_ns(&self) -> u64 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts);
        }
        (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
    }
}

impl Drop for XdpTransport {
    fn drop(&mut self) {
        unsafe {
            if self.fd_a >= 0 {
                libc::close(self.fd_a);
            }
            if self.fd_b >= 0 {
                libc::close(self.fd_b);
            }
            if !self.umem_area.is_null() {
                libc::munmap(self.umem_area as *mut libc::c_void, UMEM_TOTAL_SIZE);
            }
        }
    }
}
