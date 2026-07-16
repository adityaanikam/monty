// HMAC signing of dump payloads for the wasm worker path, mirroring the
// native pool's host-side signing so dumps stay byte-portable between the two
// backends when the same key is supplied.
//
// `crates/monty-pool/src/dump_sign.rs` is the source of truth for the format
// and the error strings — keep this file byte-identical to it:
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
  return crypto.getRandomValues(new Uint8Array(32))
}

/**
 * Imports raw dump-key bytes as a WebCrypto HMAC-SHA-256 key, rejecting keys
 * shorter than [`MIN_DUMP_KEY_LEN`]. Throws when `crypto.subtle` is
 * unavailable (a browser page not in a secure context) — dump/load needs it;
 * execution does not.
 */
export function importDumpKey(key: Uint8Array): Promise<CryptoKey> {
  if (key.length < MIN_DUMP_KEY_LEN) {
    throw new Error(`dump key must be at least ${MIN_DUMP_KEY_LEN} bytes`)
  }
  if (typeof crypto === 'undefined' || crypto.subtle === undefined) {
    throw new Error('dump signing needs WebCrypto (crypto.subtle) — unavailable outside a secure context')
  }
  return crypto.subtle.importKey('raw', copyBytes(key), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign', 'verify'])
}

/** Signs a worker dump envelope, prepending the version byte and MAC tag. */
export async function signDump(key: CryptoKey, state: Uint8Array): Promise<Uint8Array> {
  const tag = await crypto.subtle.sign('HMAC', key, withContext(state))
  const signed = new Uint8Array(1 + TAG_LEN + state.length)
  signed[0] = SIGNED_DUMP_VERSION
  signed.set(new Uint8Array(tag), 1)
  signed.set(state, 1 + TAG_LEN)
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
  const inner = signed.subarray(1 + TAG_LEN)
  const ok = await crypto.subtle.verify('HMAC', key, tag, withContext(inner))
  if (!ok) {
    throw new Error(
      'invalid dump: signature verification failed — the dump was signed with a different key or corrupted',
    )
  }
  return inner
}

/** `CONTEXT || state`, the exact bytes the MAC covers. */
function withContext(state: Uint8Array): Uint8Array<ArrayBuffer> {
  const buf = new Uint8Array(CONTEXT.length + state.length)
  buf.set(CONTEXT, 0)
  buf.set(state, CONTEXT.length)
  return buf
}

/** Copies bytes into a fresh `ArrayBuffer`-backed array (WebCrypto rejects
 *  views over a possible `SharedArrayBuffer` at the type level). */
function copyBytes(bytes: Uint8Array): Uint8Array<ArrayBuffer> {
  const copy = new Uint8Array(bytes.length)
  copy.set(bytes)
  return copy
}
