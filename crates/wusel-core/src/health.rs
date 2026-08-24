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
//! So this is the [`Notice::ConnectionLost`] / [`Notice::ConnectionRestored`]
//! pair from the architecture's _User-facing notifications_, wired to the one
//! thing that can actually observe it: the outcome of every HTTP request.
//!
//! **The bar stays high.** Every network-touching path reports here — a
//! directory listing, a content read, an upload, the notify_push discovery —
//! which is thousands of events, and exactly one notification per outage. Three
//! rules make that true:
//!
//! * *Only transport failures count* ([`crate::Error::is_transport`]): no answer
//!   at all — DNS, connect, TLS, timeout, a dropped connection. A server that
//!   answers, even with a 500, is reachable; that is a different problem with a
//!   different message.
//! * *A blip is not an outage.* The first failure only starts the clock; the user
//!   is told when failures are **still** happening [`CONFIRM_AFTER`] later. The
//!   WebDAV client already retries a dropped keep-alive connection internally, so
//!   what reaches us is rare to begin with.
//! * *One notice per incident.* While an outage lasts, later failures are
//!   silent — and the first success clears the state, so the *next* outage is
//!   announced again.
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
    state: Mutex<State>,
    confirm_after: Duration,
}

/// The outage in progress, if any.
#[derive(Default)]
struct State {
    /// When the current run of transport failures started.
    since: Option<Instant>,
    /// Whether the user has already been told about *this* outage.
    announced: bool,
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
        Self {
            server: display_host(server_url),
            desktop,
            down: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            confirm_after,
        }
    }

    /// A request reached the server. Ends an outage, and tells the user it is
    /// over if they were told it had begun.
    pub fn ok(&self) {
        // The overwhelmingly common case: nothing was wrong, nothing to do.
        if !self.down.load(Ordering::Relaxed) {
            return;
        }
        let announced = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let announced = state.announced;
            *state = State::default();
            self.down.store(false, Ordering::Relaxed);
            announced
        };
        if announced {
            tracing::info!(server = %self.server, "the server is reachable again");
            // Outside the lock: a notification goes over D-Bus and may block,
            // and no request should ever wait behind one.
            self.desktop.notify(&Notice::ConnectionRestored {
                server: self.server.clone(),
            });
            self.desktop.set_status(Status::Idle);
        }
    }

    /// A request failed. Only a transport failure counts (see the module docs);
    /// anything else is somebody else's problem and returns immediately.
    pub fn failed(&self, error: &Error) {
        if !error.is_transport() {
            return;
        }
        let announce = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match state.since {
                // First failure: start the clock, say nothing yet.
                None => {
                    state.since = Some(Instant::now());
                    self.down.store(true, Ordering::Relaxed);
                    false
                }
                // Still failing, long enough to be an outage rather than a blip.
                Some(since) => {
                    let confirmed = !state.announced && since.elapsed() >= self.confirm_after;
                    if confirmed {
                        state.announced = true;
                    }
                    confirmed
                }
            }
        };
        if announce {
            tracing::warn!(
                server = %self.server, error = %error,
                "the server has been unreachable for a while — telling the user"
            );
            self.desktop.notify(&Notice::ConnectionLost {
                server: self.server.clone(),
            });
            self.desktop.set_status(Status::Error);
        }
    }

    /// Whether a transport failure is currently outstanding. For callers that
    /// want to behave differently while offline; the notification decision is
    /// made here, not by them.
    pub fn is_down(&self) -> bool {
        self.down.load(Ordering::Relaxed)
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

    /// A tracker that announces on the second failure, whenever it comes.
    fn immediate() -> (Arc<Spy>, Reachability) {
        let spy = Arc::new(Spy::default());
        let reach = Reachability::with_confirm_after(
            "https://cloud.example.org/",
            spy.clone(),
            Duration::ZERO,
        );
        (spy, reach)
    }

    fn offline() -> Error {
        Error::Http("[connect] dns error".into())
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

    #[test]
    fn a_server_that_answers_is_not_an_outage() {
        let (spy, reach) = immediate();
        for _ in 0..5 {
            // A 500 is an answer: the server is reachable and something else is
            // wrong. So are a 404 and a rejected password.
            reach.failed(&Error::HttpStatus {
                status: 500,
                message: "boom".into(),
            });
            reach.failed(&Error::NotFound);
            reach.failed(&Error::Auth("nope".into()));
        }
        assert!(!reach.is_down());
        assert!(spy.notices().is_empty(), "answers are not unreachability");
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
