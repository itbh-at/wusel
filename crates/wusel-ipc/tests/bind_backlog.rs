// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The start-up race that wedged the File Provider at "Preparing": the extension
//! is launched by the system and points at our socket the instant it wants to
//! work, which can be while the daemon is still building its engine. The fix is
//! to [`wusel_ipc::bind`] the socket *first*, before that slow setup — a bound
//! Unix socket already listens, so the connect lands in the kernel backlog and
//! waits instead of being refused. These tests pin that invariant; `serve` need
//! not be running yet.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;

/// A short, unique socket path in the (sticky, or per-user) temp dir —
/// `prepare_socket_dir` accepts it, and it stays well inside the 108-byte
/// `sun_path` limit.
fn socket_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("wusel-ipc-{tag}-{}.sock", std::process::id()))
}

#[test]
fn a_bound_socket_accepts_connects_before_serve_runs() {
    let path = socket_path("bind-backlog");
    let _ = std::fs::remove_file(&path);
    let listener = wusel_ipc::bind(&path).expect("bind");

    // Nothing is calling `accept` yet — but a client must still connect cleanly,
    // its request queued in the backlog until `serve` drains it. This is exactly
    // the window that used to refuse the extension and leave it stuck.
    UnixStream::connect(&path).expect("first connect before serve must succeed");
    UnixStream::connect(&path).expect("a second connect must succeed too");

    // Owner-only, as `serve` requires of the socket it accepts on.
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the socket must be private to the owner");

    drop(listener);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn bind_clears_a_stale_socket_file() {
    let path = socket_path("bind-stale");
    // A leftover file where the socket goes, as an unclean exit leaves behind;
    // without the tidy-up the rebind would fail with EADDRINUSE.
    std::fs::write(&path, b"stale").unwrap();

    let listener = wusel_ipc::bind(&path).expect("bind must remove the stale file and succeed");
    UnixStream::connect(&path).expect("connect to the freshly-bound socket");

    drop(listener);
    let _ = std::fs::remove_file(&path);
}
