//! The worker's global allocator: it counts live bytes against a ceiling (see
//! [`arm_ceiling`]) and exits with [`OOM_EXIT_CODE`] rather than aborting, as
//! `SIGABRT` is indistinguishable from a stack overflow. Counting beats
//! `RLIMIT_AS` — portable, and it bounds requested bytes rather than address
//! space — but binds only what reaches here. Only binaries may declare one.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    fmt,
    io::{self, Write},
    process,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use monty_proto::OOM_EXIT_CODE;

/// Bytes currently charged, and the ceiling they may not exceed (`usize::MAX`
/// until [`arm_ceiling`] lowers it). Counting starts with the process: a counter
/// armed later would see `dealloc`s it never charged and underflow.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);

/// The leanest the process has ever been at an arming point, which is the
/// worker's own footprint. Deriving each ceiling from the *current* live total
/// instead would let memory retained between sessions (the type checker's memo
/// graphs) ratchet the ceiling up checkout after checkout.
static BASELINE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// How far above `max_memory` the ceiling sits, covering what the interpreter's
/// tracker undercounts (arena slots, capacity slack, allocator rounding) —
/// measured at worst ~3.8× for the smallest objects, so this is a backstop.
const CEILING_MULTIPLE: usize = 5;

/// Fixed headroom for the worker's own machinery, which no `max_memory` covers:
/// repl and frame buffers, or typeshed and salsa caches when type checking. Both
/// are generous, since too tight a ceiling kills healthy workers.
const BASE_HEADROOM: usize = 4 * 1024 * 1024;
const TYPE_CHECK_HEADROOM: usize = 32 * 1024 * 1024;

/// Derives the ceiling from the current session's sandbox budget, or lifts it
/// while no session has one. Called after every request, since a session can
/// also arrive (or end) through `Load` and `Reset`; the result depends only on
/// the budget and the baseline, so re-deriving mid-session is a no-op.
pub(crate) fn arm_ceiling(max_memory: Option<u64>, type_check: bool) {
    let live = LIVE.load(Ordering::Relaxed);
    // `fetch_min` both reads and lowers the baseline: the first arming, on a
    // pristine worker, sets it, and a later leaner moment can only improve it.
    let baseline = BASELINE.fetch_min(live, Ordering::Relaxed).min(live);
    let limit = match max_memory {
        Some(bytes) => {
            let headroom = if type_check { TYPE_CHECK_HEADROOM } else { BASE_HEADROOM };
            baseline
                .saturating_add(
                    usize::try_from(bytes)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(CEILING_MULTIPLE),
                )
                .saturating_add(headroom)
        }
        None => usize::MAX,
    };
    LIMIT.store(limit, Ordering::Relaxed);
}

/// The system allocator, plus a live-byte count and a null check that exits
/// rather than aborts.
pub(crate) struct LimitedAllocator;

// SAFETY: every method forwards its arguments unchanged to `System` and returns
// what `System` returned (or diverges). No pointer is fabricated, aliased or
// freed here, so this upholds exactly the invariants `System` upholds.
unsafe impl GlobalAlloc for LimitedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        // SAFETY: `layout` comes from the caller and is forwarded unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if ptr.is_null() {
            oom_exit(format_args!(
                "monty worker: allocation of {} bytes failed",
                layout.size()
            ));
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        refund(layout.size());
        // SAFETY: `ptr` came from our `alloc`/`realloc` with this same `layout`.
        unsafe { System.dealloc(ptr, layout) };
    }

    // Overridden rather than left to the default (which routes through `alloc`)
    // so `System` keeps using calloc's pre-zeroed pages.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        // SAFETY: `layout` comes from the caller and is forwarded unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if ptr.is_null() {
            oom_exit(format_args!(
                "monty worker: allocation of {} bytes failed",
                layout.size()
            ));
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
        // SAFETY: `ptr`/`layout` describe a live block from this allocator, and
        // `new_size` is the caller's — all forwarded unchanged.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if new_ptr.is_null() {
            oom_exit(format_args!("monty worker: allocation of {new_size} bytes failed"));
        }
        new_ptr
    }
}

/// Adds `size` to the live total, exiting if that breaches the ceiling. Charges
/// *before* allocating, so a breach is refused rather than committed; the
/// overcount left when an allocation then fails is moot, as both paths exit.
#[inline]
fn charge(size: usize) {
    if LIVE.fetch_add(size, Ordering::Relaxed).saturating_add(size) > LIMIT.load(Ordering::Relaxed) {
        oom_exit(format_args!(
            "monty worker: allocation of {size} bytes exceeds the memory ceiling"
        ));
    }
}

/// Returns `size` to the live total. `Relaxed` throughout: the count only has to
/// be eventually right, and no other memory is published through it.
#[inline]
fn refund(size: usize) {
    LIVE.fetch_sub(size, Ordering::Relaxed);
}

/// Reports why memory ran out and exits with [`OOM_EXIT_CODE`] — not a panic
/// (that machinery allocates) and not an abort (a stack overflow looks the
/// same). Skipping destructors can leave a partial frame on stdout, which the
/// parent already treats as a dead worker.
#[cold]
#[inline(never)]
fn oom_exit(reason: fmt::Arguments<'_>) -> ! {
    // A genuinely exhausted host can fail the write and re-enter: let the first
    // caller write and send any re-entrant one straight to `exit`.
    static REPORTING: AtomicBool = AtomicBool::new(false);
    // Lift the ceiling first — writing to stderr allocates (the handle's lock),
    // which under a breached ceiling would re-enter and be silenced below,
    // losing the message. Safe because this path always ends in `exit`.
    LIMIT.store(usize::MAX, Ordering::Relaxed);
    if !REPORTING.swap(true, Ordering::Relaxed) {
        let _ = writeln!(io::stderr(), "{reason}");
    }
    process::exit(OOM_EXIT_CODE)
}
