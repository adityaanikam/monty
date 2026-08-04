// The wasm worker's hard memory ceiling, driven in Node so it needs no browser.
//
// The module declares the same `monty-alloc` global allocator as the subprocess
// worker, so a session's `maxMemory` bounds its linear memory too — but a
// trapped module has no exit status, so a breach surfaces as a crash rather
// than the `MemoryError` the subprocess pool reports.

import { test } from 'vitest'

import { t } from './assertions.js'
import { skipIfBrowser } from './env.js'

import { Monty } from '@pydantic/monty/wasm'
import { MontyCrashedError, MontyRuntimeError } from '@pydantic/monty'

test('a session budget leaves normal wasm work alone', async (ctx) => {
  skipIfBrowser(ctx)
  const pool = await Monty.create()
  const session = await pool.checkout({ limits: { maxMemory: 1024 * 1024 } })
  t.is(await session.feedRun('sum(range(1000))'), 499500)
  await session.close()
  await pool.close()
})

test('a tracked overrun stays an in-sandbox MemoryError', async (ctx) => {
  skipIfBrowser(ctx)
  const pool = await Monty.create()
  const session = await pool.checkout({ limits: { maxMemory: 1024 * 1024 } })
  // five times below the ceiling, so the interpreter's own tracker fires first
  // and the instance survives to serve the next feed
  const error = await t.throwsAsync(() => session.feedRun("xs = []\nwhile True:\n    xs.append('x' * 24)"), {
    instanceOf: MontyRuntimeError,
  })
  t.is(error.message, 'MemoryError: memory limit exceeded: 1048600 bytes > 1048576 bytes')
  await session.close()
  await pool.close()
})

test('a breach of the derived ceiling kills the instance and the pool recovers', async (ctx) => {
  skipIfBrowser(ctx)
  const pool = await Monty.create()
  const session = await pool.checkout({ limits: { maxMemory: 1024 } })
  // the fed snippet is untracked — the module buys a frame buffer for it before
  // the interpreter sees the code — so this reaches the ceiling
  const error = await t.throwsAsync(() => session.feedRun('# ' + 'a'.repeat(16 * 1024 * 1024)), {
    instanceOf: MontyCrashedError,
  })
  // the trap, not a classified MemoryError: a wasm module has no exit status
  t.is(error.message, 'RuntimeError: worker exited without a turn-ending event')
  const next = await pool.checkout()
  t.is(await next.feedRun('3 + 3'), 6)
  await next.close()
  await pool.close()
})
