// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The **OS-integration surface**, as one swappable seam.
//!
//! Everything we want to tell the user *at the level of their filesystem/desktop*
//! goes through a single trait, [`Desktop`], so the platform backend is a drop-in
//! module: freedesktop notifications + `libcloudproviders` on Linux today, a
//! File Provider on macOS or Cloud Filter on Windows later — swap the
//! implementation, the engine does not change.
//!
//! Two kinds of message, both platform-independent here:
//!
//! * [`Notice`] — a *rare, actionable* user notification (a conflict copy was
//!   made, an upload cannot complete, the connection/credentials are gone). The
//!   bar is deliberately high; see the architecture's _User-facing
//!   notifications_. Good news is a notice only when it *resolves* a problem the
//!   user was told about (e.g. the connection came back) — never routine success.
//!   Each notice carries a [`Severity`] so the backend shows good vs bad
//!   distinctly (a green/amber/red toast, mapped to platform urgency + icon).
//! * [`Status`] — the *continuous* overall sync state a file manager shows next to
//!   the mount (idle / syncing / error), e.g. via `libcloudproviders`.
//!
//! Because a [`Notice`] carries *structured data*, not a finished sentence, it is
//! **localized at render time** ([`Notice::localize`]): the notification the user
//! sees is in their language (the one place we translate — many end users do not
//! read English). Logs and CLI output stay English.
//!
//! **i18n approach (considered, deliberate).** The translations are hand-rolled —
//! a `match` on the locale with inline strings, no crate — because there is only a
//! handful of messages and the project keeps dependencies minimal. We looked at a
//! framework and decided against it *for now*. If the need grows — many languages,
//! or independent translators who should edit text files rather than Rust — the
//! plan is to adopt **`fluent`** (`.ftl` catalogs; plural/gender/format rules).
//! The seam is ready for that swap: only the body of [`Notice::localize`] changes,
//! no call site does. (`gettext`/`.po` is the alternative if translator tooling
//! like Weblate/Poedit weighs more than Fluent's syntax.)
//!
//! **This crate holds no platform code** (per the project's platform-independence
//! rule): only the trait, the message types, and a no-op default. The Linux D-Bus
//! backend lives in the daemon/frontend layer and is *injected*
//! ([`crate::provider::Provider::set_desktop`]). Everything is **best-effort and
//! fail-soft**: a missing service, an unsupported desktop, or a headless box just
//! gets [`NullDesktop`] and loses nothing but the cosmetics — desktop integration
//! is never allowed to affect whether the filesystem works.

use std::sync::Arc;

/// How a notice reads — so the backend shows it the right way (a green/info vs
/// amber/warning vs red/error toast). Maps to platform urgency + icon in the
/// backend (freedesktop `urgency` + an icon name on Linux); the engine stays
/// platform-independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Good news — typically the resolution of a prior problem.
    Success,
    /// Needs attention, but nothing is lost.
    Warning,
    /// Something failed or is broken; the user should act.
    Error,
}

/// A user-facing notice — actionable, "your data is silently at risk", or the
/// good-news resolution of one of those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A concurrent edit forced a conflicted copy: the user's version is at
    /// `copy`, the original now shows the server's.
    ConflictCopy { path: String, copy: String },
    /// An upload could not complete after retries; the edit is only local.
    UploadFailed { path: String, reason: String },
    /// The server is unreachable or the app password was rejected — the mount is
    /// now silently stale until it recovers.
    ConnectionLost { server: String },
    /// Good news: the connection is back after a [`Notice::ConnectionLost`]. We
    /// notify success only when it *resolves* a problem the user was told about —
    /// never routine success (the "sync finished" spam).
    ConnectionRestored { server: String },
}

impl Notice {
    /// How this notice reads (good / attention / bad), for the backend's urgency
    /// and icon.
    pub fn severity(&self) -> Severity {
        match self {
            Notice::ConnectionRestored { .. } => Severity::Success,
            Notice::ConflictCopy { .. } => Severity::Warning,
            Notice::UploadFailed { .. } | Notice::ConnectionLost { .. } => Severity::Error,
        }
    }
}

/// A localized, ready-to-display notification (what a backend hands to the OS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub body: String,
}

impl Notice {
    /// Render this notice as a localized title + body for `locale` (a POSIX/BCP-47
    /// tag such as `de`, `de_AT.UTF-8`, `fr-FR`). **This is the one place we speak
    /// the user's language** — notifications reach non-technical users who may not
    /// read English. Logs and CLI output stay English. Unknown locales fall back to
    /// English. Add a language by adding one arm.
    pub fn localize(&self, locale: &str) -> Message {
        let lang = locale
            .split(['_', '-', '.', ':'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match lang.as_str() {
            "de" => self.de(),
            _ => self.en(),
        }
    }

    fn en(&self) -> Message {
        match self {
            Notice::ConflictCopy { path, copy } => Message {
                title: "Edit conflict".into(),
                body: format!(
                    "Your change to '{path}' clashed with a newer version on the server. \
                     It was saved as '{copy}'."
                ),
            },
            Notice::UploadFailed { path, reason } => Message {
                title: "Upload failed".into(),
                body: format!(
                    "'{path}' could not be uploaded ({reason}). Your change is still saved \
                     locally and will be retried."
                ),
            },
            Notice::ConnectionLost { server } => Message {
                title: "Connection lost".into(),
                body: format!(
                    "wusel cannot reach {server} at the moment. Your Nextcloud folder may be \
                     out of date until the connection returns."
                ),
            },
            Notice::ConnectionRestored { server } => Message {
                title: "Connection restored".into(),
                body: format!("wusel is connected to {server} again; your folder is up to date."),
            },
        }
    }

    fn de(&self) -> Message {
        match self {
            Notice::ConflictCopy { path, copy } => Message {
                title: "Bearbeitungskonflikt".into(),
                body: format!(
                    "Ihre Änderung an „{path}“ kollidierte mit einer neueren Version auf dem \
                     Server. Sie wurde als „{copy}“ gespeichert."
                ),
            },
            Notice::UploadFailed { path, reason } => Message {
                title: "Upload fehlgeschlagen".into(),
                body: format!(
                    "„{path}“ konnte nicht hochgeladen werden ({reason}). Ihre Änderung ist \
                     lokal gespeichert und wird erneut versucht."
                ),
            },
            Notice::ConnectionLost { server } => Message {
                title: "Verbindung verloren".into(),
                body: format!(
                    "wusel erreicht {server} gerade nicht. Ihr Nextcloud-Ordner ist \
                     möglicherweise nicht aktuell, bis die Verbindung zurück ist."
                ),
            },
            Notice::ConnectionRestored { server } => Message {
                title: "Verbindung wiederhergestellt".into(),
                body: format!("wusel ist wieder mit {server} verbunden; Ihr Ordner ist aktuell."),
            },
        }
    }
}

/// The user's UI locale from the environment (`LC_ALL` > `LC_MESSAGES` > `LANG`),
/// or `"en"` if none is set — the gettext resolution order. A background systemd
/// service may not inherit `LANG`; then it defaults to English, which is safe.
pub fn ui_locale() -> String {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() && v != "C" && v != "POSIX" {
                return v;
            }
        }
    }
    "en".to_string()
}

/// The overall sync state a file manager / cloud-provider surface shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Nothing in flight; everything the user changed is on the server.
    Idle,
    /// An upload is in progress.
    Syncing,
    /// Something needs attention (details arrived as a [`Notice`]).
    Error,
}

/// The OS-integration backend a frontend plugs in. Every method is best-effort
/// and must never fail or block the caller — the engine calls these on its hot
/// path and does not care whether anyone is listening.
pub trait Desktop: Send + Sync {
    /// Surface a user notification (rare — see the module docs).
    fn notify(&self, notice: &Notice);
    /// Report the overall sync status (continuous, cheap, may be called often).
    fn set_status(&self, status: Status);
    /// A single file's state changed (e.g. it was just hydrated into the cache):
    /// tell the desktop so a file manager re-reads its per-file emblem live,
    /// without a manual refresh. `abs_path` is the on-disk path in the mount.
    /// Default: no-op (kernel invalidation alone does not always suffice).
    fn file_changed(&self, _abs_path: &str) {}
}

/// The default backend: do nothing. Used whenever no OS integration is available
/// — a headless box, an unsupported desktop, or a platform we have no module for
/// yet. Choosing this can never break the mount.
pub struct NullDesktop;

impl Desktop for NullDesktop {
    fn notify(&self, _notice: &Notice) {}
    fn set_status(&self, _status: Status) {}
}

impl Default for NullDesktop {
    fn default() -> Self {
        NullDesktop
    }
}

/// A ready-to-share no-op backend (the engine's default).
pub fn null() -> Arc<dyn Desktop> {
    Arc::new(NullDesktop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict() -> Notice {
        Notice::ConflictCopy {
            path: "Docs/plan.md".into(),
            copy: "Docs/plan (conflicted copy 1700).md".into(),
        }
    }

    #[test]
    fn localizes_to_german_and_falls_back_to_english() {
        // German for a `de*` locale.
        let de = conflict().localize("de_AT.UTF-8");
        assert_eq!(de.title, "Bearbeitungskonflikt");
        assert!(de.body.contains("Docs/plan.md"));
        assert!(de.body.contains("kollidierte"));

        // English for anything we do not translate, and for plain English.
        for loc in ["en_US.UTF-8", "fr_FR", "C", "", "xx"] {
            let m = conflict().localize(loc);
            assert_eq!(m.title, "Edit conflict", "locale {loc:?} → English");
        }
    }

    #[test]
    fn every_notice_localizes_in_each_language() {
        let notices = [
            conflict(),
            Notice::UploadFailed {
                path: "big.iso".into(),
                reason: "quota exceeded".into(),
            },
            Notice::ConnectionLost {
                server: "https://cloud.example.org".into(),
            },
            Notice::ConnectionRestored {
                server: "https://cloud.example.org".into(),
            },
        ];
        for n in &notices {
            for loc in ["de", "en"] {
                let m = n.localize(loc);
                assert!(!m.title.is_empty() && !m.body.is_empty(), "{loc}: {n:?}");
            }
        }
    }

    #[test]
    fn severity_marks_good_vs_bad_news() {
        assert_eq!(
            Notice::ConnectionRestored { server: "x".into() }.severity(),
            Severity::Success
        );
        assert_eq!(conflict().severity(), Severity::Warning);
        assert_eq!(
            Notice::UploadFailed {
                path: "x".into(),
                reason: "y".into()
            }
            .severity(),
            Severity::Error
        );
        assert_eq!(
            Notice::ConnectionLost { server: "x".into() }.severity(),
            Severity::Error
        );
    }
}
