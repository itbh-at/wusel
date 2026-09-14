// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! notify_push client — instant cache invalidation over WebSocket.
//!
//! Nextcloud's `notify_push` app pushes a `notify_file` message whenever the
//! user's files change, so we no longer have to wait for the TTL to re-list.
//! The protocol is deliberately tiny:
//!
//! 1. connect to the WebSocket endpoint (from [`crate::capabilities`]),
//! 2. send the login name, then the app password,
//! 3. the server replies `authenticated`,
//! 4. thereafter it sends `notify_file` / `notify_activity` / `notify_notification`.
//!
//! On any `notify_file` we stamp a shared `invalidate_after` timestamp with the
//! current time; the provider re-lists every directory that was listed before
//! then (see `state::dir_needs_reload`). The signal is coarse (no path), so this
//! "revalidate on next access" response is the correct one.
//!
//! The WebSocket rides the same reqwest client as every other call (via
//! `reqwest-websocket`), so it inherits one TLS configuration (see [`crate::tls`]).
//!
//! ## Threading
//!
//! The listener runs on its **own** OS thread with its own single-threaded tokio
//! runtime. The provider's runtime is `current_thread` and only advances while a
//! FUSE call is inside `block_on`, which would never drive a long-lived read.
//! Keeping the socket on a separate thread decouples the two entirely; the only
//! shared state is one atomic.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use reqwest_websocket::{Message, RequestBuilderExt};

use crate::config::TlsSettings;
use crate::diag::{PushPhase, PushReport};
use crate::{capabilities, tls, Error, Result};

/// Handle to the background listener. Dropping it asks the loop to stop between
/// reconnects; the daemon normally keeps it for the mount's lifetime.
pub struct PushListener {
    stop: Arc<AtomicBool>,
    status: Arc<PushStatus>,
    // Kept so the thread is owned by the handle; joined on a clean stop only.
    _handle: Option<JoinHandle<()>>,
}

impl PushListener {
    /// The listener's live state, for the diagnostics socket. Shared, so the
    /// daemon can hand it to the mount while keeping the listener itself.
    #[must_use]
    pub fn status(&self) -> Arc<PushStatus> {
        Arc::clone(&self.status)
    }
}

/// What the listener is doing right now, as `wusel doctor` will see it.
///
/// Written by the listener thread at every phase change and failure, read by
/// the diagnostics socket when somebody asks. A plain mutex: both sides touch
/// it a few times a minute at most, and nothing on the FUSE path ever does.
///
/// It exists because a listener that cannot hold its connection is silent by
/// design — it logs one `WARN` per attempt and retries forever — and from the
/// outside that looks exactly like a server that offers no notify_push. The
/// two need opposite advice (fix the reverse proxy, against nothing to do), so
/// the daemon has to say which it is.
#[derive(Default)]
pub struct PushStatus {
    inner: Mutex<Inner>,
}

struct Inner {
    phase: PushPhase,
    since: Instant,
    endpoint: Option<String>,
    failures: u32,
    connects: u32,
    last_error: Option<String>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            phase: PushPhase::Discovering,
            since: Instant::now(),
            endpoint: None,
            failures: 0,
            connects: 0,
            last_error: None,
        }
    }
}

impl PushStatus {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Enter `phase`; the clock restarts only if it actually changes.
    fn set_phase(&self, phase: PushPhase) {
        let mut s = self.lock();
        if s.phase != phase {
            s.phase = phase;
            s.since = Instant::now();
        }
    }

    fn set_endpoint(&self, endpoint: &str) {
        self.lock().endpoint = Some(endpoint.to_string());
    }

    /// One more failed attempt, and its reason.
    fn failed(&self, error: &Error) {
        let mut s = self.lock();
        s.failures = s.failures.saturating_add(1);
        s.last_error = Some(error.to_string());
    }

    /// Authenticated: the run of failures is over and its last error is no
    /// longer news.
    fn connected(&self) {
        {
            let mut s = self.lock();
            s.connects = s.connects.saturating_add(1);
            s.failures = 0;
            s.last_error = None;
        }
        self.set_phase(PushPhase::Connected);
    }

    /// The state as plain data for the wire.
    #[must_use]
    pub fn snapshot(&self) -> PushReport {
        let s = self.lock();
        PushReport {
            phase: s.phase,
            since_secs: s.since.elapsed().as_secs(),
            endpoint: s.endpoint.clone(),
            failures: s.failures,
            connects: s.connects,
            last_error: s.last_error.clone(),
        }
    }
}

impl Drop for PushListener {
    fn drop(&mut self) {
        // The socket read may block past this point; the thread is a daemon that
        // exits with the process. Prompt cancellation is a later refinement.
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Spawns the notify_push listener. It discovers the endpoint, connects, and on
/// every file-change event stamps `invalidate_after` with the current time.
pub fn spawn(
    server_url: &str,
    login: &str,
    password: &str,
    tls_settings: TlsSettings,
    invalidate_after: Arc<AtomicI64>,
    sync_trigger: std::sync::mpsc::Sender<()>,
    health: Option<Arc<crate::health::Reachability>>,
) -> PushListener {
    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(PushStatus::default());
    let (server, login, password) = (
        server_url.to_string(),
        login.to_string(),
        password.to_string(),
    );
    let stop_thread = stop.clone();
    let status_thread = Arc::clone(&status);

    let handle = std::thread::Builder::new()
        .name("nc-notify-push".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!(%e, "notify_push: could not build runtime");
                    status_thread.failed(&Error::Other(format!("no runtime: {e}")));
                    status_thread.set_phase(PushPhase::Stopped);
                    return;
                }
            };
            rt.block_on(run(
                &server,
                &login,
                &password,
                &tls_settings,
                &invalidate_after,
                &sync_trigger,
                &stop_thread,
                health.as_deref(),
                &status_thread,
            ));
            status_thread.set_phase(PushPhase::Stopped);
        })
        .expect("spawn notify-push thread");

    PushListener {
        stop,
        status,
        _handle: Some(handle),
    }
}

/// Discover the endpoint once, then keep a connection alive with backoff.
#[allow(clippy::too_many_arguments)]
async fn run(
    server: &str,
    login: &str,
    password: &str,
    tls_settings: &TlsSettings,
    invalidate_after: &AtomicI64,
    sync_trigger: &std::sync::mpsc::Sender<()>,
    stop: &AtomicBool,
    health: Option<&crate::health::Reachability>,
    status: &PushStatus,
) {
    let client = match tls::client(tls_settings) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(%e, "notify_push: no HTTP client");
            status.failed(&e);
            status.set_phase(PushPhase::Unavailable);
            return;
        }
    };

    let Some(info) = discover(&client, server, login, password, stop, health, status).await else {
        return;
    };
    if let Some(version) = &info.version {
        tracing::info!(nextcloud_version = %version, "connected to Nextcloud");
    }
    let endpoint = match info.push_websocket {
        Some(url) => url,
        None => {
            tracing::info!("notify_push not available — relying on TTL revalidation");
            status.set_phase(PushPhase::Unavailable);
            return;
        }
    };
    tracing::info!(%endpoint, "notify_push: connecting");
    status.set_endpoint(&endpoint);
    status.set_phase(PushPhase::Connecting);

    let mut backoff = 1u64;
    // Failures of the socket while the server itself answers. They say the
    // *endpoint* is broken, not the server, and after a few of them this loop
    // stops pretending otherwise: see [`WS_GIVE_UP_AFTER`].
    let mut endpoint_failures = 0u32;
    while !stop.load(Ordering::SeqCst) {
        match listen_once(
            &client,
            &endpoint,
            login,
            password,
            invalidate_after,
            sync_trigger,
            stop,
            health,
            status,
        )
        .await
        {
            Ok(()) => {
                // A clean close: the endpoint worked, reconnect promptly.
                backoff = 1;
                endpoint_failures = 0;
            }
            Err(e) => {
                status.failed(&e);
                // A failed socket is no verdict on the server. The endpoint is a
                // URL the server *advertises*, and it is wrong more often than
                // the server is down — a loopback address, a proxy that does not
                // upgrade — while every plain request sails through. Reported as
                // a failure, that made a mount with a broken endpoint announce
                // "connection lost" after every successful listing, forever.
                //
                // So the server is judged the one way that cannot lie about it:
                // an HTTP request. That keeps this loop the idle mount's
                // heartbeat (an outage is still noticed, and its end) without
                // letting the endpoint's problems speak for the server's.
                let server_answers = match health {
                    Some(health) => probe_server(&client, server, login, password, health).await,
                    None => true,
                };
                if server_answers {
                    endpoint_failures = endpoint_failures.saturating_add(1);
                    if endpoint_failures == WS_GIVE_UP_AFTER {
                        tracing::warn!(
                            %endpoint, error = %e,
                            "notify_push: the server answers but its WebSocket endpoint does not \
                             ({WS_GIVE_UP_AFTER} failures in a row) — falling back to TTL \
                             polling and retrying every {DEGRADED_BACKOFF_SECS}s. `wusel doctor` \
                             names the likely cause (notify_push's base_endpoint, or a reverse \
                             proxy without WebSocket upgrades)"
                        );
                    }
                } else {
                    // The server is gone; the endpoint may be fine. Judge it afresh
                    // once the server is back.
                    endpoint_failures = 0;
                }
                tracing::warn!(%e, "notify_push: connection ended, retrying in {backoff}s");
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        status.set_phase(PushPhase::Reconnecting);
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        // While the server is down this loop is what notices its return, so it
        // keeps the short cap. A broken endpoint behind a working server is worth
        // a look now and then, not two requests a minute.
        let cap = if endpoint_failures >= WS_GIVE_UP_AFTER {
            DEGRADED_BACKOFF_SECS
        } else {
            MAX_BACKOFF_SECS
        };
        backoff = (backoff * 2).min(cap);
    }
}

/// Judge the server by a plain HTTP request, on behalf of a loop whose own
/// failures cannot. Reports the outcome to `health` exactly as the WebDAV
/// client does for every request — an answer of any status is a reachable
/// server, only no answer or a 5xx is an incident — and returns whether the
/// server answered.
async fn probe_server(
    client: &reqwest::Client,
    server: &str,
    login: &str,
    password: &str,
    health: &crate::health::Reachability,
) -> bool {
    let answered = match capabilities::fetch(client, server, login, password).await {
        Ok(_) => true,
        Err(e) => {
            let incident = e.is_transport() || e.is_server_fault();
            if incident {
                health.failed(&e);
            }
            !incident
        }
    };
    if answered {
        health.ok();
    }
    answered
}

/// The longest wait between endpoint-discovery attempts. Matches the reconnect
/// cap: often enough that a returning network is noticed while the user is still
/// waiting for it, rare enough to be free.
const MAX_BACKOFF_SECS: u64 = 30;

/// How many socket failures in a row, each with the server demonstrably
/// answering, before the endpoint is written off as broken. Three, not one: a
/// proxy restart or a notify_push binary coming up after Nextcloud produces a
/// couple of refused connections that are nobody's misconfiguration.
const WS_GIVE_UP_AFTER: u32 = 3;

/// The reconnect cap once the endpoint is written off. Still retried — the
/// admin may fix it — but at a rate that costs nothing and logs nothing new.
const DEGRADED_BACKOFF_SECS: u64 = 300;

/// Ask the server what it can do — waiting out a network outage instead of
/// giving up on the mount's live updates.
///
/// Discovery happens exactly once per mount, which used to make a hiccup at
/// start-up permanent: a daemon that comes up before the network (or before DNS)
/// spent its single attempt on a dead network, logged one `WARN`, and ran
/// without push until somebody restarted the service. So a **transport** failure
/// is retried on a growing backoff.
///
/// That retry doubles as the mount's heartbeat while nothing else is talking to
/// the server: every attempt reports to `health`, so even a daemon nobody is
/// using notices that the server went away — and, more usefully, that it came
/// back — and can say so (see [`crate::health`]).
///
/// A server that answers with a *settled* client refusal — a rejected password
/// (`401`), or an OCS endpoint that is simply not there (`404`) — will answer the
/// same way forever, so it is not retried and TTL revalidation is the correct
/// fallback. A transient answer, though, *is* retried (see
/// [`retriable_for_discovery`]): transport blips, `408`/`429`/`5xx`, and a `403`,
/// which on the public capabilities endpoint is a proxy/CSRF-window or
/// brute-force-throttle artifact rather than a real refusal.
async fn discover(
    client: &reqwest::Client,
    server: &str,
    login: &str,
    password: &str,
    stop: &AtomicBool,
    health: Option<&crate::health::Reachability>,
    status: &PushStatus,
) -> Option<capabilities::ServerInfo> {
    let mut backoff = 1u64;
    loop {
        if stop.load(Ordering::SeqCst) {
            return None;
        }
        match capabilities::fetch(client, server, login, password).await {
            Ok(info) => {
                if let Some(health) = health {
                    health.ok();
                }
                return Some(info);
            }
            Err(e) if retriable_for_discovery(&e) => {
                // Every retriable failure is a reachability event, and which
                // *kind* is `health`'s to decide — no answer at all, or a server
                // answering that it cannot serve. Deciding it here (transport
                // only) is what made a discovery loop hammering a `502` the
                // quietest part of an outage.
                if let Some(health) = health {
                    health.failed(&e);
                }
                status.failed(&e);
                tracing::warn!(%e, "notify_push: capability lookup failed — retrying in {backoff}s");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(MAX_BACKOFF_SECS);
            }
            Err(e) => {
                tracing::warn!(%e, "notify_push: capability lookup refused — relying on TTL");
                status.failed(&e);
                status.set_phase(PushPhase::Unavailable);
                return None;
            }
        }
    }
}

/// Whether a failed capability lookup is worth retrying rather than giving up on
/// notify_push for the whole session.
///
/// Retry transport blips and transient HTTP (`408`/`429`/`5xx`) — and, unlike
/// [`Error::is_permanent`], also **403**: the capabilities endpoint is public, so
/// a 403 there is a proxy/CSRF-window or brute-force-throttle artifact during a
/// connection wobble, not a real permission denial. Treating it as final (the
/// old behaviour) stranded instant push on TTL until the next restart, even
/// though the very next request would have succeeded. A genuinely settled client
/// refusal (`401` bad credentials, `404` no such endpoint, other `4xx`) still
/// gives up — TTL revalidation is the right fallback there.
fn retriable_for_discovery(e: &Error) -> bool {
    e.is_transport() || !e.is_permanent() || matches!(e, Error::HttpStatus { status: 403, .. })
}

/// One connection: authenticate, then translate events into invalidations until
/// the socket closes or errors.
#[allow(clippy::too_many_arguments)]
async fn listen_once(
    client: &reqwest::Client,
    endpoint: &str,
    login: &str,
    password: &str,
    invalidate_after: &AtomicI64,
    sync_trigger: &std::sync::mpsc::Sender<()>,
    stop: &AtomicBool,
    health: Option<&crate::health::Reachability>,
    status: &PushStatus,
) -> Result<()> {
    // reqwest speaks http(s); map the ws(s) scheme the server advertises.
    let http_url = endpoint
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);

    let mut ws = client
        .get(&http_url)
        .upgrade()
        .send()
        .await
        .map_err(ws_err)?
        .into_websocket()
        .await
        .map_err(ws_err)?;

    // Authentication handshake: login name, then app password.
    ws.send(Message::Text(login.to_string()))
        .await
        .map_err(ws_err)?;
    ws.send(Message::Text(password.to_string()))
        .await
        .map_err(ws_err)?;

    while let Some(msg) = ws.next().await {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match msg.map_err(ws_err)? {
            Message::Text(t) => {
                let text = t.trim();
                if text == "authenticated" {
                    // The unambiguous "the server is there and talking to us"
                    // moment — and on an idle mount, often the only one.
                    if let Some(health) = health {
                        health.ok();
                    }
                    status.connected();
                    tracing::info!("notify_push: authenticated");
                } else if is_file_event(text) {
                    invalidate_after.store(now_secs(), Ordering::SeqCst);
                    // Kick the background syncer to find *what* changed (the event
                    // carries no path) by walking the cached tree's ETags.
                    let _ = sync_trigger.send(());
                    tracing::debug!(
                        event = text,
                        "notify_push: file change → invalidating listings + syncing"
                    );
                } else if text.starts_with("err") {
                    return Err(Error::Auth(format!("notify_push rejected us: {text}")));
                }
                // notify_activity / notify_notification carry no cache impact.
            }
            Message::Close { .. } => break,
            // Ping/Pong are handled by the library; other frames are ignored.
            _ => {}
        }
    }
    Ok(())
}

/// True for events meaning "the user's files changed" (a re-list is due).
fn is_file_event(msg: &str) -> bool {
    // Base signal is the bare word; newer servers may append an id/scope.
    msg == "notify_file" || msg.starts_with("notify_file ")
}

/// Map a websocket failure onto our error type, **keeping the transport layer
/// visible**.
///
/// A failed upgrade is an ordinary HTTP failure underneath, and only that layer
/// can tell "the server refused us" from "the server is not there at all" —
/// which is exactly what decides whether the user is told about it (see
/// [`crate::health`]). Folding everything into a string would throw that away.
///
/// Two layers can answer, so both are unfolded:
///
/// * `Reqwest` — the upgrade request itself failed. Its own conversion already
///   keeps a status where there is one, and reports none where nobody answered.
/// * `Handshake(UnexpectedStatusCode)` — the request arrived and the answer was
///   not `101`. That status is the whole point: a `502` from a proxy in front of
///   a server under maintenance is a user-facing event, and it is *not* an
///   unreachable server.
///
/// What is left — a protocol error mid-stream, a missing header — is one lost
/// connection and nothing the user can act on. It stays opaque deliberately: the
/// reconnect that follows is what judges the server, and it judges it on an
/// answer rather than on a dropped socket.
fn ws_err(e: reqwest_websocket::Error) -> Error {
    use reqwest_websocket::{Error as Ws, HandshakeError};
    match e {
        Ws::Reqwest(e) => Error::from(e),
        Ws::Handshake(HandshakeError::UnexpectedStatusCode(status)) => Error::HttpStatus {
            status: status.as_u16(),
            message: format!("websocket upgrade refused with {status}"),
        },
        other => Error::Other(format!("websocket: {other}")),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state `doctor` reads must tell a run of failures from a healthy
    /// connection, and forget the run once the connection is back.
    #[test]
    fn the_status_counts_a_run_of_failures_and_clears_it_on_connect() {
        let status = PushStatus::default();
        assert_eq!(status.snapshot().phase, PushPhase::Discovering);

        status.set_endpoint("wss://cloud.example.org/push/ws");
        status.set_phase(PushPhase::Connecting);
        status.failed(&Error::Http("[connect] connection refused".into()));
        status.set_phase(PushPhase::Reconnecting);
        status.failed(&Error::HttpStatus {
            status: 502,
            message: "websocket upgrade refused with 502 Bad Gateway".into(),
        });
        let s = status.snapshot();
        assert_eq!(s.phase, PushPhase::Reconnecting);
        assert_eq!(s.failures, 2);
        assert_eq!(s.connects, 0);
        assert_eq!(
            s.endpoint.as_deref(),
            Some("wss://cloud.example.org/push/ws")
        );
        assert!(
            s.last_error.as_deref().unwrap_or("").contains("502"),
            "the latest failure is the one reported: {:?}",
            s.last_error
        );

        status.connected();
        let s = status.snapshot();
        assert_eq!(s.phase, PushPhase::Connected);
        assert_eq!((s.failures, s.connects), (0, 1));
        assert_eq!(s.last_error, None, "old news once the socket is up");
        assert!(s.endpoint.is_some(), "the endpoint outlives the incident");
    }

    fn http(status: u16) -> Error {
        Error::HttpStatus {
            status,
            message: "test".into(),
        }
    }

    /// The upgrade's status is what tells "the server is down for maintenance"
    /// from "nothing answered", and the health tracker draws a different
    /// notification from each. Flattening it into a string — which is what
    /// happened to every non-reqwest websocket error — made a `502` during a
    /// backup window indistinguishable from noise, so nobody was ever told.
    #[test]
    fn a_refused_upgrade_keeps_its_status() {
        let e = ws_err(reqwest_websocket::Error::Handshake(
            reqwest_websocket::HandshakeError::UnexpectedStatusCode(
                reqwest::StatusCode::BAD_GATEWAY,
            ),
        ));
        assert!(
            matches!(e, Error::HttpStatus { status: 502, .. }),
            "a refused upgrade is the server answering, with its status: {e}"
        );
        assert!(!e.is_transport(), "the server answered, so it is reachable");
        assert!(
            e.is_server_fault(),
            "…but it cannot serve, which is its own event"
        );
    }

    #[test]
    fn discovery_retries_transient_failures_including_403() {
        // Retriable: a transport blip, transient HTTP, and a 403 (proxy/CSRF/
        // throttle artifact on the public capabilities endpoint).
        assert!(retriable_for_discovery(&Error::Http(
            "connection reset".into()
        )));
        assert!(retriable_for_discovery(&http(500)));
        assert!(retriable_for_discovery(&http(503)));
        assert!(retriable_for_discovery(&http(429)));
        assert!(retriable_for_discovery(&http(408)));
        assert!(
            retriable_for_discovery(&http(403)),
            "a 403 during a wobble must not strand push on TTL until restart"
        );
        // Settled client refusals: give up, TTL is the right fallback.
        assert!(!retriable_for_discovery(&http(401)));
        assert!(!retriable_for_discovery(&http(404)));
        assert!(!retriable_for_discovery(&http(400)));
    }

    #[test]
    fn classifies_events() {
        assert!(is_file_event("notify_file"));
        assert!(is_file_event("notify_file 12345"));
        assert!(!is_file_event("notify_activity"));
        assert!(!is_file_event("notify_notification"));
        assert!(!is_file_event("authenticated"));
    }
}
