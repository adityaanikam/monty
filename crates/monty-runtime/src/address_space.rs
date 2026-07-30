//! Linux-only hard ceiling on a protocol worker's address space (`RLIMIT_AS`).
//!
//! The sandbox's own `max_memory` is enforced by `ResourceTracker`, which is
//! best-effort: every allocation site has to remember to consult it, and one
//! missed site (cf. the `str.expandtabs` bug) grows host memory unbounded until
//! the OS OOM killer picks an arbitrary victim. This is the belt underneath it —
//! the kernel fails the offending allocation, Rust's allocation-error handler
//! aborts the process, and the parent pool replaces the dead worker.
//!
//! Two properties to keep in mind when choosing a value:
//!
//! - It bounds *virtual* address space, not live heap: thread stacks, allocator
//!   arena reservations and file mappings all count, so the ceiling must sit
//!   well above the sandbox budget it backstops.
//! - A breach is unrecoverable and untargeted. The worker dies wherever the
//!   failing allocation happened, with no chance to raise `MemoryError`.

/// Lowers this process's address-space limit to `bytes`.
///
/// `Ok(Some(limit))` means the ceiling is in force at `limit` bytes; `Ok(None)`
/// that this platform has no usable knob and the process stays unbounded. `Err`
/// is a real failure: the caller asked for a guarantee that cannot be honoured.
///
/// Never *raises* the limit — an outer sandbox (container, `ulimit -v`) may
/// already impose something stricter, and asking for more than the inherited
/// hard limit would fail outright. Both soft and hard limits are set, so
/// nothing later in the process can lift the ceiling again.
#[cfg(target_os = "linux")]
pub(crate) fn apply(bytes: u64) -> Result<Option<u64>, String> {
    let (_, hard) = rlimit::Resource::AS
        .get()
        .map_err(|err| format!("cannot read RLIMIT_AS: {err}"))?;
    let limit = bytes.min(hard);
    rlimit::Resource::AS
        .set(limit, limit)
        .map(|()| Some(limit))
        .map_err(|err| format!("cannot set RLIMIT_AS to {limit} bytes: {err}"))
}

/// Non-Linux hosts have no usable equivalent: Darwin refuses to set `RLIMIT_AS`
/// (aliased onto `RLIMIT_RSS`) or `RLIMIT_DATA` at all, and Windows has no
/// rlimits — a Job Object would be the analogue there.
#[cfg(not(target_os = "linux"))]
#[expect(clippy::unnecessary_wraps, reason = "signature must match the Linux impl")]
pub(crate) fn apply(_bytes: u64) -> Result<Option<u64>, String> {
    Ok(None)
}
