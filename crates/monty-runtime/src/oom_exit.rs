//! A [`System`] allocator that enforces the worker's memory ceiling and exits
//! with [`OOM_EXIT_CODE`] rather than aborting, so the pool can report
//! `MemoryError`.
//!
//! Two things end a worker here: the live-byte total exceeding a ceiling set by
//! [`set_limit`], and the allocator refusing a request outright (host OOM, or a
//! size beyond the usable address space). Rust would abort on the latter, and
//! `SIGABRT` is also what a stack overflow produces — the parent could not tell
//! them apart. `set_alloc_error_hook` is the API for this and is still unstable,
//! hence an allocator.
//!
//! Counting here rather than asking the kernel for `RLIMIT_AS` is what makes the
//! ceiling portable and its units meaningful: it bounds bytes the process asked
//! for, not virtual address space, so mapped text, thread stacks and file
//! mappings do not consume the host's budget. The tradeoff is that it binds only
//! what reaches this allocator — everything the sandbox can allocate, but not a
//! direct `mmap` — so it is a backstop under the interpreter's own tracker, not
//! a kernel-enforced bound on the process.
//!
//! Only binaries may declare a `#[global_allocator]` — in a library or cdylib it
//! would hijack the allocator of the embedding host process.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    fmt,
    io::{self, Write},
    process,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use monty_proto::OOM_EXIT_CODE;

/// Written to stderr before exiting, so a human reading worker diagnostics sees
/// why the process vanished. Followed by the request size and whether it was
/// refused by the allocator or by the ceiling.
pub(crate) const OOM_MARKER: &str = "monty worker: allocation of";

/// Bytes currently charged by [`OomExitAlloc`], and the ceiling they may not
/// exceed (`usize::MAX` until [`set_limit`] lowers it).
///
/// Counting starts with the process, not with the ceiling: the flag carrying it
/// is parsed in `main`, by which point std and the argument parser have already
/// allocated, and a counter armed then would see `dealloc`s it never charged and
/// underflow. So the count is always paid; only the comparison is configurable.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Sets the ceiling on live allocated bytes, exiting at once if already past it.
///
/// Called before the worker serves any request. An immediate exit means the
/// ceiling is below what merely starting up costs — better reported now than as
/// an arbitrary allocation failing mid-turn.
pub(crate) fn set_limit(bytes: u64) {
    let limit = usize::try_from(bytes).unwrap_or(usize::MAX);
    LIMIT.store(limit, Ordering::Relaxed);
    let live = LIVE.load(Ordering::Relaxed);
    if live > limit {
        oom_exit(format_args!(
            "monty worker: memory ceiling of {limit} bytes is below the {live} bytes already in use"
        ));
    }
}

/// The system allocator, plus a live-byte count and a null check that exits
/// rather than aborts.
pub(crate) struct OomExitAlloc;

// SAFETY: every method forwards its arguments unchanged to `System`, whose
// `GlobalAlloc` contract is identical to this one, and returns what `System`
// returned (or diverges). No pointer is fabricated, aliased, or freed here, so
// the impl upholds exactly the invariants `System` upholds.
unsafe impl GlobalAlloc for OomExitAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        // SAFETY: `layout` comes from the caller and is forwarded unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if ptr.is_null() {
            oom_exit(format_args!("{OOM_MARKER} {} bytes failed", layout.size()));
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        refund(layout.size());
        // SAFETY: `ptr` was returned by our `alloc`/`realloc`, i.e. by
        // `System`, with this same `layout` — precisely `System`'s requirement.
        unsafe { System.dealloc(ptr, layout) };
    }

    // Overridden rather than left to the default (which routes through `alloc`)
    // so `System` keeps using calloc's pre-zeroed pages.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        // SAFETY: `layout` comes from the caller and is forwarded unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if ptr.is_null() {
            oom_exit(format_args!("{OOM_MARKER} {} bytes failed", layout.size()));
        }
        ptr
    }

    // Overridden for the same reason: the default reallocates and copies, while
    // `System` can often grow a block in place.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size >= layout.size() {
            charge(new_size - layout.size());
        } else {
            refund(layout.size() - new_size);
        }
        // SAFETY: `ptr`/`layout` describe a live block from this allocator and
        // `new_size` is the caller's, all forwarded unchanged.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if new_ptr.is_null() {
            oom_exit(format_args!("{OOM_MARKER} {new_size} bytes failed"));
        }
        new_ptr
    }
}

/// Adds `size` to the live total, exiting if that breaches the ceiling.
///
/// Charges *before* allocating, so a breach is refused rather than committed —
/// the point of a ceiling is that the host never has to find the memory. The
/// charge is not rolled back when the allocation then fails because both paths
/// exit; nothing survives to observe the overcount.
#[inline]
fn charge(size: usize) {
    if LIVE.fetch_add(size, Ordering::Relaxed).saturating_add(size) > LIMIT.load(Ordering::Relaxed) {
        oom_exit(format_args!("{OOM_MARKER} {size} bytes exceeds the memory ceiling"));
    }
}

/// Returns `size` to the live total. `Relaxed` throughout: the count only has to
/// be eventually right, and no other memory is published through it.
#[inline]
fn refund(size: usize) {
    LIVE.fetch_sub(size, Ordering::Relaxed);
}

/// Reports why memory ran out and exits with [`OOM_EXIT_CODE`].
///
/// Deliberately not a panic (the panic machinery allocates) and not an abort
/// (indistinguishable from a stack overflow, and dumps core). Skips destructors,
/// so a half-written protocol frame may be left on stdout — the parent already
/// treats a truncated tail as a dead worker and then classifies by exit code.
/// Takes pre-formatted arguments so nothing on this path allocates.
#[cold]
#[inline(never)]
fn oom_exit(reason: fmt::Arguments<'_>) -> ! {
    // Even with the ceiling lifted below, a genuinely exhausted host can fail
    // the write and re-enter here: let the first caller write and send any
    // re-entrant one straight to `exit`.
    static REPORTING: AtomicBool = AtomicBool::new(false);
    // Lift the ceiling first: writing to stderr allocates (the handle's lock, on
    // first use), and under a breached ceiling that allocation would re-enter
    // here and be silenced by the guard below — losing the very message that
    // explains the exit. Safe because this path always ends in `exit`.
    LIMIT.store(usize::MAX, Ordering::Relaxed);
    if !REPORTING.swap(true, Ordering::Relaxed) {
        let _ = writeln!(io::stderr(), "{reason}");
    }
    process::exit(OOM_EXIT_CODE)
}
