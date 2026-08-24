// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Persisting Nextcloud credentials — **reliably first, keyring optionally**.
//!
//! The app password is always recorded in a `0600` JSON file under the config
//! directory (`config::credentials_path`). That file is the dependable store: it
//! needs no service, no session, no unlock, and it is a *revocable app password*,
//! not the account password.
//!
//! Optionally (`[auth] keyring = true`, or `login --keyring`) the secret is kept
//! in the OS keyring instead, and the file holds only non-secret metadata. This
//! is **strictly fail-soft**: if the keyring cannot be written *and verified* at
//! login, we silently keep the password in the file; if a keyring-backed password
//! cannot be read at mount time, we return a clear, actionable error instead of a
//! cryptic failure. The keyring is never allowed to be the reason the tool does
//! not work — see the architecture's _Credential storage_.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::auth::Credentials;
use crate::{keyring, Error, Result};

/// Where a login's app password ended up — for the message printed after login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// In the OS keyring (the file holds only server + login name).
    Keyring,
    /// In the `0600` file — the reliable default, and the fallback.
    File,
}

/// On-disk format. `app_password` is empty when `in_keyring` is set. The
/// `loginName`/`appPassword` names match the historical file, so old files (which
/// lack `in_keyring`) still load — as plain-file credentials.
#[derive(Serialize, Deserialize)]
struct Stored {
    server: String,
    #[serde(rename = "loginName")]
    login_name: String,
    #[serde(rename = "appPassword", default)]
    app_password: String,
    #[serde(default)]
    in_keyring: bool,
}

fn write_file(path: &Path, stored: &Stored) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(stored).map_err(|e| Error::Other(e.to_string()))?;
    // Create a same-directory temp file that is `0600` *from birth* and rename
    // it into place. Writing the final path first and tightening afterwards
    // (`fs::write` + `set_permissions`) would expose the app password
    // umask-readable for a moment — and permanently if we crashed in between.
    // The rename also means readers only ever see a complete file.
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp); // a stale tmp could carry old permissions
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    std::io::Write::write_all(&mut f, &json)?;
    // …and the rename is only atomic *for readers*, not against a crash: it
    // orders nothing on disk by itself. Without this fsync, a power loss right
    // after the rename can leave the directory entry pointing at a file whose
    // data blocks were never written — a zero-length `credentials.json`
    // published over a perfectly good one, i.e. the login silently lost. Sync
    // the bytes first, then rename, then sync the directory so the rename
    // itself is durable too.
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        // Best-effort: not every filesystem allows opening a directory for
        // fsync, and failing the login over that would be the worse outcome —
        // the credentials are already written and readable at this point.
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

fn read_file(path: &Path) -> Result<Stored> {
    let data = std::fs::read(path)?;
    serde_json::from_slice(&data).map_err(|e| {
        Error::Other(format!(
            "could not parse credentials at {}: {e}",
            path.display()
        ))
    })
}

/// Store `creds` for account `key`. With `use_keyring` we try the keyring **and
/// verify a read-back**; only if that fully succeeds is the secret kept out of the
/// file. Any hitch — no service, locked, value did not round-trip — falls back to
/// the `0600` file with a warning. Storing therefore never fails on the keyring.
pub fn store(path: &Path, key: &str, creds: &Credentials, use_keyring: bool) -> Result<Storage> {
    store_with(path, key, creds, use_keyring, &keyring::Os)
}

/// [`store`], with the keyring supplied instead of taken from the OS.
///
/// The product always passes [`keyring::Os`]; the tests pass a keyring that is
/// absent, locked, or forgetful, because those are the cases the fallback exists
/// for and none of them can be arranged on a real machine (see
/// [`crate::keyring`]).
pub fn store_with(
    path: &Path,
    key: &str,
    creds: &Credentials,
    use_keyring: bool,
    secrets: &dyn keyring::Secrets,
) -> Result<Storage> {
    if use_keyring && secrets.available() {
        let verified = secrets.store(key, &creds.app_password).is_ok()
            && matches!(secrets.retrieve(key), Ok(Some(v)) if v == creds.app_password);
        if verified {
            write_file(
                path,
                &Stored {
                    server: creds.server.clone(),
                    login_name: creds.login_name.clone(),
                    app_password: String::new(),
                    in_keyring: true,
                },
            )?;
            return Ok(Storage::Keyring);
        }
        secrets.delete(key); // drop a half-written / unverifiable entry
        tracing::warn!(
            "the OS keyring is unavailable or did not verify — keeping the app \
             password in the 0600 file instead (fully functional, just not in the keyring)"
        );
    }
    write_file(
        path,
        &Stored {
            server: creds.server.clone(),
            login_name: creds.login_name.clone(),
            app_password: creds.app_password.clone(),
            in_keyring: false,
        },
    )?;
    Ok(Storage::File)
}

/// Load full credentials for account `key`. Reads the file; if the password is in
/// the keyring, fetches it — with a clear, actionable error if that fails, never a
/// raw keyring/D-Bus error.
pub fn load(path: &Path, key: &str) -> Result<Credentials> {
    load_with(path, key, &keyring::Os)
}

/// [`load`], with the keyring supplied instead of taken from the OS — see
/// [`store_with`].
pub fn load_with(path: &Path, key: &str, secrets: &dyn keyring::Secrets) -> Result<Credentials> {
    let s = read_file(path)?;
    if !s.in_keyring {
        return Ok(Credentials {
            server: s.server,
            login_name: s.login_name,
            app_password: s.app_password,
        });
    }
    let acct = if key == crate::config::DEFAULT_ACCOUNT {
        String::new()
    } else {
        format!(" --account {key}")
    };
    // The two failures are not the same failure, and saying so is the whole
    // point of [`keyring::Secrets::retrieve`] distinguishing them. Collapsing
    // both into "the keyring could not be read" sent people to unlock a keyring
    // that was working perfectly and simply had nothing in it — a wrong lead
    // that costs an afternoon, because everything the message suggests is
    // already true.
    match secrets.retrieve(key) {
        Ok(Some(pw)) => Ok(Credentials {
            server: s.server,
            login_name: s.login_name,
            app_password: pw,
        }),
        // The keyring answered, and the answer is "there is no such entry". The
        // file says the password was put there, so it has since been removed —
        // by a keyring reset, another tool, or a wiped login keyring. Nothing is
        // broken and nothing needs unlocking; the secret just has to be stored
        // again.
        Ok(None) => Err(Error::Other(format!(
            "the app password for account '{key}' is not in your OS keyring. The credentials \
             file says it was stored there, so the entry has since been removed — the keyring \
             itself is working. Run `wusel login{acct} <server-url>` to store it again. To keep \
             the password in the 0600 file instead, set `[auth] keyring = false` in config.toml."
        ))),
        // The keyring could not be consulted at all. Here the advice about
        // unlocking is the right advice — and the cause travels with it, because
        // "locked" and "no service" want different answers and guessing between
        // them is what the raw error already knows.
        Err(e) => Err(Error::Other(format!(
            "the app password for account '{key}' is kept in your OS keyring, but the keyring \
             could not be read ({e}). It may be locked, or its service may not be running. \
             Unlock it and try again, or run `wusel login{acct} <server-url>` to re-store the \
             password. To skip the keyring entirely, set `[auth] keyring = false` in config.toml."
        ))),
    }
}

/// Non-secret metadata (server + login name), always straight from the file. Used
/// by the duplicate-instance check, which must not touch the keyring.
pub fn load_metadata(path: &Path) -> Result<(String, String)> {
    let s = read_file(path)?;
    Ok((s.server, s.login_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyring::fake::{Fake, Mode};
    use crate::keyring::Secrets;

    fn creds() -> Credentials {
        Credentials {
            server: "https://cloud.example.org".into(),
            login_name: "alice".into(),
            app_password: "secret-token".into(),
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("wusel-cred-{}-{}", name, std::process::id()))
            .join("credentials.json")
    }

    /// Every test names its own keyring; nothing here ever reaches the machine's.
    const KEY: &str = "default";

    #[test]
    fn file_store_roundtrips_and_is_0600() {
        let path = tmp("file");
        let kr = Fake::new(Mode::Works);
        // `use_keyring = false`: a working keyring sits right there and must not
        // be touched, because the user asked for the file.
        let s = store_with(&path, KEY, &creds(), false, &kr).unwrap();
        assert_eq!(s, Storage::File);
        assert!(
            kr.retrieve(KEY).unwrap().is_none(),
            "nothing was offered to the keyring"
        );

        let loaded = load_with(&path, KEY, &kr).unwrap();
        assert_eq!(loaded.server, creds().server);
        assert_eq!(loaded.login_name, creds().login_name);
        assert_eq!(loaded.app_password, creds().app_password);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");
        }
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temp file is renamed into place, not left behind"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_working_keyring_takes_the_secret_out_of_the_file() {
        let path = tmp("keyring");
        let kr = Fake::new(Mode::Works);
        let s = store_with(&path, KEY, &creds(), true, &kr).unwrap();
        assert_eq!(s, Storage::Keyring);

        // The file keeps the metadata and *not* the password — that is the whole
        // benefit of the keyring, so it is worth asserting on the bytes.
        let stored = read_file(&path).unwrap();
        assert!(stored.in_keyring);
        assert!(stored.app_password.is_empty(), "no secret left in the file");
        assert_eq!(stored.login_name, "alice");
        assert_eq!(kr.retrieve(KEY).unwrap().as_deref(), Some("secret-token"));

        assert_eq!(
            load_with(&path, KEY, &kr).unwrap().app_password,
            "secret-token",
            "and it comes back out again"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn without_a_keyring_the_password_stays_in_the_file() {
        // The platform stub, or a Linux session with no Secret Service: storing
        // must transparently keep the password in the file and stay fully usable.
        let path = tmp("absent");
        let kr = Fake::new(Mode::Absent);
        let s = store_with(&path, KEY, &creds(), true, &kr).unwrap();
        assert_eq!(s, Storage::File, "no keyring at all → file fallback");
        assert_eq!(
            load_with(&path, KEY, &kr).unwrap().app_password,
            "secret-token"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_locked_keyring_falls_back_to_the_file() {
        // A keyring that is *there* but refuses to serve — the common case on a
        // headless login, and the one that used to make `wusel login` useless.
        let path = tmp("locked-store");
        let kr = Fake::new(Mode::Locked);
        let s = store_with(&path, KEY, &creds(), true, &kr).unwrap();
        assert_eq!(s, Storage::File, "a locked keyring is not a failed login");
        assert_eq!(
            load_with(&path, KEY, &kr).unwrap().app_password,
            "secret-token"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_missing_entry_is_reported_as_missing_not_as_a_broken_keyring() {
        // The message that cost an afternoon: the entry had been deleted, the
        // keyring was fine, and the daemon said "the keyring may be locked, or
        // its service is not running". Everything it suggested was already true,
        // so the advice led away from the one thing that would have helped.
        let path = tmp("gone");
        let kr = Fake::new(Mode::Works);
        store_with(&path, KEY, &creds(), true, &kr).unwrap();
        assert!(
            read_file(&path).unwrap().in_keyring,
            "the file points there"
        );

        kr.delete(KEY); // a keyring reset, another tool, a wiped login keyring

        let err = load_with(&path, KEY, &kr)
            .expect_err("no password to load")
            .to_string();
        assert!(
            err.contains("is not in your OS keyring"),
            "says the entry is gone: {err}"
        );
        assert!(
            err.contains("wusel login"),
            "and names the one thing that fixes it: {err}"
        );
        assert!(
            !err.contains("locked"),
            "and does not send the user to unlock a working keyring: {err}"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_keyring_that_cannot_be_consulted_says_so_and_carries_the_cause() {
        // The other half: here "unlock it" *is* the right advice, and the
        // underlying error travels with it — locked and no-service want
        // different answers, and the raw error already knows which it is.
        let path = tmp("unreadable");
        // Stored while the keyring worked, so the file points at it …
        let working = Fake::new(Mode::Works);
        store_with(&path, KEY, &creds(), true, &working).unwrap();

        // … and read back in a session where it cannot be consulted.
        let err = load_with(&path, KEY, &Fake::new(Mode::Locked))
            .expect_err("the keyring cannot be read")
            .to_string();
        assert!(err.contains("could not be read"), "{err}");
        assert!(err.contains("locked"), "the advice still fits: {err}");
        assert!(
            !err.contains("is not in your OS keyring"),
            "and it does not claim the entry is gone, which it cannot know: {err}"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_keyring_that_loses_the_secret_falls_back_and_leaves_nothing_behind() {
        // The reason `store` reads back before it trusts the keyring: a write
        // that reports success and keeps nothing would otherwise produce a file
        // pointing at a secret that does not exist — a login that breaks at the
        // next mount, not at login time.
        let path = tmp("amnesiac");
        let kr = Fake::new(Mode::Amnesiac);
        let s = store_with(&path, KEY, &creds(), true, &kr).unwrap();
        assert_eq!(s, Storage::File, "unverifiable is as good as unusable");
        assert!(!read_file(&path).unwrap().in_keyring);
        assert_eq!(kr.deleted(), vec![KEY], "the half-written entry is dropped");
        assert_eq!(
            load_with(&path, KEY, &kr).unwrap().app_password,
            "secret-token"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn keyring_backed_file_without_a_readable_keyring_errors_helpfully() {
        // A file whose secret "lives in the keyring" while no keyring can serve
        // it: load must fail with an actionable message, not a raw error.
        let path = tmp("locked");
        write_file(
            &path,
            &Stored {
                server: "https://cloud.example.org".into(),
                login_name: "alice".into(),
                app_password: String::new(),
                in_keyring: true,
            },
        )
        .unwrap();

        let err = load_with(&path, KEY, &Fake::new(Mode::Locked))
            .unwrap_err()
            .to_string();
        assert!(err.contains("keyring"), "mentions the keyring: {err}");
        assert!(err.contains("wusel login"), "tells the user how to recover");
        assert!(err.contains("keyring = false"), "offers the escape hatch");

        // The same message for a keyring that answers but has no such entry —
        // somebody wiped it, or logged in on another machine. "Not there" is as
        // unrecoverable for us as "cannot look", and just as fixable by the user.
        let err = load_with(&path, KEY, &Fake::new(Mode::Works))
            .unwrap_err()
            .to_string();
        assert!(err.contains("wusel login"), "same advice: {err}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn old_plain_file_without_in_keyring_still_loads() {
        // Backward compatibility: a pre-keyring file (no `in_keyring`) loads as a
        // plain-file credential.
        let path = tmp("legacy");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            br#"{"server":"https://x","loginName":"bob","appPassword":"pw"}"#,
        )
        .unwrap();
        // A locked keyring proves the point: a plain file never consults one.
        let loaded = load_with(&path, KEY, &Fake::new(Mode::Locked)).unwrap();
        assert_eq!(loaded.login_name, "bob");
        assert_eq!(loaded.app_password, "pw");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
