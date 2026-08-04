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

## Hard memory ceiling (worker pools)

`max_memory` is enforced by the interpreter's own tracker, so it only bounds
allocations the interpreter remembers to account for. A worker subprocess
therefore backstops it with a second ceiling below the interpreter, enforced in
its own global allocator by counting live bytes. There is nothing to configure:
setting `max_memory` on a session arms it, and the ceiling is `5 × max_memory`,
plus the worker's own baseline footprint, plus fixed headroom (4 MiB, or 32 MiB
when type checking) for machinery no `max_memory` covers. A session with no
`max_memory` gets no ceiling. Divergences from every other limit documented
here:

- **It kills the worker, though it still reports `MemoryError`.** A breach fails
  an allocation the interpreter cannot handle, so the worker exits mid-turn. The
  host gets `PoolError::Runtime` / `MontyRuntimeError` wrapping a `MemoryError`
  whose message names the ceiling — but unlike every other runtime error, and
  unlike an in-sandbox `max_memory` breach, the session is gone with the worker
  (later calls on that checkout report `Finished`; the pool itself recovers).
  Sandboxed code cannot observe or catch it.
- **It binds the worker's allocator, not the process.** Only bytes requested
  from Rust's global allocator are counted, which is everything sandboxed code
  can cause to be allocated, but not memory obtained another way: thread stacks,
  the binary's own mapped image, or a direct `mmap`. It is a backstop under the
  interpreter's tracker, not a kernel-enforced bound on process memory — an
  inherited `ulimit -v` or cgroup limit is the tool for that, and still applies
  independently (a worker whose allocation the kernel then refuses reports the
  same `MemoryError`).
- **It counts requested bytes, not resident ones.** Per-allocation overhead and
  fragmentation sit between the count and the process's real footprint, so RSS
  runs somewhat above the ceiling.
- **Nothing the interpreter allocates can reach it.** The multiple covers what
  the tracker undercounts — the per-object arena slot, `Vec`/`HashMap` capacity
  slack, allocator rounding — which is worst (~3.8×) for the smallest objects.
  A tracked allocation therefore raises the in-sandbox `MemoryError` at
  `max_memory` first, five times lower, leaving the worker alive; the ceiling
  only fires for allocations outside the tracker's view.
- **`max_memory` alone does not bound worker memory.** A tiny budget still
  leaves the worker the fixed headroom above, which is deliberately generous:
  too tight a ceiling kills healthy workers, most often on the first
  type-checked feed (typeshed and salsa caches load then). Use `max_processes`
  and an OS-level limit to bound a host, not this.
- **Per session, but against a fixed baseline.** A worker serves many checkouts
  and re-derives the ceiling for each session's budget, always from the leanest
  the process has been. Memory retained between sessions — the type checker
  keeps a small pool of salsa databases alive process-wide — therefore consumes
  the headroom instead of raising the ceiling, and a worker whose residue
  outgrows it is killed and replaced rather than allowed to grow indefinitely.
- **Restoring a dump is bounded by the checkout it lands in.** `load_session` /
  `load_snapshot` restore the dump's own limits (see
  `limitations/pool-architecture.md`), and the ceiling is re-derived from them
  once the session exists — but the load *itself* runs under the ceiling the
  `checkout()` config armed. Restoring a large dump into a checkout with a much
  smaller `max_memory` can therefore breach the ceiling while loading; pass a
  comparable budget to `checkout()`.
- Only worker subprocesses apply it: WebSocket workers are remote processes this
  pool does not spawn, and the wasm worker (a module, not a binary) declares no
  allocator of its own.

Independently of any ceiling, **any** allocation a worker's allocator refuses —
plain host OOM, or a request beyond the usable address space such as
`' ' * (1 << 60)` — takes this same path on every platform: the worker exits and
the host sees that `MemoryError` with its session gone. CPython raises a
catchable `MemoryError` in-process and carries on. Monty cannot: the failure
happens below the interpreter, where no Python-level exception can be raised, so
the worker classifies the failure into a dedicated exit code and dies. (Without
that, the process would abort with `SIGABRT` — indistinguishable from a stack
overflow, which is why the sandbox exits deliberately instead.)

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
