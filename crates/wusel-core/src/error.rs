// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Central error types for wusel-core.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// A transport-level HTTP failure. The message already carries the full
    /// cause chain (see `From<reqwest::Error>`), because reqwest's own `Display`
    /// prints only its outermost layer ("error sending request for url …") and
    /// hides what actually went wrong.
    #[error("HTTP error: {0}")]
    Http(String),

    #[error("Authentication failed: {0}")]
    Auth(String),

    #[error("Login flow still in progress (user has not confirmed yet)")]
    LoginPending,

    #[error("WebDAV response could not be parsed: {0}")]
    WebDav(String),

    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("Invalid URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("permission denied")]
    Denied,

    /// The resource is gone on the server (HTTP 404) — e.g. a file deleted
    /// server-side that our cache still lists. Distinct from a transport error so
    /// callers can prune the stale node and report a stale-handle error rather
    /// than a hard failure.
    #[error("not found on the server")]
    NotFound,

    #[error("{0}")]
    Other(String),
}

impl From<reqwest::Error> for Error {
    /// Fold a reqwest error into a message that names the *real* cause.
    ///
    /// `reqwest::Error`'s `Display` shows only its top layer — for a broken
    /// connection that is the near-useless "error sending request for url …",
    /// while the actual reason (connection reset, TLS failure, HTTP/2 GOAWAY,
    /// DNS, timeout) sits one or more `source()` levels down. We prepend a coarse
    /// kind tag for quick triage and then append the whole source chain, so a log
    /// line pins the failure without a debugger.
    fn from(e: reqwest::Error) -> Self {
        use std::error::Error as _;
        let kind = if e.is_timeout() {
            "[timeout] "
        } else if e.is_connect() {
            "[connect] "
        } else if e.is_request() {
            "[request] "
        } else if e.is_body() {
            "[body] "
        } else if e.is_decode() {
            "[decode] "
        } else {
            ""
        };
        let mut msg = format!("{kind}{e}");
        let mut src = e.source();
        while let Some(s) = src {
            msg.push_str(" → ");
            msg.push_str(&s.to_string());
            src = s.source();
        }
        Error::Http(msg)
    }
}
