//! `ResourceLimits` ↔ `pb::ResourceLimits` conversions.
//!
//! Wire fields are optional overrides of Monty's finite defaults. Wire integers
//! saturate to `usize::MAX` on narrower hosts; durations use integer microseconds.

use std::time::Duration;

use monty::ResourceLimits;

use crate::pb;

impl From<&ResourceLimits> for pb::ResourceLimits {
    fn from(limits: &ResourceLimits) -> Self {
        Self {
            max_allocations: Some(limits.max_allocations as u64),
            max_duration_micros: Some(u64::try_from(limits.max_duration.as_micros()).unwrap_or(u64::MAX)),
            max_memory_bytes: Some(limits.max_memory as u64),
            gc_interval: Some(limits.gc_interval as u64),
            max_recursion_depth: Some(limits.max_recursion_depth as u64),
        }
    }
}

impl From<pb::ResourceLimits> for ResourceLimits {
    fn from(limits: pb::ResourceLimits) -> Self {
        let mut output = Self::default();
        if let Some(value) = limits.max_allocations {
            output.max_allocations = narrow_usize(value);
        }
        if let Some(value) = limits.max_duration_micros {
            output.max_duration = Duration::from_micros(value);
        }
        if let Some(value) = limits.max_memory_bytes {
            output.max_memory = narrow_usize(value);
        }
        if let Some(value) = limits.gc_interval {
            output.gc_interval = narrow_usize(value);
        }
        if let Some(value) = limits.max_recursion_depth {
            output.max_recursion_depth = narrow_usize(value);
        }
        output
    }
}

/// Narrows a wire integer, saturating on hosts with a narrower pointer width.
fn narrow_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}
