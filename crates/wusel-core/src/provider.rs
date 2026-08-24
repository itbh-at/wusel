// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The frontend-agnostic `Provider` facade.
//!
//! Every OS frontend (FUSE on Linux, File Provider on macOS, Cloud Filter on
//! Windows) talks to the engine through this one type. It owns the SQLite state,
//! the WebDAV client and the sync↔async bridge (a tokio runtime), and holds all
//! the logic — listing (with lazy PROPFIND) and, from Priority 4, reading
//! contents. A frontend only translates OS callbacks into these calls.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::content::{CachingSource, ContentSource, LiveWebDav};
use crate::desktop::{self, Desktop, Notice};
use crate::model::{basename, RemoteEntry};
use crate::state::{NodeRow, StateDb, ROOT_INODE};
use crate::webdav::WebDavClient;
use crate::{Error, Result};

/// A finished background revalidation: `(inode, path, listing, write_epoch)`.
/// `write_epoch` is the value read *just before* the PROPFIND was issued, so the
/// FUSE thread can discard a listing that a concurrent local write has made stale
/// (see [`Provider::write_epoch`]).
type RevalResult = (u64, String, Option<Vec<RemoteEntry>>, u64);

/// Join a child name onto a parent path (the root parent path is empty).
/// Join a parent's server path with a child name.
///
/// Public because the substrate builds the same paths when it creates, deletes
/// or moves on the server — one spelling of the rule, not two.
pub fn child_path(parent_path: &str, name: &str) -> String {
    if parent_path.is_empty() {
        name.to_string()
    } else {
        format!("{parent_path}/{name}")
    }
}

/// Whether an account-relative `path` is inside a freedesktop top-directory
/// trash — its first segment is `.Trash` or `.Trash-<uid>`. Nothing is ever
/// created or moved there: deletion should go straight to the server (and
/// Nextcloud's own trash), not into a `.Trash-<uid>` folder that would sync into
/// the user's cloud and onto every device. See the desktop integration docs.
#[must_use]
pub fn is_trash_path(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or("");
    first == ".Trash" || first.starts_with(".Trash-")
}

#[cfg(test)]
mod trash_tests {
    use super::is_trash_path;

    #[test]
    fn trash_paths_are_recognised_by_their_top_segment() {
        assert!(is_trash_path(".Trash"));
        assert!(is_trash_path(".Trash-1000"));
        assert!(is_trash_path(".Trash-1000/files/report.odt"));
        assert!(is_trash_path(".Trash-1000/info/report.odt.trashinfo"));
        // Ordinary paths, including look-alikes without the dot-prefix or a
        // deeper segment that only resembles one, are not trash.
        assert!(!is_trash_path("Documents/report.odt"));
        assert!(!is_trash_path("Trash/report.odt"));
        assert!(!is_trash_path("Photos/.Trash-1000")); // not at the top
        assert!(!is_trash_path(""));
    }
}

/// A sibling path for a conflicted copy: `stem (conflicted copy <unix>).ext`.
/// An `attempt` > 0 appends a `-<attempt>` de-duplicator: the timestamp has
/// 1-second resolution, so a second conflict within the same second would
/// otherwise produce the *same* name and overwrite the first copy.
fn conflict_copy_path(path: &str, attempt: u32) -> String {
    let ts = now_secs();
    let tag = if attempt == 0 {
        format!("conflicted copy {ts}")
    } else {
        format!("conflicted copy {ts}-{attempt}")
    };
    match path.rsplit_once('.') {
        // Only treat a dot as an extension if it is in the final segment.
        Some((stem, ext)) if !ext.contains('/') && !stem.ends_with('/') && !stem.is_empty() => {
            format!("{stem} ({tag}).{ext}")
        }
        _ => format!("{path} ({tag})"),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read a byte range from an open scratch (write buffer) file.
/// Read one window out of a write buffer.
///
/// Public because the execution substrate serves buffer reads on its own
/// workers (see [`crate::runtime`]) — the same primitive, called from the pool
/// that owns file I/O rather than from whoever happens to hold the state.
pub fn read_range_from_scratch(path: &std::path::Path, offset: u64, len: u32) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let end = f.metadata()?.len();
    if len == 0 || offset >= end {
        return Ok(Vec::new());
    }
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len as usize];
    let mut total = 0;
    while total < buf.len() {
        let n = f.read(&mut buf[total..])?;
        if n == 0 {
            break; // EOF
        }
        total += n;
    }
    buf.truncate(total);
    Ok(buf)
}

pub struct Provider {
    state: StateDb,
    dav: WebDavClient,
    /// Sync→async bridge: FUSE/OS callbacks are synchronous, WebDAV is async.
    rt: Arc<tokio::runtime::Runtime>,
    /// FUSE dispatch-thread count (see [`crate::config::Settings::dispatch_threads`]).
    /// The frontend reads this to configure the FUSE session; the runtime above is
    /// already sized to match. 1 = single-threaded (the default).
    dispatch_threads: usize,
    /// Content delivery: a caching decorator over the live WebDAV source. An
    /// `Arc`, not a `Box`, so a read can hand a cheap clone to a blocking task
    /// off the FUSE dispatch thread (see [`Provider::read_plan`]). `ContentSource`
    /// is `Send + Sync`, so the shared handle is safe.
    content: Arc<dyn ContentSource>,
    /// Re-PROPFIND a directory if its last listing is older than this. The
    /// fallback change-detection when notify_push is unavailable.
    revalidate_secs: u64,
    /// Rate-limit for push-triggered re-lists (see [`crate::config::Settings`]).
    push_floor_secs: u64,
    /// Opt-in: expose synthetic desktop-indexer exclusion markers at the mount
    /// root (a frontend reads this; see [`Self::exclude_from_indexers`]).
    exclude_from_indexers: bool,
    /// Unix seconds of the last notify_push signal (0 = none). A directory
    /// listed at/before this is stale regardless of the TTL. Written by the
    /// push listener thread (see [`crate::push`]), read here.
    invalidate_after: Arc<AtomicI64>,
    /// Directory holding scratch files (`<cache>/scratch`).
    scratch_dir: PathBuf,
    /// Where the state database lives, so the substrate's workers can each open
    /// their own connection to it.
    db_path: PathBuf,
    /// Opt-in: try a 3-way text merge on conflict before a conflict copy.
    text_merge: bool,
    /// Monotonic counter bumped on every local mutation of directory membership
    /// (create/mkdir/rename/remove, and an upload that materialises a deferred
    /// create). A background PROPFIND records this before it starts; if it has
    /// advanced by the time the listing comes back, that listing predates a local
    /// write and must not drive deletions — otherwise a reconcile could delete a
    /// file the user just created (a stale snapshot racing the upload). Shared with
    /// both background threads; only ever bumped on the FUSE thread.
    write_epoch: Arc<AtomicU64>,
    /// Send a directory (inode, path) to the background revalidator.
    reval_tx: Sender<(u64, String)>,
    /// Receive finished background revalidations to apply on the FUSE thread.
    reval_rx: Receiver<RevalResult>,
    /// Directories with a background revalidation in flight (dedup). Only ever
    /// touched on the FUSE thread, so a plain set — no lock needed.
    reval_pending: HashSet<u64>,
    /// Kept so the worker thread is owned by the provider; it exits when the
    /// provider drops (the job channel closes).
    _reval_handle: std::thread::JoinHandle<()>,
    /// Glob patterns for ephemeral editor/OS files kept purely local.
    ignore_patterns: Vec<String>,
    /// OS-integration backend (notifications + filesystem status). Defaults to a
    /// no-op; the frontend injects a platform backend via [`Self::set_desktop`].
    desktop: Arc<dyn Desktop>,
    /// Trigger the background syncer (a notify_push arrived). Handed to the push
    /// listener; cloneable.
    sync_trigger: Sender<()>,
    /// The user's "keep this offline" list. Beside the database rather than in
    /// it, and shared with the syncer and the substrate's workers — see
    /// [`crate::pins`].
    pins: Arc<crate::pins::Pins>,
    /// What to serve when an outdated pinned file is opened.
    open_pinned: crate::config::OpenPinned,
    /// Async (default) vs synchronous write-back; see `[sync] upload`.
    async_upload: bool,
    /// The connection's cost, cached and shared with every worker.
    metered: Arc<crate::runtime::Metered>,
    /// The slot the syncer reads its desktop backend from. Held here so
    /// [`Provider::set_desktop`] can reach a thread that is already running —
    /// see [`PinnedRefresh::desktop`].
    desktop_cell: Arc<Mutex<Arc<dyn Desktop>>>,
    /// Kernel-invalidation events from the syncer, for the frontend to drain and
    /// turn into FUSE notifications. Taken once by the frontend at mount.
    invalidations: Option<Receiver<Invalidation>>,
    /// The sending half, kept so a reconcile started anywhere reports the same
    /// add/removes the syncer's does. One place reconciles, one place reports.
    inval_tx: Sender<Invalidation>,
    _sync_handle: std::thread::JoinHandle<()>,
}

/// What the syncer needs to act on a pinned file whose server copy moved on.
///
/// Carried as one value because the three parts belong together: the policy,
/// the way to find out what a refresh would cost, and the means to carry one
/// out.
/// Swap in this walk's stale set and say whether it brought anything new.
///
/// A free function, and therefore testable on its own: the interesting part of
/// "do not repeat yourself every thirty seconds" is this three-line rule, not
/// the machinery around it.
fn anything_new(
    announced: &mut std::collections::HashSet<u64>,
    now: std::collections::HashSet<u64>,
) -> bool {
    let fresh = now.iter().any(|id| !announced.contains(id));
    *announced = now;
    fresh
}

#[derive(Clone)]
pub struct PinnedRefresh {
    pub mode: crate::config::RefreshPinned,
    pub content: Arc<dyn ContentSource>,
    /// Where the desktop backend is kept — not the backend itself.
    ///
    /// The syncer thread starts before the real backend is known: the daemon
    /// installs it afterwards, through [`Provider::set_desktop`]. A copy taken
    /// at start-up would therefore be the no-op placeholder, and would stay the
    /// placeholder, because `set_desktop` replaces the Provider's field and
    /// cannot reach into a running thread. `ask` would then never say anything
    /// — no error, no log, just silence that looks exactly like `manual`.
    ///
    /// Pointing both at one slot instead means the thread reads the real
    /// backend as soon as it is put there.
    pub desktop: Arc<Mutex<Arc<dyn Desktop>>>,
    /// Which files we have already told the user about. Without it, `ask` would
    /// repeat the same message every `revalidate_secs` for as long as the user
    /// declines to act — which teaches people to ignore our notifications.
    pub announced: Arc<Mutex<std::collections::HashSet<u64>>>,
}

impl PinnedRefresh {
    /// Act on everything one walk found out of date — once, for all of them.
    ///
    /// Aggregated on purpose. A colleague reorganising a shared folder can
    /// change hundreds of pinned files at once, and that has to be one message
    /// ("12 pinned files changed") rather than hundreds.
    /// Record this walk's stale set and report whether it holds anything the
    /// user has not been told about yet.
    ///
    /// Replacing the set rather than growing it is what makes a file that is
    /// updated, and later goes stale a second time, announceable again — while
    /// an unchanged backlog stays quiet for ever.
    fn note_and_check_new(&self, stale: &[NodeRow]) -> bool {
        let mut announced = self.announced.lock().unwrap_or_else(|e| e.into_inner());
        anything_new(&mut announced, stale.iter().map(|n| n.inode).collect())
    }

    fn settle(&self, stale: &[NodeRow]) {
        if stale.is_empty() {
            // Nothing stale: forget the backlog, so a file that goes stale
            // again later is announced again rather than silently suppressed.
            self.announced
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            return;
        }
        let desktop = self
            .desktop
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match self.mode.decide(desktop.is_metered()) {
            crate::config::RefreshAction::ShowOnly => {
                // The emblem already says so; the user picks the moment.
                tracing::debug!(count = stale.len(), "pinned files are out of date");
            }
            crate::config::RefreshAction::Ask => {
                // Speak only when there is something new to say. The message
                // still names the *whole* backlog, so a user who ignored the
                // first one and comes back later sees the real count, not just
                // the increment.
                if self.note_and_check_new(stale) {
                    desktop.notify(&Notice::PinnedOutOfDate {
                        count: stale.len(),
                        first: stale[0].path.clone(),
                    });
                }
            }
            crate::config::RefreshAction::Fetch => {
                for node in stale {
                    if let Err(e) = self.content.pin_file(node) {
                        // Not fatal: the copy we have is still there, and the
                        // emblem still says it is out of date.
                        tracing::warn!(path = %node.path, %e,
                            "could not bring the pinned copy up to date");
                    }
                }
            }
        }
    }
}

/// The background revalidator: it does only the slow work — the PROPFIND — off
/// the FUSE thread, and sends the listing back for the provider to apply. It
/// never touches SQLite, so all state stays single-connection, single-thread.
fn revalidate_loop(
    dav: WebDavClient,
    jobs: Receiver<(u64, String)>,
    results: Sender<RevalResult>,
    write_epoch: Arc<AtomicU64>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(%e, "revalidator: no runtime — background revalidation disabled");
            return;
        }
    };
    while let Ok((inode, path)) = jobs.recv() {
        // Snapshot the write epoch *before* the PROPFIND: a local write during the
        // round-trip means this listing may predate it (see `write_epoch`).
        let epoch = write_epoch.load(Ordering::SeqCst);
        let outcome = match rt.block_on(dav.propfind_dir(&path)) {
            Ok(entries) => {
                tracing::debug!(%path, inode, "PROPFIND (background revalidate)");
                Some(entries)
            }
            Err(e) => {
                tracing::debug!(%e, %path, "background revalidate PROPFIND failed");
                None
            }
        };
        if results.send((inode, path, outcome, epoch)).is_err() {
            break; // the provider is gone
        }
    }
}

/// Something a frontend's caches should stop believing, because the engine
/// found it changed.
///
/// # Whose vocabulary this is
///
/// Object *ids* and *paths*, never inodes. The engine's stable per-object id
/// happens to be the number our FUSE frontend hands the kernel as an inode, but
/// that is the frontend's business: Windows CfAPI and macOS File Provider
/// identify objects their own way, and naming the field after one platform's
/// word for it would quietly make this channel FUSE-only. The frontend
/// translates; this enum says what changed.
#[derive(Debug)]
pub enum Invalidation {
    /// An entry in a directory changed — added, removed, or its *availability*
    /// flipped: hydrated, evicted, pinned, unpinned.
    ///
    /// `parent` + `name` are what a frontend needs to drop a cached directory
    /// entry (under FUSE, `notify_inval_entry`); `path` is the remote,
    /// account-relative path, which it joins with the mountpoint to tell the
    /// desktop *which* file to re-read for its emblem.
    Entry {
        parent: ObjectId,
        name: String,
        path: String,
    },
    /// A file whose *contents* changed on the server. The frontend turns this
    /// into `notify_inval_inode`, so the kernel drops the pages and attributes
    /// it has cached for this file.
    ///
    /// Without it the kernel keeps serving what it had until the one-second
    /// attribute TTL runs out, so "reload" in an editor showed the current
    /// version only by the grace of a timer. This makes it dependable.
    ///
    /// It is **not** inotify: FUSE's reverse invalidation does not produce
    /// fsnotify events, so an editor *watching* the file is still not woken by
    /// itself. What this buys is that asking for it always works.
    Content { object: ObjectId, path: String },
}

/// The engine's stable identity for one object, as this channel speaks it.
///
/// A plain `u64` today — the number the state database keys its rows by. It has
/// a name of its own so each frontend can map it to whatever its platform wants
/// without the *engine* having picked a side.
pub type ObjectId = u64;

/// A file's local-availability state, for OS-integration emblems (the
/// file-manager badges "online-only / here / kept offline"). Deliberately
/// coarse and cheap — see [`Provider::file_state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// Online-only: no local copy; opening it fetches from the server.
    OnlineOnly,
    /// A fresh copy is cached locally but evictable (not pinned).
    Cached,
    /// Kept offline on purpose — pinned, or under a pinned directory/root.
    Pinned,
    /// Pinned, and the copy we keep is **out of date**: the server has moved on.
    ///
    /// Only pinned files get this. For an ordinary cached file, going stale
    /// merely means the next read goes live, which is what a VFS does all day.
    /// A pin promises the file is there when the server is not, and an outdated
    /// copy is a promise half-kept — so it is worth showing *before* somebody
    /// opens the file, not only when they are already offline.
    PinnedStale,
    /// A local edit not yet committed — an open write buffer being edited,
    /// before `flush`. Once flushed it becomes [`FileState::Uploading`].
    Modified,
    /// A committed change on its way to the server: the asynchronous upload is
    /// queued or in flight. This is the "syncing" symbol the user watches after
    /// dropping files into the folder.
    Uploading,
    /// A committed change whose upload failed for good (wrong permissions, a
    /// conflict, no quota). The bytes are safe locally and the user has been
    /// told; it will not retry on its own.
    SyncError,
}

impl FileState {
    /// The stable value for the `user.wusel.state` xattr that file-manager
    /// extensions read. **A public contract — keep these strings stable.**
    pub fn as_xattr(self) -> &'static str {
        match self {
            FileState::OnlineOnly => "online-only",
            FileState::Cached => "cached",
            FileState::Pinned => "pinned",
            FileState::PinnedStale => "pinned-stale",
            FileState::Modified => "modified",
            FileState::Uploading => "uploading",
            FileState::SyncError => "sync-error",
        }
    }
}

/// How deep the sync walk descends — a guard against a pathological tree, far
/// beyond any real directory nesting.
const SYNC_MAX_DEPTH: u32 = 64;

/// The background syncer. On a notify_push trigger it walks the **cached** tree
/// top-down, guided by Nextcloud's propagated ETags: a change deep in the tree
/// bumps every ancestor's ETag up to the root, so we descend only into the
/// directories whose ETag actually changed and reconcile them — finding a
/// server-side delete/add without the (path-less) push telling us where, and
/// without re-listing everything.
///
/// It owns a **second** state connection and its own HTTP client/runtime, so it
/// reconciles autonomously and never blocks the FUSE thread. Changed entries are
/// reported for kernel invalidation (see [`Invalidation`]).
fn sync_loop(
    mut state: StateDb,
    pins: Arc<crate::pins::Pins>,
    dav: WebDavClient,
    triggers: Receiver<()>,
    invalidations: Sender<Invalidation>,
    write_epoch: Arc<AtomicU64>,
    pinned: PinnedRefresh,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(%e, "syncer: no runtime — proactive sync disabled");
            return;
        }
    };
    while triggers.recv().is_ok() {
        // Coalesce a burst of push events into a single walk.
        while triggers.try_recv().is_ok() {}
        // Nothing cached at the root → nothing to keep in sync yet.
        if !matches!(state.children_loaded(ROOT_INODE), Ok(true)) {
            continue;
        }
        // Collected across the whole walk and settled once: hundreds of pinned
        // files can change together when somebody reorganises a shared folder.
        let mut stale = Vec::new();
        if let Err(e) = walk_dir(
            &mut state,
            &pins,
            &dav,
            &rt,
            &invalidations,
            &write_epoch,
            &mut stale,
            ROOT_INODE,
            "",
            0,
        ) {
            tracing::debug!(%e, "syncer: walk aborted");
        }
        pinned.settle(&stale);
    }
}

/// Reconcile one directory, emit its add/remove invalidations, and recurse into
/// the loaded child directories whose ETag changed. See [`sync_loop`].
// A recursive helper threading the syncer's borrowed context (state/dav/rt/
// invalidations/epoch) plus the per-node cursor; grouping them into a struct would
// only obscure the plain recursion.
#[allow(clippy::too_many_arguments)]
fn walk_dir(
    state: &mut StateDb,
    pins: &crate::pins::Pins,
    dav: &WebDavClient,
    rt: &tokio::runtime::Runtime,
    invalidations: &Sender<Invalidation>,
    write_epoch: &AtomicU64,
    // Pinned files found to have moved on. Collected here and settled once by
    // the caller: the decision is per walk, not per directory.
    stale: &mut Vec<NodeRow>,
    inode: u64,
    path: &str,
    depth: u32,
) -> Result<()> {
    if depth > SYNC_MAX_DEPTH {
        return Ok(());
    }
    // Snapshot the children (name → etag/inode) *before* reconcile overwrites them.
    let old = state.children_of(inode)?;
    // Record the write epoch before the PROPFIND: if the FUSE thread mutates this
    // directory while the listing is in flight, the listing predates that write and
    // reconciling it could delete a file the user just created. Discard it; the
    // next push re-walks with a fresh listing.
    let epoch = write_epoch.load(Ordering::SeqCst);
    let entries = rt.block_on(dav.propfind_dir(path))?;
    if write_epoch.load(Ordering::SeqCst) != epoch {
        tracing::debug!(%path, inode, "sync walk: listing stale (concurrent local write) — skipping");
        return Ok(());
    }
    state.reconcile_children(inode, path, &entries)?;
    tracing::debug!(%path, inode, "PROPFIND (sync walk)");

    let new_names: std::collections::HashSet<&str> =
        entries.iter().map(|e| basename(&e.path)).collect();
    let old_by_name: std::collections::HashMap<&str, &NodeRow> =
        old.iter().map(|n| (n.name.as_str(), n)).collect();

    // Removed: a server-backed child gone from the listing.
    for n in &old {
        if n.file_id.is_some() && !new_names.contains(n.name.as_str()) {
            let _ = invalidations.send(Invalidation::Entry {
                parent: inode,
                name: n.name.clone(),
                path: n.path.clone(),
            });
        }
    }
    // A file whose contents moved on. Two things follow from it, and they are
    // for different audiences: the kernel has to drop what it cached, and — if
    // the file is pinned — the user may want to hear about it.
    for e in &entries {
        let name = basename(&e.path);
        if let Some(o) = old_by_name.get(name) {
            if o.is_dir || o.etag == e.etag {
                continue;
            }
            let _ = invalidations.send(Invalidation::Content {
                object: o.inode,
                path: o.path.clone(),
            });
            if pins.is_pinned(&o.path).unwrap_or(false) {
                // Collected rather than acted on here: the decision is one per
                // walk, not one per file.
                stale.push((*o).clone());
            }
        }
    }
    // Added: a listing entry we did not have before.
    for e in &entries {
        let name = basename(&e.path);
        if !old_by_name.contains_key(name) {
            let _ = invalidations.send(Invalidation::Entry {
                parent: inode,
                name: name.to_string(),
                path: child_path(path, name),
            });
        }
    }
    // Descend only where the ETag changed (that is the path to the real change)
    // and only into directories we have actually listed.
    for e in &entries {
        if !e.is_dir {
            continue;
        }
        let name = basename(&e.path);
        if let Some(o) = old_by_name.get(name) {
            if o.etag != e.etag && state.children_loaded(o.inode)? {
                let child_path = child_path(path, name);
                walk_dir(
                    state,
                    pins,
                    dav,
                    rt,
                    invalidations,
                    write_epoch,
                    stale,
                    o.inode,
                    &child_path,
                    depth + 1,
                )?;
            }
        }
    }
    Ok(())
}

/// Count and total size of the cached content blobs (files without an
/// extension in the blob dir — `.etag`/`.pin`/`.part`/`.ra`/`.dl` are sidecars).
fn blob_stats(dir: &std::path::Path) -> (u64, u64) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    rd.flatten()
        .filter(|e| e.path().extension().is_none())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .fold((0, 0), |(n, bytes), m| (n + 1, bytes + m.len()))
}

/// Everything the networked write path needs, and nothing borrowed from the
/// [`Provider`] — so a substrate worker can run it.
///
/// Every field is either an `Arc` or cheap to clone, which is not a
/// coincidence: it is what lets the same logic run on whichever thread the
/// machine hands the step to, instead of only on whoever happens to own the
/// state.
#[derive(Clone)]
pub struct WriteContext {
    pub dav: WebDavClient,
    pub rt: Arc<tokio::runtime::Runtime>,
    pub content: Arc<dyn ContentSource>,
    pub desktop: Arc<dyn Desktop>,
    /// Opt-in: try a 3-way text merge before falling back to a second copy.
    pub text_merge: bool,
    /// Bumped on every local mutation of directory membership, so a listing
    /// taken before it cannot delete what we just made.
    pub write_epoch: Arc<AtomicU64>,
    /// Where add/removes are reported for kernel invalidation.
    pub invalidations: Sender<Invalidation>,
}

/// Handle an upload the server refused (412): merge if we can, otherwise keep
/// the server version and park ours beside it under a second name.
///
/// This also covers a **deferred create** that lost the race against a
/// same-named server-side create — its `If-None-Match: *` failed, there is no
/// merge base, so the copy path runs and the parent reload pulls in theirs.
///
/// # Errors
/// If the network fails, or every candidate copy name is taken.
pub fn run_conflict_resolution(
    ctx: &WriteContext,
    state: &mut StateDb,
    node: &NodeRow,
    scratch: &std::path::Path,
    size: u64,
) -> Result<()> {
    if ctx.text_merge {
        let local = std::fs::read(scratch)?;
        if let Some((merged, theirs)) = run_text_merge(ctx, node, &local)? {
            // Upload the merge **conditionally against the very version it was
            // merged from**. The result is only correct for that "theirs"; an
            // unconditional PUT here would silently discard a third change that
            // landed between our GET and this PUT — the exact lost update the
            // whole 412 machinery exists to prevent.
            let result = ctx.rt.block_on(ctx.dav.put_conditional(
                &node.path,
                merged.clone(),
                &theirs,
                None,
            ))?;
            match result {
                crate::webdav::PutResult::Uploaded(new_etag) => {
                    let new_etag = new_etag.unwrap_or_default();
                    let size = merged.len() as u64;
                    state.set_etag_size(node.inode, &new_etag, size)?;
                    let mut updated = node.clone();
                    updated.etag = new_etag.clone();
                    updated.size = size;
                    ctx.content.store(&updated, &merged, &new_etag)?;
                    tracing::info!(path = %node.path, "conflict auto-merged");
                    return Ok(());
                }
                crate::webdav::PutResult::Conflict => {
                    // The server moved on again while we were merging, so the
                    // merge is stale. Fall through — the bytes survive either way.
                    tracing::debug!(path = %node.path, "merge raced another server change — falling back to a conflicted copy");
                }
            }
        }
    }
    // The copy uploads with `If-None-Match: *` because its timestamped name has
    // one-second resolution: a second conflict in the same second would
    // otherwise silently overwrite the first copy.
    const MAX_COPY_ATTEMPTS: u32 = 4;
    let fresh = crate::webdav::Precondition::MustNotExist;
    let mut copy = conflict_copy_path(&node.path, 0);
    for attempt in 0..MAX_COPY_ATTEMPTS {
        let result = if size > crate::webdav::CHUNK_SIZE {
            ctx.rt
                .block_on(ctx.dav.put_chunked(&copy, scratch, size, &fresh, None))?
        } else {
            let local = std::fs::read(scratch)?;
            ctx.rt
                .block_on(ctx.dav.put_conditional(&copy, local, &fresh, None))?
        };
        match result {
            crate::webdav::PutResult::Uploaded(_) => break,
            crate::webdav::PutResult::Conflict if attempt + 1 < MAX_COPY_ATTEMPTS => {
                copy = conflict_copy_path(&node.path, attempt + 1);
            }
            crate::webdav::PutResult::Conflict => {
                // Vanishingly unlikely, and erroring out is safe: the buffer is
                // kept, so the content is retried rather than dropped.
                return Err(Error::Other(format!(
                    "conflicted copy of {}: every candidate name already exists",
                    node.path
                )));
            }
        }
    }
    tracing::warn!(path = %node.path, copy = %copy, "upload conflict — saved a conflicted copy");
    // Their edit is safe, but under a name they would not otherwise find.
    ctx.desktop.notify(&Notice::ConflictCopy {
        path: node.path.clone(),
        copy: copy.clone(),
    });
    ctx.write_epoch.fetch_add(1, Ordering::SeqCst);
    run_reload_dir(ctx, state, node.parent)?;
    Ok(())
}

/// Attempt a 3-way text merge (base = the cached last-known copy, ours = the
/// buffer, theirs = the server's current version).
///
/// `None` when a merge is not possible: no clean base, non-UTF-8 content, or a
/// real conflict. On success it returns the merged bytes **and** the
/// precondition naming the version it merged against, so the caller can upload
/// conditionally on exactly that one.
fn run_text_merge(
    ctx: &WriteContext,
    node: &NodeRow,
    local: &[u8],
) -> Result<Option<(Vec<u8>, crate::webdav::Precondition)>> {
    let Some(base) = ctx.content.cached_bytes(node) else {
        return Ok(None); // no clean base to merge against
    };
    let (theirs, theirs_etag) = ctx.rt.block_on(ctx.dav.get_with_etag(&node.path))?;
    // It demonstrably exists — we just read it — so an absent ETag means
    // "version unknown", never "absent".
    let pre =
        crate::webdav::Precondition::for_upload(theirs_etag.as_deref().unwrap_or_default(), true);
    let (Ok(base), Ok(ours), Ok(theirs)) = (
        String::from_utf8(base),
        String::from_utf8(local.to_vec()),
        String::from_utf8(theirs.to_vec()),
    ) else {
        return Ok(None); // binary content → cannot text-merge
    };
    match diffy::merge(&base, &ours, &theirs) {
        Ok(merged) => Ok(Some((merged.into_bytes(), pre))),
        Err(_conflicted) => Ok(None), // a real conflict → fall back to a copy
    }
}

/// Drop the pins a deleted subtree carried, and the eviction markers beside its
/// cache blobs.
///
/// Both are path-keyed promises about files that no longer exist. A leftover
/// pin marker is not merely untidy: eviction skips a pinned blob **and** does
/// not count it against the cache budget, so those bytes would be exempt from
/// it for ever — and a later file handed the same id would inherit the
/// protection.
///
/// # Errors
/// If the state cannot be read or written.
pub fn run_unpin_subtree(
    state: &mut StateDb,
    pins: &crate::pins::Pins,
    content: &dyn ContentSource,
    node: &NodeRow,
) -> Result<()> {
    // An ancestor (or root) pin covers this path without being *under* it.
    let covered = pins.is_pinned(&node.path)?;
    let removed = pins.remove_under(&node.path)?;
    if !covered && removed == 0 {
        return Ok(());
    }
    for (_, file_id) in state.descendant_file_ids(&node.path)? {
        content.unpin_file(file_id);
    }
    Ok(())
}

/// Re-list a directory (PROPFIND + reconcile), so a mutation we just made shows
/// up with its server-assigned ids.
///
/// # Errors
/// If the listing or the reconcile fails.
pub fn run_reload_dir(ctx: &WriteContext, state: &mut StateDb, inode: u64) -> Result<()> {
    let Some(dir) = state.node_by_inode(inode)? else {
        return Ok(());
    };
    let before = state.children_of(inode)?;
    let entries = ctx.rt.block_on(ctx.dav.propfind_dir(&dir.path))?;
    state.reconcile_children(inode, &dir.path, &entries)?;

    // Report what changed, exactly as the syncer's own walk does. Whoever
    // re-lists a directory owes the kernel the same notification, or a file
    // manager sitting in it keeps showing what is no longer there.
    let new_names: std::collections::HashSet<&str> =
        entries.iter().map(|e| basename(&e.path)).collect();
    let old_names: std::collections::HashSet<&str> =
        before.iter().map(|n| n.name.as_str()).collect();
    for n in &before {
        if n.file_id.is_some() && !new_names.contains(n.name.as_str()) {
            let _ = ctx.invalidations.send(Invalidation::Entry {
                parent: inode,
                name: n.name.clone(),
                path: n.path.clone(),
            });
        }
    }
    for e in &entries {
        let name = basename(&e.path);
        if !old_names.contains(name) {
            let _ = ctx.invalidations.send(Invalidation::Entry {
                parent: inode,
                name: name.to_string(),
                path: child_path(&dir.path, name),
            });
        }
    }
    Ok(())
}

impl Provider {
    pub fn new(
        dav: WebDavClient,
        state: StateDb,
        account: &crate::config::Account,
    ) -> Result<Self> {
        let settings = account.settings();
        // The engine's runtime. Single-threaded by default (behaviour-identical to
        // before concurrency), so a read/upload `block_on` still serialises with
        // the others. With `dispatch_threads > 1` it becomes multi-threaded, so the
        // `spawn_blocking` reads and uploads actually run their network I/O in
        // parallel rather than contending for one runtime thread — the FUSE
        // dispatch threads alone would not achieve that.
        let dispatch_threads = settings.dispatch_threads.max(1);
        // Always multi-threaded, even for a single dispatch thread. The network
        // workers each call `block_on` on this runtime; a *current-thread*
        // runtime lets only one `block_on` proceed at a time, so a single stalled
        // read would freeze every other network operation — which is exactly how
        // a stalled content-sniff read wedged a whole directory listing. A
        // multi-threaded runtime drives all the connections concurrently, so a
        // stall (bounded by `read_timeout`) stays contained to its own read.
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(dispatch_threads.max(2))
                .enable_all()
                .build()?,
        );
        let live = LiveWebDav::new(dav.clone(), rt.clone());
        let cache_dir = account.blob_cache_dir();
        // One startup line for context: it says how much is served locally —
        // cached metadata and content produce no per-operation log lines, so a
        // later "quiet" log usually means this, not inactivity.
        let (blobs, blob_bytes) = blob_stats(&cache_dir);
        tracing::info!(
            cached_nodes = state.node_count().unwrap_or(0),
            cached_files = blobs,
            cached_bytes = blob_bytes,
            "state loaded — locally served metadata/content does not log per operation"
        );
        // The background hydrator fetches through its OWN live source — a fresh
        // reqwest client and a dedicated runtime — so a whole-file download never
        // contends with the FUSE thread's runtime (a client shared across
        // runtimes deadlocks; see `WebDavClient::with_http_client`).
        let hydrate_rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        );
        let hydrate_source: Box<dyn ContentSource> = Box::new(LiveWebDav::new(
            dav.with_http_client(crate::tls::client(&settings.tls)?),
            hydrate_rt,
        ));
        // Kernel-invalidation channel: the syncer and the hydrator both announce
        // changes on it; the FUSE frontend drains it into `notify_inval_entry`,
        // so a file manager refreshes without a manual reload. Created here so the
        // hydrator can share it — a finished hydration flips an emblem live.
        let (inval_tx, inval_rx) = std::sync::mpsc::channel::<Invalidation>();
        let sync_inval = inval_tx.clone();
        // Eviction knows a blob's file id and nothing else — it walks the cache
        // directory by age and size, and never sees a path. The names are
        // resolved on this side, where the state database is, and only when
        // something was actually dropped.
        let (evicted_tx, evicted_rx) = std::sync::mpsc::channel::<u64>();
        let content: Arc<dyn ContentSource> = Arc::new(CachingSource::new(
            Box::new(live),
            cache_dir,
            settings.cache_max_bytes,
            settings.cache_max_age_secs,
            Some(crate::content::HydrationConfig {
                source: hydrate_source,
                invalidations: Some(inval_tx.clone()),
                evicted: Some(evicted_tx),
            }),
        ));
        // An env override stays handy for tests/tuning; otherwise config.toml wins.
        let revalidate_secs = std::env::var("WUSEL_REVALIDATE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(settings.revalidate_secs);

        // Scratch present at startup used to be pure garbage — no open write
        // survived a restart. With asynchronous write-back that is no longer
        // true: a scratch file with a pending-upload record is a change
        // committed on close that has not yet reached the server, and must be
        // resumed, not deleted. So keep those and clear only genuinely orphaned
        // scratch; the matching local-only nodes are spared by
        // `remove_unmaterialized_files` for the same reason. The uploader picks
        // the survivors up when the substrate starts.
        let db_path = account.state_db_path();
        let scratch_dir = account.cache_dir().join("scratch");
        let owed: std::collections::HashSet<u64> = state
            .pending_uploads()
            .map(|v| v.iter().map(|p| p.object.0).collect())
            .unwrap_or_default();
        if let Ok(entries) = std::fs::read_dir(&scratch_dir) {
            for e in entries.flatten() {
                let keep = e
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .is_some_and(|id| owed.contains(&id));
                if !keep {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        std::fs::create_dir_all(&scratch_dir)?;
        state.remove_unmaterialized_files()?;

        // Background revalidator: PROPFINDs of stale directories run off the FUSE
        // thread so an indexer's revalidations never block interactive operations.
        // It runs on its OWN runtime, so it must not share the FUSE thread's
        // reqwest client — a client shared across runtimes deadlocks. Give it a
        // fresh HTTP client (its own connection pool), like the push listener.
        // Shared with both background threads so a stale PROPFIND (issued before a
        // local write, applied after) never drives a reconcile — see `write_epoch`.
        let write_epoch = Arc::new(AtomicU64::new(0));

        let (reval_tx, job_rx) = std::sync::mpsc::channel::<(u64, String)>();
        let (res_tx, reval_rx) = std::sync::mpsc::channel::<RevalResult>();
        let worker_dav = dav.with_http_client(crate::tls::client(&settings.tls)?);
        let reval_epoch = write_epoch.clone();
        let reval_handle = std::thread::Builder::new()
            .name("nc-revalidate".into())
            .spawn(move || revalidate_loop(worker_dav, job_rx, res_tx, reval_epoch))
            .expect("spawn revalidate thread");

        // Background syncer: on a push it walks the cached tree by propagated
        // ETags to find server-side changes (a delete nobody re-listed). It owns a
        // second state connection + its own HTTP client, so it never blocks FUSE.
        // Pins live beside the configuration, not in the database. A database
        // written by an older version still carries them, so they are taken
        // over once — before anything reads them, or the first walk would find
        // an account with nothing pinned and quietly unprotect every blob.
        let pins = Arc::new(crate::pins::Pins::new(&account.config_dir()));
        match pins.migrate_from(&state.legacy_pins().unwrap_or_default()) {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                count = n,
                file = %pins.file().display(),
                "moved the pins out of the state database"
            ),
            // Not fatal, and deliberately not silent: the pins are still in the
            // database, so a later start can try again.
            Err(e) => tracing::warn!(%e, "could not move the pins out of the state database"),
        }

        let (sync_trigger, sync_rx) = std::sync::mpsc::channel::<()>();
        // The syncer opens its own connection to the same DB; make sure the
        // directory exists (the daemon creates it, but a direct Provider caller
        // may not).
        if let Some(dir) = account.state_db_path().parent() {
            std::fs::create_dir_all(dir)?;
        }
        let sync_state = StateDb::open(&account.state_db_path())?;

        // Turning evicted file ids back into paths needs a database connection,
        // and the cache has none — deliberately. Its own thread rather than a
        // job on an existing one: it blocks on a channel that is silent for
        // hours at a time, and the alternative was giving the cache layer a
        // database, which is the dependency this arrangement exists to avoid.
        let evict_db = StateDb::open(&db_path)?;
        let evict_inval = inval_tx.clone();
        let _evict_handle = std::thread::Builder::new()
            .name("wusel-evicted".into())
            .spawn(move || {
                while let Ok(file_id) = evicted_rx.recv() {
                    match evict_db.node_by_file_id(file_id) {
                        // It is online-only again; the emblem has to stop
                        // claiming otherwise.
                        Ok(Some(node)) => {
                            let _ = evict_inval.send(Invalidation::Entry {
                                parent: node.parent,
                                name: node.name.clone(),
                                path: node.path,
                            });
                        }
                        // A blob whose row is gone: the file was deleted and the
                        // cache is catching up. Nothing to refresh.
                        Ok(None) => {}
                        Err(e) => tracing::debug!(%e, file_id, "could not name an evicted blob"),
                    }
                }
            })
            .expect("spawn the eviction-name thread");

        let sync_dav = dav.with_http_client(crate::tls::client(&settings.tls)?);
        let sync_epoch = write_epoch.clone();
        // The syncer is where a pinned file is discovered to have moved on, so
        // it carries the policy for what to do about it. It starts here, before
        // anybody has installed a desktop backend, so it is given the slot the
        // backend will land in rather than today's placeholder. Until it does
        // land, `ask` stays silent — which is the harmless direction: nothing is
        // ever fetched unasked because a notification could not be delivered.
        let desktop_cell: Arc<Mutex<Arc<dyn Desktop>>> =
            Arc::new(Mutex::new(Arc::new(desktop::NullDesktop)));
        let sync_pinned = PinnedRefresh {
            mode: settings.refresh_pinned,
            content: Arc::clone(&content),
            desktop: Arc::clone(&desktop_cell),
            announced: Arc::new(Mutex::new(std::collections::HashSet::new())),
        };
        let sync_pins = Arc::clone(&pins);
        let sync_handle = std::thread::Builder::new()
            .name("wusel-walk".into())
            .spawn(move || {
                sync_loop(
                    sync_state,
                    sync_pins,
                    sync_dav,
                    sync_rx,
                    sync_inval,
                    sync_epoch,
                    sync_pinned,
                )
            })
            .expect("spawn syncer thread");

        Ok(Self {
            state,
            pins,
            open_pinned: settings.open_pinned,
            async_upload: matches!(settings.upload, crate::config::UploadMode::Async),
            metered: Arc::new(crate::runtime::Metered::new(Arc::clone(&desktop_cell))),
            dav,
            rt,
            dispatch_threads,
            content,
            revalidate_secs,
            push_floor_secs: settings.push_floor_secs,
            exclude_from_indexers: settings.exclude_from_indexers,
            invalidate_after: Arc::new(AtomicI64::new(0)),
            scratch_dir,
            db_path,
            text_merge: settings.text_merge,
            write_epoch,
            reval_tx,
            reval_rx,
            reval_pending: HashSet::new(),
            _reval_handle: reval_handle,
            ignore_patterns: settings.ignore_patterns,
            desktop: desktop::null(),
            sync_trigger,
            desktop_cell,
            invalidations: Some(inval_rx),
            inval_tx,
            _sync_handle: sync_handle,
        })
    }

    /// Inject the OS-integration backend (notifications + filesystem status). The
    /// frontend calls this once with a platform backend (Linux D-Bus today) — or
    /// leaves the no-op default on headless/unsupported systems. Never required.
    pub fn set_desktop(&mut self, desktop: Arc<dyn Desktop>) {
        // Down to the cache as well: it is the one that discovers, mid-read,
        // that the server is unreachable and an outdated copy is all we have.
        self.content.set_desktop(Arc::clone(&desktop));
        // And into the slot the syncer reads from. It has been running since
        // start-up with only the placeholder in there; this is the moment its
        // notifications start reaching a screen.
        *self.desktop_cell.lock().unwrap_or_else(|e| e.into_inner()) = Arc::clone(&desktop);
        self.desktop = desktop;
    }

    /// Whether the frontend should expose synthetic desktop-indexer exclusion
    /// markers at the mount root (opt-in; `[desktop] exclude_from_indexers`).
    pub fn exclude_from_indexers(&self) -> bool {
        self.exclude_from_indexers
    }

    /// The OS-integration backend, so the frontend can announce per-file changes
    /// (emblem refresh) from its invalidation-drain thread.
    pub fn desktop(&self) -> Arc<dyn Desktop> {
        self.desktop.clone()
    }

    /// A trigger for the background syncer: the push listener sends `()` on every
    /// server-side change so the syncer walks the tree and reconciles what moved.
    pub fn sync_trigger(&self) -> Sender<()> {
        self.sync_trigger.clone()
    }

    /// The frontend takes this once (at mount) to drain kernel-invalidation events
    /// the syncer produces and turn them into FUSE notifications. `None` on a
    /// second call.
    pub fn take_invalidations(&mut self) -> Option<Receiver<Invalidation>> {
        self.invalidations.take()
    }

    /// A clone of the shared invalidation timestamp. The daemon hands this to the
    /// notify_push listener, which stamps it on every server-side file change;
    /// the provider then re-lists affected directories on next access.
    pub fn invalidation_handle(&self) -> Arc<AtomicI64> {
        self.invalidate_after.clone()
    }

    /// Attributes of one node (root included). `None` if unknown.
    pub fn node(&self, inode: u64) -> Result<Option<NodeRow>> {
        self.state.node_by_inode(inode)
    }

    /// Resolve a child by name, filling the parent directory first if needed.
    pub fn lookup(&mut self, parent: u64, name: &str) -> Result<Option<NodeRow>> {
        if let Some(dir) = self.state.node_by_inode(parent)? {
            self.ensure_loaded(&dir)?;
        }
        self.state.child_by_name(parent, name)
    }

    /// List a directory's children, filling it with a lazy PROPFIND on first entry.
    pub fn list_dir(&mut self, inode: u64) -> Result<Vec<NodeRow>> {
        if let Some(dir) = self.state.node_by_inode(inode)? {
            self.ensure_loaded(&dir)?;
        }
        self.state.children_of(inode)
    }

    /// A handle to the provider's tokio runtime. A frontend uses it to run a
    /// [`ReadPlan::Fetch`] off the dispatch thread: `ContentSource::read` is
    /// synchronous and blocks inside on this very runtime, so it must run on a
    /// blocking thread, not the runtime's worker.
    pub fn runtime(&self) -> tokio::runtime::Handle {
        self.rt.handle().clone()
    }

    /// The provider's runtime itself (not just a `Handle`). A frontend uses it to
    /// `spawn_blocking` a flush and, *inside* that blocking task, `block_on` the
    /// upload — which must go through the owning `Runtime`, not a `Handle`:
    /// `Handle::block_on` from inside a `spawn_blocking` on a current-thread
    /// runtime deadlocks, whereas `Runtime::block_on` drives it correctly (the
    /// same pattern the read path uses in `content.rs`).
    pub fn runtime_arc(&self) -> Arc<tokio::runtime::Runtime> {
        self.rt.clone()
    }

    /// How many FUSE dispatch threads the frontend should run — the configured
    /// concurrency (see [`crate::config::Settings::dispatch_threads`]). The engine
    /// runtime is already sized to match. 1 = single-threaded (the default).
    pub fn dispatch_threads(&self) -> usize {
        self.dispatch_threads
    }

    // --- Writing (buffer strategy B) ----------------------------------------

    /// Everything the execution substrate needs to run this account's work.
    ///
    /// The Provider knows where its own parts live, so it hands them over
    /// rather than having the frontend assemble them from pieces it would have
    /// to be told about.
    #[must_use]
    pub fn substrate_context(&self) -> crate::runtime::Context {
        crate::runtime::Context {
            pins: Arc::clone(&self.pins),
            open_pinned: self.open_pinned,
            metered: Arc::clone(&self.metered),
            db_path: self.db_path.clone(),
            content: Arc::clone(&self.content),
            scratch_dir: self.scratch_dir.clone(),
            ignore_patterns: self.ignore_patterns.clone(),
            revalidate_secs: self.revalidate_secs,
            push_floor_secs: self.push_floor_secs,
            invalidate_after: Arc::clone(&self.invalidate_after),
            async_upload: self.async_upload,
            write: Some(self.write_context()),
        }
    }

    /// The shareable half of this Provider, for work that runs elsewhere.
    #[must_use]
    pub fn write_context(&self) -> WriteContext {
        WriteContext {
            dav: self.dav.clone(),
            rt: Arc::clone(&self.rt),
            content: Arc::clone(&self.content),
            desktop: Arc::clone(&self.desktop),
            text_merge: self.text_merge,
            write_epoch: Arc::clone(&self.write_epoch),
            invalidations: self.inval_tx.clone(),
        }
    }

    // --- Pins ("always keep offline") ---------------------------------------

    /// Resolve a server-relative path to its node, listing directories along the
    /// way as needed. The empty path is the account root.
    pub fn resolve(&mut self, path: &str) -> Result<Option<NodeRow>> {
        let path = path.trim_matches('/');
        if path.is_empty() {
            return self.state.node_by_inode(ROOT_INODE);
        }
        let mut parent = ROOT_INODE;
        let mut found = None;
        for component in path.split('/') {
            match self.lookup(parent, component)? {
                Some(node) => {
                    parent = node.inode;
                    found = Some(node);
                }
                None => return Ok(None),
            }
        }
        Ok(found)
    }

    /// Pin `path` and hydrate it now (a directory recursively). Pinning the root
    /// (`""`) keeps the whole account offline — the legacy "download everything".
    /// Returns the number of files hydrated.
    pub fn pin(&mut self, path: &str) -> Result<usize> {
        let path = path.trim_matches('/').to_string();
        let (inode, is_dir) = if path.is_empty() {
            (ROOT_INODE, true)
        } else {
            let node = self
                .resolve(&path)?
                .ok_or_else(|| Error::Other(format!("pin: path not found: {path}")))?;
            (node.inode, node.is_dir)
        };
        self.pins.set(&path, is_dir)?;
        if is_dir {
            self.hydrate_dir(inode)
        } else {
            let node = self
                .state
                .node_by_inode(inode)?
                .ok_or_else(|| Error::Other(format!("pin: vanished: {path}")))?;
            self.content.pin_file(&node)?;
            Ok(1)
        }
    }

    /// Fetch the current content for pinned files that have gone out of date.
    ///
    /// Deliberately **not** "unpin, then pin again". That would drop the
    /// eviction marker first, so a re-download that failed — no network, server
    /// down, disk full — would leave the file worse off than before: still
    /// outdated *and* no longer protected. Re-fetching in place keeps the
    /// promise intact throughout, and a failure leaves exactly what was there.
    ///
    /// Only pinned paths, because only a pin promises the file is there when
    /// the server is not. Returns how many files were actually re-fetched;
    /// those already current are not touched and not counted.
    ///
    /// # Errors
    /// If the path is unknown, is not pinned, or a download fails.
    pub fn refresh(&mut self, path: &str) -> Result<usize> {
        let path = path.trim_matches('/').to_string();
        if !self.pins.is_pinned(&path)? {
            return Err(Error::Other(format!(
                "not pinned, so there is nothing to keep up to date: {path}"
            )));
        }
        let (inode, is_dir) = if path.is_empty() {
            (ROOT_INODE, true)
        } else {
            let node = self
                .resolve(&path)?
                .ok_or_else(|| Error::Other(format!("update: path not found: {path}")))?;
            (node.inode, node.is_dir)
        };
        if is_dir {
            self.refresh_dir(inode, 0)
        } else {
            let node = self
                .state
                .node_by_inode(inode)?
                .ok_or_else(|| Error::Other(format!("update: vanished: {path}")))?;
            if !self.content.is_stale(&node) {
                return Ok(0);
            }
            self.content.pin_file(&node)?;
            Ok(1)
        }
    }

    /// [`refresh`](Self::refresh) over a directory, depth-capped like the sync
    /// walk — the same guard against a pathological tree exhausting the stack.
    fn refresh_dir(&mut self, inode: u64, depth: u32) -> Result<usize> {
        if depth >= SYNC_MAX_DEPTH {
            tracing::warn!(inode, "update: depth cap reached — not descending further");
            return Ok(0);
        }
        let mut done = 0;
        for child in self.state.children_of(inode)? {
            if child.is_dir {
                done += self.refresh_dir(child.inode, depth + 1)?;
            } else if self.content.is_stale(&child) {
                self.content.pin_file(&child)?;
                done += 1;
            }
        }
        Ok(done)
    }

    /// Recursively hydrate and protect every file under a directory.
    fn hydrate_dir(&mut self, inode: u64) -> Result<usize> {
        self.hydrate_dir_at(inode, 0)
    }

    /// [`hydrate_dir`](Self::hydrate_dir) with the recursion depth threaded
    /// through, capped at [`SYNC_MAX_DEPTH`] like the sync walk — the same guard
    /// against a pathological (or maliciously deep) tree exhausting the stack.
    fn hydrate_dir_at(&mut self, inode: u64, depth: u32) -> Result<usize> {
        if depth > SYNC_MAX_DEPTH {
            return Ok(0);
        }
        let mut count = 0;
        for child in self.list_dir(inode)? {
            if child.is_dir {
                count += self.hydrate_dir_at(child.inode, depth + 1)?;
            } else {
                self.content.pin_file(&child)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Remove the pin on `path` and drop eviction protection for the files it no
    /// longer covers (they become normal, evictable cache entries).
    pub fn unpin(&mut self, path: &str) -> Result<()> {
        let path = path.trim_matches('/');
        self.pins.remove(path)?;
        for (node_path, file_id) in self.state.descendant_file_ids(path)? {
            if !self.pins.is_pinned(&node_path)? {
                self.content.unpin_file(file_id);
            }
        }
        Ok(())
    }

    /// All pins as `(path, is_dir)`.
    ///
    /// # Errors
    /// If the pins file cannot be read.
    pub fn pins(&self) -> Result<Vec<(String, bool)>> {
        self.pins.all()
    }

    /// Ensure a directory's children are in the state and fresh. Skips the
    /// PROPFIND if the directory was listed within `revalidate_secs`, so the hot
    /// path stays cheap; otherwise re-lists and reconciles (updates ETags — which
    /// invalidates the content cache — and drops entries gone from the server).
    fn ensure_loaded(&mut self, dir: &NodeRow) -> Result<()> {
        if !dir.is_dir {
            return Ok(());
        }
        // First, apply anything the background revalidator has finished.
        self.drain_revalidations();

        // Never listed → we have nothing to serve, so load synchronously (once).
        if !self.state.children_loaded(dir.inode)? {
            tracing::debug!(path = %dir.path, inode = dir.inode, "PROPFIND (initial load)");
            let entries = self.rt.block_on(self.dav.propfind_dir(&dir.path))?;
            self.state
                .reconcile_children(dir.inode, &dir.path, &entries)?;
            return Ok(());
        }

        // Listed but stale → refresh in the *background* and serve the cached
        // listing now. This is the crux of staying responsive: a slow PROPFIND
        // (an indexer walking the tree, a large directory) must never block the
        // single FUSE thread and, with it, the user's tab-completion or `stat`.
        let invalidate_after = self.invalidate_after.load(Ordering::SeqCst);
        if self.state.dir_needs_reload(
            dir.inode,
            self.revalidate_secs,
            invalidate_after,
            self.push_floor_secs,
        )? {
            self.schedule_revalidation(dir.inode, &dir.path);
        }
        Ok(())
    }

    /// Queue a background revalidation of a directory, unless one is already in
    /// flight for it (dedup, so a burst of accesses collapses to one PROPFIND).
    fn schedule_revalidation(&mut self, inode: u64, path: &str) {
        if self.reval_pending.insert(inode)
            && self.reval_tx.send((inode, path.to_string())).is_err()
        {
            self.reval_pending.remove(&inode); // worker gone — undo
        }
    }

    /// Apply any directory listings the background revalidator has finished, then
    /// clear their pending flag. Cheap: only local SQLite writes, on the FUSE
    /// thread, so state stays single-connection and never races the worker.
    fn drain_revalidations(&mut self) {
        while let Ok((inode, path, outcome, epoch)) = self.reval_rx.try_recv() {
            self.reval_pending.remove(&inode);
            let Some(entries) = outcome else {
                continue; // the PROPFIND failed — re-scheduled on next access
            };
            // Discard a listing that a local write has outdated since the PROPFIND
            // started: applying it could delete a file just created locally (its
            // upload finished after this snapshot was taken). A fresh reval follows.
            if self.write_epoch.load(Ordering::SeqCst) != epoch {
                tracing::debug!(%path, inode, "background revalidation stale (concurrent local write) — discarding");
                continue;
            }
            // Skip if the directory vanished (rename/delete) since we scheduled it,
            // so we never reconcile children under a parent that no longer exists.
            if matches!(self.state.node_by_inode(inode), Ok(Some(_))) {
                if let Err(e) = self.state.reconcile_children(inode, &path, &entries) {
                    tracing::warn!(%e, %path, "applying background revalidation failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RemoteEntry;
    use crate::state::ROOT_INODE;

    fn dummy_dav() -> WebDavClient {
        WebDavClient::new(reqwest::Client::new(), "https://example.org", "alice", "pw")
    }

    /// `XDG_*` are process-global; serialize the tests that mutate them so a
    /// parallel run cannot read another test's paths (poison-tolerant). Shared
    /// crate-wide (see `crate::TEST_ENV_LOCK`) — config.rs tests read the same
    /// variables, and a per-module lock would not exclude those.
    use crate::TEST_ENV_LOCK as ENV_LOCK;

    #[test]
    fn conflict_copy_names_are_deduplicated_per_attempt() {
        // Attempt 0 is the classic timestamped name; later attempts append a
        // counter before the extension, so two conflicts in the same second
        // cannot overwrite each other's copy.
        let first = conflict_copy_path("Docs/report.txt", 0);
        assert!(first.starts_with("Docs/report (conflicted copy "));
        assert!(first.ends_with(").txt"));

        let second = conflict_copy_path("Docs/report.txt", 1);
        assert!(
            second.ends_with("-1).txt"),
            "counter lands before the extension: {second}"
        );
        assert_ne!(
            first, second,
            "the retry name must differ within one second"
        );

        // No extension → the tag is appended to the whole name.
        let bare = conflict_copy_path("README", 2);
        assert!(bare.starts_with("README (conflicted copy "));
        assert!(bare.ends_with("-2)"));

        // A dot in a parent directory is not an extension.
        let dotted_dir = conflict_copy_path("v1.2/notes", 0);
        assert!(dotted_dir.starts_with("v1.2/notes (conflicted copy "));
    }

    #[test]
    fn list_dir_serves_from_state_without_network() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Pre-populate the root so `list_dir` does not trigger a PROPFIND.
        let mut state = StateDb::open_in_memory().unwrap();
        state
            .reconcile_children(
                ROOT_INODE,
                "",
                &[RemoteEntry {
                    path: "Notes.txt".into(),
                    is_dir: false,
                    size: 10,
                    etag: "e".into(),
                    mtime: 0,
                    file_id: Some(1),
                    permissions: "RGDNVW".into(),
                }],
            )
            .unwrap();

        // Point XDG at a throwaway dir so the syncer's on-disk state DB (a second
        // connection) lands there, not in the real home.
        let tmp = std::env::temp_dir().join(format!("nc-prov-unit-{}", std::process::id()));
        std::env::set_var("XDG_STATE_HOME", tmp.join("state"));
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        let account = crate::config::Account::new("default");
        let mut provider = Provider::new(dummy_dav(), state, &account).unwrap();
        let kids = provider.list_dir(ROOT_INODE).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].name, "Notes.txt");
        assert!(provider.lookup(ROOT_INODE, "Notes.txt").unwrap().is_some());
    }
}

#[cfg(test)]
mod pinned_refresh_tests {
    use super::anything_new;
    use std::collections::HashSet;

    fn set(ids: &[u64]) -> HashSet<u64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn the_first_stale_file_is_worth_saying() {
        let mut announced = HashSet::new();
        assert!(anything_new(&mut announced, set(&[7])));
    }

    #[test]
    fn the_same_backlog_on_the_next_walk_stays_quiet() {
        let mut announced = HashSet::new();
        assert!(anything_new(&mut announced, set(&[7, 8])));
        assert!(!anything_new(&mut announced, set(&[7, 8])));
        // Even a shrinking backlog: nothing new happened to the user.
        assert!(!anything_new(&mut announced, set(&[7])));
    }

    #[test]
    fn one_more_file_going_stale_speaks_up_again() {
        let mut announced = HashSet::new();
        assert!(anything_new(&mut announced, set(&[7])));
        assert!(anything_new(&mut announced, set(&[7, 9])));
    }

    #[test]
    fn a_file_that_goes_stale_twice_is_announced_twice() {
        let mut announced = HashSet::new();
        assert!(anything_new(&mut announced, set(&[7])));
        // Updated: the backlog empties, and with it the memory of it.
        announced.clear();
        assert!(anything_new(&mut announced, set(&[7])));
    }
}
