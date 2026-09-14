// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `wusel-ipc` — a socket frontend that speaks the engine's **intent protocol**.
//!
//! This is the Rust half of a future macOS File Provider extension (and, later,
//! a Windows Cloud Filter host): the platform side is written in Swift/C# and
//! cannot link the engine, so it drives it over a local Unix-domain socket
//! instead. Everything here is deliberately platform-independent — no FUSE, no
//! OS integration — so the whole frontend boundary is testable natively on
//! Linux and macOS.
//!
//! The pieces:
//!
//! * [`Driver`] — owns the running engine and turns its asynchronous answer
//!   stream into synchronous, concurrent [`Driver::call`]s, plus an intent-only
//!   [`Driver::resolve`].
//! * [`wire`] — the framed request/response protocol (**provisional**; see the
//!   module docs).
//! * [`bind`] then [`serve`] — bind the `UnixListener` up front (so a client's
//!   connect is never refused while the engine is still starting), then run a
//!   blocking accept loop that drives each connection through the driver.
//!
//! The protocol covers the **read path** — `stat`, `enumerate`, `fetch` — the
//! **write path** — `create`, `write`, `publish`, `remove`, `move`, `setattr` —
//! and a pushed **change stream** (`watch`). A `publish` blocks until the upload
//! lands (the driver runs synchronous write-back), so a frontend's item-upload
//! completion means the server has the bytes.

use std::io::{BufReader, BufWriter};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use wusel_core::state::NodeRow;
use wusel_fsm::{Intent, Outcome};

pub mod client;
pub mod driver;
pub mod events;
pub mod notices;
pub mod wire;

pub use client::Client;
pub use driver::Driver;
pub use events::{Change, Events};
pub use notices::{IpcDesktop, NoticeOut};
pub use wire::{ChangeKind, Entry, ErrorKind, Request, Response, Severity};

/// The most connections served at once.
///
/// A connection costs a thread, and nothing about the protocol makes a client
/// close one promptly — `watch` and `notices` hold theirs open for the session.
/// Without a ceiling a client in a reconnect loop, or a deliberate flood from
/// another process of this user, spawns threads until the daemon dies and takes
/// the mount with it. Far above what a desktop needs: a file manager holds a
/// status connection and a `watch`, the agent one more.
const MAX_CONNECTIONS: usize = 64;

/// Prepare the directory the socket lives in, and prove it is private to us.
///
/// This is a security boundary, not tidiness. Linux checks a socket's
/// permissions on `connect`, and the directory's on the traversal to it — and
/// what lies behind this one is not diagnostics but the whole intent protocol:
/// `fetch` reads any file's content, `write`/`publish`/`remove`/`move` change
/// the *server's* data, `watch` streams every path the user touches. A socket
/// another local user can reach is full access to the account.
///
/// Created with `DirBuilder::mode`, not chmod-ed afterwards: a create-then-chmod
/// leaves a window in which the directory is traversable by everyone. An
/// existing directory is tightened if it is ours (an install that predates this,
/// or an inherited 0755) and refused if it is not — including when it is a
/// symlink, which is how a hostile `/tmp` would redirect us.
fn prepare_socket_dir(dir: &Path) -> std::io::Result<()> {
    let refuse = |why: &str| {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to serve in {}: {why}", dir.display()),
        ))
    };
    // `symlink_metadata`, so a symlink is seen as a symlink rather than
    // silently followed to whatever it points at.
    match std::fs::symlink_metadata(dir) {
        Ok(md) if md.file_type().is_symlink() => refuse("it is a symlink"),
        Ok(md) if !md.is_dir() => refuse("it is not a directory"),
        Ok(md) => {
            let mode = md.permissions().mode();
            // SAFETY: `geteuid` has no preconditions and cannot fail.
            if md.uid() != unsafe { libc::geteuid() } {
                // Someone else's directory is tolerable only when it is sticky:
                // that is the bit stopping another user from deleting our
                // socket and binding their own in its place, which would have
                // every client connect to them instead. `/tmp` (1777) passes;
                // an attacker-created `/tmp/wusel` (0755) does not, and that is
                // the case worth refusing.
                if mode & 0o1000 == 0 {
                    return refuse("it belongs to another user and is not sticky");
                }
                return Ok(());
            }
            // Ours: tighten it. An install predating this check, or a directory
            // inherited at 0755, becomes 0700 rather than being refused.
            if mode & 0o077 != 0 {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(dir),
        Err(e) => Err(e),
    }
}

/// The effective uid on the other end of `stream`, as the kernel recorded it at
/// `connect` time — not something the peer can claim or forge.
///
/// `std::os::unix::net::UnixStream::peer_cred` does exactly this, but it is
/// still unstable (`peer_credentials_unix_socket`, rust#42839), so the question
/// is asked of each platform directly. The two spellings are not
/// interchangeable: `SO_PEERCRED` is Linux's, and glibc ships no `getpeereid`,
/// while `getpeereid` is what Apple and the BSDs provide. This crate builds on
/// both — it is the Rust half of the macOS File Provider — so both are here.
#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a correctly-sized, initialised `ucred` and `len` says
    // so; the fd stays owned by `stream` for the duration of the call.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut cred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0u32, 0u32);
    // SAFETY: both out-parameters are valid, writable and correctly typed; the
    // fd stays owned by `stream` for the duration of the call.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(uid)
}

/// Bind `socket_path` and return a listening socket, ready to hand to [`serve`].
///
/// Split from [`serve`] so the daemon can bind **before** its slow start-up —
/// credentials, DAV, the push connection, the engine — has run. A bound Unix
/// socket is already *listening*: a client's `connect` lands in the kernel
/// backlog and succeeds at once, even while nothing is calling `accept` yet. The
/// File Provider extension, which the system may launch and point at us at any
/// instant, therefore never meets a refused connect during that window — it used
/// to, then give up after a few retries and leave every item wedged at
/// "Preparing". [`serve`] drains the backlog once the engine is ready.
///
/// The socket is **private to the user running the daemon**: its directory is
/// proved to be ours and 0700 (see [`prepare_socket_dir`]) and the socket itself
/// is narrowed to 0600. A stale socket file from an unclean exit is removed
/// first, so the rebind does not fail with `EADDRINUSE`.
///
/// # Errors
/// If the socket's directory is not private to this user, or the socket cannot
/// be bound.
pub fn bind(socket_path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = socket_path.parent() {
        prepare_socket_dir(dir)?;
    }
    // A leftover socket from a crashed predecessor makes `bind` fail with
    // EADDRINUSE; removing it first is the same tidy-up the diagnostics socket
    // does. Best-effort: if it is not there, nothing to remove.
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    // Narrow the socket from the umask-derived mode (0755 under the usual 0022)
    // to owner-only. There is a window between the two calls; it is closed by
    // the directory above being 0700 and ours, which is why that check is not
    // optional. Setting the process umask around the bind instead would be
    // worse here — it is process-global, and this runs inside a daemon whose
    // other threads are creating files of their own.
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serve intent requests over an already-bound `listener` (from [`bind`]) until
/// it fails. Blocks the calling thread; each accepted connection is handled on
/// its own thread, so several clients (or a client with several connections) are
/// served concurrently through the shared [`Driver`].
///
/// Every accepted connection's peer credentials must name our own uid — file
/// permissions depend on where the caller put the socket, `SO_PEERCRED` does
/// not, so a `--socket` pointed somewhere unfortunate is still not a way in.
///
/// # Errors
/// A failure on an individual connection is logged and does not stop the loop;
/// the `Err` return is reserved for the listener itself failing.
pub fn serve(
    driver: Arc<Driver>,
    events: Arc<Events>,
    desktop: Arc<IpcDesktop>,
    listener: UnixListener,
) -> std::io::Result<()> {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    let live = Arc::new(AtomicUsize::new(0));

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // Who is on the other end? A peer we cannot identify is refused
                // rather than trusted — an unreadable credential is not a
                // reason to hand over the account.
                match peer_uid(&stream) {
                    Ok(uid) if uid == me => {}
                    Ok(uid) => {
                        tracing::warn!(uid, "refused an ipc connection from another user");
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "refused an ipc connection with unreadable credentials");
                        continue;
                    }
                }
                if live.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    tracing::warn!(
                        max = MAX_CONNECTIONS,
                        "refused an ipc connection: too many already open"
                    );
                    continue;
                }
                let driver = Arc::clone(&driver);
                let events = Arc::clone(&events);
                let desktop = Arc::clone(&desktop);
                let live_here = Arc::clone(&live);
                live.fetch_add(1, Ordering::Relaxed);
                // A thread per connection, detached: a connection outlives the
                // accept, and none of them coordinate — the driver is the only
                // shared state and it is internally synchronised.
                if let Err(e) = std::thread::Builder::new()
                    .name("wusel-ipc-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle_connection(&driver, &events, &desktop, stream) {
                            tracing::debug!(error = %e, "ipc connection ended with an error");
                        }
                        live_here.fetch_sub(1, Ordering::Relaxed);
                    })
                {
                    // The slot was taken before the spawn; give it back, or a
                    // run of failed spawns would retire the whole budget.
                    live.fetch_sub(1, Ordering::Relaxed);
                    tracing::warn!(error = %e, "could not spawn an ipc connection thread");
                }
            }
            Err(e) => tracing::debug!(error = %e, "ipc accept failed"),
        }
    }
    Ok(())
}

/// Read framed requests off one connection until the peer hangs up, answering
/// each in turn. A malformed request is answered with an error frame rather than
/// closing the connection, so one bad frame does not drop a healthy client.
fn handle_connection(
    driver: &Driver,
    events: &Events,
    desktop: &IpcDesktop,
    stream: UnixStream,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    while let Some(frame) = wire::read_frame(&mut reader)? {
        let request = match serde_json::from_slice::<wire::Request>(&frame) {
            Ok(request) => request,
            Err(e) => {
                tracing::debug!(error = %e, "unparseable ipc request frame");
                let resp = Response::error(ErrorKind::BadRequest);
                wire::write_frame(
                    &mut writer,
                    &resp.to_frame().map_err(std::io::Error::other)?,
                )?;
                writer.flush()?;
                continue;
            }
        };
        // `watch` turns the connection one-way: from here it only carries pushed
        // change events, until the client hangs up (noticed on the next write) or
        // the daemon shuts the event stream down.
        if request.op == "watch" {
            return stream_changes(events, &mut writer);
        }
        // `notices` is the twin of `watch` for user-facing toasts: the connection
        // goes one-way and carries pushed, already-localized notices until the
        // client hangs up. A separate channel so a frontend can subscribe to just
        // the notices (the agent) or just the changes (the extension).
        if request.op == "notices" {
            return stream_notices(desktop, &mut writer);
        }
        // `test-notice` injects a sample notice of the severity named in `path`
        // (`success` | `warning` | `error`) into the fan-out, so `wusel desktop
        // notify` can verify the banner path end-to-end without a real sync event.
        // The reply says how many subscribers received it.
        if request.op == "test-notice" {
            let resp = match parse_severity(&request.path) {
                Some(severity) => Response::notified(desktop.emit_test(severity) as u32),
                None => Response::error(ErrorKind::BadRequest),
            };
            wire::write_frame(
                &mut writer,
                &resp.to_frame().map_err(std::io::Error::other)?,
            )?;
            writer.flush()?;
            continue;
        }
        // `reachable` answers "is the server reachable right now?" from the
        // daemon's health tracker. The macOS File Provider asks before a
        // destructive reimport: a reimport that races an unreachable server
        // wedges the folder with a stuck upload error, so it must skip when this
        // says no. Answered here (not in `dispatch`) because the tracker lives on
        // the desktop backend.
        if request.op == "reachable" {
            let resp = Response::reachable(desktop.reachable_now());
            wire::write_frame(
                &mut writer,
                &resp.to_frame().map_err(std::io::Error::other)?,
            )?;
            writer.flush()?;
            continue;
        }
        // `changes` is the pull twin of `watch`: a replicated frontend replays the
        // log from its sync anchor instead of holding a live stream open. Answered
        // here rather than in `dispatch` because the log lives in `events`.
        if request.op == "changes" {
            let (seq, changes) = events.changes_since(request.since);
            let resp = Response::changes(
                seq,
                changes
                    .into_iter()
                    .map(|c| wire::ChangeEntry {
                        change: c.kind,
                        path: c.path,
                    })
                    .collect(),
            );
            wire::write_frame(
                &mut writer,
                &resp.to_frame().map_err(std::io::Error::other)?,
            )?;
            writer.flush()?;
            continue;
        }
        // A `write` carries its bytes in the frame right after the request — the
        // mirror of a `bytes` response, which is a header frame then a content
        // frame. A peer that hangs up between the two is simply gone.
        let data = if request.op == "write" {
            match wire::read_frame(&mut reader)? {
                Some(bytes) => Some(bytes),
                None => return Ok(()),
            }
        } else {
            None
        };
        let (header, body) = dispatch(driver, &request, data);
        wire::write_frame(
            &mut writer,
            &header.to_frame().map_err(std::io::Error::other)?,
        )?;
        if let Some(bytes) = body {
            wire::write_frame(&mut writer, &bytes)?;
        }
        // Flush per request: the client blocks for its answer before sending the
        // next, so nothing is gained by holding bytes back, and a stuck buffer
        // would look like a hung server.
        writer.flush()?;
    }
    Ok(())
}

/// Serve a `watch` connection: subscribe to the change fan-out and write each
/// change as a `changed` frame, until the stream ends (daemon shutdown) or a
/// write fails (the client is gone).
fn stream_changes(events: &Events, writer: &mut BufWriter<UnixStream>) -> std::io::Result<()> {
    use std::io::Write;
    let changes = events.subscribe();
    for change in changes {
        let resp = Response::changed(change.kind, change.path);
        wire::write_frame(writer, &resp.to_frame().map_err(std::io::Error::other)?)?;
        writer.flush()?;
    }
    Ok(())
}

/// Serve a `notices` connection: subscribe to the notice fan-out and write each
/// localized notice as a `notice` frame, until the stream ends or the client is
/// gone. The mirror of [`stream_changes`] for the second push channel.
fn stream_notices(desktop: &IpcDesktop, writer: &mut BufWriter<UnixStream>) -> std::io::Result<()> {
    use std::io::Write;
    let feed = desktop.subscribe();
    for notice in feed {
        let resp = Response::notice(notice.kind, notice.severity, notice.title, notice.body);
        wire::write_frame(writer, &resp.to_frame().map_err(std::io::Error::other)?)?;
        writer.flush()?;
    }
    Ok(())
}

/// The severity a `test-notice` names in its `path` field. `info` is accepted as
/// a synonym for `success`, matching the CLI's `TestSeverity::Info`.
fn parse_severity(name: &str) -> Option<wusel_core::desktop::Severity> {
    use wusel_core::desktop::Severity;
    match name {
        "success" | "info" => Some(Severity::Success),
        "warning" => Some(Severity::Warning),
        "error" => Some(Severity::Error),
        _ => None,
    }
}

/// Turn one request into a response header and, for `fetch`, its content frame.
/// `data` carries a `write`'s bytes (already read off the wire); it is `None`
/// for every other op.
///
/// The whole frontend contract lives here: resolve the path through intents,
/// issue the matching intent, and map the engine's `(Outcome, Payload)` onto the
/// wire. Every failure becomes an error response — a frontend never sees a
/// panic.
fn dispatch(
    driver: &Driver,
    request: &wire::Request,
    data: Option<Vec<u8>>,
) -> (Response, Option<Vec<u8>>) {
    use wusel_core::runtime::Payload;
    match request.op.as_str() {
        "stat" => {
            let Some(object) = driver.resolve(&request.path) else {
                return (Response::error(ErrorKind::NotFound), None);
            };
            match driver.call(object, Intent::Stat) {
                (Outcome::Ok, wusel_core::runtime::Payload::Node(node)) => {
                    let (pinned, stale, state, folder_kind) = status_of(driver, &node);
                    (
                        node_response(&node, pinned, stale, state, folder_kind),
                        None,
                    )
                }
                (outcome, _) => (Response::error(failure_kind(outcome)), None),
            }
        }
        "enumerate" => {
            let Some(object) = driver.resolve(&request.path) else {
                return (Response::error(ErrorKind::NotFound), None);
            };
            match driver.call(object, Intent::Enumerate) {
                (Outcome::Ok, wusel_core::runtime::Payload::Entries(rows)) => {
                    let entries = rows
                        .iter()
                        .map(|row| {
                            let (pinned, stale, state, folder_kind) = status_of(driver, row);
                            entry_of(row, pinned, stale, state, folder_kind)
                        })
                        .collect();
                    (Response::entries(entries), None)
                }
                (outcome, _) => (Response::error(failure_kind(outcome)), None),
            }
        }
        "fetch" => {
            let Some(object) = driver.resolve(&request.path) else {
                return (Response::error(ErrorKind::NotFound), None);
            };
            match driver.call(
                object,
                Intent::Fetch {
                    offset: request.offset,
                    len: request.len,
                },
            ) {
                (Outcome::Ok, wusel_core::runtime::Payload::Bytes(bytes)) => {
                    (Response::bytes(bytes.len() as u64), Some(bytes))
                }
                (outcome, _) => (Response::error(failure_kind(outcome)), None),
            }
        }
        "write" => {
            // The bytes were read off the wire before dispatch; their count is
            // the authoritative length, not the request's `len` field.
            let Some(bytes) = data else {
                return (Response::error(ErrorKind::BadRequest), None);
            };
            let Some(object) = driver.resolve(&request.path) else {
                return (Response::error(ErrorKind::NotFound), None);
            };
            let len = bytes.len() as u32;
            match driver.call_write(
                object,
                Intent::Write {
                    offset: request.offset,
                    len,
                },
                bytes,
            ) {
                (Outcome::Ok, Payload::Written(n)) => (Response::written(n), None),
                (outcome, _) => (Response::error(failure_kind(outcome)), None),
            }
        }
        "create" => {
            // Materialise names the *parent* and the new child; the child does
            // not exist yet, so the path is split rather than resolved whole.
            let Some((parent, name)) = split_parent(&request.path) else {
                return (Response::error(ErrorKind::BadRequest), None);
            };
            let Some(dir) = driver.resolve(&parent) else {
                return (Response::error(ErrorKind::NotFound), None);
            };
            match driver.call(
                dir,
                Intent::Materialise {
                    name,
                    dir: request.dir,
                },
            ) {
                (Outcome::Ok, Payload::Node(node)) => {
                    let (pinned, stale, state, folder_kind) = status_of(driver, &node);
                    (
                        node_response(&node, pinned, stale, state, folder_kind),
                        None,
                    )
                }
                (outcome, _) => (Response::error(failure_kind(outcome)), None),
            }
        }
        "publish" => {
            let Some(object) = driver.resolve(&request.path) else {
                return (Response::error(ErrorKind::NotFound), None);
            };
            match driver.call(object, Intent::Publish) {
                (Outcome::Ok, _) => (Response::done(), None),
                (outcome, _) => (Response::error(failure_kind(outcome)), None),
            }
        }
        "remove" => {
            let Some((parent, name)) = split_parent(&request.path) else {
                return (Response::error(ErrorKind::BadRequest), None);
            };
            let Some(dir) = driver.resolve(&parent) else {
                return (Response::error(ErrorKind::NotFound), None);
            };
            match driver.call(dir, Intent::Remove { name }) {
                (Outcome::Ok, _) => (Response::done(), None),
                (outcome, _) => (Response::error(failure_kind(outcome)), None),
            }
        }
        "move" => {
            // Both ends are named; the flow is keyed on the source parent, so
            // resolve each parent and hand the child names to the intent.
            let (Some((from_parent, from_name)), Some((to_parent_path, to_name))) =
                (split_parent(&request.path), split_parent(&request.to))
            else {
                return (Response::error(ErrorKind::BadRequest), None);
            };
            let (Some(from), Some(to)) = (
                driver.resolve(&from_parent),
                driver.resolve(&to_parent_path),
            ) else {
                return (Response::error(ErrorKind::NotFound), None);
            };
            match driver.call(
                from,
                Intent::Move {
                    from_name,
                    to_parent: to,
                    to_name,
                },
            ) {
                (Outcome::Ok, _) => (Response::done(), None),
                (outcome, _) => (Response::error(failure_kind(outcome)), None),
            }
        }
        "setattr" => {
            let Some(object) = driver.resolve(&request.path) else {
                return (Response::error(ErrorKind::NotFound), None);
            };
            match driver.call(
                object,
                Intent::SetAttr {
                    size: request.size,
                    mtime: request.mtime,
                },
            ) {
                (Outcome::Ok, Payload::Node(node)) => {
                    let (pinned, stale, state, folder_kind) = status_of(driver, &node);
                    (
                        node_response(&node, pinned, stale, state, folder_kind),
                        None,
                    )
                }
                (outcome, _) => (Response::error(failure_kind(outcome)), None),
            }
        }
        // Keep a file/folder offline (a directory recursively), or drop that. Not
        // an intent — a direct provider action — so it goes straight to the
        // driver. `pin` hydrates now and can be slow; the frontend runs it off the
        // main thread. The path resolves inside the provider, so no `resolve` here.
        "pin" => match driver.pin(&request.path) {
            Ok(_) => (Response::done(), None),
            Err(e) => (Response::error(pin_failure_kind(&e)), None),
        },
        "unpin" => match driver.unpin(&request.path) {
            Ok(()) => (Response::done(), None),
            Err(e) => (Response::error(pin_failure_kind(&e)), None),
        },
        // An unknown op is the client's mistake, not a missing object.
        _ => (Response::error(ErrorKind::BadRequest), None),
    }
}

/// The keep-offline state of `node` for the wire: whether it is pinned, and
/// whether its kept copy is stale. Stale only makes sense for a pinned file, so
/// it is gated on `pinned` — an un-pinned online-only file is never "stale", it
/// is simply fetched fresh.
fn pin_state(driver: &Driver, node: &NodeRow) -> (bool, bool) {
    let pinned = driver.is_pinned(&node.path).unwrap_or(false);
    let stale = pinned && driver.is_stale(node);
    (pinned, stale)
}

/// Both decoration axes for the wire, asked of the engine exactly as the FUSE
/// frontend asks them — one `Intent::State` round-trip carrying the sync state
/// and the group-folder flag together, because they come from the same database
/// read. That is the per-object cost a file manager pays for every visible
/// file's emblem; a client offsets it with a cache the change stream keeps warm
/// (see the status-protocol ADR under `explanation/`).
///
/// The engine's state is an `Option` for a reason: a directory has no content of
/// its own and so no emblem, yet it can still be a group-folder root. So a
/// missing state falls to the neutral default while the kind travels regardless
/// — which is the whole point of keeping the two axes apart. FUSE draws the same
/// distinction by serving `user.wusel.state` and `user.wusel.kind` separately.
fn state_and_kind(driver: &Driver, node: &NodeRow) -> (Option<wire::SyncState>, wire::Kind) {
    use wusel_core::runtime::Payload;
    match driver.call(wusel_fsm::ObjectId(node.inode), Intent::State) {
        (Outcome::Ok, Payload::State { state, group_root }) => (
            // `None` stays `None`: the engine means "this object has no content
            // state", and substituting the default would tell a file manager
            // "online-only" about every plain directory.
            state.map(wire::SyncState::from),
            if group_root {
                wire::Kind::GroupFolder
            } else {
                wire::Kind::Plain
            },
        ),
        _ => (None, wire::Kind::Plain),
    }
}

/// The whole per-object status a frontend draws: pin/stale, the full sync state,
/// and the folder kind — the two axes (sync emblem, group-folder badge) plus the
/// pin booleans an unmigrated client still reads.
fn status_of(driver: &Driver, node: &NodeRow) -> (bool, bool, Option<wire::SyncState>, wire::Kind) {
    let (pinned, stale) = pin_state(driver, node);
    let (state, folder_kind) = state_and_kind(driver, node);
    (pinned, stale, state, folder_kind)
}

/// A `node` response from a row's frontend-relevant fields. The status axes
/// (`pinned`/`stale`/`state`/`folder_kind`) are looked up separately — they are
/// not columns on the row.
fn node_response(
    node: &NodeRow,
    pinned: bool,
    stale: bool,
    state: Option<wire::SyncState>,
    folder_kind: wire::Kind,
) -> Response {
    Response::node(
        node.name.clone(),
        node.is_dir,
        node.size,
        node.mtime,
        node.etag.clone(),
        node.file_id,
        pinned,
        stale,
        state,
        folder_kind,
    )
}

/// Split an account-relative path into `(parent, last-segment)`. The parent is
/// always rooted (`/` for a top-level name); the name is the final non-empty
/// segment. `None` when there is no name — the path is the root, or all slashes
/// — which the write ops report as a bad request.
///
/// Kept purely lexical on purpose: the create and move targets do not exist yet,
/// so the split cannot go through [`Driver::resolve`] the way a whole path does.
fn split_parent(path: &str) -> Option<(String, String)> {
    let trimmed = path.trim_end_matches('/');
    let (parent, name) = match trimmed.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", trimmed),
    };
    if name.is_empty() {
        return None;
    }
    let parent = if parent.is_empty() { "/" } else { parent };
    Some((parent.to_string(), name.to_string()))
}

/// One directory child for an `entries` response. `pinned`/`stale` are looked up
/// per child (they are not columns on the row).
fn entry_of(
    node: &NodeRow,
    pinned: bool,
    stale: bool,
    state: Option<wire::SyncState>,
    folder_kind: wire::Kind,
) -> Entry {
    Entry {
        name: node.name.clone(),
        is_dir: node.is_dir,
        size: node.size,
        mtime: node.mtime,
        etag: node.etag.clone(),
        file_id: node.file_id,
        pinned,
        stale,
        state,
        folder_kind,
    }
}

/// Map a pin/unpin failure onto the wire's error set. A path that does not
/// resolve is `NotFound`; everything else (a failed download, a write error) is
/// reported as I/O.
fn pin_failure_kind(error: &wusel_core::Error) -> ErrorKind {
    match error {
        wusel_core::Error::NotFound => ErrorKind::NotFound,
        _ => ErrorKind::Io,
    }
}

/// Map an engine failure onto the wire's small error set. `NotFound` survives as
/// itself; everything else is reported as an I/O failure, which is what a
/// frontend can act on.
fn failure_kind(outcome: Outcome) -> ErrorKind {
    match outcome {
        Outcome::Failed(wusel_fsm::Failure::NotFound | wusel_fsm::Failure::Stale) => {
            ErrorKind::NotFound
        }
        _ => ErrorKind::Io,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of our own, so these never touch a shared path.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wusel-ipc-dirtest-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .expect("stat")
            .permissions()
            .mode()
            & 0o7777
    }

    #[test]
    fn a_missing_directory_is_created_private() {
        let dir = scratch("missing").join("nested");
        prepare_socket_dir(&dir).expect("create");
        // 0700 at creation, not chmod-ed afterwards: no window in which the
        // directory holding a full-access socket is traversable by others.
        assert_eq!(mode_of(&dir), 0o700, "{}", dir.display());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn our_own_loose_directory_is_tightened_rather_than_refused() {
        let dir = scratch("loose");
        std::fs::create_dir_all(&dir).expect("create");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        prepare_socket_dir(&dir).expect("an existing directory of ours is acceptable");
        assert_eq!(mode_of(&dir), 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_is_refused_rather_than_followed() {
        let base = scratch("symlink");
        std::fs::create_dir_all(base.join("real")).expect("create");
        let link = base.join("link");
        std::os::unix::fs::symlink(base.join("real"), &link).expect("symlink");
        // Following it is how a hostile directory redirects us somewhere it
        // controls; the socket must not be bound behind a link we did not make.
        let err = prepare_socket_dir(&link).expect_err("a symlink must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_plain_file_is_refused() {
        let base = scratch("file");
        std::fs::create_dir_all(&base).expect("create");
        let file = base.join("not-a-dir");
        std::fs::write(&file, b"").expect("write");
        let err = prepare_socket_dir(&file).expect_err("a file must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_foreign_non_sticky_directory_is_refused() {
        // `/` is root-owned and not sticky — the shape of an attacker-created
        // `/tmp/wusel`, without needing a second uid to set one up. `/tmp`
        // itself is sticky and must stay acceptable, or `--socket /tmp/x.sock`
        // would break.
        let err = prepare_socket_dir(Path::new("/")).expect_err("a foreign, non-sticky dir");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        prepare_socket_dir(&std::env::temp_dir()).expect("a sticky shared temp dir is acceptable");
    }
}
