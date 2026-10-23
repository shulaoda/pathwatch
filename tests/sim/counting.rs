//! The system allocator with counters: how often this thread asked for memory, and the bytes
//! that are allocated now. `allocations` reads the first, `cost` both.

use std::{
  alloc::{GlobalAlloc, Layout, System},
  cell::Cell,
  sync::atomic::{AtomicUsize, Ordering},
};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
  static REQUESTS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: every call goes to the system allocator with the arguments it came with. The counters
// do not touch the memory.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    count(layout.size(), 0);
    // SAFETY: the caller keeps the rules of `GlobalAlloc::alloc`.
    unsafe { System.alloc(layout) }
  }

  unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
    LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    // SAFETY: the caller keeps the rules of `GlobalAlloc::dealloc`.
    unsafe { System.dealloc(block, layout) }
  }

  unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    count(new_size, layout.size());
    // SAFETY: the caller keeps the rules of `GlobalAlloc::realloc`.
    unsafe { System.realloc(block, layout, new_size) }
  }
}

fn count(added: usize, freed: usize) {
  LIVE.fetch_add(added, Ordering::Relaxed);
  LIVE.fetch_sub(freed, Ordering::Relaxed);
  REQUESTS.set(REQUESTS.get() + 1);
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The bytes that are allocated now.
pub fn live() -> usize {
  LIVE.load(Ordering::Relaxed)
}

/// How often this thread asked for memory.
pub fn requests() -> u64 {
  REQUESTS.get()
}
