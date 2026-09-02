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
    /// Pinned files have gone out of date on the server. One message for all of
    /// them: a colleague reorganising a shared folder can change hundreds at
    /// once, and hundreds of notifications is a denial of service dressed as
    /// helpfulness.
    PinnedOutOfDate { count: usize, first: String },
    /// An outdated local copy was handed out. The bytes are real and complete,
    /// they are simply not the newest — and an application that opens them has
    /// no other way to know.
    ///
    /// The reason changes what the user can do about it, so it changes the
    /// message: waiting for a connection is not the same advice as "update it
    /// when you want to".
    StaleCopyServed { path: String, reason: Stale },
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
            Notice::StaleCopyServed { .. } | Notice::PinnedOutOfDate { .. } => Severity::Warning,
            Notice::UploadFailed { .. } | Notice::ConnectionLost { .. } => Severity::Error,
        }
    }

    /// Stable, unlocalized identifier for this notice's kind — the `"kind"` field
    /// in [`Notice::to_json`]. **A public contract** — keep these strings stable,
    /// like [`FileState::as_xattr`](crate::provider::FileState::as_xattr): a
    /// notify-hook script may already be matching on them.
    fn kind(&self) -> &'static str {
        match self {
            Notice::ConflictCopy { .. } => "conflict-copy",
            Notice::UploadFailed { .. } => "upload-failed",
            Notice::ConnectionLost { .. } => "connection-lost",
            Notice::PinnedOutOfDate { .. } => "pinned-out-of-date",
            Notice::StaleCopyServed { .. } => "stale-copy-served",
            Notice::ConnectionRestored { .. } => "connection-restored",
        }
    }

    /// The raw, **unlocalized** payload — `kind`, `severity`, and this notice's
    /// own fields as JSON. For the notify-hook's `WUSEL_NOTICE_JSON`: a script
    /// that wants to act on a specific kind of notice (not just forward the
    /// human sentence) reads this instead of parsing [`Notice::localize`]'s
    /// `Message`, which changes with the user's language and wording.
    pub fn to_json(&self) -> serde_json::Value {
        let mut fields = match self {
            Notice::ConflictCopy { path, copy } => {
                serde_json::json!({ "path": path, "copy": copy })
            }
            Notice::UploadFailed { path, reason } => {
                serde_json::json!({ "path": path, "reason": reason })
            }
            Notice::ConnectionLost { server } => serde_json::json!({ "server": server }),
            Notice::PinnedOutOfDate { count, first } => {
                serde_json::json!({ "count": count, "first": first })
            }
            Notice::StaleCopyServed { path, reason } => serde_json::json!({
                "path": path,
                "reason": match reason {
                    Stale::Unreachable => "unreachable",
                    Stale::ByChoice => "by-choice",
                },
            }),
            Notice::ConnectionRestored { server } => serde_json::json!({ "server": server }),
        };
        // `fields` is always an object literal from the arms above, so indexing
        // it to add two more members cannot panic.
        fields["kind"] = serde_json::json!(self.kind());
        fields["severity"] = serde_json::json!(match self.severity() {
            Severity::Success => "success",
            Severity::Warning => "warning",
            Severity::Error => "error",
        });
        fields
    }
}

/// Why an outdated copy was served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stale {
    /// The server could not be reached. Nothing to decide; it resolves itself
    /// when the connection does.
    ///
    /// The file is **not** read-only here: that follows from
    /// [`crate::config::OpenPinned`], and this path is reached whatever it is
    /// set to — the engine only learns the server is gone once a fetch has
    /// already failed, which is too late to have withheld the permission. So
    /// this message warns rather than reassures.
    Unreachable,
    /// The configured open policy asked for it — `offline`, or
    /// `newest-unmetered` on a connection that costs money. The user has a
    /// choice here, so the message names it.
    ByChoice,
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
            Notice::PinnedOutOfDate { count, first } => Message {
                title: "Offline files have changed".into(),
                body: if *count == 1 {
                    format!(
                        "'{first}' has a newer version on the server. Update it to keep \
                             your offline copy current."
                    )
                } else {
                    format!(
                        "{count} offline files have newer versions on the server, \
                             including '{first}'. Update them to keep your offline copies \
                             current."
                    )
                },
            },
            Notice::StaleCopyServed {
                path,
                reason: Stale::Unreachable,
            } => Message {
                title: "Opened an older version".into(),
                body: format!(
                    "The server could not be reached, so '{path}' was opened from your offline \
                     copy. It may not be the newest — take care before saving over it."
                ),
            },
            Notice::StaleCopyServed {
                path,
                reason: Stale::ByChoice,
            } => Message {
                title: "Opened the offline version".into(),
                body: format!(
                    "'{path}' was opened from your offline copy, as configured. It is read-only \
                     while it is out of date: use 'Wusel - Update Now' to fetch the current \
                     version, or copy it elsewhere if you want to work on it right now."
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
            Notice::PinnedOutOfDate { count, first } => Message {
                title: "Offline-Dateien haben sich geändert".into(),
                body: if *count == 1 {
                    format!(
                        "Von „{first}“ gibt es auf dem Server eine neuere Fassung. \
                             Aktualisieren Sie sie, damit Ihre Offline-Kopie aktuell bleibt."
                    )
                } else {
                    format!(
                        "Von {count} Offline-Dateien gibt es auf dem Server neuere \
                             Fassungen, darunter „{first}“. Aktualisieren Sie sie, damit Ihre \
                             Offline-Kopien aktuell bleiben."
                    )
                },
            },
            Notice::StaleCopyServed {
                path,
                reason: Stale::Unreachable,
            } => Message {
                title: "Ältere Fassung geöffnet".into(),
                body: format!(
                    "Der Server war nicht erreichbar, deshalb wurde „{path}“ aus Ihrer \
                     Offline-Kopie geöffnet. Sie ist möglicherweise nicht die neueste — \
                     Vorsicht beim Überschreiben."
                ),
            },
            Notice::StaleCopyServed {
                path,
                reason: Stale::ByChoice,
            } => Message {
                title: "Offline-Fassung geöffnet".into(),
                body: format!(
                    "„{path}“ wurde wie eingestellt aus Ihrer Offline-Kopie geöffnet. Sie ist \
                     schreibgeschützt, solange sie veraltet ist: „Wusel - Jetzt aktualisieren“ \
                     holt die aktuelle Fassung, oder kopieren Sie die Datei woanders hin, wenn \
                     Sie jetzt daran arbeiten wollen."
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

    /// Is the active connection metered — a phone hotspot, a mobile stick?
    ///
    /// `None` means "we do not know", and callers must treat that as *not
    /// cheap*. Spending somebody's mobile data on a guess is exactly the harm
    /// this question exists to prevent, and NetworkManager tells us plainly
    /// enough that guessing has no excuse.
    fn is_metered(&self) -> Option<bool> {
        None
    }
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

    #[test]
    fn to_json_carries_kind_severity_and_the_raw_fields() {
        let v = conflict().to_json();
        assert_eq!(v["kind"], "conflict-copy");
        assert_eq!(v["severity"], "warning");
        assert_eq!(v["path"], "Docs/plan.md");
        assert_eq!(v["copy"], "Docs/plan (conflicted copy 1700).md");
    }

    #[test]
    fn to_json_is_unaffected_by_locale() {
        // Unlike `localize`, `to_json` never translates — a script matching on
        // `kind` must not have to handle every language.
        let en = conflict().to_json();
        assert_eq!(en["kind"], conflict().to_json()["kind"]);
        assert!(!en["kind"].as_str().unwrap().is_empty());
        // Sanity: every variant gets a distinct, non-empty kind.
        let kinds: std::collections::HashSet<_> = [
            conflict(),
            Notice::UploadFailed {
                path: "x".into(),
                reason: "y".into(),
            },
            Notice::ConnectionLost { server: "x".into() },
            Notice::ConnectionRestored { server: "x".into() },
            Notice::PinnedOutOfDate {
                count: 1,
                first: "x".into(),
            },
            Notice::StaleCopyServed {
                path: "x".into(),
                reason: Stale::Unreachable,
            },
        ]
        .iter()
        .map(|n| n.to_json()["kind"].as_str().unwrap().to_string())
        .collect();
        assert_eq!(kinds.len(), 6, "every variant must have a distinct kind");
    }
}
