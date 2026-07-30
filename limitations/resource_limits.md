# Resource limits

Monty enforces hard limits on memory, time, and recursion to keep untrusted
code bounded. Memory limits surface to the host as terminal `MemoryError`s,
while time limits surface as terminal `TimeoutError`s; sandboxed code cannot
catch them. `RecursionError` is catchable, as in CPython.

## Compilation

`ResourceLimits` starts when the VM is created; parsing, preparation, and
bytecode compilation are not charged to its memory or duration budgets.
Compilation has separate structural caps for parser nesting, bytecode operand
sizes, comprehension nesting, and repeated `finally` expansion. In particular,
a code object requiring more than 1,024 emitted copies of `finally` bodies is
rejected with `SyntaxError`; CPython has no equivalent limit. Production hosts
should still isolate compilation when accepting untrusted source, as the
subprocess and WebAssembly runtimes do.

## Memory / size limits

- Memory tracking is global; the host sets the bytes budget when
  constructing the VM.
- The byte count is **approximate**: per-object sizing uses `py_estimate_size`,
  which elides bookkeeping overhead (HashMap bucket padding, `Vec` capacity
  slack, `SmallVec` inline buffers, scheduler queue allocations) and rounds
  per-spawn task overhead to a fixed conservative constant. The configured
  `max_memory` is a budget on user-visible data, not a hard ceiling on
  process RSS.
- Operations whose result is bounded by simple arithmetic on input sizes
  are **pre-checked** before allocating: integer multiplication, left
  shift, integer power, sequence repeat (`'x' * n`), replacement
  (`str.replace`, `bytes.replace`), padding (`str.ljust`, `str.center`,
  `str.zfill`, `bytes.ljust`, …), and f-string formatting
  (both dynamic width `f"{v:>{w}}"` and dynamic precision on float
  formats `f"{v:.{p}f}"` / `e` / `%`). The pre-check threshold is 100 KB —
  estimates above that are checked against the remaining budget and rejected
  with `MemoryError` before allocation when they would exceed it.
- `bigint.pow(base, exp)` estimates result size as `bits(base) * exp` with
  a 4× safety multiplier to cover repeated-squaring intermediate values.

## Hard address-space ceiling (worker pools, Linux only)

`max_memory` is enforced by the interpreter's own tracker, so it only bounds
allocations the interpreter remembers to account for. Hosts that spawn worker
subprocesses can set a second, independent ceiling below the interpreter —
`PoolConfig::worker_address_space_limit` in Rust,
`Monty(worker_address_space_limit=...)` in Python, `workerAddressSpaceLimit` in
JavaScript — which the worker applies to itself as `RLIMIT_AS` before serving
any request. Divergences from every other limit documented here:

- **It is not a Python-visible error.** A breach fails an allocation the
  interpreter cannot handle, so the worker aborts (`SIGABRT`) mid-turn. The host
  sees a crash (`PoolError::Crashed` / `MontyCrashedError`), *not* a
  `MemoryError`, and the session is lost — unlike `max_memory`, where the worker
  survives and stays usable. Sandboxed code cannot observe or catch it.
- **Linux only.** Darwin refuses to set `RLIMIT_AS` (aliased onto `RLIMIT_RSS`)
  or `RLIMIT_DATA` at all, and Windows has no rlimits. On those platforms the
  worker prints a warning to stderr and runs unbounded, so the ceiling is a
  production backstop, not a portable guarantee.
- **It bounds virtual address space, not live heap.** Thread stacks, allocator
  arena reservations and file mappings all count against it, so the value must
  sit well above the `max_memory` budget it backstops — a few hundred MiB of
  headroom at minimum. Too tight a ceiling kills healthy workers, most likely on
  the first type-checked feed (typeshed and salsa caches load then).
- **Per process, not per session.** The ceiling is fixed at pool creation and
  never re-derived, so a recycled worker's ceiling still covers whatever residue
  earlier sessions left behind.
- **Never raised.** The requested value is clamped down to any lower limit
  already inherited (container, `ulimit -v`), and both the soft and hard limits
  are set, so nothing in the process can lift it afterwards.
- Only the subprocess transport applies it: WebSocket workers are remote
  processes this pool does not spawn, and the wasm worker has no rlimits.

## Integer-specific caps

- `pow(base, exp)` / `base ** exp` with an exponent larger than `u32::MAX`
  (≈ 4.3 × 10⁹) raises `OverflowError: "exponent too large"`.
- `pow(base, exp, mod)` requires all integer arguments and rejects negative
  exponents (`ValueError`).
- `int(str_or_bytes, base)` rejects inputs over 4,300 digits before the
  potentially quadratic BigInt parse when the effective base is not a power
  of two. The fixed cap matches CPython's
  `sys.int_info.default_max_str_digits`.

## Recursion

- Python-level call depth is hardcoded at **1000 frames**. The 1001st
  nested call raises `RecursionError`.
- Production sandbox code cannot change the recursion limit. Test builds may
  expose `sys.setrecursionlimit()` as a lowering-only fixture hook; it cannot
  raise the host-configured ceiling.
- Async stacks count toward the limit but each `await` boundary is treated
  as one frame, so `await`-chains do not amplify depth.
- Callbacks evaluated synchronously by the interpreter itself re-enter on the
  native Rust call stack rather than the heap-allocated frame stack used by
  ordinary function calls. This includes `map()`, `filter()`,
  `sorted()`/`list.sort(key=...)`, `min()`/`max(key=...)`, recursive
  `__repr__`/`__str__`, and non-plain-function `__init__` values that recurse
  during construction. Native re-entry is capped independently at a lower
  fixed depth than the 1000-frame Python limit, so Monty raises
  `RecursionError` before a native stack overflow would abort the process. See
  `limitations/classes.md`'s `__repr__`/`__str__` entry for the main
  user-visible divergence this causes.

## Time

- The host can set a `max_duration` budget; if exceeded the VM stops on
  the next bytecode boundary with `ResourceError`.
- The budget covers cumulative **execution time**, not wall-clock time:
  the clock runs only while the interpreter executes bytecode, and is
  paused while execution is suspended waiting on the host (external
  function calls, OS callbacks) and between REPL feeds. It accumulates
  across feeds for the life of the session.
- The accumulated time is serialized into dumps/snapshots, so a restored
  session resumes its budget where it left off rather than restarting
  from zero.
- There is no in-sandbox way to observe the budget or remaining time.

## JSON

- `json.loads` rejects input nested deeper than 200 levels with
  `json.JSONDecodeError` (independent of the Python recursion limit).

## After a terminal resource error

After a memory or time limit fires, **no guarantees are made about
heap state or reference counts**. The host should discard the VM rather than
try to recover and continue running code in it. A caught `RecursionError` does
not invalidate the VM and execution may continue inside the sandbox.
