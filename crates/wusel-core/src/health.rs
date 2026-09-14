// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Is the server reachable? — one shared answer, and the two notifications it
//! owes the user.
//!
//! **Why this exists.** A network outage is the one failure that makes the whole
//! mount *look broken*: a file manager stops drawing a folder, an application
//! stops opening a document, and nothing on screen says why. The engine knew all
//! along — it logged `[connect] … dns error` and moved on — but nobody reads the
//! journal, so the user is left guessing between "my network is down" and "this
//! program has hung". Guessing badly costs far more than a notification: they
//! kill the daemon, unmount, or re-install. Telling them plainly ("wusel cannot
//! reach *server*") turns an inexplicable freeze into an ordinary, patient wait.
//!
//! So this is the [`Notice::ConnectionLost`] / [`Notice::ServerUnavailable`] /
//! [`Notice::ConnectionRestored`] set from the architecture's _User-facing
//! notifications_, wired to the one thing that can actually observe it: the
//! outcome of every HTTP request.
//!
//! **The bar stays high.** Every network-touching path reports here — a
//! directory listing, a content read, an upload, the notify_push discovery —
//! which is thousands of events, and exactly one notification per outage. Four
//! rules make that true:
//!
//! * *Only a failure of the server itself counts*: no answer at all
//!   ([`crate::Error::is_transport`] — DNS, connect, TLS, timeout, a dropped
//!   connection), or an answer that is the server saying it cannot serve
//!   ([`crate::Error::is_server_fault`] — the 5xx range: maintenance, a backup
//!   window, a proxy with nothing behind it). A refusal about *one request* — a
//!   404, a rejected password — is somebody else's problem and passes through.
//!
//!   The two get **different messages** ([`Notice::ConnectionLost`] against
//!   [`Notice::ServerUnavailable`]) because they need different action from the
//!   user: one sends them to their network, the other tells them to wait. They
//!   share one incident and one clock, since the mount is equally unusable
//!   either way.
//! * *A blip is not an outage.* The first failure only starts the clock; the user
//!   is told when failures are **still** happening [`CONFIRM_AFTER`] later. The
//!   WebDAV client already retries a dropped keep-alive connection internally, so
//!   what reaches us is rare to begin with.
//! * *One notice per incident.* While an outage lasts, later failures are
//!   silent — and the first success clears the state, so the *next* outage is
//!   announced again.
//! * *A flapping link is not narrated wobble by wobble.* For
//!   [`QUIET_AFTER_RESTORE`] after a recovery notice, a new outage changes the
//!   status but earns no toast; it is announced only if it is still going on
//!   when the window ends.
//!
//! **Good news only as resolution.** [`Notice::ConnectionRestored`] fires on the
//! first successful request after an announced outage — never otherwise. A
//! recovery nobody was told about needs no announcement.
//!
//! **Who drives it while nothing else does.** An idle mount issues no requests,
//! and the interesting moment during an outage is precisely the one nobody is
//! asking about: the recovery. The notify_push listener is the heartbeat — it is
//! the only thing that keeps talking to the server on its own. Both of its
//! retry loops report here (see [`crate::push`]): endpoint discovery while the
//! socket has never come up, and the reconnect loop once it has. So a mount
//! nobody is touching still learns, within about half a minute, that the server
//! went away — and that it is back.
//!
//! The reconnect loop reports the outcome of an HTTP request, never the
//! socket's own failure: the WebSocket endpoint is a URL the server advertises,
//! and a wrong one (a loopback `base_endpoint`, a proxy without upgrades) fails
//! forever while the server is perfectly reachable. Letting it speak for the
//! server announced "connection lost" after every successful listing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::desktop::{Desktop, Notice, Status};
use crate::Error;

/// How long transport failures must persist before the user is told. Short
/// enough that somebody staring at a stalled file manager gets the explanation
/// while they are still staring, long enough that a single lost connection —
/// which the WebDAV client retries anyway — never earns a toast.
pub const CONFIRM_AFTER: Duration = Duration::from_secs(10);

/// How long after an announced recovery the next outage stays quiet. A link
/// that flaps — a train, a bad wireless cell — would otherwise be narrated
/// wobble by wobble, two toasts each, which is the one thing a user reliably
/// turns off. An outage that starts inside this window is still tracked, and
/// the file manager's status still shows it; it is *announced* only if it is
/// still going on when the window ends. So a long outage is never lost, and a
/// short one right after a recovery is never news.
pub const QUIET_AFTER_RESTORE: Duration = Duration::from_secs(300);

/// Shared "can we reach the server?" state, and the notifications it owes.
///
/// Cheap to call from anywhere: the success path is a single relaxed atomic load
/// unless an outage is in progress, so putting it on every request costs nothing
/// measurable next to the request itself.
pub struct Reachability {
    /// What the notification names — the host, not the full base URL: it is the
    /// part the user recognises.
    server: String,
    desktop: Arc<dyn Desktop>,
    /// Fast path only; [`Reachability::state`] is the truth.
    down: AtomicBool,
    /// Set the first time a request actually reaches the server, and never
    /// cleared. It distinguishes "we have positive evidence the server is
    /// reachable" from a cold start that simply has not tried yet — a
    /// destructive action (the File Provider reimport) must not run on the
    /// latter, so "not tried yet" has to read as "do not".
    ever_ok: AtomicBool,
    state: Mutex<State>,
    confirm_after: Duration,
    quiet_after_restore: Duration,
}

/// The outage in progress, if any.
/// What is wrong with the server, as the last failure saw it.
///
/// Two kinds, because they need different advice: nobody answering sends the
/// user to their network, a server answering `502` sends them to wait. They
/// share one incident and one clock — the mount is equally unusable either way,
/// and an outage that starts as one and ends as the other (a proxy that first
/// refuses connections, then answers 502 as it comes up) is still one outage,
/// so it must still be one notification.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// No answer at all: DNS, connect, TLS, timeout, a dropped connection.
    Unreachable,
    /// The server answered, with a fault of its own (5xx).
    Refusing { status: u16 },
}

#[derive(Default)]
struct State {
    /// When the current run of failures started.
    since: Option<Instant>,
    /// Whether the user has already been told about *this* outage.
    announced: bool,
    /// Whether the file manager's status already shows this outage. Set as
    /// soon as the outage is confirmed, told or not: the emblem is the quiet
    /// channel, and it is right for exactly the outages a toast is not.
    marked: bool,
    /// The most recent failure's kind — what the message will say. The latest
    /// one wins: it describes the state the server is in *now*, which is what
    /// the user is about to act on.
    fault: Option<Fault>,
    /// When the last *announced* outage ended — the start of the quiet window
    /// (see [`QUIET_AFTER_RESTORE`]). Survives the reset that ends an outage,
    /// since it is about the one before.
    restored_at: Option<Instant>,
}

impl Reachability {
    /// Track reachability of `server_url`, reporting to `desktop`.
    pub fn new(server_url: &str, desktop: Arc<dyn Desktop>) -> Self {
        Self::with_confirm_after(server_url, desktop, CONFIRM_AFTER)
    }

    /// As [`new`](Self::new), with the confirmation delay chosen explicitly —
    /// the tests use it to observe the announcement without waiting.
    pub fn with_confirm_after(
        server_url: &str,
        desktop: Arc<dyn Desktop>,
        confirm_after: Duration,
    ) -> Self {
        Self::with_timings(server_url, desktop, confirm_after, QUIET_AFTER_RESTORE)
    }

    /// Both clocks chosen explicitly — the tests use it to see a flap, or to
    /// switch the quiet window off and see every incident.
    pub fn with_timings(
        server_url: &str,
        desktop: Arc<dyn Desktop>,
        confirm_after: Duration,
        quiet_after_restore: Duration,
    ) -> Self {
        Self {
            server: display_host(server_url),
            desktop,
            down: AtomicBool::new(false),
            ever_ok: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            confirm_after,
            quiet_after_restore,
        }
    }

    /// A request reached the server. Ends an outage, and tells the user it is
    /// over if they were told it had begun.
    pub fn ok(&self) {
        // Positive evidence the server is reachable, recorded before the fast
        // path so even the very first success (when nothing was "down") counts.
        self.ever_ok.store(true, Ordering::Relaxed);
        // The overwhelmingly common case: nothing was wrong, nothing to do.
        if !self.down.load(Ordering::Relaxed) {
            return;
        }
        let (announced, marked) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let (announced, marked) = (state.announced, state.marked);
            *state = State {
                // The quiet window starts at an announced recovery and is
                // otherwise inherited: an unannounced outage inside it does not
                // extend it.
                restored_at: if announced {
                    Some(Instant::now())
                } else {
                    state.restored_at
                },
                ..State::default()
            };
            self.down.store(false, Ordering::Relaxed);
            (announced, marked)
        };
        if announced {
            tracing::info!(server = %self.server, "the server is reachable again");
            // Outside the lock: a notification goes over D-Bus and may block,
            // and no request should ever wait behind one.
            self.desktop.notify(&Notice::ConnectionRestored {
                server: self.server.clone(),
            });
        }
        if marked {
            self.desktop.set_status(Status::Idle);
        }
    }

    /// A request failed. Only a failure of the *server or the way there* counts
    /// (see the module docs); anything else is somebody else's problem and
    /// returns immediately.
    pub fn failed(&self, error: &Error) {
        let Some(fault) = classify(error) else {
            return;
        };
        let (mark, announce) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.fault = Some(fault);
            match state.since {
                // First failure: start the clock, say nothing yet.
                None => {
                    state.since = Some(Instant::now());
                    self.down.store(true, Ordering::Relaxed);
                    (false, false)
                }
                // Still failing, long enough to be an outage rather than a blip.
                Some(since) => {
                    let confirmed = since.elapsed() >= self.confirm_after;
                    // Inside the quiet window after a recovery the emblem
                    // changes and the toast waits: if the outage is still on
                    // when the window ends, a later failure announces it.
                    let quiet = state
                        .restored_at
                        .is_some_and(|t| t.elapsed() < self.quiet_after_restore);
                    let mark = confirmed && !state.marked;
                    let announce = confirmed && !quiet && !state.announced;
                    if mark {
                        state.marked = true;
                    }
                    if announce {
                        state.announced = true;
                    }
                    (mark, announce)
                }
            }
        };
        if mark {
            self.desktop.set_status(Status::Error);
        }
        if announce {
            let notice = match fault {
                Fault::Unreachable => {
                    tracing::warn!(
                        server = %self.server, error = %error,
                        "the server has been unreachable for a while — telling the user"
                    );
                    Notice::ConnectionLost {
                        server: self.server.clone(),
                    }
                }
                Fault::Refusing { status } => {
                    tracing::warn!(
                        server = %self.server, error = %error, status,
                        "the server has been refusing requests for a while — telling the user"
                    );
                    Notice::ServerUnavailable {
                        server: self.server.clone(),
                        status,
                    }
                }
            };
            self.desktop.notify(&notice);
        }
    }

    /// Whether the server is currently unusable — unreachable or refusing. For
    /// callers that want to behave differently while it is; the notification
    /// decision is made here, not by them.
    pub fn is_down(&self) -> bool {
        self.down.load(Ordering::Relaxed)
    }

    /// Whether the server is *known* reachable right now: we have reached it at
    /// least once and the most recent attempt did not fail. Unlike [`is_down`],
    /// this is `false` on a cold start that has not yet tried — a caller that is
    /// about to do something destructive (the macOS File Provider reimport, which
    /// deletes and re-creates the folder subtree) wants positive evidence, so
    /// "not known yet" must read as "do not".
    pub fn reachable_now(&self) -> bool {
        self.ever_ok.load(Ordering::Relaxed) && !self.down.load(Ordering::Relaxed)
    }
}

/// Which kind of incident this error is, or `None` if it is not one at all.
///
/// The two questions are asked in this order because only one of them can be
/// true: an error either carries a status (the server answered) or it does not.
fn classify(error: &Error) -> Option<Fault> {
    if error.is_transport() {
        return Some(Fault::Unreachable);
    }
    match error {
        Error::HttpStatus { status, .. } if error.is_server_fault() => {
            Some(Fault::Refusing { status: *status })
        }
        _ => None,
    }
}

/// The host a user recognises (`cloud.example.org:8443`), out of whatever URL the
/// credentials carry. Falls back to the input if it does not parse — a message
/// naming something odd still beats no message.
fn display_host(server_url: &str) -> String {
    let trimmed = server_url.trim_end_matches('/');
    match url::Url::parse(trimmed) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            (None, _) => trimmed.to_string(),
        },
        Err(_) => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what the user would have seen.
    #[derive(Default)]
    struct Spy {
        notices: Mutex<Vec<Notice>>,
        statuses: Mutex<Vec<Status>>,
    }

    impl Desktop for Spy {
        fn notify(&self, notice: &Notice) {
            self.notices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(notice.clone());
        }
        fn set_status(&self, status: Status) {
            self.statuses
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(status);
        }
    }

    impl Spy {
        fn notices(&self) -> Vec<Notice> {
            self.notices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    /// A tracker that announces on the second failure, whenever it comes, and
    /// every incident — no quiet window — so each test sees its own.
    fn immediate() -> (Arc<Spy>, Reachability) {
        let spy = Arc::new(Spy::default());
        let reach = Reachability::with_timings(
            "https://cloud.example.org/",
            spy.clone(),
            Duration::ZERO,
            Duration::ZERO,
        );
        (spy, reach)
    }

    fn offline() -> Error {
        Error::Http("[connect] dns error".into())
    }

    /// A link that flaps: outage, recovery, outage again seconds later. The
    /// second outage is shown (status) but not told (no toast) — and its end,
    /// never announced, needs no announcement either.
    #[test]
    fn a_flap_right_after_a_recovery_changes_the_status_but_says_nothing() {
        let spy = Arc::new(Spy::default());
        let reach = Reachability::with_timings(
            "https://cloud.example.org/",
            spy.clone(),
            Duration::ZERO,
            Duration::from_secs(3600),
        );
        reach.failed(&offline());
        reach.failed(&offline());
        reach.ok();
        assert_eq!(spy.notices().len(), 2, "lost, restored");
        assert_eq!(
            spy.statuses.lock().unwrap().as_slice(),
            &[Status::Error, Status::Idle]
        );

        reach.failed(&offline());
        reach.failed(&offline());
        assert!(reach.is_down());
        assert_eq!(spy.notices().len(), 2, "inside the quiet window: no toast");
        assert_eq!(
            spy.statuses.lock().unwrap().last(),
            Some(&Status::Error),
            "but the file manager shows it"
        );
        reach.ok();
        assert_eq!(spy.notices().len(), 2, "nothing to resolve, nothing to say");
        assert_eq!(spy.statuses.lock().unwrap().last(), Some(&Status::Idle));
    }

    /// The quiet window delays, it does not swallow: an outage that outlasts it
    /// is announced by the first failure after the window ends.
    #[test]
    fn an_outage_that_outlasts_the_quiet_window_is_announced() {
        let spy = Arc::new(Spy::default());
        let reach = Reachability::with_timings(
            "https://cloud.example.org/",
            spy.clone(),
            Duration::ZERO,
            Duration::from_millis(50),
        );
        reach.failed(&offline());
        reach.failed(&offline());
        reach.ok();
        reach.failed(&offline());
        reach.failed(&offline());
        assert_eq!(spy.notices().len(), 2, "still quiet");
        std::thread::sleep(Duration::from_millis(60));
        reach.failed(&offline());
        assert_eq!(
            spy.notices().len(),
            3,
            "the window ended, the outage did not"
        );
        assert!(matches!(spy.notices()[2], Notice::ConnectionLost { .. }));
        reach.ok();
        assert_eq!(spy.notices().len(), 4, "announced, so its end is too");
    }

    #[test]
    fn reachable_now_wants_positive_evidence_and_no_failure() {
        let (_spy, reach) = immediate();
        // Cold start: the server has never been reached, so a destructive
        // reimport must not run — "not known yet" reads as "do not".
        assert!(
            !reach.reachable_now(),
            "a cold start that has not tried is not known reachable"
        );

        reach.ok();
        assert!(reach.reachable_now(), "a success is positive evidence");

        reach.failed(&offline());
        assert!(
            !reach.reachable_now(),
            "the most recent attempt failed — not reachable right now"
        );

        reach.ok();
        assert!(
            reach.reachable_now(),
            "reachable again once a request lands"
        );
    }

    #[test]
    fn a_single_failure_is_a_blip_and_stays_silent() {
        let (spy, reach) = immediate();
        reach.failed(&offline());
        assert!(reach.is_down(), "the clock is running");
        assert!(spy.notices().is_empty(), "one failure is not an outage");

        // …and a success right after it clears the state without a word: the
        // user was never told anything to resolve.
        reach.ok();
        assert!(!reach.is_down());
        assert!(
            spy.notices().is_empty(),
            "nothing to resolve, nothing to say"
        );
    }

    #[test]
    fn a_persistent_outage_is_announced_once_and_its_end_once() {
        let (spy, reach) = immediate();
        for _ in 0..5 {
            reach.failed(&offline());
        }
        assert_eq!(
            spy.notices(),
            vec![Notice::ConnectionLost {
                server: "cloud.example.org".into()
            }],
            "five failures, one notification"
        );

        reach.ok();
        reach.ok();
        assert_eq!(
            spy.notices(),
            vec![
                Notice::ConnectionLost {
                    server: "cloud.example.org".into()
                },
                Notice::ConnectionRestored {
                    server: "cloud.example.org".into()
                }
            ],
            "the recovery is announced exactly once"
        );

        // A later outage is a new incident, and is announced again.
        reach.failed(&offline());
        reach.failed(&offline());
        assert_eq!(spy.notices().len(), 3, "the next outage speaks up too");
    }

    #[test]
    fn the_confirmation_delay_is_respected() {
        let spy = Arc::new(Spy::default());
        let reach = Reachability::with_confirm_after(
            "https://cloud.example.org",
            spy.clone(),
            Duration::from_secs(3600),
        );
        for _ in 0..10 {
            reach.failed(&offline());
        }
        assert!(
            spy.notices().is_empty(),
            "failures inside the confirmation window are a blip, however many"
        );
    }

    fn status(status: u16) -> Error {
        Error::HttpStatus {
            status,
            message: "test".into(),
        }
    }

    /// A 4xx is the server dealing with *this request*; the mount as a whole is
    /// fine, and nothing about it is the user's to act on here.
    #[test]
    fn an_answer_about_one_request_is_not_an_incident() {
        let (spy, reach) = immediate();
        for _ in 0..5 {
            reach.failed(&status(404));
            reach.failed(&status(403));
            // 507 is 5xx by number only: it is a precise answer about the
            // user's quota, reported where the upload is parked.
            reach.failed(&status(507));
            reach.failed(&Error::NotFound);
            reach.failed(&Error::Auth("nope".into()));
        }
        assert!(!reach.is_down());
        assert!(
            spy.notices().is_empty(),
            "one refused request is not an outage"
        );
    }

    /// The case this whole distinction exists for: a backup window, where the
    /// proxy answers `502` and the server is perfectly reachable. Telling the
    /// user their connection is gone would send them to the router for nothing.
    #[test]
    fn a_server_that_refuses_is_its_own_incident() {
        let (spy, reach) = immediate();
        reach.failed(&status(502));
        reach.failed(&status(502));
        assert!(reach.is_down(), "unusable is unusable, whichever way");
        assert_eq!(
            spy.notices(),
            vec![Notice::ServerUnavailable {
                server: "cloud.example.org".into(),
                status: 502,
            }],
            "the message names the status, not a lost connection"
        );

        // And it resolves like any other incident: once, on the first success.
        reach.ok();
        reach.ok();
        assert_eq!(spy.notices().len(), 2);
        assert!(matches!(
            spy.notices()[1],
            Notice::ConnectionRestored { .. }
        ));
    }

    /// A proxy coming up refuses connections first and answers `503` a moment
    /// later. That is one outage the user sits through, so it is one message —
    /// and it should describe where things stand when it is sent.
    #[test]
    fn one_outage_that_changes_shape_is_still_one_message() {
        let (spy, reach) = immediate();
        reach.failed(&offline());
        reach.failed(&status(503));
        assert_eq!(
            spy.notices(),
            vec![Notice::ServerUnavailable {
                server: "cloud.example.org".into(),
                status: 503,
            }],
            "the clock kept running; the latest failure chose the words"
        );
    }

    #[test]
    fn the_message_names_the_host_not_the_url() {
        assert_eq!(
            display_host("https://cloud.example.org/"),
            "cloud.example.org"
        );
        assert_eq!(
            display_host("https://cloud.example.org:8443"),
            "cloud.example.org:8443"
        );
        // Anything unparseable is still worth naming.
        assert_eq!(display_host("not a url"), "not a url");
    }
}
