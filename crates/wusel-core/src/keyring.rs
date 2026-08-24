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
//! The interface is the [`Secrets`] trait with exactly one production
//! implementation, [`Os`]. The indirection buys testability: a keyring that is
//! absent, locked, or forgetful is a thing you can *write*, not a thing you can
//! arrange on the machine running the tests.
//!
//! It is compiled with a real backend only on Linux (the systemd *user* service
//! there can reach the PAM-unlocked login keyring over the session D-Bus — see the
//! architecture's _Credential storage_). Everywhere else the backend is a stub
//! that reports "unavailable", so callers fall back to the file automatically.

use crate::Result;

/// Everything [`crate::credentials`] needs from a keyring — and nothing else.
///
/// It is a trait rather than four free functions for one reason: the interesting
/// behaviour is not *storing a secret*, it is *what we do when the keyring
/// misbehaves* — and those cases cannot be produced on demand from a real
/// keyring. You cannot ask a running Secret Service to be absent, and certainly
/// not to accept a write and lose it. With the store behind this seam, the
/// fallback rules are decided by [`crate::credentials`] alone and can be tested
/// exhaustively on any machine, keyring or not.
///
/// The contract below is what a backend must satisfy; the test
/// `secrets_contract` checks it against every implementation, the real one
/// included.
pub trait Secrets {
    /// Whether a keyring backend is even compiled in on this platform.
    fn available(&self) -> bool;

    /// Store `secret` under `key`, replacing any previous value. `Err` on any
    /// failure (locked, no service, …).
    fn store(&self, key: &str, secret: &str) -> Result<()>;

    /// Retrieve the secret for `key`: `Ok(None)` if there is simply no entry,
    /// `Err` if the keyring itself could not be consulted (locked/unavailable).
    /// The distinction matters — it is what tells "never stored" from "cannot
    /// look".
    fn retrieve(&self, key: &str) -> Result<Option<String>>;

    /// Best-effort delete; errors are ignored (a stale entry is harmless — the
    /// file flag decides whether the keyring is consulted at all). Deleting an
    /// entry that is not there is not an error.
    fn delete(&self, key: &str);
}

/// The keyring of the operating system we are running on — the implementation
/// the product always uses. On Linux that is the freedesktop Secret Service; on
/// every other platform it is the stub that politely reports "unavailable".
pub struct Os;

impl Secrets for Os {
    fn available(&self) -> bool {
        backend::available()
    }

    fn store(&self, key: &str, secret: &str) -> Result<()> {
        backend::store(key, secret)
    }

    fn retrieve(&self, key: &str) -> Result<Option<String>> {
        backend::retrieve(key)
    }

    fn delete(&self, key: &str) {
        backend::delete(key)
    }
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

/// A keyring that lives in a `HashMap` — and, more importantly, one that can be
/// told to misbehave.
///
/// It exists because the failure modes are the whole point of
/// [`crate::credentials`]: no service at all, a locked one, and the nastiest of
/// them, a keyring that accepts a write and then does not have it. None of those
/// can be ordered from a real Secret Service, so testing the fallback rules
/// against the machine's keyring means testing whatever that machine happens to
/// be — green here, red there, and no statement about the code either way.
///
/// The real backend is not thereby untested: `secrets_contract` holds this fake
/// and [`Os`] to the same promises (see the tests below).
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::Secrets;
    use crate::{Error, Result};

    /// How the fake keyring behaves.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Mode {
        /// A healthy keyring: hands back what it was given.
        Works,
        /// No backend at all — a system without a Secret Service, or the stub
        /// we compile off Linux.
        Absent,
        /// There, but not usable: locked, or the service is not answering.
        Locked,
        /// Accepts a write and loses it. The reason
        /// [`crate::credentials::store_with`] reads the secret back before it
        /// trusts the keyring with it.
        Amnesiac,
    }

    pub(crate) struct Fake {
        mode: Mode,
        entries: Mutex<HashMap<String, String>>,
        /// What was deleted, so a test can assert that a half-written entry is
        /// cleaned up rather than left behind.
        deleted: Mutex<Vec<String>>,
    }

    impl Fake {
        pub(crate) fn new(mode: Mode) -> Self {
            Self {
                mode,
                entries: Mutex::new(HashMap::new()),
                deleted: Mutex::new(Vec::new()),
            }
        }

        /// The keys this fake was asked to forget.
        pub(crate) fn deleted(&self) -> Vec<String> {
            self.deleted.lock().expect("test mutex").clone()
        }
    }

    fn unusable() -> Error {
        Error::Other("keyring: locked or unavailable".into())
    }

    impl Secrets for Fake {
        fn available(&self) -> bool {
            self.mode != Mode::Absent
        }

        fn store(&self, key: &str, secret: &str) -> Result<()> {
            match self.mode {
                Mode::Works => {
                    self.entries
                        .lock()
                        .expect("test mutex")
                        .insert(key.to_string(), secret.to_string());
                    Ok(())
                }
                // The write "succeeds" and nothing is kept — the whole point.
                Mode::Amnesiac => Ok(()),
                Mode::Absent | Mode::Locked => Err(unusable()),
            }
        }

        fn retrieve(&self, key: &str) -> Result<Option<String>> {
            match self.mode {
                Mode::Works | Mode::Amnesiac => {
                    Ok(self.entries.lock().expect("test mutex").get(key).cloned())
                }
                Mode::Absent | Mode::Locked => Err(unusable()),
            }
        }

        fn delete(&self, key: &str) {
            self.deleted
                .lock()
                .expect("test mutex")
                .push(key.to_string());
            self.entries.lock().expect("test mutex").remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Fake, Mode};
    use super::*;

    /// The promises every backend makes, written once so that the fake cannot
    /// quietly drift away from the real thing. Uses `key`, and leaves it as it
    /// found it.
    pub(crate) fn secrets_contract(secrets: &dyn Secrets, key: &str) {
        assert!(
            matches!(secrets.retrieve(key), Ok(None)),
            "a key that was never stored is `Ok(None)`, not an error"
        );

        secrets.store(key, "erstes").expect("store");
        assert_eq!(
            secrets.retrieve(key).expect("retrieve").as_deref(),
            Some("erstes"),
            "what goes in comes back out"
        );

        secrets.store(key, "zweites").expect("overwrite");
        assert_eq!(
            secrets.retrieve(key).expect("retrieve").as_deref(),
            Some("zweites"),
            "a second store replaces the first"
        );

        secrets.delete(key);
        assert!(
            matches!(secrets.retrieve(key), Ok(None)),
            "after a delete the entry is gone, and its absence is not an error"
        );
        secrets.delete(key); // deleting nothing is not an error either
    }

    #[test]
    fn the_fake_keeps_the_contract() {
        secrets_contract(&Fake::new(Mode::Works), "beliebig");
    }

    /// The same promises, against the actual Secret Service.
    ///
    /// This covers the one thing the fake cannot: that our thin delegation maps
    /// the `keyring` crate's answers the way the trait says — in particular that
    /// "no such entry" becomes `Ok(None)` and not an error.
    ///
    /// It is deliberately **not** `#[ignore]`d. A test that only runs when
    /// somebody remembers to ask for it does not run. `mise run test` therefore
    /// provides what it needs: a throwaway D-Bus session with an empty, unlocked
    /// keyring (see `scripts/test.sh`), so this touches nobody's real secrets
    /// and gives the same answer on every machine.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_os_keyring_keeps_the_contract() {
        // Unique per process, so a stray entry can never collide with a real
        // account's — and so two test runs cannot tread on each other.
        let key = format!("selftest-{}", std::process::id());

        // Probe first: without a Secret Service every assertion below would fail
        // for a reason that has nothing to do with the assertion.
        if let Err(e) = Os.retrieve(&key) {
            panic!(
                "no usable OS keyring in this session ({e}).\n\
                 Run the tests with `mise run test`, which starts a throwaway \
                 keyring for exactly this purpose. Missing tools are named there \
                 (Fedora/Debian: the `gnome-keyring` package)."
            );
        }

        // Clean up even if an assertion panics: this may be a developer's real
        // session keyring, and we do not leave litter in it.
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                Os.delete(&self.0);
            }
        }
        let _cleanup = Cleanup(key.clone());

        secrets_contract(&Os, &key);
    }
}
