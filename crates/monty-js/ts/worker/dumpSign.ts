// HMAC signing of dump payloads for the wasm worker path, mirroring the
// native pool's host-side signing so dumps stay byte-portable between the two
// backends when the same key is supplied.
//
// `crates/monty-pool/src/dump_sign.rs` is the source of truth for the format
// and the error strings — this file must stay byte-compatible with it:
//
//   signed_dump := [version u8 = 0x01][tag: 32-byte HMAC-SHA256(key, CONTEXT || inner)][inner]
//
// Uses WebCrypto (async, constant-time verify), which both Node and
// secure-context browsers provide as `crypto.subtle`.

/** Minimum accepted dump-key length, matching `MIN_DUMP_KEY_LEN` in Rust. */
export const MIN_DUMP_KEY_LEN = 16

/** Format version prepended to every signed dump. */
const SIGNED_DUMP_VERSION = 0x01

/** Domain-separation prefix mixed into every MAC. */
const CONTEXT = new TextEncoder().encode('monty-dump-sign-v1')

/** HMAC-SHA256 output length in bytes. */
const TAG_LEN = 32

/** Generates a random 32-byte ephemeral dump key (pool-local dumps). */
export function generateDumpKey(): Uint8Array {
  // requireSubtle so a crypto-less environment gets the clear WebCrypto error
  // here rather than a bare ReferenceError on `crypto` (and a key generated in
  // a non-secure context would be unusable anyway — import needs `subtle`)
  requireSubtle()
  return crypto.getRandomValues(new Uint8Array(32))
}

/** Coerces a dump key — raw bytes, or a string (UTF-8-encoded, matching the
 *  Python binding's `dump_key: str`) — to bytes. */
export function dumpKeyBytes(key: string | Uint8Array): Uint8Array {
  return typeof key === 'string' ? new TextEncoder().encode(key) : key
}

/**
 * Imports a dump key — raw bytes, or a string (UTF-8-encoded) — as a WebCrypto
 * HMAC-SHA-256 key, rejecting keys shorter than [`MIN_DUMP_KEY_LEN`] bytes.
 * Throws when `crypto.subtle` is unavailable (a browser page not in a secure
 * context) — dump/load needs it; execution does not.
 */
export function importDumpKey(key: string | Uint8Array): Promise<CryptoKey> {
  const bytes = dumpKeyBytes(key)
  if (bytes.length < MIN_DUMP_KEY_LEN) {
    throw new Error(`dump key must be at least ${MIN_DUMP_KEY_LEN} bytes`)
  }
  return requireSubtle().importKey('raw', copyBytes(bytes), { name: 'HMAC', hash: 'SHA-256' }, false, [
    'sign',
    'verify',
  ])
}

/**
 * A memoizing lazy dump-key provider: validates `key` eagerly (so a short
 * key fails at pool creation) but defers key generation and the WebCrypto
 * import to the first call — pools that never dump touch no crypto, and a
 * non-secure browser context only fails on actual dump/load use.
 */
export function lazyDumpKey(key?: string | Uint8Array): () => Promise<CryptoKey> {
  // snapshot the key bytes now — the import is deferred, and a caller mutating
  // its Uint8Array after pool creation must not change the pool's signing key
  const bytes = key === undefined ? undefined : copyBytes(dumpKeyBytes(key))
  if (bytes !== undefined && bytes.length < MIN_DUMP_KEY_LEN) {
    throw new Error(`dump key must be at least ${MIN_DUMP_KEY_LEN} bytes`)
  }
  let imported: Promise<CryptoKey> | undefined
  return () => (imported ??= importDumpKey(bytes ?? generateDumpKey()))
}

/** Signs a worker dump envelope, prepending the version byte and MAC tag. */
export async function signDump(key: CryptoKey, state: Uint8Array): Promise<Uint8Array> {
  // MAC and output are both built from the `withContext` copy, not `state`:
  // `state` may be a view over a transport buffer, and re-reading it after the
  // await could yield different bytes than were MAC'd
  const payload = withContext(state)
  const tag = await crypto.subtle.sign('HMAC', key, payload)
  const signed = new Uint8Array(1 + TAG_LEN + state.length)
  signed[0] = SIGNED_DUMP_VERSION
  signed.set(new Uint8Array(tag), 1)
  signed.set(payload.subarray(CONTEXT.length), 1 + TAG_LEN)
  return signed
}

/**
 * Verifies a signed dump and returns the inner worker envelope, throwing an
 * `Error` (whose message matches the Rust `PoolError::InvalidDump` display)
 * on any mismatch — callers must never send unverified bytes to a worker.
 */
export async function verifyDump(key: CryptoKey, signed: Uint8Array): Promise<Uint8Array> {
  if (signed.length < 1 + TAG_LEN) {
    throw new Error('invalid dump: too short to be a signed dump')
  }
  if (signed[0] !== SIGNED_DUMP_VERSION) {
    throw new Error(`invalid dump: unsupported signed-dump version ${signed[0]} (expected ${SIGNED_DUMP_VERSION})`)
  }
  const tag = copyBytes(signed.subarray(1, 1 + TAG_LEN))
  // the returned envelope is a view over the `withContext` copy, never the
  // caller's buffer: `signed` could be mutated during the verify await,
  // letting unverified bytes reach a worker
  const payload = withContext(signed.subarray(1 + TAG_LEN))
  const ok = await crypto.subtle.verify('HMAC', key, tag, payload)
  if (!ok) {
    throw new Error(
      'invalid dump: signature verification failed — the dump was signed with a different key or corrupted',
    )
  }
  return payload.subarray(CONTEXT.length)
}

/** `CONTEXT || state`, the exact bytes the MAC covers. */
function withContext(state: Uint8Array): Uint8Array<ArrayBuffer> {
  const buf = new Uint8Array(CONTEXT.length + state.length)
  buf.set(CONTEXT, 0)
  buf.set(state, CONTEXT.length)
  return buf
}

/** Returns `crypto.subtle`, throwing the standard dump-signing error when
 *  WebCrypto is unavailable (a browser page not in a secure context). */
function requireSubtle(): SubtleCrypto {
  if (typeof crypto === 'undefined' || crypto.subtle === undefined) {
    throw new Error('dump signing needs WebCrypto (crypto.subtle) — unavailable outside a secure context')
  }
  return crypto.subtle
}

/** Copies bytes into a fresh `ArrayBuffer`-backed array (WebCrypto rejects
 *  views over a possible `SharedArrayBuffer` at the type level). */
function copyBytes(bytes: Uint8Array): Uint8Array<ArrayBuffer> {
  const copy = new Uint8Array(bytes.length)
  copy.set(bytes)
  return copy
}
