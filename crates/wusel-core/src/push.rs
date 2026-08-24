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
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use reqwest_websocket::{Message, RequestBuilderExt};

use crate::config::TlsSettings;
use crate::{capabilities, tls, Error, Result};

/// Handle to the background listener. Dropping it asks the loop to stop between
/// reconnects; the daemon normally keeps it for the mount's lifetime.
pub struct PushListener {
    stop: Arc<AtomicBool>,
    // Kept so the thread is owned by the handle; joined on a clean stop only.
    _handle: Option<JoinHandle<()>>,
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
    let (server, login, password) = (
        server_url.to_string(),
        login.to_string(),
        password.to_string(),
    );
    let stop_thread = stop.clone();

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
            ));
        })
        .expect("spawn notify-push thread");

    PushListener {
        stop,
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
) {
    let client = match tls::client(tls_settings) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(%e, "notify_push: no HTTP client");
            return;
        }
    };

    let Some(info) = discover(&client, server, login, password, stop, health).await else {
        return;
    };
    if let Some(version) = &info.version {
        tracing::info!(nextcloud_version = %version, "connected to Nextcloud");
    }
    let endpoint = match info.push_websocket {
        Some(url) => url,
        None => {
            tracing::info!("notify_push not available — relying on TTL revalidation");
            return;
        }
    };
    tracing::info!(%endpoint, "notify_push: connecting");

    let mut backoff = 1u64;
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
        )
        .await
        {
            Ok(()) => backoff = 1, // clean close → reconnect promptly
            Err(e) => {
                // The other half of the heartbeat: once the socket is up, this
                // reconnect loop is the only thing still talking to the server on
                // an idle mount, so its failures are what notice an outage that
                // starts while nobody is using the folder.
                if let Some(health) = health {
                    health.failed(&e);
                }
                tracing::warn!(%e, "notify_push: connection ended, retrying in {backoff}s");
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(MAX_BACKOFF_SECS);
    }
}

/// The longest wait between endpoint-discovery attempts. Matches the reconnect
/// cap: often enough that a returning network is noticed while the user is still
/// waiting for it, rare enough to be free.
const MAX_BACKOFF_SECS: u64 = 30;

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
/// A server that *answers* is not retried: a rejected password, or an OCS
/// endpoint that is simply not there, will answer the same way forever, and TTL
/// revalidation is the correct fallback for it.
async fn discover(
    client: &reqwest::Client,
    server: &str,
    login: &str,
    password: &str,
    stop: &AtomicBool,
    health: Option<&crate::health::Reachability>,
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
            Err(e) if e.is_transport() => {
                if let Some(health) = health {
                    health.failed(&e);
                }
                tracing::warn!(%e, "notify_push: the server cannot be reached — retrying in {backoff}s");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(MAX_BACKOFF_SECS);
            }
            Err(e) => {
                tracing::warn!(%e, "notify_push: capability lookup failed — relying on TTL");
                return None;
            }
        }
    }
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
fn ws_err(e: reqwest_websocket::Error) -> Error {
    match e {
        reqwest_websocket::Error::Reqwest(e) => Error::from(e),
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

    #[test]
    fn classifies_events() {
        assert!(is_file_event("notify_file"));
        assert!(is_file_event("notify_file 12345"));
        assert!(!is_file_event("notify_activity"));
        assert!(!is_file_event("notify_notification"));
        assert!(!is_file_event("authenticated"));
    }
}
