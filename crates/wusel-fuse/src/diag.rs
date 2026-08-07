// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Serving the diagnostics snapshot on a unix socket.
//!
//! The mount binds a per-user socket and answers each connection with one JSON
//! [`DiagReport`] — the machine's occupancy plus the count of parked replies —
//! then closes. `wusel doctor`, a separate run of the binary, connects and reads
//! it. Read-only, same-user only (a `0600` socket in a `0700` directory), and
//! name-free, so nothing private crosses it.
//!
//! Everything here is best-effort: a diagnostics socket that cannot be bound
//! must never stop a mount, so failures are logged and swallowed, not
//! propagated.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;

use wusel_core::diag::DiagReport;
use wusel_core::runtime::DiagHandle;

use crate::dispatch::Replies;

/// A bound diagnostics socket. Dropping it removes the socket file.
pub struct DiagSocket {
    path: PathBuf,
}

impl DiagSocket {
    /// Bind the socket and serve snapshots on a background thread. Returns
    /// `None` — never an error — when it cannot bind, because a mount must go
    /// ahead without its diagnostics socket.
    #[must_use]
    pub fn bind(path: PathBuf, handle: DiagHandle, replies: Arc<Replies>) -> Option<Self> {
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!(
                    dir = %dir.display(), error = %e,
                    "diagnostics socket: cannot create the runtime directory"
                );
                return None;
            }
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        // A stale socket from a crashed predecessor makes `bind` fail with
        // EADDRINUSE, and it is ours to clear.
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "diagnostics socket: bind failed");
                return None;
            }
        };
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        tracing::info!(path = %path.display(), "diagnostics socket ready");

        std::thread::Builder::new()
            .name("wusel-diag".into())
            .spawn(move || serve(&listener, &handle, &replies))
            .expect("spawn the diagnostics socket thread");

        Some(Self { path })
    }
}

impl Drop for DiagSocket {
    fn drop(&mut self) {
        // Tidy up so a later mount does not meet a stale socket. The tmpfs would
        // clear it at logout anyway; this makes it prompt.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Answer each connection with one JSON report. Runs until the listener is
/// dropped (at process exit); an error on one connection never ends the loop.
fn serve(listener: &UnixListener, handle: &DiagHandle, replies: &Arc<Replies>) {
    for conn in listener.incoming() {
        let Ok(mut stream) = conn else { continue };
        let payload = match build_report(handle, replies) {
            Ok(json) => json,
            // The decider did not answer in time — itself a diagnosis. Hand it
            // over as something `doctor` can show rather than nothing.
            Err(e) => format!("{{\"error\":\"{}\"}}", e.to_string().replace('"', "'")),
        };
        let _ = stream.write_all(payload.as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
    }
}

/// The report a connection is answered with: the substrate's snapshot plus the
/// count of parked replies, which only the frontend knows.
fn build_report(handle: &DiagHandle, replies: &Arc<Replies>) -> wusel_core::Result<String> {
    let snapshot = handle.snapshot()?;
    let mut report = DiagReport::from_substrate(&snapshot);
    report.replies_pending = Some(replies.pending_count());
    report.to_json()
}
