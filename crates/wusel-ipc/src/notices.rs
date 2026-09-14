// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The user-notice fan-out — the socket's second push channel, alongside the
//! change stream ([`crate::events`]).
//!
//! The engine reports rare, actionable notices (a conflict copy, a failed
//! upload, the connection lost or restored) by calling
//! [`wusel_core::desktop::Desktop::notify`] on the backend a frontend plugs in.
//! On the socket path that backend is [`IpcDesktop`]: it **localizes the notice
//! once** — the engine is the single place we speak the user's language
//! ([`wusel_core::desktop::Notice::localize`]) — and hands the ready title/body
//! to every current `notices` subscriber. The platform agent (only an app may
//! post to Notification Center) turns each into a banner.
//!
//! Two channels, not one: a `watch` client re-lists directories, a `notices`
//! client shows toasts. They fan out independently so neither's cadence or
//! back-pressure touches the other, and a frontend can subscribe to just what it
//! serves.

use std::sync::{mpsc, Arc, Mutex};

use wusel_core::desktop::{self, Desktop, Notice, Status};

use crate::wire::Severity;

/// One notice as a `notices` subscriber receives it: already localized, ready to
/// display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeOut {
    /// The stable notice id (`Notice::kind`), e.g. `"connection-restored"`. The
    /// localized title/body are for the banner; the agent also acts on some kinds
    /// (a restored connection re-drives the File Provider reconcile), which it
    /// cannot key off the translated text.
    pub kind: String,
    pub severity: Severity,
    pub title: String,
    pub body: String,
}

/// The subscriber registry a `notices` connection joins. [`IpcDesktop`] pushes
/// each localized notice here; every live subscriber gets a copy, and a
/// subscriber whose client has hung up is pruned on the next send.
///
/// Unlike [`crate::events::Events`], there is no background reader thread and no
/// replay log: a notice is delivered by the engine's direct `notify` call (not
/// drained from a channel), and a missed toast is not worth persisting — the
/// condition it describes (a lost connection, a stale copy) re-announces itself
/// or is visible in the files. A subscriber joining late simply starts fresh.
///
/// Owned by [`IpcDesktop`], which is the only thing that broadcasts to it.
#[derive(Default)]
struct Notices {
    subscribers: Mutex<Vec<mpsc::Sender<NoticeOut>>>,
}

impl Notices {
    /// Register a `notices` connection; it receives every notice from now on.
    fn subscribe(&self) -> mpsc::Receiver<NoticeOut> {
        let (tx, rx) = mpsc::channel();
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }

    /// Deliver a localized notice to every subscriber, dropping the dead ones.
    /// Returns how many live subscribers received it — the `desktop notify`
    /// self-test reports this, so "sent, but nobody was listening" is visible.
    fn broadcast(&self, notice: NoticeOut) -> usize {
        let mut list = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|tx| tx.send(notice.clone()).is_ok());
        list.len()
    }
}

/// The [`Desktop`] backend for the socket frontend: it localizes each notice and
/// fans it out to `notices` subscribers. It owns the fan-out, so it is the one
/// object the daemon holds — the engine reports through it (`Desktop::notify`), a
/// connection subscribes through it ([`subscribe`](IpcDesktop::subscribe)), and
/// the self-test injects through it ([`emit_test`](IpcDesktop::emit_test)).
///
/// Injected into the engine via `Provider::set_desktop`. Every method is
/// best-effort and non-blocking, as the trait requires — the fan-out send is on
/// an unbounded channel, so the engine's hot path never waits on a slow client.
///
/// `Status` and `file_changed` are no-ops here: the macOS File Provider surfaces
/// per-item state through enumeration, not through a continuous status the way
/// `libcloudproviders` does on Linux, so there is nothing for them to drive yet.
pub struct IpcDesktop {
    notices: Notices,
    /// The UI locale, resolved once at construction. The whole `serve` process is
    /// one user, so one locale renders every notice; the agent sets `LANG` when it
    /// spawns `serve` so this reflects the logged-in user's language rather than a
    /// launchd default.
    locale: String,
    /// The server-reachability tracker, attached after construction (it needs
    /// this object as its `Desktop` first, so the wiring is circular and settled
    /// with a `OnceLock`). It answers the `reachable` op, which the macOS File
    /// Provider consults before a destructive reimport. Absent on paths that do
    /// not track health (the mount's status socket, tests) — there the op
    /// defaults to reachable, since nothing gates on it.
    reachable: std::sync::OnceLock<Arc<wusel_core::health::Reachability>>,
}

impl IpcDesktop {
    /// Build the backend, resolving the UI locale from the environment
    /// (`LC_ALL` > `LC_MESSAGES` > `LANG`; English if unset).
    #[must_use]
    pub fn new() -> Arc<IpcDesktop> {
        Self::with_locale(desktop::ui_locale())
    }

    /// Build the backend with an explicit locale, bypassing the environment —
    /// used by tests so a render assertion does not depend on the host's locale.
    #[must_use]
    fn with_locale(locale: String) -> Arc<IpcDesktop> {
        Arc::new(IpcDesktop {
            notices: Notices::default(),
            locale,
            reachable: std::sync::OnceLock::new(),
        })
    }

    /// Attach the reachability tracker so the `reachable` op can answer. Called
    /// once, after both objects exist (the tracker took this desktop as its
    /// notifier first). A second call is ignored.
    pub fn set_reachability(&self, reachable: Arc<wusel_core::health::Reachability>) {
        let _ = self.reachable.set(reachable);
    }

    /// Whether the server is known reachable right now, for the `reachable` op.
    /// Defaults to `true` when no tracker is attached (the mount's status socket,
    /// tests) — nothing there gates on it, and "reachable" is the non-disruptive
    /// answer.
    #[must_use]
    pub fn reachable_now(&self) -> bool {
        self.reachable.get().is_none_or(|r| r.reachable_now())
    }

    /// Register a `notices` connection; it receives every notice from now on.
    #[must_use]
    pub fn subscribe(&self) -> mpsc::Receiver<NoticeOut> {
        self.notices.subscribe()
    }

    /// Inject a representative notice of `severity` — the `desktop notify`
    /// self-test — exactly as if the engine had reported it, and report how many
    /// subscribers received it.
    pub fn emit_test(&self, severity: desktop::Severity) -> usize {
        self.deliver(&Notice::sample(severity))
    }

    /// Localize a notice and fan it out; returns the number of subscribers reached.
    fn deliver(&self, notice: &Notice) -> usize {
        let message = notice.localize(&self.locale);
        self.notices.broadcast(NoticeOut {
            kind: notice.kind().to_string(),
            severity: notice.severity().into(),
            title: message.title,
            body: message.body,
        })
    }
}

impl Desktop for IpcDesktop {
    fn notify(&self, notice: &Notice) {
        let _ = self.deliver(notice);
    }

    fn set_status(&self, _status: Status) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use wusel_core::desktop::Stale;

    #[test]
    fn a_notice_reaches_every_subscriber_localized() {
        // An explicit locale so the assertion does not depend on the host's.
        let desktop = IpcDesktop::with_locale("de_AT.UTF-8".into());

        let a = desktop.subscribe();
        let b = desktop.subscribe();

        desktop.notify(&Notice::ConnectionLost {
            server: "https://cloud.example.org".into(),
        });

        for rx in [&a, &b] {
            let got = rx.try_recv().expect("each subscriber gets the notice");
            // The stable id rides alongside the localized text so the agent can
            // act on it (a restored connection re-drives the reconcile).
            assert_eq!(got.kind, "connection-lost");
            assert_eq!(got.severity, Severity::Error);
            assert_eq!(got.title, "Verbindung verloren");
            assert!(got.body.contains("cloud.example.org"));
        }
    }

    #[test]
    fn reachable_op_defaults_reachable_and_follows_the_tracker() {
        let desktop = IpcDesktop::with_locale("en".into());
        // No tracker attached (the mount's status socket, tests): the `reachable`
        // op must not gate anything there, so it defaults to reachable.
        assert!(
            desktop.reachable_now(),
            "with no tracker attached the op defaults to reachable"
        );

        // Attach the tracker (the daemon does this once, after both exist). A cold
        // tracker has not reached the server yet — not known reachable.
        let health = Arc::new(wusel_core::health::Reachability::new(
            "https://cloud.example.org/",
            Arc::clone(&desktop) as Arc<dyn wusel_core::desktop::Desktop>,
        ));
        desktop.set_reachability(Arc::clone(&health));
        assert!(
            !desktop.reachable_now(),
            "a cold tracker is not yet known reachable"
        );

        health.ok();
        assert!(
            desktop.reachable_now(),
            "reachable once the server has answered a request"
        );
    }

    #[test]
    fn a_hung_up_subscriber_is_pruned() {
        let desktop = IpcDesktop::with_locale("en".into());

        let live = desktop.subscribe();
        drop(desktop.subscribe()); // its receiver is gone immediately

        // Two subscribers, one dead: the delivery count reports the one that took it.
        let delivered = desktop.deliver(&Notice::StaleCopyServed {
            path: "Docs/plan.md".into(),
            reason: Stale::Unreachable,
        });
        assert_eq!(delivered, 1);
        assert!(live.try_recv().is_ok());
    }

    #[test]
    fn emit_test_delivers_a_sample_of_the_asked_severity() {
        let desktop = IpcDesktop::with_locale("en".into());
        let rx = desktop.subscribe();

        let delivered = desktop.emit_test(desktop::Severity::Warning);
        assert_eq!(delivered, 1);
        let got = rx.try_recv().expect("the sample is delivered");
        assert_eq!(got.severity, Severity::Warning);
        assert!(!got.title.is_empty() && !got.body.is_empty());
    }
}
