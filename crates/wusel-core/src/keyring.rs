// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! OS keyring access (freedesktop Secret Service on Linux), behind a tiny,
//! **fail-soft** interface.
//!
//! The keyring is a hardening *option*, never a requirement: a locked, missing,
//! or absent keyring must never stop a user from logging in or mounting. So this
//! module only ever reports success/failure; the calling code
//! ([`crate::credentials`]) treats any failure as "use the 0600 file instead".
//!
//! It is compiled with a real backend only on Linux (the systemd *user* service
//! there can reach the PAM-unlocked login keyring over the session D-Bus — see the
//! architecture's _Credential storage_). Everywhere else the backend is a stub
//! that reports "unavailable", so callers fall back to the file automatically.

use crate::Result;

/// Whether a keyring backend is even compiled in on this platform.
pub fn available() -> bool {
    backend::available()
}

/// Store `secret` under `key`. `Err` on any failure (locked, no service, …).
pub fn store(key: &str, secret: &str) -> Result<()> {
    backend::store(key, secret)
}

/// Retrieve the secret for `key`: `Ok(None)` if there is simply no entry, `Err`
/// if the keyring itself could not be consulted (locked / unavailable).
pub fn retrieve(key: &str) -> Result<Option<String>> {
    backend::retrieve(key)
}

/// Best-effort delete; errors are ignored (a stale entry is harmless — the file
/// flag decides whether the keyring is consulted at all).
pub fn delete(key: &str) {
    backend::delete(key)
}

/// The service name our entries live under in the keyring.
#[cfg(target_os = "linux")]
const SERVICE: &str = "wusel";

#[cfg(target_os = "linux")]
mod backend {
    use super::SERVICE;
    use crate::{Error, Result};

    fn entry(key: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, key).map_err(|e| Error::Other(format!("keyring: {e}")))
    }

    pub fn available() -> bool {
        true
    }

    pub fn store(key: &str, secret: &str) -> Result<()> {
        entry(key)?
            .set_password(secret)
            .map_err(|e| Error::Other(format!("keyring store: {e}")))
    }

    pub fn retrieve(key: &str) -> Result<Option<String>> {
        match entry(key)?.get_password() {
            Ok(pw) => Ok(Some(pw)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::Other(format!("keyring read: {e}"))),
        }
    }

    pub fn delete(key: &str) {
        if let Ok(e) = entry(key) {
            let _ = e.delete_credential();
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod backend {
    use crate::{Error, Result};

    pub fn available() -> bool {
        false
    }

    pub fn store(_key: &str, _secret: &str) -> Result<()> {
        Err(unavailable())
    }

    pub fn retrieve(_key: &str) -> Result<Option<String>> {
        Err(unavailable())
    }

    pub fn delete(_key: &str) {}

    fn unavailable() -> Error {
        Error::Other("no OS keyring backend on this platform".into())
    }
}
