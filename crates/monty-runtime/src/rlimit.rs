//! Linux-only hard memory ceiling for a protocol worker, set as `RLIMIT_AS`.
//!
//! The sandbox's own `max_memory` is best-effort: every allocation site has to
//! remember to consult `ResourceTracker`, and one missed site (cf. the
//! `str.expandtabs` bug) grows host memory until the OOM killer picks a victim.
//! This is the belt underneath — the kernel refuses the allocation and the worker
//! exits (see [`crate::oom_exit`]), losing the session but not the pool.
//!
//! Because `RLIMIT_AS` bounds *virtual* address space — thread stacks, allocator
//! arenas and file mappings included — the ceiling must sit well above the
//! sandbox budget it backstops.

// The module shares its name with the crate it wraps, so name the import: a bare
// `rlimit::Resource` here would read as a path into this module.
#[cfg(target_os = "linux")]
use rlimit::Resource;

/// Lowers this process's `RLIMIT_AS` to `bytes`.
///
/// `Ok(Some(limit))` means the ceiling is in force at `limit` bytes; `Ok(None)`
/// that this platform has no usable knob and the process stays unbounded. `Err`
/// is a real failure: the caller asked for a guarantee that cannot be honoured.
///
/// Never *raises* the limit: an outer sandbox (container, `ulimit -v`) may
/// already impose something stricter, so the request is clamped to *both*
/// inherited limits — asking for more than the hard limit fails outright, and
/// exceeding the inherited soft limit would loosen the very constraint this is
/// meant to tighten. Both soft and hard are then set, so nothing later in the
/// process can lift the ceiling again.
#[cfg(target_os = "linux")]
pub(crate) fn apply(bytes: u64) -> Result<Option<u64>, String> {
    let (soft, hard) = Resource::AS
        .get()
        .map_err(|err| format!("cannot read RLIMIT_AS: {err}"))?;
    let limit = bytes.min(soft).min(hard);
    Resource::AS
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
