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

    /// An HTTP response with a status code — kept structurally (not only in the
    /// message) so a failed upload can tell a *permanent* refusal (wrong
    /// permissions, a conflict, no quota — retrying will not help) from a
    /// transient one (a 5xx or a timeout — retrying will). See
    /// [`Error::is_permanent`].
    #[error("HTTP {status}: {message}")]
    HttpStatus { status: u16, message: String },

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
        // Captured before the error is folded into a message, so the permanence
        // of an upload failure can be judged structurally later.
        let status = e.status().map(|s| s.as_u16());
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
        match status {
            Some(status) => Error::HttpStatus {
                status,
                message: msg,
            },
            None => Error::Http(msg),
        }
    }
}

impl Error {
    /// Whether this is a **transport** failure — the server never answered at
    /// all (DNS, connect, TLS, a timeout, a dropped connection), as opposed to a
    /// server that answered, even badly.
    ///
    /// The distinction is structural, not textual: `From<reqwest::Error>` keeps
    /// a status code as [`Error::HttpStatus`] and everything without one as
    /// [`Error::Http`], and "no status" is precisely "nobody answered". It is
    /// what [`crate::health`] watches: an unreachable server is a user-facing
    /// event ("your folder cannot be reached"), a 500 is not.
    #[must_use]
    pub fn is_transport(&self) -> bool {
        matches!(self, Error::Http(_))
    }

    /// Whether retrying this failure is pointless. Used by the asynchronous
    /// uploader: a permanent failure is parked and the user is told; a transient
    /// one is retried until it lands.
    ///
    /// Permanent: a client refusal (4xx — wrong permissions, a name conflict, a
    /// malformed request) that will not fix itself, plus `507 Insufficient
    /// Storage` (no quota). Not permanent: `408`/`429` (slow down / try again),
    /// every other 5xx, and every transport error (timeout, connection reset).
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        match self {
            Error::Denied => true,
            Error::HttpStatus { status, .. } => {
                *status == 507 || ((400..500).contains(status) && *status != 408 && *status != 429)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    fn http(status: u16) -> Error {
        Error::HttpStatus {
            status,
            message: "test".into(),
        }
    }

    #[test]
    fn permanent_failures_are_not_retried() {
        // Client refusals and a full quota will not fix themselves.
        assert!(http(403).is_permanent(), "forbidden");
        assert!(http(409).is_permanent(), "conflict");
        assert!(http(400).is_permanent(), "bad request");
        assert!(http(507).is_permanent(), "insufficient storage");
        assert!(Error::Denied.is_permanent());
    }

    #[test]
    fn transient_failures_are_retried() {
        // Server hiccups and "slow down" are worth another try.
        assert!(!http(500).is_permanent(), "internal server error");
        assert!(!http(503).is_permanent(), "service unavailable");
        assert!(!http(408).is_permanent(), "request timeout");
        assert!(!http(429).is_permanent(), "too many requests");
        assert!(
            !Error::Http("connection reset".into()).is_permanent(),
            "a transport error is transient"
        );
        assert!(!Error::NotFound.is_permanent());
    }

    #[test]
    fn only_a_missing_answer_counts_as_unreachable() {
        // No status code — nobody answered.
        assert!(Error::Http("[connect] dns error".into()).is_transport());
        assert!(Error::Http("[timeout] operation timed out".into()).is_transport());
        // A status code is an answer, however unwelcome; so is everything that
        // never left this machine.
        assert!(!http(500).is_transport(), "the server answered");
        assert!(!http(401).is_transport(), "the server refused us");
        assert!(!Error::NotFound.is_transport());
        assert!(!Error::Auth("revoked".into()).is_transport());
        assert!(!Error::Other("no write context".into()).is_transport());
    }
}
