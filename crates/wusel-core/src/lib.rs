// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! wusel-core — platform-independent core of wusel.
//!
//! Contains everything except the FUSE kernel adapter:
//!
//! * [`auth`]  — Nextcloud Login Flow v2 (OAuth2-like, browser-based) + token handling
//! * [`credentials`] — persisting the app password for the single account
//! * [`webdav`] — WebDAV client (PROPFIND, GET with Range, chunked PUT)
//! * [`model`]  — domain types (remote entries, ETags, permissions)
//! * [`state`]  — local state in SQLite (inode↔path mapping, ETags, pinning)
//! * [`config`] — configuration & paths
//! * [`content`] — content delivery: `ContentSource` (live WebDAV) + a caching decorator
//! * [`capabilities`] — OCS capability discovery (the notify_push endpoint)
//! * [`push`]   — notify_push WebSocket client for instant cache invalidation
//! * [`health`] — is the server reachable? one shared answer, and the connection-lost/restored notices
//! * [`provider`] — the frontend-agnostic facade (list/stat/lookup/read) every OS frontend calls
//!
//! This crate builds and tests natively on Linux and macOS — no FUSE, no kernel module.

pub mod auth;
pub mod capabilities;
pub mod config;
pub mod content;
pub mod credentials;
pub mod desktop;
pub mod diag;
pub mod error;
pub mod health;
pub mod ignore;
pub mod keyring;
pub mod model;
pub mod mount;
pub mod pins;
pub mod provider;
pub mod push;
pub mod runtime;
pub mod search;
pub mod state;
pub mod storage;
pub mod tls;
pub mod webdav;

pub use error::{Error, Result};

/// The tokio runtime handle [`provider::Provider::runtime`] hands out, re-exported
/// so a frontend can name and store it (to run reads off its dispatch thread)
/// without taking its own tokio dependency — the crate's dependency-minimalism
/// rule. Calling `spawn_blocking` on it needs no `tokio` import (inherent method).
pub use tokio::runtime::Handle as RuntimeHandle;

/// Serializes unit tests that mutate or read process-global environment
/// variables (the `XDG_*` overrides): `cargo test` runs tests in parallel
/// threads, and `std::env::set_var` in one test races `std::env::var` in
/// another. **One** crate-wide lock (not one per test module — separate locks
/// would not exclude each other) makes those tests take turns. Poison-tolerant:
/// a panicking test must not cascade into every later env-touching test.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
