# monty-alloc

The global allocator [Monty](https://github.com/pydantic/monty) workers run
under: it counts live bytes against a ceiling derived from the sandbox session's
`max_memory`, and ends the process deliberately when that ceiling — or the
system allocator itself — refuses.

Monty executes untrusted Python. Its interpreter tracks memory itself
(`ResourceLimits::max_memory`), but only what it remembers to account for, so a
worker backstops that budget down here where nothing can be missed. Counting
rather than asking the kernel for `RLIMIT_AS` is what makes the ceiling portable
and its units meaningful: it bounds bytes the process asked for, not virtual
address space, so mapped text, thread stacks and file mappings do not consume
the budget. The tradeoff is that it binds only what reaches this allocator —
everything sandboxed code can allocate, but not a direct `mmap`.

```rust
#[global_allocator]
static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;

// After each request, from the session the worker now holds.
monty_alloc::arm_ceiling(Some(8 * 1024 * 1024), false);
```

The ceiling is `5 × max_memory`, plus the worker's own baseline footprint, plus
fixed headroom for machinery no `max_memory` covers. `None` lifts it. See
`limitations/resource_limits.md` in the repository for the reasoning behind
those numbers and every way the ceiling diverges from an in-sandbox limit.

## Ending the process

A breach cannot raise a Python exception — it happens below the interpreter —
so the worker dies and its host replaces it. Neither a panic (whose machinery
allocates) nor a plain abort will do: `SIGABRT` is also what a stack overflow
produces, and a host that cannot tell those apart cannot report `MemoryError`.

The `exit-code` feature picks how the process ends:

- **on** — `process::exit(monty_proto::OOM_EXIT_CODE)`, the dedicated status a
  parent reads to classify the death. Used by the `monty subprocess` worker,
  whose parent is [`monty-pool`](https://crates.io/crates/monty-pool).
- **off** (default) — `process::abort()`, which on wasm is a trap. A wasm
  module has no exit status to offer, and its host already treats a turn that
  ends without a terminating event as a dead instance.

Only a binary or a wasm module may declare a `#[global_allocator]`: in a native
cdylib it would hijack the allocator of the embedding host process.

## Only crates in this workspace

Published so the `monty` binary can be, not for direct use. On a 32-bit target
a budget beyond ~800 MiB saturates the ceiling arithmetic and leaves the worker
uncapped.
