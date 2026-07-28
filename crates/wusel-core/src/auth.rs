// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Nextcloud **Login Flow v2** — the recommended, browser-based way
//! to obtain an app password without ever seeing the real user password.
//!
//! Flow:
//! 1. [`begin`] — `POST /index.php/login/v2` → returns `login` URL + poll token.
//! 2. The user opens the `login` URL in the browser and confirms.
//! 3. Call [`poll`] repeatedly until the [`Credentials`] come back instead of
//!    [`Error::LoginPending`] (`server`, `loginName`, `appPassword`).
//!
//! The app password is revocable and is stored locally in encrypted form
//! (the storage backend follows in a later phase — e.g. Secret Service).

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Result of [`begin`]: what the user has to open in the browser and
/// what we poll with afterwards.
#[derive(Debug, Clone)]
pub struct LoginInitiation {
    /// Open this URL in the browser (`login` from the server response).
    pub login_url: String,
    /// Endpoint that [`poll`] runs against.
    pub poll_endpoint: String,
    /// One-time token for the polling.
    pub poll_token: String,
}

/// Successfully obtained credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub server: String,
    #[serde(rename = "loginName")]
    pub login_name: String,
    #[serde(rename = "appPassword")]
    pub app_password: String,
}

#[derive(Debug, Deserialize)]
struct RawInit {
    poll: RawPoll,
    login: String,
}

#[derive(Debug, Deserialize)]
struct RawPoll {
    token: String,
    endpoint: String,
}

/// Starts Login Flow v2 against `server_url` (e.g. `https://cloud.example.org`).
pub async fn begin(client: &reqwest::Client, server_url: &str) -> Result<LoginInitiation> {
    let url = format!("{}/index.php/login/v2", server_url.trim_end_matches('/'));
    let raw: RawInit = client
        .post(&url)
        // Kept per-request even though `tls::client` already sets a default
        // `wusel` User-Agent: on THIS request the UA is not just telemetry —
        // Nextcloud records it as the device/app name shown next to the app
        // password in the user's security settings. `begin` accepts any
        // injected `reqwest::Client`, so pin the name here rather than rely on
        // every caller building the client through our `tls` module.
        .header("User-Agent", "wusel")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(LoginInitiation {
        login_url: raw.login,
        poll_endpoint: raw.poll.endpoint,
        poll_token: raw.poll.token,
    })
}

/// Queries the poll endpoint once.
///
/// * `Ok(Credentials)` — user has confirmed, app password received.
/// * `Err(Error::LoginPending)` — not confirmed yet, call again later.
pub async fn poll(client: &reqwest::Client, init: &LoginInitiation) -> Result<Credentials> {
    let resp = client
        .post(&init.poll_endpoint)
        .form(&[("token", init.poll_token.as_str())])
        .send()
        .await?;

    match resp.status() {
        reqwest::StatusCode::OK => Ok(resp.json::<Credentials>().await?),
        // As long as the user has not confirmed, the server responds with 404.
        reqwest::StatusCode::NOT_FOUND => Err(Error::LoginPending),
        other => Err(Error::Auth(format!("unexpected poll status: {other}"))),
    }
}
