#![cfg(not(loom))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tacho_ring::Ring;

struct CountingAlloc;

static TRACK_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACK_ALLOCATIONS.load(Ordering::SeqCst) {
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::SeqCst);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

#[test]
fn test_no_allocation_after_warmup() {
    let mut ring = Ring::new(64 * 1024); // 64 KiB = 16 chunks

    // Pre-allocated stack buffers of various sizes
    let packet_64 = [0x11u8; 64];
    let packet_256 = [0x22u8; 256];
    let packet_1024 = [0x33u8; 1024];
    let packet_4088 = [0x44u8; 4088];

    // Warm-up: write packets to exercise chunk transitions and wrapping
    for i in 0..100 {
        let p: &[u8] = match i % 4 {
            0 => &packet_64,
            1 => &packet_256,
            2 => &packet_1024,
            _ => &packet_4088,
        };
        ring.write(p).expect("warm-up write failed");
    }

    // Start tracking allocations
    ALLOCATED_BYTES.store(0, Ordering::SeqCst);
    TRACK_ALLOCATIONS.store(true, Ordering::SeqCst);

    // Perform 10,000 writes across wrapping and chunk boundaries
    for i in 0..10_000 {
        let p: &[u8] = match i % 4 {
            0 => &packet_64,
            1 => &packet_256,
            2 => &packet_1024,
            _ => &packet_4088,
        };
        ring.write(p).expect("write failed");
    }

    // Stop tracking
    TRACK_ALLOCATIONS.store(false, Ordering::SeqCst);

    let allocated = ALLOCATED_BYTES.load(Ordering::SeqCst);
    assert_eq!(
        allocated, 0,
        "10,000 writes allocated {allocated} bytes; expected 0 bytes"
    );
}
