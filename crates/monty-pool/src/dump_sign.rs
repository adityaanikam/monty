//! Host-side HMAC signing of dump payloads.
//!
//! [`crate::Checkout::dump`] bytes are persisted by callers and later fed back
//! to [`crate::Checkout::restore`], where they reach deserialization of
//! interpreter state inside a worker — so a forged or tampered dump is a real
//! attack surface. Every dump is therefore signed with the pool's [`DumpKey`]
//! and verified before a `Load` is ever sent to a worker. The key lives only
//! in the parent process; workers (which run untrusted code) never see it.
//!
//! Signed payload layout (`SIGNED_DUMP_VERSION` = 1):
//!
//! ```text
//! [version u8 = 0x01][tag: 32-byte HMAC-SHA256(key, CONTEXT || inner)][inner]
//! ```
//!
//! where `inner` is the worker's opaque dump envelope, untouched. `CONTEXT`
//! domain-separates the MAC so a dump key reused elsewhere cannot be tricked
//! into signing/verifying non-dump data. The TypeScript mirror for the wasm
//! worker path (`crates/monty-js/ts/worker/dumpSign.ts`) must stay
//! byte-compatible with this file — it is the source of truth for the format.

use std::{error, fmt};

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::PoolError;

/// Minimum accepted [`DumpKey`] length; shorter keys are rejected at
/// construction so weak keys never make it into a pool.
pub const MIN_DUMP_KEY_LEN: usize = 16;

/// Format version prepended to every signed dump.
const SIGNED_DUMP_VERSION: u8 = 1;

/// Domain-separation prefix mixed into every MAC (see module docs).
const CONTEXT: &[u8] = b"monty-dump-sign-v1";

/// HMAC-SHA256 output length in bytes.
const TAG_LEN: usize = 32;

/// A pool's dump-signing key.
///
/// Supply one via [`crate::PoolConfig::dump_key`] to make dumps restorable
/// across pools/processes; without one the pool generates a random ephemeral
/// key on first use, so its dumps only restore into that same pool instance.
#[derive(Clone)]
pub struct DumpKey(Vec<u8>);

impl DumpKey {
    /// Wraps user-supplied key bytes, rejecting keys shorter than
    /// [`MIN_DUMP_KEY_LEN`].
    pub fn new(key: Vec<u8>) -> Result<Self, InvalidDumpKey> {
        if key.len() < MIN_DUMP_KEY_LEN {
            Err(InvalidDumpKey)
        } else {
            Ok(Self(key))
        }
    }

    /// Generates a random 32-byte key from OS randomness, used when
    /// [`crate::PoolConfig::dump_key`] is `None`. Created lazily on the first
    /// dump/restore, so pools that never dump draw no randomness. Panics if
    /// the OS RNG fails — an unrecoverable environment fault, not a pool error.
    pub(crate) fn ephemeral() -> Self {
        let mut key = vec![0u8; 32];
        getrandom::fill(&mut key).expect("OS randomness unavailable — cannot generate a dump signing key");
        Self(key)
    }

    /// Signs a worker dump envelope, prepending the version byte and MAC tag.
    pub(crate) fn sign(&self, state: &[u8]) -> Vec<u8> {
        let mut mac = self.mac();
        mac.update(state);
        let tag = mac.finalize().into_bytes();
        let mut signed = Vec::with_capacity(1 + TAG_LEN + state.len());
        signed.push(SIGNED_DUMP_VERSION);
        signed.extend_from_slice(&tag);
        signed.extend_from_slice(state);
        signed
    }

    /// Verifies a signed dump and returns the inner worker envelope, or
    /// [`PoolError::InvalidDump`] — the caller must not send unverified bytes
    /// to a worker. The tag comparison is constant-time (`Mac::verify_slice`).
    pub(crate) fn verify(&self, mut signed: Vec<u8>) -> Result<Vec<u8>, PoolError> {
        if signed.len() < 1 + TAG_LEN {
            return Err(PoolError::InvalidDump("too short to be a signed dump".to_owned()));
        }
        if signed[0] != SIGNED_DUMP_VERSION {
            return Err(PoolError::InvalidDump(format!(
                "unsupported signed-dump version {} (expected {SIGNED_DUMP_VERSION})",
                signed[0]
            )));
        }
        let inner = signed.split_off(1 + TAG_LEN);
        let mut mac = self.mac();
        mac.update(&inner);
        match mac.verify_slice(&signed[1..]) {
            Ok(()) => Ok(inner),
            Err(_) => Err(PoolError::InvalidDump(
                "signature verification failed — the dump was signed with a different key or corrupted".to_owned(),
            )),
        }
    }

    /// A MAC instance keyed with this key, pre-fed with the domain-separation
    /// context.
    fn mac(&self) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC-SHA256 accepts keys of any length");
        mac.update(CONTEXT);
        mac
    }
}

/// Redacted — key material must never reach logs (`PoolConfig` derives Debug).
impl fmt::Debug for DumpKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DumpKey(..)")
    }
}

/// Error from [`DumpKey::new`]: the supplied key is shorter than
/// [`MIN_DUMP_KEY_LEN`].
#[derive(Debug)]
pub struct InvalidDumpKey;

impl fmt::Display for InvalidDumpKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dump key must be at least {MIN_DUMP_KEY_LEN} bytes")
    }
}

impl error::Error for InvalidDumpKey {}
