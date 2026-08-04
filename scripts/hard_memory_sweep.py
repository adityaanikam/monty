"""
Measure what `worker_hard_memory_limit` actually costs, to derive guidance.

The ceiling counts live allocated bytes, so the floor is whatever heap the
worker itself holds — the session machinery, and (with type checking) typeshed
plus salsa. That is far steadier than it was under `RLIMIT_AS`, where the
binary's own mapped text and every thread stack counted too, but it is still
measured per build rather than asserted — hence this sweep.

    make dev-py && uv run scripts/hard_memory_sweep.py

Thresholds are bisected, so this spawns a few hundred workers over a few minutes.
Lines like `monty worker: allocation of N bytes exceeds the memory ceiling` are
the worker's own stderr, which the pool inherits — expected here, not an error. Output is
deliberately narrow, for pasting back into an issue or PR.
"""

from __future__ import annotations

import os
import platform
import sys
import time
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from pydantic_monty import Monty, MontyError, ResourceLimits
from pydantic_monty._binary import find_monty_binary

MIB = 1024**2

# --- knobs ---------------------------------------------------------------------
# Thresholds are bisected rather than laddered: the pass/fail boundary sits within
# a MiB or two of `worker VA + peak sandbox bytes`, which a coarse ladder rounds
# away entirely. `SEARCH_HI` must be comfortably above any threshold probed.
SEARCH_LO_MIB, SEARCH_HI_MIB = 8, 1536
# Resolution of the bisection, in MiB. 1 costs ~8 extra probes per threshold.
RESOLUTION_MIB = 1
# Sandbox `max_memory` budgets to find the required ceiling for, in MiB.
BUDGETS_MIB = [8, 16, 32, 64, 128, 256]
# Single-allocation sizes to find the required ceiling for, in MiB.
ALLOC_MIB = [1, 8, 32, 128, 256]
# Type-checking a feed loads typeshed and salsa, which is the big VA consumer.
TYPE_CHECK_MODES = [False, True]


@dataclass
class Outcome:
    """One cell's result: a short code for the table plus the raw detail."""

    code: str
    detail: str = ''

    def __str__(self) -> str:
        return self.code


# Unexpected outcomes seen anywhere in the sweep. A bisection reads any failure as
# "ceiling too small", so a crash for an unrelated reason would quietly shift every
# threshold — collect them and say so rather than reporting a confident wrong number.
UNEXPECTED: list[str] = []

# `ok` ran to completion; `soft` hit the in-sandbox tracker and left the worker
# alive; `hard` breached the ceiling and killed the worker; `no-start` means the
# worker could not serve even `1 + 1`, i.e. the ceiling is below its floor.
OK, SOFT, HARD, NO_START, OTHER = 'ok', 'soft', 'hard', 'no-start', 'other'


def classify(exc: BaseException) -> Outcome:
    """Maps an exception to a cell code by the message the pool produced."""
    message = str(exc)
    if 'memory limit exceeded' in message:
        return Outcome(SOFT, message)
    if 'exceeded its memory ceiling' in message:
        return Outcome(HARD, message)
    return Outcome(OTHER, f'{type(exc).__name__}: {message}')


def run_cell(
    ceiling: int | None,
    code: str,
    *,
    max_memory: int | None = None,
    type_check: bool = False,
) -> Outcome:
    """Runs one snippet in a fresh pool under `ceiling`, classifying the result.

    Probes `1 + 1` first: a worker that cannot manage even that is reported as
    `no-start` rather than blamed on the snippet under test.
    """
    limits: ResourceLimits | None = None if max_memory is None else {'max_memory': max_memory}
    kwargs = {} if ceiling is None else {'worker_hard_memory_limit': ceiling}
    try:
        with Monty(**kwargs) as pool:  # pyright: ignore[reportArgumentType]
            with pool.checkout(limits=limits, type_check=type_check) as session:
                try:
                    session.feed_run('1 + 1', skip_type_check=True)
                except MontyError as exc:
                    # A worker that cannot manage `1 + 1` is `no-start`, whatever
                    # memory outcome it reported. Anything else is unrelated to
                    # the ceiling and must reach the summary rather than silently
                    # reading as "ceiling too small" and shifting a threshold.
                    outcome = record(classify(exc))
                    return outcome if outcome.code == OTHER else Outcome(NO_START, outcome.detail)
                session.feed_run(code)
                return Outcome(OK)
    except MontyError as exc:
        return record(classify(exc))
    except Exception as exc:  # a diagnostic sweep reports every cell, never aborts
        return record(Outcome(OTHER, f'{type(exc).__name__}: {exc}'))


def record(outcome: Outcome) -> Outcome:
    """Notes an `other` outcome for the end-of-run summary, then passes it on."""
    if outcome.code == OTHER and outcome.detail not in UNEXPECTED:
        UNEXPECTED.append(outcome.detail)
    return outcome


def alloc(mib: int) -> str:
    """A snippet allocating roughly `mib` MiB as one string."""
    return f"x = ' ' * ({mib} * 1024 * 1024)"


def fill() -> str:
    """Retains 64 KiB pieces until the tracker refuses, peaking at `max_memory`.

    Sequence repeat is *pre-checked* against the tracker above 100 KB, so one
    huge `' ' * n` never reaches the allocator once `max_memory` is set and would
    measure nothing about the ceiling. Pieces under that threshold take the
    ordinary tracked path, so the gap between tracked bytes and allocated ones
    (capacity slack, per-object overhead) lands against the ceiling as a real
    workload's would.
    """
    return "xs = []\nwhile True:\n    xs.append(' ' * (64 * 1024))"


def worker_vm(type_check: bool) -> str:
    """Reads the worker's own VA/RSS from /proc, for context on the floor below.

    The ceiling counts requested bytes while these count mapped and resident
    pages, so they never match exactly — allocator overhead, fragmentation and
    the binary's own image sit between them. Linux only, since it reads /proc.
    """
    if sys.platform != 'linux':
        return 'unavailable: needs /proc'
    with Monty() as pool:
        with pool.checkout(type_check=type_check) as session:
            session.feed_run('x: int = 1' if type_check else '1 + 1')
            pid = session.worker_pid
            if pid is None:
                return 'worker_pid unavailable'
            try:
                status = Path(f'/proc/{pid}/status').read_text()
            except OSError as exc:
                return f'unreadable: {exc}'
    wanted = ('VmPeak', 'VmSize', 'VmRSS', 'Threads')
    fields: dict[str, str] = {}
    for line in status.splitlines():
        key, _, value = line.partition(':')
        if key in wanted:
            fields[key] = value.strip()
    return '  '.join(f'{k}={fields.get(k, "?")}' for k in wanted)


def bisect_min(works: Callable[[int], bool]) -> int | None:
    """Smallest ceiling in MiB for which `works` holds, to `RESOLUTION_MIB`.

    Assumes monotonicity — more address space never breaks a worker that already
    fitted. Returns `None` when even `SEARCH_HI_MIB` fails, which means the
    workload does not fit any probed ceiling rather than that the floor is high.
    """
    if not works(SEARCH_HI_MIB):
        return None
    lo, hi = SEARCH_LO_MIB, SEARCH_HI_MIB
    if works(lo):
        return lo
    # invariant: `lo` fails, `hi` works
    while hi - lo > RESOLUTION_MIB:
        mid = (lo + hi) // 2
        if works(mid):
            hi = mid
        else:
            lo = mid
    return hi


def probe_baseline() -> None:
    """What a worker actually occupies, for context on the measured floor."""
    print('== worker footprint (no ceiling) ==')
    for type_check in TYPE_CHECK_MODES:
        label = 'type_check=True ' if type_check else 'type_check=False'
        print(f'  {label}  {worker_vm(type_check)}')
    print()


def probe_floor() -> None:
    """The smallest ceiling under which a worker can serve a trivial feed at all.

    This is the number below which the knob yields no usable workers, whatever
    `max_memory` says — it is the worker's own heap, nothing to do with the
    sandbox budget.
    """
    print('== floor: smallest ceiling that serves a trivial feed ==')
    for type_check in TYPE_CHECK_MODES:
        code = 'x: int = 1' if type_check else '1 + 1'
        floor = bisect_min(lambda mib, c=code, t=type_check: run_cell(mib * MIB, c, type_check=t).code == OK)
        label = 'type_check=True ' if type_check else 'type_check=False'
        print(f'  {label}  {f"{floor} MiB" if floor else "no ceiling worked"}')
    print()


def probe_headroom() -> None:
    """Smallest ceiling at which `max_memory` — not the kernel — stops a workload.

    A well-sized ceiling is invisible: the tracker fires first, as a catchable
    `MemoryError` that leaves the worker alive. The workload runs until the
    tracker stops it, so the ceiling is measured against a peak of exactly
    `max_memory` — the worst case the budget permits, not a sample below it.
    Reports the ceiling minus the budget, which is the number to add to
    `max_memory` when picking one.
    """
    for type_check in TYPE_CHECK_MODES:
        print(f'== smallest ceiling that keeps max_memory in charge (type_check={type_check}) ==')
        print('  max_memory   ceiling   ceiling-max_memory')
        for budget in BUDGETS_MIB:
            # exhausting the budget must end as the tracker's refusal, not a hard kill
            found = bisect_min(
                lambda mib, b=budget, t=type_check: (
                    run_cell(mib * MIB, fill(), max_memory=b * MIB, type_check=t).code == SOFT
                )
            )
            if found is None:
                print(f'  {budget:>7} MiB   (none of the probed ceilings behaved as a backstop)')
            else:
                print(f'  {budget:>7} MiB   {found:>4} MiB   +{found - budget} MiB')
        print()


def probe_ceiling_per_alloc() -> None:
    """Smallest ceiling that survives a single allocation of N MiB, no `max_memory`.

    With no budget the tracker never intervenes, so this isolates the allocator's
    boundary: the result should be roughly the worker's own heap plus N,
    which is what makes the guidance a sum rather than a percentage.
    """
    for type_check in TYPE_CHECK_MODES:
        print(f'== smallest ceiling per allocation, no max_memory (type_check={type_check}) ==')
        print('  allocation   ceiling   ceiling-allocation')
        for mib in ALLOC_MIB:
            found = bisect_min(
                lambda ceiling, a=mib, t=type_check: run_cell(ceiling * MIB, alloc(a), type_check=t).code == OK
            )
            if found is None:
                print(f'  {mib:>7} MiB   (no probed ceiling fitted it)')
            else:
                print(f'  {mib:>7} MiB   {found:>4} MiB   +{found - mib} MiB')
        print()


def main() -> int:
    print(f'platform    {platform.platform()}')
    print(f'python      {sys.version.split()[0]}')
    print(f'cpus        {os.cpu_count()}')
    # a debug build allocates differently (and type-checks far more slowly), so
    # always report which binary produced the numbers
    try:
        binary = find_monty_binary()
        print(f'binary      {binary} ({os.path.getsize(binary) / MIB:.0f} MiB)')
    except Exception as exc:
        print(f'binary      unresolved: {exc}')
    print()

    started = time.monotonic()
    probe_baseline()
    probe_floor()
    probe_headroom()
    probe_ceiling_per_alloc()
    print(f'done in {time.monotonic() - started:.0f}s')
    if UNEXPECTED:
        print('\nunexpected outcomes (thresholds above may be wrong):')
        for detail in UNEXPECTED:
            print(f'  {detail}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
