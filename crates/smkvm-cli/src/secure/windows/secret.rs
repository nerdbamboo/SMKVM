//! The unguessable half of the pipe's name.
//!
//! The pipe's access control list keeps everyone but the system account
//! out of the pipe the service made. It cannot keep the worker out of a
//! *different* pipe of the same name, and any authenticated user may
//! create a name in the pipe namespace. So the name must be one nobody can
//! create in advance.
//!
//! What this replaced was a counter: `smkvm-worker-0000000000000001`,
//! `...0002`, in that order, from every boot, with a comment above it
//! claiming the name was unguessable. Anything on the machine could have
//! made the next one first and had a process running as the system account
//! connect to it.

#![allow(unsafe_code)]

use anyhow::{Context, Result};
use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};

use crate::secure::acl::NAME_BYTES;

/// Bytes from the system's random number generator.
///
/// `BCRYPT_USE_SYSTEM_PREFERRED_RNG` rather than an algorithm handle we
/// opened: it is the same generator the rest of the system trusts, and it
/// takes no handle to leak. Nothing here falls back to a clock or a
/// counter if it fails -- a name that is merely hard to guess is the thing
/// being got rid of, so a failure is a failure and the worker is not
/// started.
pub fn name_bytes() -> Result<[u8; NAME_BYTES]> {
    let mut bytes = [0u8; NAME_BYTES];
    // SAFETY: the buffer is valid for its own length, and no algorithm
    // handle is passed because the flag says to use the system's.
    unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .context("asking the system for a name nobody can guess")?;
    Ok(bytes)
}
