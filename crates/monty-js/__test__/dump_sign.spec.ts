// Dump signing: the TS signer (wasm path), pool-level dumpKey behavior on
// both backends, and Rust ↔ TS byte-format parity.

import { test } from 'vitest'
import { t } from './assertions.js'
import { skipIfBrowser } from './env.js'

import { Monty, MontyInvalidDumpError } from '@pydantic/monty'
import { generateDumpKey, importDumpKey, MIN_DUMP_KEY_LEN, signDump, verifyDump } from '@pydantic/monty/wasm'

const SHARED_KEY = new TextEncoder().encode('an example 32-byte test dump key')
const TAMPERED_MESSAGE =
  'invalid dump: signature verification failed — the dump was signed with a different key or corrupted'

test('signer round-trips and rejects tampering', async () => {
  const key = await importDumpKey(generateDumpKey())
  const state = new TextEncoder().encode('inner dump envelope')
  const signed = await signDump(key, state)
  t.is(signed[0], 1) // pin the signed format version
  t.deepEqual(Array.from(await verifyDump(key, signed)), Array.from(state))
  const tampered = signed.slice()
  tampered[tampered.length - 1]! ^= 0xff
  await t.throwsAsync(() => verifyDump(key, tampered), { message: TAMPERED_MESSAGE })
})

test('signer rejects short keys, short dumps, and unknown versions', async () => {
  t.throws(() => importDumpKey(new Uint8Array(MIN_DUMP_KEY_LEN - 1)), {
    message: 'dump key must be at least 16 bytes',
  })
  const key = await importDumpKey(SHARED_KEY)
  await t.throwsAsync(() => verifyDump(key, new Uint8Array(10)), {
    message: 'invalid dump: too short to be a signed dump',
  })
  const wrongVersion = await signDump(key, new Uint8Array(1))
  wrongVersion[0] = 2
  await t.throwsAsync(() => verifyDump(key, wrongVersion), {
    message: 'invalid dump: unsupported signed-dump version 2 (expected 1)',
  })
})

test('dump restores across pools sharing a dumpKey', async () => {
  const poolA = await Monty.create({ dumpKey: SHARED_KEY })
  let state: Uint8Array
  try {
    const session = await poolA.checkout()
    try {
      await session.feedRun('x = 42')
      state = await session.dump()
    } finally {
      await session.close()
    }
  } finally {
    await poolA.close()
  }

  const poolB = await Monty.create({ dumpKey: SHARED_KEY })
  try {
    const session = await poolB.checkout()
    try {
      // a rejected load happens before any worker I/O and leaves the session
      // fresh, so the load is retryable with the untampered bytes
      // (new Uint8Array, not .slice(): `state` is a Buffer, whose slice aliases)
      const tampered = new Uint8Array(state)
      tampered[tampered.length - 1]! ^= 0xff
      await t.throwsAsync(() => session.load(tampered), { instanceOf: MontyInvalidDumpError })
      await session.load(state)
      t.is(await session.feedRun('x'), 42)
    } finally {
      await session.close()
    }
  } finally {
    await poolB.close()
  }
})

test('ephemeral-key dumps do not restore into another pool', async () => {
  const poolA = await Monty.create({})
  let state: Uint8Array
  try {
    const session = await poolA.checkout()
    try {
      await session.feedRun('x = 1')
      state = await session.dump()
    } finally {
      await session.close()
    }
  } finally {
    await poolA.close()
  }

  const poolB = await Monty.create({})
  try {
    const session = await poolB.checkout()
    const error = await t.throwsAsync(() => session.load(state), { instanceOf: MontyInvalidDumpError })
    t.is(error.message, `ValueError: ${TAMPERED_MESSAGE}`)
    // the rejection happened before any worker I/O — the session stays fresh
    // and usable (a later load would also still be accepted)
    t.is(await session.feedRun('1 + 1'), 2)
    await session.close()
  } finally {
    await poolB.close()
  }
})

test('short pool dumpKey is rejected at creation', async () => {
  await t.throwsAsync(() => Monty.create({ dumpKey: new Uint8Array(15) }), {
    message: 'dump key must be at least 16 bytes',
  })
  await t.throwsAsync(() => Monty.create({ dumpKey: 'short' }), {
    message: 'dump key must be at least 16 bytes',
  })
})

// a string key is UTF-8-encoded: a pool keyed with the string restores dumps
// from a pool keyed with the equivalent bytes, matching the Python binding
test('string dumpKey encodes to the same key as bytes', async () => {
  const poolA = await Monty.create({ dumpKey: SHARED_KEY })
  let state: Uint8Array
  try {
    const session = await poolA.checkout()
    try {
      await session.feedRun('x = 42')
      state = await session.dump()
    } finally {
      await session.close()
    }
  } finally {
    await poolA.close()
  }

  const poolB = await Monty.create({ dumpKey: 'an example 32-byte test dump key' })
  try {
    const session = await poolB.checkout()
    try {
      await session.load(state)
      t.is(await session.feedRun('x'), 42)
    } finally {
      await session.close()
    }
  } finally {
    await poolB.close()
  }
})

// The Rust signer (native pool) and the TS signer must be byte-compatible:
// a native dump signed with key K verifies in TS with the same key, and a
// flipped byte is rejected. Node-only — the native pool needs subprocesses.
test('native dumps verify with the TS signer (format parity)', async (ctx) => {
  skipIfBrowser(ctx)
  const pool = await Monty.create({ dumpKey: SHARED_KEY })
  try {
    const session = await pool.checkout()
    let state: Uint8Array
    try {
      await session.feedRun('x = 1')
      state = await session.dump()
    } finally {
      await session.close()
    }
    const key = await importDumpKey(SHARED_KEY)
    const inner = await verifyDump(key, state)
    t.true(inner.length > 0)
    const tampered = new Uint8Array(state)
    tampered[tampered.length - 1]! ^= 0xff
    await t.throwsAsync(() => verifyDump(key, tampered), { message: TAMPERED_MESSAGE })
  } finally {
    await pool.close()
  }
})
