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
    drop(f);
    std::fs::rename(&tmp, path)?;
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
    if use_keyring && keyring::available() {
        let verified = keyring::store(key, &creds.app_password).is_ok()
            && matches!(keyring::retrieve(key), Ok(Some(v)) if v == creds.app_password);
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
        keyring::delete(key); // drop a half-written / unverifiable entry
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
    let s = read_file(path)?;
    if !s.in_keyring {
        return Ok(Credentials {
            server: s.server,
            login_name: s.login_name,
            app_password: s.app_password,
        });
    }
    match keyring::retrieve(key) {
        Ok(Some(pw)) => Ok(Credentials {
            server: s.server,
            login_name: s.login_name,
            app_password: pw,
        }),
        _ => {
            let acct = if key == crate::config::DEFAULT_ACCOUNT {
                String::new()
            } else {
                format!(" --account {key}")
            };
            Err(Error::Other(format!(
                "the app password for account '{key}' is kept in your OS keyring, but it could \
                 not be read (the keyring may be locked, or its service is not running). Unlock \
                 your keyring and try again, or run `wusel login{acct} <server-url>` to re-store \
                 it. To skip the keyring entirely, set `[auth] keyring = false` in config.toml."
            )))
        }
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

    #[test]
    fn file_store_roundtrips_and_is_0600() {
        let path = tmp("file");
        let s = store(&path, "default", &creds(), false).unwrap();
        assert_eq!(s, Storage::File);

        let loaded = load(&path, "default").unwrap();
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
    fn keyring_requested_but_unusable_falls_back_to_the_file() {
        // On this test host the keyring is either the non-Linux stub or a Linux
        // session with no Secret Service — either way it cannot verify, so storing
        // must transparently keep the password in the file and stay fully usable.
        let path = tmp("fallback");
        let s = store(&path, "default", &creds(), true).unwrap();
        assert_eq!(s, Storage::File, "no working keyring → file fallback");
        assert_eq!(load(&path, "default").unwrap().app_password, "secret-token");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn keyring_backed_file_without_a_readable_keyring_errors_helpfully() {
        // Simulate a file whose secret "lives in the keyring" while no keyring can
        // serve it: load must fail with an actionable message, not a raw error.
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

        let err = load(&path, "default").unwrap_err().to_string();
        assert!(err.contains("keyring"), "mentions the keyring: {err}");
        assert!(err.contains("wusel login"), "tells the user how to recover");
        assert!(err.contains("keyring = false"), "offers the escape hatch");
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
        let loaded = load(&path, "default").unwrap();
        assert_eq!(loaded.login_name, "bob");
        assert_eq!(loaded.app_password, "pw");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
