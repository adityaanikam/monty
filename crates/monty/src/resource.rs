use std::{
    cell::Cell,
    error::Error,
    fmt,
    time::{Duration, Instant},
};

use crate::{
    ExcType,
    exception_private::{ExceptionRaise, RawStackFrame, RunError, SimpleException},
};

/// Threshold in bytes above which `check_large_result` is called.
///
/// Operations that may produce results larger than this threshold (100KB) should call
/// `check_large_result` before performing the operation. This prevents DoS attacks
/// where operations like `2 ** 10_000_000` allocate huge amounts of memory before
/// the allocation check can catch them.
pub const LARGE_RESULT_THRESHOLD: usize = 100_000;

/// Pre-checks that an operation producing `item_len * count` bytes won't exceed resource limits.
///
/// Used for sequence repeats (`'x' * 999_999_999`), padding operations
/// (`str.ljust`, `str.center`, `str.zfill`, etc.), and any other operation
/// where the result size is a simple product of two known values.
pub fn check_repeat_size(item_len: usize, count: usize, tracker: &ResourceTracker) -> Result<(), ResourceError> {
    check_estimated_size(item_len.saturating_mul(count), tracker)
}

/// Pre-checks that `base ** exponent` won't exceed resource limits before computing.
///
/// The result of `base ** exp` has approximately `base_bits * exp` bits.
/// For bases with 0 or 1 significant bits (0, 1, -1), the result is always
/// small regardless of exponent, so the check is skipped.
///
/// The estimate includes a 4× safety multiplier because `BigInt::pow` uses repeated squaring,
/// which allocates intermediate values on the Rust heap (not tracked by the resource tracker).
/// At peak, old/new base and old/new accumulator coexist simultaneously during each
/// multiplication step, requiring roughly 4× the final result size in memory.
pub fn check_pow_size(base_bits: u64, exponent: u64, tracker: &ResourceTracker) -> Result<(), ResourceError> {
    // 0**n = 0, 1**n = 1, (-1)**n = ±1 — always small
    if base_bits <= 1 {
        return Ok(());
    }
    let result_bytes = estimate_bits_to_bytes(base_bits.saturating_mul(exponent));
    // Repeated squaring needs ~4× result size in peak memory (old/new base + old/new accumulator
    // coexist during each multiplication step), and these are Rust heap allocations not tracked
    // by the resource tracker.
    check_estimated_size(result_bytes.saturating_mul(4), tracker)
}

/// Pre-checks that an integer multiplication won't exceed resource limits.
///
/// The result of multiplying two numbers has at most `a_bits + b_bits` bits.
pub fn check_mult_size(a_bits: u64, b_bits: u64, tracker: &ResourceTracker) -> Result<(), ResourceError> {
    check_estimated_size(estimate_bits_to_bytes(a_bits.saturating_add(b_bits)), tracker)
}

/// Pre-checks that a left shift won't exceed resource limits.
///
/// The result of `value << shift` has approximately `value_bits + shift` bits.
/// For zero values the result is always zero, so the check is skipped.
pub fn check_lshift_size(value_bits: u64, shift_amount: u64, tracker: &ResourceTracker) -> Result<(), ResourceError> {
    if value_bits == 0 {
        return Ok(());
    }
    check_estimated_size(estimate_bits_to_bytes(value_bits.saturating_add(shift_amount)), tracker)
}

/// Pre-checks that an integer division overflow promotion won't exceed resource limits.
///
/// Division results are bounded by the dividend size, but we still check for consistency
/// with other BigInt promotion paths.
pub fn check_div_size(dividend_bits: u64, tracker: &ResourceTracker) -> Result<(), ResourceError> {
    check_estimated_size(estimate_bits_to_bytes(dividend_bits), tracker)
}

/// Pre-checks that a string/bytes replace won't exceed resource limits before allocating.
///
/// This prevents DoS via expressions like `('a' * 1000).replace('a', 'b' * 10_000_000)`
/// where a small tracked input is amplified into a huge untracked Rust `String`/`Vec`
/// by `String::replace()` before `allocate_string()` can check the result.
///
/// The upper bound on result size is: if `old` is non-empty, at most `input_len / old_len`
/// replacements can occur, each producing `new_len` bytes instead of `old_len`. When `count`
/// is specified, replacements are capped to that value.
pub fn check_replace_size(
    input_len: usize,
    old_len: usize,
    new_len: usize,
    count: i64,
    tracker: &ResourceTracker,
) -> Result<(), ResourceError> {
    // Empty pattern (old_len == 0): inserts before each element + after the last = input_len + 1
    let max_replacements = input_len
        .checked_div(old_len)
        .unwrap_or_else(|| input_len.saturating_add(1));

    let replacements = if count < 0 {
        max_replacements
    } else {
        max_replacements.min(usize::try_from(count).unwrap_or(usize::MAX))
    };

    // Result = input_len - (replacements * old_len) + (replacements * new_len)
    let removed = replacements.saturating_mul(old_len);
    let added = replacements.saturating_mul(new_len);
    let estimated = input_len.saturating_sub(removed).saturating_add(added);

    check_estimated_size(estimated, tracker)
}

/// Checks an estimated result size against the resource tracker.
///
/// Only calls the tracker when the estimate exceeds `LARGE_RESULT_THRESHOLD`
/// to avoid overhead on small operations.
pub(crate) fn check_estimated_size(estimated_bytes: usize, tracker: &ResourceTracker) -> Result<(), ResourceError> {
    if estimated_bytes > LARGE_RESULT_THRESHOLD {
        tracker.check_large_result(estimated_bytes)?;
    }
    Ok(())
}

/// Converts an estimated bit count to bytes, saturating to `usize::MAX` on overflow.
///
/// Overflow means the result is astronomically large, so saturating ensures
/// the resource limit check always triggers rather than being silently skipped.
fn estimate_bits_to_bytes(bits: u64) -> usize {
    usize::try_from(bits.saturating_add(7) / 8).unwrap_or(usize::MAX)
}

/// Error returned when a resource limit is exceeded during execution.
///
/// This allows the sandbox to enforce strict limits on allocation count,
/// execution time, and memory usage.
#[derive(Debug, Clone)]
pub enum ResourceError {
    /// Maximum number of allocations exceeded.
    Allocation { limit: usize, count: usize },
    /// The attempted allocation count is not representable.
    AllocationUnrepresentable { limit: usize },
    /// Maximum execution time exceeded.
    Time { limit: Duration, elapsed: Duration },
    /// The attempted cumulative execution duration is not representable.
    TimeUnrepresentable { limit: Duration },
    /// Maximum memory usage exceeded.
    Memory { limit: usize, used: usize },
    /// The attempted memory usage is not representable.
    MemoryUnrepresentable { limit: usize },
    /// Maximum recursion depth exceeded.
    Recursion { limit: usize, depth: usize },
    /// The attempted recursion depth is not representable.
    RecursionUnrepresentable { limit: usize },
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allocation { limit, count } => {
                write!(f, "allocation limit exceeded: {count} > {limit}")
            }
            Self::AllocationUnrepresentable { limit } => {
                write!(
                    f,
                    "allocation limit exceeded: attempted count is unrepresentable (limit: {limit})"
                )
            }
            Self::Time { limit, elapsed } => {
                write!(f, "time limit exceeded: {elapsed:?} > {limit:?}")
            }
            Self::TimeUnrepresentable { limit } => {
                write!(
                    f,
                    "time limit exceeded: attempted duration is unrepresentable (limit: {limit:?})"
                )
            }
            Self::Memory { limit, used } => {
                write!(f, "memory limit exceeded: {used} bytes > {limit} bytes")
            }
            Self::MemoryUnrepresentable { limit } => {
                write!(
                    f,
                    "memory limit exceeded: attempted usage is unrepresentable (limit: {limit} bytes)"
                )
            }
            Self::Recursion { .. } | Self::RecursionUnrepresentable { .. } => {
                write!(f, "maximum recursion depth exceeded")
            }
        }
    }
}

impl Error for ResourceError {}

impl ResourceError {
    /// Converts this resource error to a Python exception with optional stack frame.
    ///
    /// Maps resource error types to Python exception types:
    /// - `Allocation` → `MemoryError`
    /// - `Memory` → `MemoryError`
    /// - `Time` → `TimeoutError`
    /// - `Recursion` → `RecursionError`
    #[must_use]
    pub(crate) fn into_exception(self, frame: Option<RawStackFrame>) -> ExceptionRaise {
        let exc_type = match &self {
            Self::Allocation { .. } | Self::AllocationUnrepresentable { .. } => ExcType::MemoryError,
            Self::Memory { .. } | Self::MemoryUnrepresentable { .. } => ExcType::MemoryError,
            Self::Time { .. } | Self::TimeUnrepresentable { .. } => ExcType::TimeoutError,
            Self::Recursion { .. } | Self::RecursionUnrepresentable { .. } => ExcType::RecursionError,
        };
        let exc = SimpleException::new(exc_type, Some(self.to_string()));
        match frame {
            Some(f) => exc.with_frame(f),
            None => exc.into(),
        }
    }
}

impl From<ResourceError> for RunError {
    fn from(err: ResourceError) -> Self {
        // RecursionError is catchable in CPython, so it must be catchable here too.
        // Other resource errors (memory, time, allocation) remain uncatchable to prevent
        // untrusted code from suppressing resource limit violations.
        if matches!(
            err,
            ResourceError::Recursion { .. } | ResourceError::RecursionUnrepresentable { .. }
        ) {
            Self::Exc(err.into_exception(None))
        } else {
            Self::UncatchableExc(err.into_exception(None))
        }
    }
}

/// Configuration for finite resource limits.
///
/// Every execution is bounded. Hosts which need effectively unrestricted execution
/// can configure ceilings high enough to be irrelevant for their workload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResourceLimits {
    /// Maximum number of heap allocations allowed.
    pub max_allocations: usize,
    /// Maximum cumulative execution time.
    pub max_duration: Duration,
    /// Maximum heap memory in bytes (approximate).
    pub max_memory: usize,
    /// Run garbage collection every N GC-tracked allocations.
    pub gc_interval: usize,
    /// Maximum function-call recursion depth.
    pub max_recursion_depth: usize,
}

/// Default maximum heap memory: 100 MB.
pub const DEFAULT_MAX_MEMORY: usize = 100_000_000;
/// Default maximum cumulative allocations.
pub const DEFAULT_MAX_ALLOCATIONS: usize = 10_000_000;
/// Default maximum cumulative execution duration.
pub const DEFAULT_MAX_DURATION: Duration = Duration::from_mins(1);
/// Default cycle-collection scheduling interval.
///
/// Memory-model checks collect at every opportunity to stress ownership paths.
pub const DEFAULT_GC_INTERVAL: usize = if cfg!(feature = "memory-model-checks") {
    1
} else {
    100_000
};
/// Recommended maximum recursion depth if not otherwise specified.
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 1000;

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_allocations: DEFAULT_MAX_ALLOCATIONS,
            max_duration: DEFAULT_MAX_DURATION,
            max_memory: DEFAULT_MAX_MEMORY,
            gc_interval: DEFAULT_GC_INTERVAL,
            max_recursion_depth: DEFAULT_MAX_RECURSION_DEPTH,
        }
    }
}

impl ResourceLimits {
    /// Creates resource limits using Monty's finite defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the maximum number of allocations.
    #[must_use]
    pub fn max_allocations(mut self, limit: usize) -> Self {
        self.max_allocations = limit;
        self
    }

    /// Sets the maximum execution duration.
    #[must_use]
    pub fn max_duration(mut self, limit: Duration) -> Self {
        self.max_duration = limit;
        self
    }

    /// Sets the maximum memory usage in bytes.
    #[must_use]
    pub fn max_memory(mut self, limit: usize) -> Self {
        self.max_memory = limit;
        self
    }

    /// Sets the garbage collection interval.
    #[must_use]
    pub fn gc_interval(mut self, interval: usize) -> Self {
        self.gc_interval = interval;
        self
    }

    /// Sets the maximum recursion depth.
    #[must_use]
    pub fn max_recursion_depth(mut self, limit: usize) -> Self {
        self.max_recursion_depth = limit;
        self
    }
}

/// How often to read the monotonic clock while executing bytecode.
const TIME_CHECK_INTERVAL: u16 = 10;

/// Tracks and enforces resources for one execution session.
///
/// Accounting uses interior mutability because heap allocations are permitted through
/// shared heap references. Allocation count and execution time are cumulative across
/// feeds, while current memory falls when values are freed.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ResourceTracker {
    limits: ResourceLimits,
    #[serde(default)]
    total_execution_time: Cell<Duration>,
    #[serde(default)]
    execution_time_overflowed: Cell<bool>,
    #[serde(skip)]
    running_since: Cell<Option<Instant>>,
    allocation_count: Cell<usize>,
    current_memory: Cell<usize>,
    check_counter: Cell<u16>,
    #[serde(default)]
    recursion_limit_override: Cell<Option<usize>>,
}

impl ResourceTracker {
    /// Creates a tracker with the supplied finite limits.
    #[must_use]
    pub fn new(limits: ResourceLimits) -> Self {
        Self {
            limits,
            total_execution_time: Cell::new(Duration::ZERO),
            execution_time_overflowed: Cell::new(false),
            running_since: Cell::new(None),
            allocation_count: Cell::new(0),
            current_memory: Cell::new(0),
            check_counter: Cell::new(0),
            recursion_limit_override: Cell::new(None),
        }
    }

    /// Creates a tracker with Monty's default limits.
    #[must_use]
    pub fn default_limits() -> Self {
        Self::new(ResourceLimits::default())
    }

    /// Creates a fresh accounting epoch for a validated restored heap.
    pub(crate) fn restored(
        limits: ResourceLimits,
        allocation_count: usize,
        current_memory: usize,
    ) -> Result<Self, ResourceError> {
        if allocation_count > limits.max_allocations {
            Err(ResourceError::Allocation {
                limit: limits.max_allocations,
                count: allocation_count,
            })
        } else if current_memory > limits.max_memory {
            Err(ResourceError::Memory {
                limit: limits.max_memory,
                used: current_memory,
            })
        } else {
            let tracker = Self::new(limits);
            tracker.allocation_count.set(allocation_count);
            tracker.current_memory.set(current_memory);
            Ok(tracker)
        }
    }

    /// Returns the live recursion ceiling.
    fn active_recursion_limit(&self) -> usize {
        self.recursion_limit_override
            .get()
            .unwrap_or(self.limits.max_recursion_depth)
    }

    /// Returns the current allocation count.
    #[must_use]
    pub fn allocation_count(&self) -> usize {
        self.allocation_count.get()
    }

    /// Returns the current approximate memory usage.
    #[must_use]
    pub fn current_memory(&self) -> usize {
        self.current_memory.get()
    }

    /// Returns cumulative bytecode execution time, including an active window.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        if self.execution_time_overflowed.get() {
            Duration::MAX
        } else {
            let running = self.running_since.get().map_or(Duration::ZERO, |t| t.elapsed());
            self.total_execution_time.get().saturating_add(running)
        }
    }

    /// Returns the configured cumulative execution ceiling.
    #[must_use]
    pub fn max_duration(&self) -> Duration {
        self.limits.max_duration
    }

    /// Replaces the execution duration with a fresh budget from now.
    pub fn set_max_duration(&mut self, duration: Duration) {
        self.limits.max_duration = duration;
        self.total_execution_time.set(Duration::ZERO);
        self.execution_time_overflowed.set(false);
        if self.running_since.get().is_some() {
            self.running_since.set(Some(Instant::now()));
        }
    }

    /// Returns the configured GC interval.
    #[must_use]
    pub(crate) fn gc_interval(&self) -> usize {
        self.limits.gc_interval
    }

    /// Charges one heap allocation, rejecting arithmetic overflow as exhaustion.
    pub(crate) fn on_allocate(&self, get_size: impl FnOnce() -> usize) -> Result<(), ResourceError> {
        let count = self.allocation_count.get();
        let new_count = count.checked_add(1).ok_or(ResourceError::AllocationUnrepresentable {
            limit: self.limits.max_allocations,
        })?;
        if new_count > self.limits.max_allocations {
            return Err(ResourceError::Allocation {
                limit: self.limits.max_allocations,
                count: new_count,
            });
        }

        let current = self.current_memory.get();
        let new_memory = current
            .checked_add(get_size())
            .ok_or(ResourceError::MemoryUnrepresentable {
                limit: self.limits.max_memory,
            })?;
        if new_memory > self.limits.max_memory {
            return Err(ResourceError::Memory {
                limit: self.limits.max_memory,
                used: new_memory,
            });
        }
        self.allocation_count.set(new_count);
        self.current_memory.set(new_memory);
        Ok(())
    }

    /// Removes the estimated memory charge for a freed heap object.
    pub(crate) fn on_free(&self, get_size: impl FnOnce() -> usize) {
        self.current_memory
            .set(self.current_memory.get().saturating_sub(get_size()));
    }

    /// Charges growth of an existing heap object.
    pub(crate) fn on_grow(&self, additional_bytes: usize) -> Result<(), ResourceError> {
        let new_memory =
            self.current_memory
                .get()
                .checked_add(additional_bytes)
                .ok_or(ResourceError::MemoryUnrepresentable {
                    limit: self.limits.max_memory,
                })?;
        if new_memory > self.limits.max_memory {
            Err(ResourceError::Memory {
                limit: self.limits.max_memory,
                used: new_memory,
            })
        } else {
            self.current_memory.set(new_memory);
            Ok(())
        }
    }

    /// Checks the cumulative execution duration.
    pub(crate) fn check_time(&self) -> Result<(), ResourceError> {
        self.check_counter.update(|c| c.wrapping_add(1));
        if self.limits.max_duration.is_zero() || self.check_counter.get().is_multiple_of(TIME_CHECK_INTERVAL) {
            if self.execution_time_overflowed.get() {
                return Err(ResourceError::TimeUnrepresentable {
                    limit: self.limits.max_duration,
                });
            }
            let running = self.running_since.get().map_or(Duration::ZERO, |t| t.elapsed());
            let elapsed =
                self.total_execution_time
                    .get()
                    .checked_add(running)
                    .ok_or(ResourceError::TimeUnrepresentable {
                        limit: self.limits.max_duration,
                    })?;
            if elapsed > self.limits.max_duration {
                self.check_counter.set(TIME_CHECK_INTERVAL.wrapping_sub(1));
                return Err(ResourceError::Time {
                    limit: self.limits.max_duration,
                    elapsed,
                });
            }
        }
        Ok(())
    }

    /// Checks whether another call frame may be pushed.
    pub(crate) fn check_recursion_depth(&self, current_depth: usize) -> Result<(), ResourceError> {
        let max = self.active_recursion_limit();
        if current_depth >= max {
            let depth = current_depth
                .checked_add(1)
                .ok_or(ResourceError::RecursionUnrepresentable { limit: max })?;
            Err(ResourceError::Recursion { limit: max, depth })
        } else {
            Ok(())
        }
    }

    /// Checks an estimated large result before constructing it.
    pub(crate) fn check_large_result(&self, estimated_bytes: usize) -> Result<(), ResourceError> {
        let used =
            self.current_memory
                .get()
                .checked_add(estimated_bytes)
                .ok_or(ResourceError::MemoryUnrepresentable {
                    limit: self.limits.max_memory,
                })?;
        if used > self.limits.max_memory {
            Err(ResourceError::Memory {
                limit: self.limits.max_memory,
                used,
            })
        } else {
            Ok(())
        }
    }

    /// Starts one non-nested bytecode execution window.
    pub(crate) fn on_execution_start(&self) {
        debug_assert!(self.running_since.get().is_none(), "nested execution resource window");
        self.running_since.set(Some(Instant::now()));
    }

    /// Stops the active execution window and accumulates its duration.
    pub(crate) fn on_execution_stop(&self) {
        if let Some(started) = self.running_since.take() {
            if let Some(total) = self.total_execution_time.get().checked_add(started.elapsed()) {
                self.total_execution_time.set(total);
            } else {
                self.execution_time_overflowed.set(true);
            }
        }
    }

    /// Lowers the live recursion ceiling without allowing sandbox code to raise it.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn lower_recursion_limit(&self, new_limit: usize) -> Result<(), Option<usize>> {
        let current = self.active_recursion_limit();
        if new_limit > current {
            Err(Some(current))
        } else {
            self.recursion_limit_override.set(Some(new_limit));
            Ok(())
        }
    }
}

impl Default for ResourceTracker {
    fn default() -> Self {
        Self::default_limits()
    }
}
