// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Shared helpers for the wusel-mock integration tests.
//!
//! This lives in `tests/common/mod.rs` — the standard way to share code between
//! integration tests. (A `tests/common.rs` would not work: cargo compiles every
//! top-level file under `tests/` as its own test binary, so a phantom, empty
//! "common" suite would run on every `cargo test`.) Each test file pulls it in
//! with `mod common;`, compiling its own copy — hence the `dead_code` allow:
//! no single test binary uses every helper.
#![allow(dead_code)]

use std::path::Path;

/// An in-process wusel-mock WebDAV server, shut down when this guard drops.
///
/// The listener is bound *here*, synchronously, and handed to
/// [`wusel_mock::serve`] pre-bound (the same pattern as
/// `wusel-fuse/tests/mount_e2e.rs`). That kills two problems of the old
/// "free port + spawned binary" helper at once:
///
/// * **No port TOCTOU.** The old helper asked the OS for a free port, released
///   it, and had the spawned `wusel-mock` binary re-bind it later — under
///   parallel CI another test could grab the port in between. A pre-bound
///   listener cannot race.
/// * **No "wait until listening" poll.** Connections queue in the listener's
///   backlog from the moment `bind` returns, even before the accept loop runs,
///   so tests can connect immediately.
pub struct Mock {
    /// `host:port` the server listens on, e.g. `127.0.0.1:49213`.
    pub addr: String,
    /// The server's private runtime. `Option` so `Drop` can move it out.
    rt: Option<tokio::runtime::Runtime>,
}

impl Mock {
    /// Serve `root` as user `alice` (what every test uses) on an OS-chosen port.
    pub fn serve(root: &Path) -> Self {
        // Bind synchronously so the port is ours before this returns.
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
        let addr = std_listener
            .local_addr()
            .expect("mock listener addr")
            .to_string();
        // Tokio's reactor drives readiness via non-blocking sockets; a std
        // listener is blocking by default, so flip it before the handover.
        std_listener.set_nonblocking(true).expect("set_nonblocking");

        // A private single-worker runtime, so this one helper serves plain
        // `#[test]`s and `#[tokio::test]`s alike. (Nesting *runtimes* is fine —
        // only a nested `block_on` is not, and we never block here.)
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build mock runtime");
        let root = root.to_path_buf();
        rt.spawn(async move {
            // `from_std` registers with the runtime's reactor, so it must run
            // inside the runtime — hence here, not before the spawn.
            let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
            let _ = wusel_mock::serve(listener, root, "alice").await;
        });
        Mock { addr, rt: Some(rt) }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        // Shutting down drops the pending `serve` future, whose internal guard
        // then removes the uploads scratch directory. In a plain `#[test]` we
        // can afford the *blocking* shutdown, which guarantees that cleanup ran
        // before the test binary exits. Inside a `#[tokio::test]` body blocking
        // (like dropping a runtime outright) would panic, so there we fall back
        // to the non-blocking variant and the cleanup becomes best-effort — it
        // may lose a race against process exit.
        if let Some(rt) = self.rt.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                rt.shutdown_background();
            } else {
                rt.shutdown_timeout(std::time::Duration::from_secs(1));
            }
        }
    }
}

/// Point the XDG base directories at a throwaway location under `base`, so the
/// account's config/state/cache never touch the real home directory.
pub fn xdg_sandbox(base: &Path) {
    let xdg = base.join("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.join("config"));
    std::env::set_var("XDG_STATE_HOME", xdg.join("state"));
    std::env::set_var("XDG_CACHE_HOME", xdg.join("cache"));
}
