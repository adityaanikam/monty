"""Pool lifecycle tests: checkout/reuse, crash recovery, timeouts, concurrency."""

from __future__ import annotations

import asyncio
import os
import signal
import sys
import threading

import pytest
from conftest import RunMonty
from inline_snapshot import snapshot

from pydantic_monty import AsyncMonty, Monty, MontyCrashedError, MontyRuntimeError, MontySession


def test_basic_execution(monty_run: RunMonty):
    assert monty_run('1 + 2') == snapshot(3)


def test_session_state_persists_across_feeds(pool: Monty):
    with pool.checkout() as session:
        session.feed_run('x = 40')
        assert session.feed_run('x + 2') == snapshot(42)


def test_sessions_are_isolated(pool: Monty):
    with pool.checkout() as session:
        session.feed_run('leaky = 1')
    # a new session reuses the worker process but never its state
    with pool.checkout() as session:
        with pytest.raises(MontyRuntimeError) as exc_info:
            session.feed_run('leaky')
        assert exc_info.value.display(format='msg') == snapshot("name 'leaky' is not defined")


def test_runtime_error_preserves_session(pool: Monty):
    with pool.checkout() as session:
        session.feed_run('kept = 41')
        with pytest.raises(MontyRuntimeError) as exc_info:
            session.feed_run('1 / 0')
        assert exc_info.value.display(format='msg') == snapshot('division by zero')
        # the session (and its globals) survives the error
        assert session.feed_run('kept + 1') == snapshot(42)


def test_pool_not_entered():
    pool = Monty()
    session = pool.checkout()
    with pytest.raises(RuntimeError) as exc_info:
        session.__enter__()
    assert exc_info.value.args[0] == snapshot(
        'the pool is not active — enter the Monty / AsyncMonty context manager first'
    )


def test_dump_returns_bytes(session: MontySession) -> None:
    session.feed_run('x = 1')
    state = session.dump()
    assert isinstance(state, bytes)
    assert len(state) > 0


DUMP_KEY = b'an example 32-byte test dump key'
TAMPERED_DUMP_MESSAGE = (
    'invalid dump: signature verification failed — the dump was signed with a different key or corrupted'
)


def test_dump_key_cross_pool_restore():
    with Monty(dump_key=DUMP_KEY) as pool_a:
        with pool_a.checkout() as session:
            session.feed_run('x = 42')
            state = session.dump()
    with Monty(dump_key=DUMP_KEY) as pool_b:
        with pool_b.checkout() as session:
            session.load(state)
            assert session.feed_run('x') == snapshot(42)


def test_tampered_dump_raises_value_error(pool: Monty):
    with pool.checkout() as session:
        session.feed_run('x = 1')
        state = session.dump()
    tampered = state[:-1] + bytes([state[-1] ^ 0xFF])
    with pool.checkout() as session:
        with pytest.raises(ValueError) as exc_info:
            session.load(tampered)
        assert exc_info.value.args[0] == snapshot(TAMPERED_DUMP_MESSAGE)
        # the rejection happened before any worker I/O, so the session stays
        # fresh: the load is retryable with the untampered bytes
        session.load(state)
        assert session.feed_run('x') == snapshot(1)


def test_ephemeral_dump_cross_pool_raises(pool: Monty):
    # `pool` configures no dump_key, so its dumps are signed with a random
    # per-pool key and only restore into that same pool object
    with pool.checkout() as session:
        session.feed_run('x = 1')
        state = session.dump()
    with pool.checkout() as session:
        session.load(state)  # same pool: the ephemeral key verifies
    with Monty() as other_pool:
        with other_pool.checkout() as session:
            with pytest.raises(ValueError) as exc_info:
                session.load(state)
            assert exc_info.value.args[0] == snapshot(TAMPERED_DUMP_MESSAGE)
            # the rejected session stays fresh and usable
            assert session.feed_run('1 + 1') == snapshot(2)


def test_str_dump_key_encodes_to_the_same_key_as_bytes():
    # a str key is UTF-8-encoded: pool A keyed with the str restores in pool B
    # keyed with the equivalent bytes
    with Monty(dump_key=DUMP_KEY.decode()) as pool_a:
        with pool_a.checkout() as session:
            session.feed_run('x = 42')
            state = session.dump()
    with Monty(dump_key=DUMP_KEY) as pool_b:
        with pool_b.checkout() as session:
            session.load(state)
            assert session.feed_run('x') == snapshot(42)


def test_short_dump_key_raises():
    with pytest.raises(ValueError) as exc_info:
        Monty(dump_key=b'short')
    assert exc_info.value.args[0] == snapshot('dump key must be at least 16 bytes')
    with pytest.raises(ValueError) as exc_info:
        Monty(dump_key='short')
    assert exc_info.value.args[0] == snapshot('dump key must be at least 16 bytes')


def test_wrong_type_dump_key_raises():
    with pytest.raises(TypeError) as exc_info:
        Monty(dump_key=123)  # pyright: ignore[reportArgumentType]
    assert exc_info.value.args[0] == snapshot('dump_key must be str or bytes, not int')


def test_worker_crash_raises_crashed_error_and_pool_recovers(pool: Monty):
    with pool.checkout() as session:
        pid = session.worker_pid
        assert pid is not None

        def kill_soon() -> None:
            os.kill(pid, signal.SIGKILL if sys.platform != 'win32' else signal.SIGTERM)

        killer = threading.Timer(0.2, kill_soon)
        killer.start()
        with pytest.raises(MontyCrashedError) as exc_info:
            session.feed_run('while True:\n    pass')
        killer.join()
        assert exc_info.value.timed_out is False

    # the pool replaces the dead worker transparently
    with pool.checkout() as session:
        assert session.feed_run('1 + 1') == snapshot(2)


def test_worker_pid_mid_turn_does_not_deadlock(pool: Monty):
    # regression: `worker_pid` used to block on the checkout lock while
    # holding the GIL; with the turn thread blocked in a print callback
    # needing the GIL, both threads deadlocked. The getter is now
    # non-blocking and returns None while a turn is in flight.
    with pool.checkout() as session:
        in_print = threading.Event()
        pid_checked = threading.Event()
        mid_turn_pid: list[int | None] = []

        def on_print(stream: str, text: str) -> None:
            in_print.set()
            # hold the turn (and the checkout lock) until the getter has run
            pid_checked.wait(timeout=10)

        def poll_pid() -> None:
            in_print.wait(timeout=10)
            mid_turn_pid.append(session.worker_pid)
            pid_checked.set()

        poller = threading.Thread(target=poll_pid)
        poller.start()
        session.feed_run("print('x')", print_callback=on_print)
        poller.join(timeout=10)
        assert not poller.is_alive()
        assert mid_turn_pid == [None]
        # idle again: the getter sees the worker
        assert session.worker_pid is not None


def test_request_timeout_kills_hung_worker():
    with Monty(request_timeout=0.3) as pool:
        with pool.checkout() as session:
            with pytest.raises(MontyCrashedError) as exc_info:
                session.feed_run('while True:\n    pass')
            assert exc_info.value.timed_out is True
        with pool.checkout() as session:
            assert session.feed_run('2 + 2') == snapshot(4)


def test_concurrent_sessions_run_in_parallel(pool: Monty):
    results: list[object] = []

    def worker(value: int) -> None:
        with pool.checkout() as session:
            results.append(session.feed_run('v * 2', inputs={'v': value}))

    threads = [threading.Thread(target=worker, args=(i,)) for i in (1, 2, 3)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert sorted(results) == snapshot([2, 4, 6])  # pyright: ignore[reportArgumentType]


def test_limits_enforced_in_worker(pool: Monty):
    with pool.checkout(limits={'max_duration_secs': 0.1}) as session:
        with pytest.raises(MontyRuntimeError) as exc_info:
            session.feed_run('while True:\n    pass')
        assert exc_info.value.display(format='type-msg').startswith('TimeoutError')


def test_deep_external_function_argument_is_catchable(pool: Monty):
    # arguments too deep for the wire protocol resume the call with a
    # catchable error inside the sandbox instead of corrupting the protocol
    code = """
x = [1]
for _ in range(300):
    x = [x]
try:
    f(x)
    result = 'no error'
except RuntimeError as e:
    result = str(e)
result
"""

    def f(v: object) -> None:
        raise AssertionError('the call must never reach the host')

    with pool.checkout() as session:
        result = session.feed_run(code, external_lookup={'f': f})
    assert result == snapshot('Max argument depth exceeded')


# === async variants ===


async def test_async_basic_execution():
    async with AsyncMonty() as pool:
        async with pool.checkout() as session:
            assert await session.feed_run('21 * 2') == snapshot(42)


async def test_async_crash_recovery():
    async with AsyncMonty() as pool:
        async with pool.checkout() as session:
            pid = session.worker_pid
            assert pid is not None

            async def kill_soon() -> None:
                await asyncio.sleep(0.2)
                os.kill(pid, signal.SIGKILL if sys.platform != 'win32' else signal.SIGTERM)

            kill_task = asyncio.create_task(kill_soon())
            with pytest.raises(MontyCrashedError):
                await session.feed_run('while True:\n    pass')
            await kill_task

        async with pool.checkout() as session:
            assert await session.feed_run('1 + 1') == snapshot(2)


async def test_async_concurrent_sessions():
    async with AsyncMonty(min_processes=2) as pool:

        async def run(value: int) -> object:
            async with pool.checkout() as session:
                return await session.feed_run('v * 2', inputs={'v': value})

        results = await asyncio.gather(run(1), run(2), run(3))
    assert results == snapshot([2, 4, 6])


async def test_async_dump():
    async with AsyncMonty() as pool:
        async with pool.checkout() as session:
            await session.feed_run('x = 1')
            state = await session.dump()
            assert isinstance(state, bytes)
            assert len(state) > 0


async def test_async_dump_key_cross_pool_restore():
    async with AsyncMonty(dump_key=DUMP_KEY) as pool_a:
        async with pool_a.checkout() as session:
            await session.feed_run('x = 42')
            state = await session.dump()
    async with AsyncMonty(dump_key=DUMP_KEY) as pool_b:
        async with pool_b.checkout() as session:
            await session.load(state)
            assert await session.feed_run('x') == snapshot(42)


async def test_async_tampered_dump_raises_and_session_stays_fresh():
    async with AsyncMonty(dump_key=DUMP_KEY) as pool:
        async with pool.checkout() as session:
            await session.feed_run('x = 1')
            state = await session.dump()
        tampered = state[:-1] + bytes([state[-1] ^ 0xFF])
        async with pool.checkout() as session:
            with pytest.raises(ValueError) as exc_info:
                await session.load(tampered)
            assert exc_info.value.args[0] == snapshot(TAMPERED_DUMP_MESSAGE)
            # the rejection happened before any worker I/O, so the session
            # stays fresh: the load is retryable with the untampered bytes
            await session.load(state)
            assert await session.feed_run('x') == snapshot(1)


async def test_async_short_dump_key_raises():
    with pytest.raises(ValueError) as exc_info:
        AsyncMonty(dump_key=b'short')
    assert exc_info.value.args[0] == snapshot('dump key must be at least 16 bytes')


async def test_async_pool_not_entered():
    pool = AsyncMonty()
    session = pool.checkout()
    with pytest.raises(RuntimeError) as exc_info:
        await session.__aenter__()
    assert exc_info.value.args[0] == snapshot(
        'the pool is not active — enter the Monty / AsyncMonty context manager first'
    )
