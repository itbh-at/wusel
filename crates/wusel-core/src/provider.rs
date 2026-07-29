// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The frontend-agnostic `Provider` facade.
//!
//! Every OS frontend (FUSE on Linux, File Provider on macOS, Cloud Filter on
//! Windows) talks to the engine through this one type. It owns the SQLite state,
//! the WebDAV client and the sync↔async bridge (a tokio runtime), and holds all
//! the logic — listing (with lazy PROPFIND) and, from Priority 4, reading
//! contents. A frontend only translates OS callbacks into these calls.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use crate::content::{CachingSource, ContentSource, LiveWebDav};
use crate::desktop::{self, Desktop, Notice, Status};
use crate::ignore::is_ignored;
use crate::model::{basename, RemoteEntry};
use crate::state::{NodeRow, StateDb, ROOT_INODE};
use crate::webdav::WebDavClient;
use crate::{Error, Result};

/// A finished background revalidation: `(inode, path, listing, write_epoch)`.
/// `write_epoch` is the value read *just before* the PROPFIND was issued, so the
/// FUSE thread can discard a listing that a concurrent local write has made stale
/// (see [`Provider::write_epoch`]).
type RevalResult = (u64, String, Option<Vec<RemoteEntry>>, u64);

/// A file's open local write buffer (strategy B): writes land in a scratch file
/// beside the cache, keeping the validated read cache clean until the upload
/// succeeds. Its path is `scratch_dir/<inode>`.
struct Scratch {
    dirty: bool,
    /// ETag of the server version this edit is based on (for conflict detection).
    base_etag: String,
}

/// Join a child name onto a parent path (the root parent path is empty).
fn child_path(parent_path: &str, name: &str) -> String {
    if parent_path.is_empty() {
        name.to_string()
    } else {
        format!("{parent_path}/{name}")
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
fn read_range_from_scratch(path: &std::path::Path, offset: u64, len: u32) -> Result<Vec<u8>> {
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
    /// Content delivery: a caching decorator over the live WebDAV source.
    content: Box<dyn ContentSource>,
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
    /// Open write buffers, keyed by inode (see [`Scratch`]).
    scratch: HashMap<u64, Scratch>,
    /// Directory holding scratch files (`<cache>/scratch`).
    scratch_dir: PathBuf,
    /// Opt-in: try a 3-way text merge on conflict before a conflict copy.
    text_merge: bool,
    /// mtimes set via `setattr`, to send as `X-OC-Mtime` on the next upload
    /// (so `cp -p`/`rsync -t` preserve timestamps server-side).
    pending_mtime: HashMap<u64, i64>,
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
    /// Inodes of currently-open ignored files (never uploaded). A file leaves the
    /// set on delete, or on promotion when renamed to a non-ignored name.
    ignored: HashSet<u64>,
    /// OS-integration backend (notifications + filesystem status). Defaults to a
    /// no-op; the frontend injects a platform backend via [`Self::set_desktop`].
    desktop: Arc<dyn Desktop>,
    /// Trigger the background syncer (a notify_push arrived). Handed to the push
    /// listener; cloneable.
    sync_trigger: Sender<()>,
    /// Kernel-invalidation events from the syncer, for the frontend to drain and
    /// turn into FUSE notifications. Taken once by the frontend at mount.
    invalidations: Option<Receiver<Invalidation>>,
    _sync_handle: std::thread::JoinHandle<()>,
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

/// A cache entry the kernel should drop after a background sync found a change,
/// so a file manager sitting in the directory updates without a manual refresh.
/// The FUSE frontend turns this into a `notify_inval_entry`.
pub enum Invalidation {
    /// A file/dir entry changed — added, removed, or its cache state flipped.
    /// `parent` + `name` drive the kernel `notify_inval_entry`; `path` is the
    /// entry's remote (account-relative) path, which the frontend joins with the
    /// mountpoint to tell the desktop *which* file to re-read (emblem refresh).
    Entry {
        parent: u64,
        name: String,
        path: String,
    },
}

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
    /// A local edit not yet uploaded (open write buffer, or a deferred create).
    Modified,
}

impl FileState {
    /// The stable value for the `user.wusel.state` xattr that file-manager
    /// extensions read. **A public contract — keep these strings stable.**
    pub fn as_xattr(self) -> &'static str {
        match self {
            FileState::OnlineOnly => "online-only",
            FileState::Cached => "cached",
            FileState::Pinned => "pinned",
            FileState::Modified => "modified",
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
    dav: WebDavClient,
    triggers: Receiver<()>,
    invalidations: Sender<Invalidation>,
    write_epoch: Arc<AtomicU64>,
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
        if let Err(e) = walk_dir(
            &mut state,
            &dav,
            &rt,
            &invalidations,
            &write_epoch,
            ROOT_INODE,
            "",
            0,
        ) {
            tracing::debug!(%e, "syncer: walk aborted");
        }
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
    dav: &WebDavClient,
    rt: &tokio::runtime::Runtime,
    invalidations: &Sender<Invalidation>,
    write_epoch: &AtomicU64,
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
                    dav,
                    rt,
                    invalidations,
                    write_epoch,
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

impl Provider {
    pub fn new(
        dav: WebDavClient,
        state: StateDb,
        account: &crate::config::Account,
    ) -> Result<Self> {
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        );
        let settings = account.settings();
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
        let content: Box<dyn ContentSource> = Box::new(CachingSource::new(
            Box::new(live),
            cache_dir,
            settings.cache_max_bytes,
            settings.cache_max_age_secs,
            Some(crate::content::HydrationConfig {
                source: hydrate_source,
                invalidations: Some(inval_tx.clone()),
            }),
        ));
        // An env override stays handy for tests/tuning; otherwise config.toml wins.
        let revalidate_secs = std::env::var("WUSEL_REVALIDATE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(settings.revalidate_secs);

        // Any scratch present at startup is orphaned (no open write survives a
        // restart) — clear it so unflushed leftovers do not leak disk. Its local
        // nodes (never-materialised files) are then dead too, so drop them.
        let scratch_dir = account.cache_dir().join("scratch");
        let _ = std::fs::remove_dir_all(&scratch_dir);
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
        let (sync_trigger, sync_rx) = std::sync::mpsc::channel::<()>();
        // The syncer opens its own connection to the same DB; make sure the
        // directory exists (the daemon creates it, but a direct Provider caller
        // may not).
        if let Some(dir) = account.state_db_path().parent() {
            std::fs::create_dir_all(dir)?;
        }
        let sync_state = StateDb::open(&account.state_db_path())?;
        let sync_dav = dav.with_http_client(crate::tls::client(&settings.tls)?);
        let sync_epoch = write_epoch.clone();
        let sync_handle = std::thread::Builder::new()
            .name("wusel-walk".into())
            .spawn(move || sync_loop(sync_state, sync_dav, sync_rx, inval_tx, sync_epoch))
            .expect("spawn syncer thread");

        Ok(Self {
            state,
            dav,
            rt,
            content,
            revalidate_secs,
            push_floor_secs: settings.push_floor_secs,
            exclude_from_indexers: settings.exclude_from_indexers,
            invalidate_after: Arc::new(AtomicI64::new(0)),
            scratch: HashMap::new(),
            scratch_dir,
            text_merge: settings.text_merge,
            pending_mtime: HashMap::new(),
            write_epoch,
            reval_tx,
            reval_rx,
            reval_pending: HashSet::new(),
            _reval_handle: reval_handle,
            ignore_patterns: settings.ignore_patterns,
            ignored: HashSet::new(),
            desktop: desktop::null(),
            sync_trigger,
            invalidations: Some(inval_rx),
            _sync_handle: sync_handle,
        })
    }

    /// Inject the OS-integration backend (notifications + filesystem status). The
    /// frontend calls this once with a platform backend (Linux D-Bus today) — or
    /// leaves the no-op default on headless/unsupported systems. Never required.
    pub fn set_desktop(&mut self, desktop: Arc<dyn Desktop>) {
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

    /// Read `len` bytes at `offset` from a file, through the caching content
    /// source. Online-only by default: the read serves just that range live. A
    /// pinned file (or one we just wrote) is served whole from local disk.
    pub fn read(&mut self, inode: u64, offset: u64, len: u32) -> Result<Vec<u8>> {
        let node = self
            .state
            .node_by_inode(inode)?
            .ok_or_else(|| Error::Other(format!("read: unknown inode {inode}")))?;
        if node.is_dir {
            return Err(Error::Other("read: is a directory".into()));
        }
        // If a write buffer is open, serve from it: in-progress edits are visible
        // immediately (coherence), and a freshly created file — which has no server
        // copy yet (deferred create) — is readable before its first flush.
        if self.scratch.contains_key(&inode) {
            return read_range_from_scratch(&self.scratch_path(inode), offset, len);
        }
        match self.content.read(&node, offset, len) {
            Err(Error::NotFound) => {
                // Deleted on the server since we last listed it (our cache is
                // stale). Drop the node now so it disappears from listings — a
                // desktop file manager sitting in the directory otherwise keeps
                // reading it and hammering 404s — and report a stale handle.
                tracing::debug!(path = %node.path, inode, "read: gone on the server (404) — pruning stale node");
                let _ = self.state.remove_subtree(inode);
                self.ignored.remove(&inode);
                Err(Error::NotFound)
            }
            other => other,
        }
    }

    /// The file's local-availability state for OS-integration emblems. `None`
    /// for an unknown inode or a plain (unpinned) directory — the file manager
    /// shows no emblem there.
    ///
    /// Network-free by contract: a file manager calls this for **every** visible
    /// entry, so it only consults local state (SQLite + on-disk cache markers)
    /// and never triggers a PROPFIND or GET.
    pub fn file_state(&self, inode: u64) -> Result<Option<FileState>> {
        let Some(node) = self.state.node_by_inode(inode)? else {
            return Ok(None);
        };
        // A pending local edit is the most actionable state, and also covers a
        // deferred create that has no server copy yet.
        if self.scratch.contains_key(&inode) {
            return Ok(Some(FileState::Modified));
        }
        // A pin (on the file itself, an ancestor directory, or the root) means
        // "keep offline" regardless of what is currently on disk.
        if self.state.is_pinned(&node.path)? {
            return Ok(Some(FileState::Pinned));
        }
        if node.is_dir {
            return Ok(None); // an unpinned directory carries no content state
        }
        if self.content.is_cached(&node) {
            return Ok(Some(FileState::Cached));
        }
        Ok(Some(FileState::OnlineOnly))
    }

    // --- Writing (buffer strategy B) ----------------------------------------

    /// Path of a node's scratch file.
    fn scratch_path(&self, inode: u64) -> PathBuf {
        self.scratch_dir.join(inode.to_string())
    }

    /// Forget a node's write buffer: the map entry **and** its on-disk scratch
    /// file. Dropping only the map entry would leak the file until the next
    /// process start (the startup sweep is a backstop, not the mechanism).
    fn drop_scratch(&mut self, inode: u64) {
        self.scratch.remove(&inode);
        let _ = std::fs::remove_file(self.scratch_path(inode));
    }

    /// Ensure a writable scratch exists for `node`, seeded with its current
    /// content (empty for a fresh, zero-length file).
    fn ensure_scratch(&mut self, node: &NodeRow) -> Result<()> {
        if self.scratch.contains_key(&node.inode) {
            return Ok(());
        }
        std::fs::create_dir_all(&self.scratch_dir)?;
        let path = self.scratch_path(node.inode);
        if node.size > 0 {
            // Stream the full base into the scratch — never capped or held in RAM,
            // so editing a file of any size preserves all of it.
            tracing::debug!(path = %node.path, bytes = node.size, "hydrating into the write buffer");
            self.content.hydrate_to(node, &path)?;
            // Reads no longer populate the cache, so if a later 3-way merge might
            // run, seed the cache with this base now (cheap: reuse the scratch we
            // just wrote). Without it, `try_text_merge` finds no base and falls
            // back to a conflicted copy.
            if self.text_merge {
                self.content.store_file(node, &path, &node.etag)?;
            }
        } else {
            std::fs::write(&path, [])?;
        }
        self.scratch.insert(
            node.inode,
            Scratch {
                dirty: false,
                base_etag: node.etag.clone(),
            },
        );
        Ok(())
    }

    /// Write `data` at `offset` into the file's scratch buffer. Nothing reaches
    /// the server until [`flush`](Self::flush).
    pub fn write(&mut self, inode: u64, offset: u64, data: &[u8]) -> Result<u32> {
        let node = self
            .state
            .node_by_inode(inode)?
            .ok_or_else(|| Error::Other(format!("write: unknown inode {inode}")))?;
        if node.is_dir {
            return Err(Error::Other("write: is a directory".into()));
        }
        if !node.is_writable() {
            return Err(Error::Denied);
        }
        self.ensure_scratch(&node)?;

        let path = self.scratch_path(inode);
        let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        let new_len = file.metadata()?.len();
        drop(file);

        if let Some(s) = self.scratch.get_mut(&inode) {
            s.dirty = true;
        }
        if new_len != node.size {
            self.state.set_size(inode, new_len)?; // so getattr reflects the growth
        }
        Ok(data.len() as u32)
    }

    /// Resize the file's scratch buffer (`setattr` truncate).
    pub fn truncate(&mut self, inode: u64, size: u64) -> Result<()> {
        let node = self
            .state
            .node_by_inode(inode)?
            .ok_or_else(|| Error::Other(format!("truncate: unknown inode {inode}")))?;
        if node.is_dir {
            return Err(Error::Other("truncate: is a directory".into()));
        }
        if !node.is_writable() {
            return Err(Error::Denied);
        }
        // Truncate-to-zero with no open buffer discards the entire base, so
        // hydrating it first would be a full download of bytes we are about to
        // throw away — while blocking the whole single-threaded FUSE loop. This
        // is the *common* overwrite path: `cp` onto an existing file and every
        // O_TRUNC editor land here before their first write. Start from an empty
        // scratch instead. (Without a hydrated base a later 3-way text merge has
        // no local base and falls back to a conflicted copy — the right price:
        // a truncate-rewrite replaces the file wholesale anyway.)
        if size == 0 && !self.scratch.contains_key(&inode) {
            tracing::debug!(path = %node.path, "truncate to zero — starting from an empty buffer (no hydration)");
            std::fs::create_dir_all(&self.scratch_dir)?;
            std::fs::write(self.scratch_path(inode), [])?;
            self.scratch.insert(
                inode,
                Scratch {
                    dirty: false, // set below, like the hydrated path
                    base_etag: node.etag.clone(),
                },
            );
        } else {
            self.ensure_scratch(&node)?;
        }
        let path = self.scratch_path(inode);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_len(size)?;
        if let Some(s) = self.scratch.get_mut(&inode) {
            s.dirty = true;
        }
        self.state.set_size(inode, size)?;
        Ok(())
    }

    /// Set the mtime for a node (`setattr`). Reflected locally at once and sent
    /// as `X-OC-Mtime` on the next upload (so `cp -p` preserves timestamps).
    pub fn set_mtime(&mut self, inode: u64, mtime: i64) -> Result<()> {
        self.pending_mtime.insert(inode, mtime);
        self.state.set_mtime(inode, mtime)?;
        Ok(())
    }

    /// Upload the scratch if dirty, then update the state (new ETag) and refresh
    /// the read cache. A clean scratch is simply discarded. Idempotent.
    ///
    /// **The buffer is dropped only on success.** If anything fails — a server
    /// error (500), a dropped connection — the scratch (and its `pending_mtime`)
    /// is kept so the next `flush`/`fsync`/`release` retries, instead of silently
    /// losing the user's edit. Editors trigger exactly this: `vi` writes a swap
    /// file (`.foo.swp`) whose upload the server may reject, then unlinks it —
    /// which must not take the real file's buffered content down with it.
    pub fn flush(&mut self, inode: u64) -> Result<()> {
        // Peek (don't remove yet): a failed upload must leave the buffer intact.
        let (dirty, base_etag) = match self.scratch.get(&inode) {
            Some(s) => (s.dirty, s.base_etag.clone()),
            None => return Ok(()), // nothing buffered — already flushed, or clean
        };
        // An ignored file lives entirely in its scratch and never uploads. Keep
        // the buffer (it IS the file's content until the file is removed).
        if self.ignored.contains(&inode) {
            return Ok(());
        }
        let path = self.scratch_path(inode);
        if !dirty {
            self.scratch.remove(&inode);
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }

        // If the file was unlinked meanwhile (e.g. the editor deleted its swap
        // file), there is nothing to upload — drop the buffer, do not resurrect it.
        let Some(node) = self.state.node_by_inode(inode)? else {
            self.scratch.remove(&inode);
            self.pending_mtime.remove(&inode);
            let _ = std::fs::remove_file(&path);
            return Ok(());
        };
        let size = std::fs::metadata(&path)?.len();

        self.desktop.set_status(Status::Syncing);
        // One conversion point for *every* failure past this line — the upload
        // itself, conflict resolution, or the bookkeeping after a success. Any
        // of them erroring must land the indicator on Error, never leave it
        // stuck at "Syncing" (the scratch stays, so a later flush retries).
        if let Err(e) = self.upload_scratch(inode, node, &path, size, &base_etag) {
            self.desktop.set_status(Status::Error);
            return Err(e);
        }

        // Success (uploaded, or the conflict was resolved): the buffer is now on
        // the server, so it is safe to drop it.
        self.desktop.set_status(Status::Idle);
        self.pending_mtime.remove(&inode);
        self.scratch.remove(&inode);
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    /// The upload half of [`flush`](Self::flush): the conditional PUT, then the
    /// state/cache bookkeeping — or conflict resolution on a 412. Extracted so
    /// `flush` has exactly one place that maps success/failure to the desktop
    /// status indicator.
    fn upload_scratch(
        &mut self,
        inode: u64,
        node: NodeRow,
        path: &std::path::Path,
        size: u64,
        base_etag: &str,
    ) -> Result<()> {
        let mtime = self.pending_mtime.get(&inode).copied();
        // What we may legitimately assert about the server. A file id means the
        // file already exists there, which is what separates a deferred create
        // (`If-None-Match: *`) from a plain save whose ETag we happen not to know
        // — the latter must go out unconditionally rather than claim the file is
        // absent. See [`crate::webdav::Precondition`].
        let pre = crate::webdav::Precondition::for_upload(base_etag, node.file_id.is_some());
        // Conditional upload — chunked for large files (bounded memory), else a
        // plain PUT. Both reject with 412 when the precondition fails. Note we do
        // NOT `?` the upload: a transient failure must keep the buffer.
        let result = if size > crate::webdav::CHUNK_SIZE {
            self.rt
                .block_on(self.dav.put_chunked(&node.path, path, size, &pre, mtime))
        } else {
            let bytes = std::fs::read(path)?;
            self.rt
                .block_on(self.dav.put_conditional(&node.path, bytes, &pre, mtime))
        };
        match result {
            Ok(crate::webdav::PutResult::Uploaded(new_etag)) => {
                // The core success event of a sync tool — INFO, unlike the
                // request-level PUT debug: this line means state and cache are
                // updated and the local buffer can be dropped.
                tracing::info!(path = %node.path, bytes = size, "uploaded");
                let new_etag = new_etag.unwrap_or_default();
                let was_deferred = node.file_id.is_none();
                // The file now exists server-side (a deferred create gains an id via
                // the reload below). Invalidate any PROPFIND taken before this upload,
                // so a stale listing cannot delete the just-materialised file.
                self.bump_write_epoch();
                self.state.set_etag_size(inode, &new_etag, size)?;
                // Keep the read cache hot (copy the scratch — never load it whole).
                let mut updated = node;
                updated.etag = new_etag.clone();
                updated.size = size;
                self.content.store_file(&updated, path, &new_etag)?;
                // A deferred create had no server identity yet; reconcile the
                // parent so this row picks up its server-assigned file id.
                if was_deferred {
                    self.reload_dir(updated.parent)?;
                }
                Ok(())
            }
            Ok(crate::webdav::PutResult::Conflict) => {
                self.resolve_conflict(&node, path, size, base_etag)
            }
            Err(e) => {
                // Keep the scratch + pending_mtime for a later retry, and tell the
                // user their edit is only local (an actionable, data-at-risk event).
                tracing::warn!(path = %node.path, %e, "upload failed — keeping the local buffer for retry");
                self.desktop.notify(&Notice::UploadFailed {
                    path: node.path.clone(),
                    reason: e.to_string(),
                });
                Err(e)
            }
        }
    }

    /// Handle an upload conflict (412): optionally 3-way-merge text, else keep
    /// the server version and save our local content as a "conflicted copy".
    ///
    /// This also covers a **deferred create** that lost the race against a
    /// same-named server-side create (its `If-None-Match: *` failed): there is
    /// no merge base (the node has no file id, so no cached copy), so the copy
    /// path runs — the user's local content survives under the copy name, and
    /// the parent reload below pulls in the server's own file.
    fn resolve_conflict(
        &mut self,
        node: &NodeRow,
        scratch: &std::path::Path,
        size: u64,
        base_etag: &str,
    ) -> Result<()> {
        if self.text_merge {
            let local = std::fs::read(scratch)?;
            if let Some((merged, theirs)) = self.try_text_merge(node, &local, base_etag)? {
                // Upload the merge **conditionally against the very version it
                // was merged from**. The merge result is only correct for that
                // "theirs"; an unconditional PUT here would silently discard a
                // third change that landed between our GET and this PUT — the
                // exact lost update the whole 412 machinery exists to prevent.
                let result = self.rt.block_on(self.dav.put_conditional(
                    &node.path,
                    merged.clone(),
                    &theirs,
                    None,
                ))?;
                match result {
                    crate::webdav::PutResult::Uploaded(new_etag) => {
                        let new_etag = new_etag.unwrap_or_default();
                        let size = merged.len() as u64;
                        self.state.set_etag_size(node.inode, &new_etag, size)?;
                        let mut updated = node.clone();
                        updated.etag = new_etag.clone();
                        updated.size = size;
                        self.content.store(&updated, &merged, &new_etag)?;
                        tracing::info!(path = %node.path, "conflict auto-merged");
                        return Ok(());
                    }
                    crate::webdav::PutResult::Conflict => {
                        // The server moved on again while we were merging, so the
                        // merge is stale. Fall through to the conflicted copy —
                        // the user's bytes survive either way.
                        tracing::debug!(path = %node.path, "merge raced another server change — falling back to a conflicted copy");
                    }
                }
            }
        }
        // Conflict copy: the server version stays; our edit lands beside it
        // (chunked for large files). The copy uploads with `MustNotExist`
        // (`If-None-Match: *`) because its timestamped name has 1-second
        // resolution: a second conflict in the same second would otherwise
        // silently overwrite the first copy. On a 412, retry under a
        // de-duplicated name, a small bounded number of times.
        const MAX_COPY_ATTEMPTS: u32 = 4;
        let fresh = crate::webdav::Precondition::MustNotExist;
        let mut copy = conflict_copy_path(&node.path, 0);
        for attempt in 0..MAX_COPY_ATTEMPTS {
            let result = if size > crate::webdav::CHUNK_SIZE {
                self.rt
                    .block_on(self.dav.put_chunked(&copy, scratch, size, &fresh, None))?
            } else {
                let local = std::fs::read(scratch)?;
                self.rt
                    .block_on(self.dav.put_conditional(&copy, local, &fresh, None))?
            };
            match result {
                crate::webdav::PutResult::Uploaded(_) => break,
                crate::webdav::PutResult::Conflict if attempt + 1 < MAX_COPY_ATTEMPTS => {
                    copy = conflict_copy_path(&node.path, attempt + 1);
                }
                crate::webdav::PutResult::Conflict => {
                    // Every candidate name taken — vanishingly unlikely, but
                    // erroring out is safe: flush keeps the scratch, so the
                    // user's content is retried rather than dropped.
                    return Err(Error::Other(format!(
                        "conflicted copy of {}: every candidate name already exists",
                        node.path
                    )));
                }
            }
        }
        tracing::warn!(path = %node.path, copy = %copy, "upload conflict — saved a conflicted copy");
        // Tell the user: their edit is safe but under a new name (data they would
        // otherwise not find).
        self.desktop.notify(&Notice::ConflictCopy {
            path: node.path.clone(),
            copy: copy.clone(),
        });
        // The original now reflects the server version; the copy appears.
        self.bump_write_epoch(); // the copy is a new server-backed child — protect it
        self.reload_dir(node.parent)?;
        Ok(())
    }

    /// Attempt a 3-way text merge (base = cached last-known, ours = local,
    /// theirs = current server). `None` if a merge is not possible (no clean
    /// base, non-UTF-8, or a merge conflict).
    ///
    /// On success it returns the merged bytes **and** the precondition naming the
    /// "theirs" it merged against, so the caller can upload the result
    /// conditionally on exactly that version (see [`Self::resolve_conflict`]).
    fn try_text_merge(
        &mut self,
        node: &NodeRow,
        local: &[u8],
        _base_etag: &str,
    ) -> Result<Option<(Vec<u8>, crate::webdav::Precondition)>> {
        let Some(base) = self.content.cached_bytes(node) else {
            return Ok(None); // no clean base to merge against
        };
        let (theirs, theirs_etag) = self.rt.block_on(self.dav.get_with_etag(&node.path))?;
        // The file demonstrably exists (we just read it), so an absent ETag means
        // "version unknown" → upload unconditionally, never `If-None-Match: *`.
        let pre = crate::webdav::Precondition::for_upload(
            theirs_etag.as_deref().unwrap_or_default(),
            true,
        );
        let (Ok(base), Ok(ours), Ok(theirs)) = (
            String::from_utf8(base),
            String::from_utf8(local.to_vec()),
            String::from_utf8(theirs.to_vec()),
        ) else {
            return Ok(None); // binary content → cannot text-merge
        };
        match diffy::merge(&base, &ours, &theirs) {
            Ok(merged) => Ok(Some((merged.into_bytes(), pre))),
            Err(_conflicted) => Ok(None), // real conflict → fall back to a copy
        }
    }

    /// Force-re-list a directory (PROPFIND + reconcile), so a mutation we just
    /// made is reflected in the state with its server-assigned ids.
    fn reload_dir(&mut self, inode: u64) -> Result<()> {
        if let Some(dir) = self.state.node_by_inode(inode)? {
            let entries = self.rt.block_on(self.dav.propfind_dir(&dir.path))?;
            self.state.reconcile_children(inode, &dir.path, &entries)?;
        }
        Ok(())
    }

    /// Create a file under `parent` and return its node. Writes then flow through
    /// [`write`](Self::write) / [`flush`](Self::flush).
    ///
    /// **Deferred materialisation:** nothing is sent to the server here — no PUT,
    /// no PROPFIND. We only add a local node (no file id yet) and an empty scratch
    /// marked dirty, so the first [`flush`](Self::flush) uploads the file (even if
    /// never written, so `touch` still creates it). A file that is created and
    /// deleted before any flush — an editor probe, a temp file — never touches the
    /// server at all. If the name already exists locally, return that node.
    pub fn create(&mut self, parent: u64, name: &str) -> Result<NodeRow> {
        if let Some(existing) = self.state.child_by_name(parent, name)? {
            return Ok(existing);
        }
        let node = self.state.insert_local_file(parent, name)?;
        self.bump_write_epoch(); // a new child — invalidate any in-flight PROPFIND
        std::fs::create_dir_all(&self.scratch_dir)?;
        std::fs::write(self.scratch_path(node.inode), [])?;
        self.scratch.insert(
            node.inode,
            Scratch {
                dirty: true, // a new file is a pending change even with no bytes yet
                base_etag: String::new(),
            },
        );
        // Ephemeral editor/OS file (vim swap, LibreOffice/MS Office lock, …): keep
        // it purely local — it never reaches the server (no upload on flush, no
        // DELETE on remove), which saves round-trips and dodges server quirks.
        if is_ignored(name, &self.ignore_patterns) {
            tracing::debug!(path = %node.path, "create: ignored — kept local-only");
            self.ignored.insert(node.inode);
        } else {
            tracing::debug!(path = %node.path, "create (deferred — uploads on first flush)");
        }
        Ok(node)
    }

    /// Create a directory under `parent` and return its node.
    pub fn mkdir(&mut self, parent: u64, name: &str) -> Result<NodeRow> {
        let pnode = self
            .state
            .node_by_inode(parent)?
            .ok_or_else(|| Error::Other(format!("mkdir: unknown parent {parent}")))?;
        let path = child_path(&pnode.path, name);
        self.rt.block_on(self.dav.mkcol(&path))?;
        self.bump_write_epoch(); // new server-backed dir — invalidate in-flight PROPFINDs
        self.reload_dir(parent)?;
        self.state
            .child_by_name(parent, name)?
            .ok_or_else(|| Error::Other(format!("mkdir: {name} did not appear")))
    }

    /// Delete a file or directory (with its subtree) under `parent`.
    pub fn remove(&mut self, parent: u64, name: &str) -> Result<()> {
        let node = self
            .state
            .child_by_name(parent, name)?
            .ok_or_else(|| Error::Other(format!("remove: no such entry {name}")))?;
        // A file with no file id was never materialised on the server (a deferred
        // create that was deleted before its first flush) — there is nothing to
        // DELETE remotely; just drop the local buffer and node.
        if node.file_id.is_some() {
            self.rt.block_on(self.dav.delete(&node.path, node.is_dir))?;
        }
        self.drop_scratch(node.inode);
        self.pending_mtime.remove(&node.inode);
        self.ignored.remove(&node.inode);
        self.unpin_removed_subtree(&node)?;
        self.state.remove_subtree(node.inode)?;
        self.bump_write_epoch(); // a child vanished — an in-flight PROPFIND must not re-add it
        Ok(())
    }

    /// Drop the pins a deleted subtree carried, and the eviction markers beside
    /// its cache blobs.
    ///
    /// Both are path-keyed promises about files that no longer exist. A leftover
    /// `.pin` marker is not merely untidy: eviction skips a pinned blob **and**
    /// does not count it against `cache_max_bytes`, so those bytes would be
    /// exempt from the cache budget forever — and a later file handed the same
    /// Nextcloud file id would inherit the protection.
    ///
    /// Called before the rows go away, since the file ids come from them. The
    /// scan over the node table only happens when a pin actually covers this
    /// subtree — a plain `rm` of unpinned files stays a pure local delete.
    fn unpin_removed_subtree(&mut self, node: &NodeRow) -> Result<()> {
        // An ancestor (or root) pin covers this path without being *under* it.
        let covered = self.state.is_pinned(&node.path)?;
        let removed = self.state.remove_pins_under(&node.path)?;
        if !covered && removed == 0 {
            return Ok(());
        }
        for (_, file_id) in self.state.descendant_file_ids(&node.path)? {
            self.content.unpin_file(file_id);
        }
        Ok(())
    }

    /// Drop the eviction markers of a destination that a rename is about to
    /// replace — **without** touching the pin rows.
    ///
    /// The distinction matters: on a delete the path goes away, so its pins go
    /// with it ([`unpin_removed_subtree`](Self::unpin_removed_subtree)). On an
    /// overwrite the path survives and carries new content, so a pin on it must
    /// stay and now cover the *new* object. What must not survive is the
    /// replaced object's `.pin` marker: its blob is keyed by the old file id
    /// and is about to become unreachable, yet a pin marker would keep it
    /// exempt from eviction — a leak that never shrinks, since the eviction
    /// budget deliberately does not count pinned blobs.
    fn drop_replaced_eviction_markers(&mut self, replaced: &NodeRow) -> Result<()> {
        for (_, file_id) in self.state.descendant_file_ids(&replaced.path)? {
            self.content.unpin_file(file_id);
        }
        Ok(())
    }

    /// Move/rename an entry from `(parent, name)` to `(newparent, newname)`.
    ///
    /// A source that was never materialised on the server — a local-only file (a
    /// deferred create, or an ignored file) — has no server object to MOVE. It is
    /// renamed locally; then, if the **new** name is ignored it stays local-only,
    /// otherwise it is **promoted**: the buffer is uploaded under the new name.
    /// This is exactly the atomic-save pattern of office suites (write an ignored
    /// temp, rename it onto the real document).
    pub fn rename(&mut self, parent: u64, name: &str, newparent: u64, newname: &str) -> Result<()> {
        let node = self
            .state
            .child_by_name(parent, name)?
            .ok_or_else(|| Error::Other(format!("rename: no such entry {name}")))?;
        self.bump_write_epoch(); // membership of the parent(s) changes — invalidate PROPFINDs

        if node.file_id.is_none() {
            // Free the destination name locally if it is taken (an overwrite): the
            // promotion upload below replaces the server copy, an ignored rename
            // just supersedes it locally.
            if let Some(existing) = self.state.child_by_name(newparent, newname)? {
                if existing.inode != node.inode {
                    self.drop_scratch(existing.inode);
                    self.ignored.remove(&existing.inode);
                    self.drop_replaced_eviction_markers(&existing)?;
                    self.state.remove_subtree(existing.inode)?;
                    // The promotion MUST overwrite the replaced destination —
                    // this is the office-suite atomic save (write an ignored
                    // temp, rename it onto the real document). Adopt the
                    // replaced row's ETag as the buffer's base version, so the
                    // flush below uploads with `If-Match: <that etag>` (safe
                    // replace; 412 only on a *concurrent* server change) rather
                    // than the deferred-create `If-None-Match: *`, which would
                    // wrongly flag the existing document as a conflict.
                    if !existing.etag.is_empty() {
                        if let Some(s) = self.scratch.get_mut(&node.inode) {
                            s.base_etag = existing.etag.clone();
                        }
                    }
                }
            }
            self.state.rename_node(node.inode, newparent, newname)?;
            if is_ignored(newname, &self.ignore_patterns) {
                self.ignored.insert(node.inode); // still ephemeral → stays local-only
            } else {
                // Promotion: the file must now exist on the server under the new
                // name. Flush uploads the buffer to the (renamed) path and
                // reconciles it, so it gains a real file id.
                self.ignored.remove(&node.inode);
                if let Some(s) = self.scratch.get_mut(&node.inode) {
                    s.dirty = true;
                }
                // A failed upload must NOT fail the rename. The local rename is
                // already committed above, so returning an error here would tell
                // the kernel "the rename did not happen" while our state says it
                // did — and the kernel's dentry cache would keep the old name,
                // permanently out of step with us. It is also the wrong answer
                // for the user: this is the office-suite atomic save, where EIO
                // on the rename reads as "your document could not be saved",
                // although the content is safe in the scratch and only the
                // upload is outstanding.
                //
                // So the rename succeeds locally and the upload is retried by
                // the next flush/fsync/release (`flush` keeps the buffer on
                // failure). The user is not left in the dark: `flush` has
                // already put the desktop indicator on Error and sent the
                // `UploadFailed` notification.
                if let Err(e) = self.flush(node.inode) {
                    let path = self
                        .state
                        .node_by_inode(node.inode)?
                        .map(|n| n.path)
                        .unwrap_or_default();
                    tracing::warn!(
                        %path, %e,
                        "promotion upload failed — the rename stands locally, the buffer is kept for a later retry"
                    );
                }
            }
            return Ok(());
        }

        // A real server file → server MOVE (overwrite allowed).
        let np = self
            .state
            .node_by_inode(newparent)?
            .ok_or_else(|| Error::Other(format!("rename: unknown parent {newparent}")))?;
        let dst = child_path(&np.path, newname);
        self.rt
            .block_on(self.dav.move_(&node.path, &dst, node.is_dir))?;
        // The MOVE replaced any same-named destination on the server; mirror
        // that locally first so the moved row can take the (parent, name) slot.
        if let Some(existing) = self.state.child_by_name(newparent, newname)? {
            if existing.inode != node.inode {
                self.drop_scratch(existing.inode);
                self.ignored.remove(&existing.inode);
                self.drop_replaced_eviction_markers(&existing)?;
                self.state.remove_subtree(existing.inode)?;
            }
        }
        // Move the row (and its subtree's paths) ourselves instead of letting
        // the reconcile below delete + re-insert it: that keeps the inode
        // alive, so open file handles and a pending write buffer keyed by it
        // survive the rename (a dropped buffer would silently lose the edit).
        self.state.move_subtree(node.inode, newparent, newname)?;
        self.reload_dir(parent)?;
        if newparent != parent {
            self.reload_dir(newparent)?;
        }
        Ok(())
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
        self.state.set_pin(&path, is_dir)?;
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
        self.state.remove_pin(path)?;
        for (node_path, file_id) in self.state.descendant_file_ids(path)? {
            if !self.state.is_pinned(&node_path)? {
                self.content.unpin_file(file_id);
            }
        }
        Ok(())
    }

    /// All pins as `(path, is_dir)`.
    pub fn pins(&self) -> Result<Vec<(String, bool)>> {
        self.state.pins()
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

    /// Mark that local directory membership changed, so any background PROPFIND
    /// currently in flight is treated as stale on return (its listing predates this
    /// write). Call after every create/mkdir/rename/remove and after an upload that
    /// materialises a deferred create. See [`Self::write_epoch`].
    fn bump_write_epoch(&self) {
        self.write_epoch.fetch_add(1, Ordering::SeqCst);
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

    #[test]
    fn truncate_to_zero_needs_no_hydration() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Overwriting an existing file (`cp` onto it, an O_TRUNC editor) starts
        // with truncate-to-zero. That must NOT hydrate the old content first —
        // the dummy server here is unreachable, so any attempted download fails
        // the test. (A 1 GiB size also makes the cost of regressing obvious.)
        let mut state = StateDb::open_in_memory().unwrap();
        state
            .reconcile_children(
                ROOT_INODE,
                "",
                &[RemoteEntry {
                    path: "big.bin".into(),
                    is_dir: false,
                    size: 1 << 30,
                    etag: "e1".into(),
                    mtime: 0,
                    file_id: Some(7),
                    permissions: "RGDNVW".into(),
                }],
            )
            .unwrap();

        let tmp = std::env::temp_dir().join(format!("nc-prov-unit-{}", std::process::id()));
        std::env::set_var("XDG_STATE_HOME", tmp.join("state"));
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        let account = crate::config::Account::new("truncate-test");
        let mut provider = Provider::new(dummy_dav(), state, &account).unwrap();

        let inode = provider
            .lookup(ROOT_INODE, "big.bin")
            .unwrap()
            .unwrap()
            .inode;
        provider.truncate(inode, 0).unwrap();
        assert_eq!(provider.node(inode).unwrap().unwrap().size, 0);
        // The scratch is the file now: reading it yields the truncated content.
        assert!(provider.read(inode, 0, 4096).unwrap().is_empty());
    }

    #[test]
    fn file_state_reflects_pins_cache_and_edits() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let entry = |name: &str, dir: bool, fid: u64, etag: &str| RemoteEntry {
            path: name.into(),
            is_dir: dir,
            size: 10,
            etag: etag.into(),
            mtime: 0,
            file_id: Some(fid),
            permissions: "RGDNVW".into(),
        };
        let mut state = StateDb::open_in_memory().unwrap();
        state
            .reconcile_children(
                ROOT_INODE,
                "",
                &[
                    entry("online.txt", false, 11, "e1"),
                    entry("cached.txt", false, 12, "e2"),
                    entry("pinned.txt", false, 14, "e4"),
                    entry("Photos", true, 13, "e3"),
                ],
            )
            .unwrap();
        // Pin one file *before* handing the state to the provider (pinning via
        // the provider would hydrate, i.e. hit the unreachable dummy server).
        state.set_pin("pinned.txt", false).unwrap();

        let tmp = std::env::temp_dir().join(format!("nc-fstate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_STATE_HOME", tmp.join("state"));
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        let account = crate::config::Account::new("fstate");
        // Seed a fresh cache blob for cached.txt (file_id 12, etag e2).
        let blobs = account.blob_cache_dir();
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("12"), b"xxxxxxxxxx").unwrap();
        std::fs::write(blobs.join("12.etag"), "e2").unwrap();

        let mut provider = Provider::new(dummy_dav(), state, &account).unwrap();
        let ino = |p: &mut Provider, name: &str| p.lookup(ROOT_INODE, name).unwrap().unwrap().inode;

        let online = ino(&mut provider, "online.txt");
        let cached = ino(&mut provider, "cached.txt");
        let pinned = ino(&mut provider, "pinned.txt");
        let photos = ino(&mut provider, "Photos");
        assert_eq!(
            provider.file_state(online).unwrap(),
            Some(FileState::OnlineOnly)
        );
        assert_eq!(
            provider.file_state(cached).unwrap(),
            Some(FileState::Cached)
        );
        assert_eq!(
            provider.file_state(pinned).unwrap(),
            Some(FileState::Pinned)
        );
        assert_eq!(
            provider.file_state(photos).unwrap(),
            None,
            "unpinned dir: no emblem"
        );
        assert_eq!(provider.file_state(999_999).unwrap(), None, "unknown inode");

        // A freshly created (deferred, not-yet-uploaded) file reads as Modified.
        let draft = provider.create(ROOT_INODE, "draft.txt").unwrap();
        assert_eq!(
            provider.file_state(draft.inode).unwrap(),
            Some(FileState::Modified)
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
