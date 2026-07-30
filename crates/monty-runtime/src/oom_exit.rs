//! A [`System`] allocator that exits with [`OOM_EXIT_CODE`] on a refused
//! allocation, so the pool can report `MemoryError`.
//!
//! Rust would instead abort, and `SIGABRT` is also what a stack overflow
//! produces — the parent could not tell them apart. `set_alloc_error_hook` is
//! the API for this and is still unstable, hence an allocator. It does no
//! accounting: what makes allocations fail is `RLIMIT_AS`
//! (see [`crate::address_space`]) or plain host OOM.
//!
//! Only binaries may declare a `#[global_allocator]` — in a library or cdylib it
//! would hijack the allocator of the embedding host process.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    io::{self, Write},
    process,
    sync::atomic::{AtomicBool, Ordering},
};

use monty_proto::OOM_EXIT_CODE;

/// Written to stderr before exiting, so a human reading worker diagnostics sees
/// why the process vanished. The `{n} bytes` suffix is the failed request size.
pub(crate) const OOM_MARKER: &str = "monty worker: allocation of";

/// The system allocator, plus a null check that exits rather than aborts.
pub(crate) struct OomExitAlloc;

// SAFETY: every method forwards its arguments unchanged to `System`, whose
// `GlobalAlloc` contract is identical to this one, and returns what `System`
// returned (or diverges). No pointer is fabricated, aliased, or freed here, so
// the impl upholds exactly the invariants `System` upholds.
unsafe impl GlobalAlloc for OomExitAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` comes from the caller and is forwarded unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if ptr.is_null() {
            oom_exit(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was returned by our `alloc`/`realloc`, i.e. by
        // `System`, with this same `layout` — precisely `System`'s requirement.
        unsafe { System.dealloc(ptr, layout) };
    }

    // Overridden rather than left to the default (which routes through `alloc`)
    // so `System` keeps using calloc's pre-zeroed pages.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` comes from the caller and is forwarded unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if ptr.is_null() {
            oom_exit(layout.size());
        }
        ptr
    }

    // Overridden for the same reason: the default reallocates and copies, while
    // `System` can often grow a block in place.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr`/`layout` describe a live block from this allocator and
        // `new_size` is the caller's, all forwarded unchanged.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if new_ptr.is_null() {
            oom_exit(new_size);
        }
        new_ptr
    }
}

/// Reports the refused allocation and exits with [`OOM_EXIT_CODE`].
///
/// Deliberately not a panic (the panic machinery allocates) and not an abort
/// (indistinguishable from a stack overflow, and dumps core). Skips destructors,
/// so a half-written protocol frame may be left on stdout — the parent already
/// treats a truncated tail as a dead worker and then classifies by exit code.
#[cold]
#[inline(never)]
fn oom_exit(size: usize) -> ! {
    // Formatting into unbuffered stderr should not allocate, but if it somehow
    // does and *that* fails we would re-enter here forever: let the first
    // caller write and send any re-entrant one straight to `exit`.
    static REPORTING: AtomicBool = AtomicBool::new(false);
    if !REPORTING.swap(true, Ordering::Relaxed) {
        let _ = writeln!(io::stderr(), "{OOM_MARKER} {size} bytes failed");
    }
    process::exit(OOM_EXIT_CODE)
}
