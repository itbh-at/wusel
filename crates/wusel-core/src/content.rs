// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! File-content delivery, behind one trait so caching is a transparent decorator.
//!
//! * [`ContentSource`] — read a byte range of a file.
//! * [`LiveWebDav`]    — reads live from the server (WebDAV range GET), no cache.
//! * [`CachingSource`] — wraps another source; serves a **fresh whole-file copy**
//!   from disk when one exists, otherwise reads live.
//!
//! `Provider::read` calls a `CachingSource` wrapping a `LiveWebDav` — the classic
//! decorator pattern, so the FUSE layer never knows a cache exists.
//!
//! **Opening a file caches it (LRU).** Reading an uncached file serves the
//! requested range live *immediately* and, in the background, hydrates the whole
//! file into the cache (see [`Hydrator`]). The next reads then come from the
//! local blob. This is the classic on-demand cache: what you use is kept, bounded
//! by a configurable size budget with LRU eviction.
//!
//! The hydration is deliberately **off the FUSE thread**. A synchronous
//! whole-file download on read would freeze the single-threaded FUSE loop for the
//! length of the transfer (a 4 KiB peek at a 2 GiB file → the mount hangs). So the
//! read returns at once and a dedicated worker — with its *own* client and
//! runtime — pulls the rest.
//!
//! **Live serving uses readahead.** The kernel hands a FUSE filesystem reads of
//! at most ~128 KiB; one HTTPS round-trip per 128 KiB caps `cp`/`cat`/a media
//! player at a few MB/s. Once a file is read sequentially past
//! [`READAHEAD_AFTER`], live reads escalate to [`FETCH_CHUNK`]-sized fetches (and
//! a full contiguous pass is published straight into the cache — the fast path,
//! no second download). See [`CachingSource::read`].
//!
//! **Pinning** ([`pin_file`](ContentSource::pin_file)) is the stronger promise:
//! keep the file offline permanently, exempt from eviction. A local write
//! ([`store`](ContentSource::store) / [`store_file`](ContentSource::store_file))
//! keeps the just-uploaded copy hot too.
//!
//! All cached blobs are keyed by Nextcloud file id and validated by ETag; the
//! size (LRU) and age budget evict old, unpinned ones.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use std::sync::mpsc::Sender;

use crate::provider::Invalidation;
use crate::state::NodeRow;
use crate::webdav::WebDavClient;
use crate::Result;

/// Stream `node`'s **full** content from `source` into `out`, one
/// [`FETCH_CHUNK`] at a time. Errors if the source delivers fewer than
/// `node.size` bytes (a stale listing, a file truncated server-side): a
/// truncated copy must never be treated as complete — published as a cache
/// blob it would serve wrong reads, and as a write-buffer base a later flush
/// would upload it as the file's full content. Callers clean up their own
/// partial output file on error.
fn stream_full<S: ContentSource + ?Sized>(
    source: &S,
    node: &NodeRow,
    out: &mut File,
) -> Result<()> {
    let mut off = 0u64;
    while off < node.size {
        let want = std::cmp::min(FETCH_CHUNK as u64, node.size - off) as u32;
        let bytes = source.read(node, off, want)?;
        if bytes.is_empty() {
            break; // server returned short — reported as an error below
        }
        out.write_all(&bytes)?;
        off += bytes.len() as u64;
    }
    out.flush()?;
    if off < node.size {
        return Err(crate::Error::Other(format!(
            "short read: got {off} of {} bytes for {}",
            node.size, node.path
        )));
    }
    Ok(())
}

/// Reads a byte range of a file identified by its state row.
///
/// `Send + Sync` so a single source can be shared across threads (e.g. a
/// multi-threaded frontend) — the caching decorator relies on that for its
/// single-flight coordination.
pub trait ContentSource: Send + Sync {
    fn read(&self, node: &NodeRow, offset: u64, len: u32) -> Result<Vec<u8>>;

    /// Ensure the whole file is cached and protected from eviction ("pinning").
    /// A pin overrides the size budget — even an oversized file is cached, since
    /// the user asked to keep it offline. Default: no-op (no cache to pin into).
    fn pin_file(&self, _node: &NodeRow) -> Result<()> {
        Ok(())
    }

    /// Drop eviction protection for a file id (unpin). Default: no-op.
    fn unpin_file(&self, _file_id: u64) {}

    /// Install freshly-uploaded content into the cache, validated by its new
    /// ETag, so the local copy stays hot after a write. Default: no-op.
    fn store(&self, _node: &NodeRow, _data: &[u8], _etag: &str) -> Result<()> {
        Ok(())
    }

    /// The cached bytes for `node` if a complete, ETag-matching copy is on disk —
    /// the "base" (last-known server version) for a 3-way merge. Default: `None`.
    fn cached_bytes(&self, _node: &NodeRow) -> Option<Vec<u8>> {
        None
    }

    /// Whether a complete, ETag-matching copy is already on disk — a cheap
    /// existence check (no read, unlike [`cached_bytes`](Self::cached_bytes)).
    /// The OS integration queries this per file to show a "local" emblem, so it
    /// must stay network-free and O(1). Default: `false`.
    fn is_cached(&self, _node: &NodeRow) -> bool {
        false
    }

    /// Like [`store`](Self::store), but from a file — so a large upload never
    /// materialises in memory. Default: no-op.
    fn store_file(&self, _node: &NodeRow, _src: &Path, _etag: &str) -> Result<()> {
        Ok(())
    }

    /// Write the node's **full** content to `dest`, streaming in chunks — so it
    /// works for files of any size (`u64` throughout) with only one chunk in
    /// memory. This seeds a write buffer's base. Default: via `read`.
    fn hydrate_to(&self, node: &NodeRow, dest: &Path) -> Result<()> {
        let mut out = File::create(dest)?;
        if let Err(e) = stream_full(self, node, &mut out) {
            drop(out);
            let _ = std::fs::remove_file(dest); // never leave a truncated base
            return Err(e);
        }
        Ok(())
    }
}

/// Live source: each call is a WebDAV range GET straight from the server.
pub struct LiveWebDav {
    dav: WebDavClient,
    rt: Arc<tokio::runtime::Runtime>,
}

impl LiveWebDav {
    pub fn new(dav: WebDavClient, rt: Arc<tokio::runtime::Runtime>) -> Self {
        Self { dav, rt }
    }
}

impl ContentSource for LiveWebDav {
    fn read(&self, node: &NodeRow, offset: u64, len: u32) -> Result<Vec<u8>> {
        if len == 0 || offset >= node.size {
            return Ok(Vec::new());
        }
        let bytes = self
            .rt
            .block_on(self.dav.get(&node.path, Some((offset, len as u64))))?;
        Ok(bytes.to_vec())
    }
}

/// How much to fetch per request when filling the cache. Bounds memory use while
/// still amortising round-trips.
const FETCH_CHUNK: u32 = 8 * 1024 * 1024;

/// Escalate a file's reads to chunked readahead once this many bytes have been
/// consumed strictly sequentially. Below the threshold every read stays an
/// exact 1:1 live range GET, so a thumbnailer's or MIME sniffer's peek never
/// amplifies into a bulk transfer; a contiguous run past it is clearly `cp`,
/// `cat` or a player, where per-128-KiB round-trips are what makes the mount
/// crawl. Two kernel-sized reads (2 × 128 KiB) cross it.
const READAHEAD_AFTER: u64 = 256 * 1024;

/// At most this many files tracked for sequential reading at once; the
/// least-recently-used tracker is dropped beyond that. Bounds memory to
/// `MAX_WINDOWS × FETCH_CHUNK` even if a scanner touches thousands of files.
const MAX_WINDOWS: usize = 8;

/// The read floor that separates *opening* a file from an incidental peek. A
/// MIME sniff or content-type guess reads a few KB (≤ 64 KiB) and stops; an
/// application that opens a file reads well past this. Set low enough that a
/// single open reliably crosses it, high enough to ignore sniffs.
const HYDRATE_FLOOR: u64 = 512 * 1024;

/// Cap on the per-file read-so-far tally (see [`CachingSource::seen`]); a crude
/// backstop so a session that touches a little of very many files cannot grow
/// the map without bound.
const MAX_SEEN: usize = 4096;

/// How many bytes of a file must be read (scattered — a clean sequential run is
/// cached by the readahead spill instead) before it is hydrated whole in the
/// background: [`HYDRATE_FLOOR`], or the whole file if it is smaller. So opening
/// a file reliably caches it on the *first* open (an app reads past the floor at
/// once), a tiny header sniff never does, and fully reading a small file always
/// does. NOTE: this no longer scales with file size, so a large file is hydrated
/// once read past the floor — the deliberate "opened → cached" trade-off, bounded
/// by the LRU cache budget (a per-file size cap for huge media could be added).
fn hydrate_trigger(size: u64) -> u64 {
    HYDRATE_FLOOR.min(size)
}

/// Per-file sequential-read state (see [`CachingSource::read`]).
///
/// ## Rust learning note: interior state behind `&self`
/// `ContentSource::read` takes `&self`, so this mutable tracker lives in a
/// `Mutex<HashMap<…>>` on the source — the standard interior-mutability pattern
/// when a trait fixes the receiver as shared.
struct ReadWindow {
    /// ETag the run is based on; a server-side change invalidates the window.
    etag: String,
    /// Offset the next read must start at to continue the run.
    next: u64,
    /// Bytes consumed contiguously so far.
    run: u64,
    /// Prefetched bytes covering `buf_start ..` — empty until escalation.
    buf_start: u64,
    buf: Vec<u8>,
    /// Spill file assembling the whole file, `(path, bytes written)`. Only a run
    /// that starts at byte 0 carries one; when it reaches the file size it is
    /// published as a regular cache blob. `None` once the chance is gone.
    part: Option<(PathBuf, u64)>,
    /// For evicting the least-recently-used window (see [`MAX_WINDOWS`]).
    last_use: std::time::Instant,
}

impl ReadWindow {
    fn start(node: &NodeRow, offset: u64, part_path: PathBuf) -> Self {
        Self {
            etag: node.etag.clone(),
            next: offset,
            run: 0,
            buf_start: 0,
            buf: Vec::new(),
            // Only a run starting at byte 0 can ever assemble the complete file.
            part: (offset == 0).then_some((part_path, 0)),
            last_use: std::time::Instant::now(),
        }
    }

    /// The requested slice out of the prefetch buffer, or `None` on a miss. May
    /// return fewer than `len` bytes when the buffer's tail partially covers the
    /// request — a legal short read; the follow-up read misses and refills.
    fn buffered(&self, offset: u64, len: u32) -> Option<Vec<u8>> {
        let end = self.buf_start + self.buf.len() as u64;
        if self.buf.is_empty() || offset < self.buf_start || offset >= end {
            return None;
        }
        let start = (offset - self.buf_start) as usize;
        let stop = std::cmp::min(start + len as usize, self.buf.len());
        Some(self.buf[start..stop].to_vec())
    }

    /// Account a served slice: extend the run and append to the spill file.
    fn advance(&mut self, out: &[u8]) {
        self.last_use = std::time::Instant::now();
        if out.is_empty() {
            return;
        }
        self.next += out.len() as u64;
        self.run += out.len() as u64;
        if let Some((path, written)) = &mut self.part {
            // First write truncates (a stale spill from a crashed run may exist);
            // later ones append. On any I/O error just give up on the spill —
            // serving the read matters, caching it is opportunistic.
            let res = if *written == 0 {
                File::create(&*path)
            } else {
                std::fs::OpenOptions::new().append(true).open(&*path)
            }
            .and_then(|mut f| f.write_all(out));
            match res {
                Ok(()) => *written += out.len() as u64,
                Err(_) => self.drop_part(),
            }
        }
    }

    /// True once the spill holds every byte of the file.
    fn complete(&self, size: u64) -> bool {
        self.part
            .as_ref()
            .is_some_and(|(_, written)| *written == size)
    }

    /// Abandon the spill file (broken run, eviction, or write error).
    fn drop_part(&mut self) {
        if let Some((path, _)) = self.part.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A window that goes away takes its unfinished spill file with it.
///
/// ## Rust learning note: RAII instead of cleanup at every exit
/// `read_windowed` takes the window *out* of the map and then does network I/O
/// with `?`. Every such early return drops the window, and without this impl the
/// spill file would survive as an orphan — invisible to eviction (it has an
/// extension, so [`evict`] skips it) and removed only by the next process
/// start's sweep. One orphan per failed read is an unbounded leak, because each
/// window gets its own `<file_id>.<seq>.ra` path. Tying the cleanup to the
/// value's lifetime covers *all* exits, including a panic, which no amount of
/// hand-written cleanup at each `?` can.
///
/// The paths that legitimately keep the file take the spill out first
/// ([`CachingSource::publish_window`] does `w.part.take()` before renaming), so
/// `part` is already `None` here and a published blob is never touched.
impl Drop for ReadWindow {
    fn drop(&mut self) {
        self.drop_part();
    }
}

/// Caching decorator: caches whole files on disk, keyed by their Nextcloud file
/// id and validated by ETag. A changed ETag invalidates the cached copy. A size
/// budget (LRU eviction) and an optional max age keep the cache bounded.
pub struct CachingSource {
    inner: Box<dyn ContentSource>,
    dir: PathBuf,
    /// LRU eviction budget in bytes; `None` = unlimited.
    max_bytes: Option<u64>,
    /// Drop blobs unused for longer than this; `None` = never.
    max_age_secs: Option<u64>,
    /// Per-file-id locks that coalesce concurrent downloads of the same file, so
    /// two readers of a cold blob share one fetch instead of racing on `.part`.
    inflight: Mutex<HashMap<u64, Arc<Mutex<()>>>>,
    /// Sequential-read trackers by file id (see [`ReadWindow`] and `read`).
    /// Locked only to take a window out and to put it back — never across
    /// network I/O, so concurrent uncached reads of different files don't
    /// serialize on this one mutex (see [`Self::read_windowed`]).
    windows: Mutex<HashMap<u64, ReadWindow>>,
    /// Monotonic id making every window's spill path unique: two threads
    /// reading the same cold file concurrently each own a window outside the
    /// lock, and sharing one `<file_id>.ra` path would let them corrupt each
    /// other's spill.
    window_seq: std::sync::atomic::AtomicU64,
    /// Bytes read so far per file id, across seeks — the signal that separates an
    /// incidental peek from real use (see [`hydrate_trigger`]). An entry is
    /// dropped once the file is cached or its hydration is requested.
    seen: Mutex<HashMap<u64, u64>>,
    /// Background whole-file hydration (opened → cached). `None` disables it —
    /// used in unit tests to keep the read path deterministic and offline.
    hydrator: Option<Hydrator>,
}

impl CachingSource {
    /// `hydrate` configures background hydration: the [`ContentSource`] the
    /// hydrator fetches through (in production a second live source with its
    /// *own* client and runtime, so it never contends with the FUSE thread's)
    /// plus an optional invalidation channel to refresh emblems on completion.
    /// `None` disables background hydration (unit tests).
    pub fn new(
        inner: Box<dyn ContentSource>,
        dir: PathBuf,
        max_bytes: Option<u64>,
        max_age_secs: Option<u64>,
        hydrate: Option<HydrationConfig>,
    ) -> Self {
        // Temp files from a previous process are dead: sequential spills (`.ra`),
        // hydration downloads (`.dl`) and unpublished writes (`.part`) whose
        // owners are gone. Sweep them. (Safe for `.part` because this runs at
        // construction, before any worker that could own a live one exists.)
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|e| e == "ra" || e == "dl" || e == "part")
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        let hydrator = hydrate.map(|h| {
            Hydrator::new(
                h.source,
                dir.clone(),
                max_bytes,
                max_age_secs,
                h.invalidations,
            )
        });
        Self {
            inner,
            dir,
            max_bytes,
            max_age_secs,
            inflight: Mutex::new(HashMap::new()),
            windows: Mutex::new(HashMap::new()),
            window_seq: std::sync::atomic::AtomicU64::new(0),
            seen: Mutex::new(HashMap::new()),
            hydrator,
        }
    }

    /// The per-file-id lock, creating it if absent.
    fn fetch_lock(&self, file_id: u64) -> Arc<Mutex<()>> {
        let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(file_id).or_default().clone()
    }

    /// Drop a file-id's lock slot once no reader still holds it.
    fn gc_fetch_lock(&self, file_id: u64) {
        let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lock) = map.get(&file_id) {
            if Arc::strong_count(lock) == 1 {
                map.remove(&file_id);
            }
        }
    }

    /// True if a complete, ETag-matching copy is on disk.
    fn is_fresh(&self, blob: &Path, etag: &str) -> bool {
        blob_is_fresh(blob, etag)
    }

    /// Download the whole file (in chunks) into the blob, then mark it fresh.
    fn fetch_whole(&self, blob: &Path, node: &NodeRow) -> Result<()> {
        if let Some(parent) = blob.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let part = blob.with_extension("part");
        let mut out = File::create(&part)?;
        if let Err(e) = stream_full(&*self.inner, node, &mut out) {
            drop(out);
            let _ = std::fs::remove_file(&part); // incomplete — do not publish
            return Err(e);
        }
        // Publish atomically — sidecar first, then the bytes (see `publish_blob`
        // for why that order matters), so a crashed download is never mistaken
        // for a complete one. A failed publish leaves no temp behind either.
        if let Err(e) = publish_blob(&part, blob, &node.etag) {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        Ok(())
    }

    /// Serve a kernel-sized read with sequential-read detection.
    ///
    /// A read that continues its file's window extends the run; anything else
    /// (first touch, a seek, a changed ETag) starts a fresh window. Runs below
    /// [`READAHEAD_AFTER`] are served as exact live range GETs — the sporadic
    /// case. Past the threshold the read fetches [`FETCH_CHUNK`] bytes and the
    /// following reads are served from that buffer. A run that started at byte 0
    /// spills every served byte to disk; when it covers the whole file it is
    /// published as a cache blob, so re-reading the file is local.
    fn read_windowed(
        &self,
        node: &NodeRow,
        file_id: u64,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>> {
        // Take the file's window *out* of the map and release the lock before
        // any network round-trip: holding the global lock across `inner.read`
        // (an HTTPS request) would serialize uncached reads of *all* files on
        // this one mutex. While this thread owns the window, a concurrent
        // reader of the same file simply starts a fresh one; the spill paths
        // are per-window (see `window_seq`), so they cannot corrupt each other.
        let mut w = {
            let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
            match windows.remove(&file_id) {
                Some(w) if w.etag == node.etag && offset == w.next => w,
                prior => {
                    // Broken run — its spill can never complete, and dropping
                    // the old window deletes it (see `Drop for ReadWindow`).
                    drop(prior);
                    // The spill writes into the cache dir; on a fresh account it
                    // may not exist yet and the spill would then fail silently on
                    // every run (fetch_whole/store create it, reads never did).
                    if offset == 0 {
                        let _ = std::fs::create_dir_all(&self.dir);
                    }
                    let seq = self
                        .window_seq
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    ReadWindow::start(node, offset, self.dir.join(format!("{file_id}.{seq}.ra")))
                }
            }
        };

        let out = if let Some(slice) = w.buffered(offset, len) {
            tracing::trace!(path = %node.path, offset, len, "read from readahead buffer");
            slice
        } else if w.run >= READAHEAD_AFTER {
            // Log the transition once per run (before the first chunk fetch),
            // not per refill — it explains the switch from many small GETs to
            // few large ones.
            if w.buf.is_empty() {
                tracing::debug!(path = %node.path, offset, "sequential read — escalating to chunked readahead");
            }
            let want = std::cmp::min(FETCH_CHUNK as u64, node.size - offset) as u32;
            let bytes = self.inner.read(node, offset, want)?;
            w.buf_start = offset;
            w.buf = bytes;
            w.buffered(offset, len).unwrap_or_default()
        } else {
            self.inner.read(node, offset, len)?
        };

        w.advance(&out);
        // A live from-0 sequential run (a spill in progress) will cache the file
        // for free on completion — no background download needed there. Only
        // *scattered* access needs hydration.
        let spill_active = w.part.is_some();
        if w.complete(node.size) {
            self.publish_window(w, node, file_id);
        } else if out.is_empty() {
            w.drop_part(); // EOF/short — this window is finished
        } else {
            let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
            Self::keep_window(&mut windows, file_id, w);
        }

        // Background hydration once enough of the file has been read that this is
        // clearly use, not an incidental peek (see `hydrate_trigger`). Skipped
        // while a spill handles it, and once requested we stop tallying.
        if let Some(hydrator) = &self.hydrator {
            if !spill_active && node.size > 0 && !out.is_empty() {
                let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
                let total = seen.entry(file_id).or_insert(0);
                *total = total.saturating_add(out.len() as u64);
                let reached = *total >= hydrate_trigger(node.size);
                if reached {
                    seen.remove(&file_id); // hydration requested — stop tallying it
                }
                // The backstop has to sit *outside* the branch above: the map
                // only grows on the reads that do **not** reach the trigger — a
                // scanner peeking at very many files adds an entry each time and
                // removes none. Checking it only where an entry is removed made
                // the cap unreachable (see `MAX_SEEN`).
                if seen.len() > MAX_SEEN {
                    seen.clear(); // crude, but it bounds the map
                }
                if reached {
                    drop(seen);
                    hydrator.request(node);
                }
            }
        }
        Ok(out)
    }

    /// Publish a completed spill as the file's cache blob (rename + ETag sidecar,
    /// then budget enforcement) — the same publish protocol as `fetch_whole`.
    fn publish_window(&self, mut w: ReadWindow, node: &NodeRow, file_id: u64) {
        let Some((part, _)) = w.part.take() else {
            return;
        };
        let blob = self.dir.join(file_id.to_string());
        match publish_blob(&part, &blob, &node.etag) {
            Ok(()) => {
                tracing::debug!(path = %node.path, "cached whole file after sequential read");
                self.enforce_budget();
            }
            Err(e) => {
                tracing::debug!(%e, path = %node.path, "publishing the read spill failed");
                // Only our own temp is ours to remove. The blob may already hold
                // valid bytes — ours (the rename succeeded and only the sidecar
                // write failed) or those of a publisher that won a concurrent
                // race; deleting it would throw away a correct cache entry. With
                // no matching sidecar it simply reads as "not fresh" and is a
                // normal eviction candidate.
                let _ = std::fs::remove_file(&part);
            }
        }
    }

    /// Retain a window, evicting the least-recently-used one over the cap.
    ///
    /// Both removals below abandon the evicted window's spill file implicitly:
    /// the returned value is dropped right away, and `Drop for ReadWindow`
    /// deletes the spill — no explicit `drop_part` needed.
    fn keep_window(windows: &mut HashMap<u64, ReadWindow>, file_id: u64, w: ReadWindow) {
        if windows.len() >= MAX_WINDOWS {
            if let Some(oldest) = windows
                .iter()
                .min_by_key(|(_, w)| w.last_use)
                .map(|(id, _)| *id)
            {
                windows.remove(&oldest);
            }
        }
        // A concurrent reader of the same file may have inserted its own window
        // while ours was out of the map (reads run unlocked); last one wins,
        // the loser's spill is abandoned.
        windows.insert(file_id, w);
    }

    /// Best-effort eviction: age-based expiry, then LRU down to the size budget.
    fn enforce_budget(&self) {
        if let Err(e) = evict(&self.dir, self.max_bytes, self.max_age_secs) {
            tracing::warn!(%e, "cache eviction failed");
        }
    }
}

/// True if a complete, ETag-matching blob is on disk. Free function so the
/// background hydrator (a plain thread) can use it too.
fn blob_is_fresh(blob: &Path, etag: &str) -> bool {
    blob.exists()
        && std::fs::read_to_string(etag_path(blob))
            .map(|s| s == etag)
            .unwrap_or(false)
}

/// Download the whole file through `source` into `dir/<file_id>`, atomically
/// (temp `.dl`, then rename + ETag sidecar). The blob is a normal, evictable
/// cache entry (no pin marker). Used by the background hydrator; `source` is its
/// own [`ContentSource`] (own client + runtime), so it never contends with the
/// FUSE thread's.
fn download_whole(
    source: &dyn ContentSource,
    node: &NodeRow,
    dir: &Path,
    file_id: u64,
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let part = dir.join(format!("{file_id}.dl"));
    let mut out = File::create(&part)?;
    if let Err(e) = stream_full(source, node, &mut out) {
        drop(out);
        let _ = std::fs::remove_file(&part); // incomplete — do not publish
        return Err(e);
    }
    let blob = dir.join(file_id.to_string());
    // Same publish protocol as everywhere else (see `publish_blob`); the `.dl`
    // temp is cleaned up on a failed publish too, not just on a failed download.
    if let Err(e) = publish_blob(&part, &blob, &node.etag) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    Ok(())
}

/// Best-effort cache eviction: age-based expiry, then LRU down to the size
/// budget. Free function so both the read path and the hydrator can call it.
fn evict(dir: &Path, max_bytes: Option<u64>, max_age_secs: Option<u64>) -> Result<()> {
    if max_bytes.is_none() && max_age_secs.is_none() {
        return Ok(());
    }
    // Collect blobs (files without an extension; .etag/.part/.dl are skipped).
    let mut blobs: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    for entry in rd {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some() {
            continue;
        }
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        if pin_path(&path).exists() {
            continue; // pinned ("always offline") → never evict, never count
        }
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        blobs.push((path, meta.len(), mtime));
    }

    let now = std::time::SystemTime::now();
    if let Some(max_age) = max_age_secs {
        blobs.retain(|(path, _, mtime)| {
            let age = now.duration_since(*mtime).map(|d| d.as_secs()).unwrap_or(0);
            if age > max_age {
                evict_blob(path);
                false
            } else {
                true
            }
        });
    }
    if let Some(max) = max_bytes {
        let mut total: u64 = blobs.iter().map(|(_, sz, _)| *sz).sum();
        if total > max {
            blobs.sort_by_key(|(_, _, mtime)| *mtime); // least-recently-used first
            for (path, sz, _) in &blobs {
                if total <= max {
                    break;
                }
                evict_blob(path);
                total = total.saturating_sub(*sz);
            }
        }
    }
    Ok(())
}

/// Background whole-file hydration: turns "a file was opened" into "it is in the
/// LRU cache", off the FUSE thread. The read path serves the requested range
/// live immediately and *requests* hydration; this worker downloads the rest and
/// publishes an evictable blob, at which point the file reads local and its
/// emblem flips online-only → cached.
/// What the background hydrator needs beyond the cache dir: a source to fetch
/// through (its own client + runtime) and, optionally, a channel to announce a
/// finished hydration so the frontend can refresh the file's emblem live.
pub struct HydrationConfig {
    pub source: Box<dyn ContentSource>,
    pub invalidations: Option<Sender<Invalidation>>,
}

struct Hydrator {
    tx: std::sync::mpsc::Sender<NodeRow>,
    /// File ids with a hydration in flight (dedup); shared with the worker so it
    /// clears each id when done.
    pending: Arc<Mutex<std::collections::HashSet<u64>>>,
    _worker: std::thread::JoinHandle<()>,
}

impl Hydrator {
    fn new(
        source: Box<dyn ContentSource>,
        dir: PathBuf,
        max_bytes: Option<u64>,
        max_age_secs: Option<u64>,
        invalidations: Option<Sender<Invalidation>>,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<NodeRow>();
        let pending: Arc<Mutex<std::collections::HashSet<u64>>> = Arc::default();
        let worker_pending = pending.clone();
        let worker = std::thread::Builder::new()
            .name("nc-hydrate".into())
            .spawn(move || {
                hydrate_loop(
                    rx,
                    worker_pending,
                    source,
                    dir,
                    max_bytes,
                    max_age_secs,
                    invalidations,
                )
            })
            .expect("spawn hydration thread");
        Self {
            tx,
            pending,
            _worker: worker,
        }
    }

    /// Enqueue a whole-file hydration for `node`, unless one is already in flight
    /// for it. No-op for a node without a stable cache key (no file id).
    fn request(&self, node: &NodeRow) {
        let Some(file_id) = node.file_id else {
            return;
        };
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.insert(file_id) && self.tx.send(node.clone()).is_err() {
            pending.remove(&file_id); // worker gone
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn hydrate_loop(
    rx: std::sync::mpsc::Receiver<NodeRow>,
    pending: Arc<Mutex<std::collections::HashSet<u64>>>,
    source: Box<dyn ContentSource>,
    dir: PathBuf,
    max_bytes: Option<u64>,
    max_age_secs: Option<u64>,
    invalidations: Option<Sender<Invalidation>>,
) {
    while let Ok(node) = rx.recv() {
        let file_id = node.file_id.expect("requested only with a file id");
        let blob = dir.join(file_id.to_string());
        // Skip if it was cached meanwhile (pinned, a local write, or a prior
        // hydration) — no redundant download.
        if !blob_is_fresh(&blob, &node.etag) {
            match download_whole(&*source, &node, &dir, file_id) {
                Ok(()) => {
                    tracing::debug!(path = %node.path, bytes = node.size, "hydrated into cache");
                    if let Err(e) = evict(&dir, max_bytes, max_age_secs) {
                        tracing::warn!(%e, "cache eviction failed");
                    }
                    // Nudge the frontend to re-read this file's info, so its
                    // emblem flips online-only → cached without a manual refresh.
                    if let Some(tx) = &invalidations {
                        let _ = tx.send(Invalidation::Entry {
                            parent: node.parent,
                            name: node.name.clone(),
                            path: node.path.clone(),
                        });
                    }
                }
                Err(e) => tracing::debug!(%e, path = %node.path, "background hydration failed"),
            }
        }
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&file_id);
    }
}

impl ContentSource for CachingSource {
    fn read(&self, node: &NodeRow, offset: u64, len: u32) -> Result<Vec<u8>> {
        if len == 0 || offset >= node.size {
            return Ok(Vec::new());
        }
        // A fresh whole-file copy on disk (pinned, locally written, or published
        // by a completed sequential run) serves everything locally.
        if let Some(file_id) = node.file_id {
            let blob = self.dir.join(file_id.to_string());
            if self.is_fresh(&blob, &node.etag) {
                touch(&blob); // mark recently used for LRU
                              // Cached now — stop tallying reads toward hydration for it.
                self.seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&file_id);
                // TRACE, not debug: one line per kernel read (~128 KiB) — the
                // read-by-read provenance for deep debugging, far too chatty
                // for the debug narrative.
                tracing::trace!(path = %node.path, offset, len, "read from cache blob");
                match read_range_from_file(&blob, offset, len) {
                    Ok(out) => return Ok(out),
                    // Freshness check and file read are two steps; the background
                    // hydrator's eviction can delete the blob in between (TOCTOU).
                    // The content still exists on the server, so fall through to
                    // the live path below instead of failing a servable read.
                    Err(e) => {
                        tracing::debug!(%e, path = %node.path,
                            "cache blob vanished between freshness check and read — serving live");
                    }
                }
            }
        }
        // Kernel-sized reads go through sequential-read detection (see the
        // module docs): a bulk read escalates to chunked readahead, so live
        // serving stays efficient. Internal bulk transfers (hydration) already
        // read in FETCH_CHUNK strides — pass them straight through, as does any
        // read of a file without the stable cache key (its file id).
        match node.file_id {
            // Serve live now; `read_windowed` also decides, from how much of the
            // file has been read, whether to hydrate the whole thing in the
            // background (never on this FUSE thread) — see `hydrate_trigger`.
            Some(file_id) if len < FETCH_CHUNK => self.read_windowed(node, file_id, offset, len),
            _ => self.inner.read(node, offset, len),
        }
    }

    fn pin_file(&self, node: &NodeRow) -> Result<()> {
        let Some(file_id) = node.file_id else {
            return Ok(()); // no stable key → cannot cache, so cannot pin
        };
        let blob = self.dir.join(file_id.to_string());
        let lock = self.fetch_lock(file_id);
        {
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            if !self.is_fresh(&blob, &node.etag) {
                // Force a full fetch, ignoring the size budget: a pin means keep.
                self.fetch_whole(&blob, node)?;
            }
            let _ = std::fs::write(pin_path(&blob), []); // eviction-protection marker
        }
        drop(lock);
        self.gc_fetch_lock(file_id);
        Ok(())
    }

    fn unpin_file(&self, file_id: u64) {
        let blob = self.dir.join(file_id.to_string());
        let _ = std::fs::remove_file(pin_path(&blob));
    }

    fn store(&self, node: &NodeRow, data: &[u8], etag: &str) -> Result<()> {
        let Some(file_id) = node.file_id else {
            return Ok(()); // no stable key → cannot cache (e.g. a just-created file)
        };
        let blob = self.dir.join(file_id.to_string());
        if let Some(parent) = blob.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Never write the blob in place: a concurrent reader of the old copy
        // would see torn content mid-overwrite. Stage into a temp and publish
        // with the same rename protocol as `fetch_whole` (see `publish_blob`).
        let tmp = blob.with_extension("part");
        let staged = std::fs::write(&tmp, data)
            .map_err(crate::Error::from)
            .and_then(|()| publish_blob(&tmp, &blob, etag));
        if let Err(e) = staged {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        // Reads no longer fill the cache, so a write is the only place an
        // unpinned blob is born — keep it within budget here.
        self.enforce_budget();
        Ok(())
    }

    fn cached_bytes(&self, node: &NodeRow) -> Option<Vec<u8>> {
        let file_id = node.file_id?;
        let blob = self.dir.join(file_id.to_string());
        if self.is_fresh(&blob, &node.etag) {
            std::fs::read(&blob).ok()
        } else {
            None
        }
    }

    fn is_cached(&self, node: &NodeRow) -> bool {
        node.file_id
            .is_some_and(|fid| self.is_fresh(&self.dir.join(fid.to_string()), &node.etag))
    }

    fn store_file(&self, node: &NodeRow, src: &Path, etag: &str) -> Result<()> {
        let Some(file_id) = node.file_id else {
            return Ok(());
        };
        let blob = self.dir.join(file_id.to_string());
        if let Some(parent) = blob.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Same temp + rename publish as `store` — see there and `publish_blob`.
        let tmp = blob.with_extension("part");
        let staged = std::fs::copy(src, &tmp)
            .map_err(crate::Error::from)
            .and_then(|_| publish_blob(&tmp, &blob, etag));
        if let Err(e) = staged {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        self.enforce_budget(); // see `store` — writes are the only cache source now
        Ok(())
    }

    fn hydrate_to(&self, node: &NodeRow, dest: &Path) -> Result<()> {
        // Fast path: a fresh cached copy → a local file copy, no network, no RAM.
        if let Some(file_id) = node.file_id {
            let blob = self.dir.join(file_id.to_string());
            if self.is_fresh(&blob, &node.etag) {
                std::fs::copy(&blob, dest)?;
                return Ok(());
            }
        }
        // Otherwise stream it through `read`, bounding memory to one chunk and
        // never capping the size. Note that this is not necessarily cache-free:
        // a file smaller than `FETCH_CHUNK` is fetched by a single read from
        // offset 0, which opens a spill window that immediately completes and
        // publishes a blob (see `read_windowed`). That is welcome — the bytes
        // are on disk anyway — but it means the cache may be warm afterwards.
        // Larger files read in exact `FETCH_CHUNK` strides, which bypass the
        // window entirely and cache nothing. Either way the caller decides
        // whether to *deliberately* cache this base (see the write path, which
        // stores it for a later 3-way merge).
        let mut out = File::create(dest)?;
        if let Err(e) = stream_full(self, node, &mut out) {
            drop(out);
            let _ = std::fs::remove_file(dest); // never leave a truncated base
            return Err(e);
        }
        Ok(())
    }
}

/// Publish a fully written temp file as `blob` and record its `etag` sidecar —
/// the same temp + rename protocol as `fetch_whole`, shared by the write paths.
/// The stale sidecar is removed *first*: renaming new bytes in next to the old
/// sidecar would let a concurrent reader validate them against the old ETag
/// (a torn read), and a crash between rename and sidecar write would leave
/// that lie on disk permanently. Between removal and the final sidecar write
/// the blob merely reads as "not fresh", so readers fall back to the live
/// source — safe, just not cached.
fn publish_blob(tmp: &Path, blob: &Path, etag: &str) -> Result<()> {
    let _ = std::fs::remove_file(etag_path(blob));
    std::fs::rename(tmp, blob)?;
    std::fs::write(etag_path(blob), etag)?;
    Ok(())
}

/// Delete a blob and its ETag sidecar.
fn evict_blob(blob: &Path) {
    let _ = std::fs::remove_file(blob);
    let _ = std::fs::remove_file(etag_path(blob));
    tracing::debug!(?blob, "evicted from cache");
}

/// Update a blob's mtime to now, so eviction treats it as recently used.
fn touch(blob: &Path) {
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(blob) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
}

fn etag_path(blob: &Path) -> PathBuf {
    blob.with_extension("etag")
}

/// Sidecar marking a blob as pinned (kept offline, exempt from eviction).
fn pin_path(blob: &Path) -> PathBuf {
    blob.with_extension("pin")
}

fn read_range_from_file(path: &Path, offset: u64, len: u32) -> Result<Vec<u8>> {
    let mut f = File::open(path)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A source that counts how often it is asked to read.
    struct Counting {
        calls: Arc<AtomicUsize>,
        data: Vec<u8>,
    }
    impl ContentSource for Counting {
        fn read(&self, _node: &NodeRow, offset: u64, len: u32) -> Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let start = offset as usize;
            let end = std::cmp::min(start + len as usize, self.data.len());
            Ok(self.data[start.min(self.data.len())..end].to_vec())
        }
    }

    fn node(size: u64, etag: &str) -> NodeRow {
        NodeRow {
            inode: 2,
            parent: 1,
            name: "hello.txt".into(),
            path: "hello.txt".into(),
            is_dir: false,
            size,
            etag: etag.into(),
            mtime: 0,
            file_id: Some(42),
            permissions: String::new(),
        }
    }

    #[test]
    fn unpinned_read_serves_the_range_live_and_never_downloads_the_whole_file() {
        let dir = std::env::temp_dir().join(format!("wusel-cache-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Counting {
            calls: calls.clone(),
            data: b"hello".to_vec(),
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), None, None, None);
        let n = node(5, "etag-1");

        // A partial read must ask the live source for exactly that range once —
        // and must NOT write a whole-file blob (the amplification bug).
        assert_eq!(cache.read(&n, 1, 3).unwrap(), b"ell");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one live range read");
        assert!(
            !dir.join("42").exists(),
            "a read must never download/cache the whole file"
        );

        // A second read is live again — nothing was cached to hit.
        assert_eq!(cache.read(&n, 0, 5).unwrap(), b"hello");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "each unpinned read is live"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sequential_reads_escalate_to_chunks_and_cache_the_file() {
        let dir = std::env::temp_dir().join(format!("wusel-cache-seq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 600 KiB, read in kernel-sized 128 KiB steps like `cp` produces.
        let size: usize = 600 * 1024;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Counting {
            calls: calls.clone(),
            data: data.clone(),
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), None, None, None);
        let n = node(size as u64, "etag-1");

        let step: u32 = 128 * 1024;
        let mut got = Vec::new();
        while (got.len() as u64) < n.size {
            let chunk = cache.read(&n, got.len() as u64, step).unwrap();
            assert!(!chunk.is_empty(), "no premature EOF");
            got.extend(chunk);
        }
        assert_eq!(got, data, "sequential read returns the exact content");
        // Two live 128 KiB reads reach READAHEAD_AFTER; the third fetches the
        // whole rest as one chunk — 3 round-trips instead of 5.
        assert_eq!(calls.load(Ordering::SeqCst), 3, "readahead coalesces");

        // The completed run was published: a re-read is served locally.
        assert!(dir.join("42").exists(), "blob published after full pass");
        assert_eq!(cache.read(&n, 0, step).unwrap().len(), step as usize);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "second pass hits the cache"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn short_reads_never_publish_or_seed_truncated_copies() {
        let dir = std::env::temp_dir().join(format!("wusel-cache-short-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // The listing claims 10 bytes but the server delivers only 4 — a stale
        // size (the file shrank server-side after the last PROPFIND).
        let inner = Counting {
            calls: Arc::new(AtomicUsize::new(0)),
            data: b"hell".to_vec(),
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), None, None, None);
        let n = node(10, "etag-1");

        // Pinning (a full fetch) must fail loudly and publish nothing.
        let err = cache.pin_file(&n).unwrap_err().to_string();
        assert!(err.contains("short read"), "clear error, got: {err}");
        assert!(!dir.join("42").exists(), "no truncated blob published");
        assert!(!dir.join("42.part").exists(), "partial download cleaned up");

        // Hydrating a write-buffer base must fail and leave no file behind — a
        // truncated base would be uploaded as the full content on flush.
        let dest = dir.join("scratch-base");
        assert!(cache.hydrate_to(&n, &dest).is_err());
        assert!(!dest.exists(), "no truncated base left behind");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scattered_reads_stay_live_and_never_cache() {
        let dir = std::env::temp_dir().join(format!("wusel-cache-scat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let size: usize = 500 * 1024;
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Counting {
            calls: calls.clone(),
            data: vec![7u8; size],
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), None, None, None);
        let n = node(size as u64, "etag-1");

        // Header sniff, tail probe, header again — a thumbnailer's pattern.
        assert_eq!(cache.read(&n, 0, 4096).unwrap().len(), 4096);
        assert_eq!(cache.read(&n, 450 * 1024, 4096).unwrap().len(), 4096);
        assert_eq!(cache.read(&n, 0, 4096).unwrap().len(), 4096);
        // The foreground read path (hydration disabled here) serves each read
        // 1:1 live and never writes a blob itself; caching is the hydrator's job.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "each scattered read is live"
        );
        assert!(
            !dir.join("42").exists(),
            "the foreground read path must not download whole files"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_whole_populates_the_cache() {
        // The hydrator's fetch: pull a whole file through a source into an
        // atomically-published, ETag-validated blob (tested synchronously; the
        // background worker just calls this).
        let dir = std::env::temp_dir().join(format!("wusel-cache-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let size: usize = 300 * 1024;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let calls = Arc::new(AtomicUsize::new(0));
        let source = Counting {
            calls: calls.clone(),
            data: data.clone(),
        };
        let n = node(size as u64, "etag-1");

        download_whole(&source, &n, &dir, 42).unwrap();

        let blob = dir.join("42");
        assert_eq!(
            std::fs::read(&blob).unwrap(),
            data,
            "blob holds the whole file"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("42.etag")).unwrap(),
            "etag-1"
        );
        assert!(!dir.join("42.dl").exists(), "temp renamed away");
        assert!(blob_is_fresh(&blob, "etag-1"));
        assert!(!blob_is_fresh(&blob, "etag-2"), "stale ETag is not fresh");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Put a directory where a blob belongs, so any attempt to rename new bytes
    /// onto it fails — the cheapest way to exercise a publish that dies *after*
    /// the content is staged. Returns nothing; the caller knows the path.
    fn block_blob_path(blob: &Path) {
        let _ = std::fs::remove_file(blob);
        std::fs::create_dir_all(blob.join("in-the-way")).unwrap();
    }

    #[test]
    fn publish_blob_removes_the_stale_sidecar_before_renaming() {
        // The ordering contract itself: the old ETag must be gone before the new
        // bytes appear. With the rename made impossible, a "rename first, write
        // the sidecar after" implementation would leave the old ETag in place.
        let dir = std::env::temp_dir().join(format!("wusel-publish-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let blob = dir.join("42");
        std::fs::write(&blob, b"old").unwrap();
        std::fs::write(etag_path(&blob), "etag-old").unwrap();
        block_blob_path(&blob);

        let tmp = dir.join("42.part"); // deliberately absent → the rename fails
        assert!(
            publish_blob(&tmp, &blob, "etag-new").is_err(),
            "an impossible rename must be reported"
        );
        assert!(
            !etag_path(&blob).exists(),
            "the stale sidecar must be removed before the rename is attempted"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fetch_whole_publishes_through_the_atomic_helper() {
        let dir = std::env::temp_dir().join(format!("wusel-fetch-publish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // An older cached copy of the same file id, still carrying its old ETag.
        let blob = dir.join("42");
        std::fs::write(&blob, b"old").unwrap();
        std::fs::write(etag_path(&blob), "etag-old").unwrap();

        let data = vec![3u8; 1000];
        let inner = Counting {
            calls: Arc::new(AtomicUsize::new(0)),
            data: data.clone(),
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), None, None, None);
        let n = node(1000, "etag-new");

        // Pinning forces a full fetch: bytes and sidecar must agree afterwards.
        cache.pin_file(&n).unwrap();
        assert_eq!(std::fs::read(&blob).unwrap(), data, "new bytes published");
        assert_eq!(
            std::fs::read_to_string(etag_path(&blob)).unwrap(),
            "etag-new"
        );

        // Now let the publish fail after the download is staged. The stale ETag
        // must be gone all the same — surviving it would validate the *next*
        // publisher's bytes against a dead ETag.
        block_blob_path(&blob);
        std::fs::write(etag_path(&blob), "etag-old").unwrap();
        assert!(
            cache.pin_file(&node(1000, "etag-newer")).is_err(),
            "publishing onto a directory must fail"
        );
        assert!(
            !etag_path(&blob).exists(),
            "the stale sidecar must not survive a failed publish"
        );
        assert!(
            !dir.join("42.part").exists(),
            "the staged download must be cleaned up"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_whole_publishes_through_the_atomic_helper() {
        let dir = std::env::temp_dir().join(format!("wusel-dl-publish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let blob = dir.join("42");
        std::fs::write(&blob, b"old").unwrap();
        std::fs::write(etag_path(&blob), "etag-old").unwrap();

        let data = vec![5u8; 1000];
        let source = Counting {
            calls: Arc::new(AtomicUsize::new(0)),
            data: data.clone(),
        };
        download_whole(&source, &node(1000, "etag-new"), &dir, 42).unwrap();
        assert_eq!(std::fs::read(&blob).unwrap(), data, "new bytes published");
        assert_eq!(
            std::fs::read_to_string(etag_path(&blob)).unwrap(),
            "etag-new"
        );

        block_blob_path(&blob);
        std::fs::write(etag_path(&blob), "etag-old").unwrap();
        assert!(
            download_whole(&source, &node(1000, "etag-newer"), &dir, 42).is_err(),
            "publishing onto a directory must fail"
        );
        assert!(
            !etag_path(&blob).exists(),
            "the stale sidecar must not survive a failed publish"
        );
        assert!(
            !dir.join("42.dl").exists(),
            "the staged download must be cleaned up"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Read `n` end to end in kernel-sized steps — the `cp` pattern that makes a
    /// from-0 run spill to disk and publish itself as a cache blob.
    fn sequential_pass(cache: &CachingSource, n: &NodeRow) {
        let step: u32 = 128 * 1024;
        let mut got = 0u64;
        while got < n.size {
            let chunk = cache.read(n, got, step).unwrap();
            assert!(!chunk.is_empty(), "no premature EOF");
            got += chunk.len() as u64;
        }
    }

    #[test]
    fn publish_window_publishes_through_the_atomic_helper() {
        let dir = std::env::temp_dir().join(format!("wusel-window-publish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let blob = dir.join("42");
        std::fs::write(&blob, b"old").unwrap();
        std::fs::write(etag_path(&blob), "etag-old").unwrap();

        let size: usize = 600 * 1024; // past READAHEAD_AFTER, so the run escalates
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let inner = Counting {
            calls: Arc::new(AtomicUsize::new(0)),
            data: data.clone(),
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), None, None, None);
        let n = node(size as u64, "etag-new");

        sequential_pass(&cache, &n);
        assert_eq!(std::fs::read(&blob).unwrap(), data, "spill published");
        assert_eq!(
            std::fs::read_to_string(etag_path(&blob)).unwrap(),
            "etag-new"
        );

        block_blob_path(&blob);
        std::fs::write(etag_path(&blob), "etag-old").unwrap();
        sequential_pass(&cache, &node(size as u64, "etag-newer"));
        assert!(
            !etag_path(&blob).exists(),
            "the stale sidecar must not survive a failed publish"
        );
        assert!(
            spill_files(&dir).is_empty(),
            "a failed publish must not orphan the spill, leftovers: {:?}",
            spill_files(&dir)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_sidecar_write_does_not_destroy_the_published_blob() {
        // Rename succeeds, the sidecar write cannot (its path is a directory).
        // The blob at that point holds valid bytes — ours, or those of a
        // publisher that won a concurrent race. Deleting it throws away a
        // correct cache entry; without a sidecar it merely reads as "not fresh".
        let dir = std::env::temp_dir().join(format!("wusel-sidecar-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blob = dir.join("42");
        std::fs::create_dir_all(etag_path(&blob).join("in-the-way")).unwrap();

        let size: usize = 600 * 1024;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let inner = Counting {
            calls: Arc::new(AtomicUsize::new(0)),
            data: data.clone(),
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), None, None, None);

        sequential_pass(&cache, &node(size as u64, "etag-1"));

        assert!(blob.exists(), "a valid blob must not be deleted");
        assert_eq!(std::fs::read(&blob).unwrap(), data, "and stays intact");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_seen_tally_stays_bounded_when_many_files_are_only_peeked_at() {
        // A scanner touching a little of very many files: no file ever reaches
        // its hydration trigger, so no entry is ever removed. MAX_SEEN is the
        // documented backstop against the map growing without bound.
        let dir = std::env::temp_dir().join(format!("wusel-seen-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let size: usize = 100 * 1024; // trigger = the whole file; a 1 KiB peek is far below
        let counting = |data: Vec<u8>| Counting {
            calls: Arc::new(AtomicUsize::new(0)),
            data,
        };
        // The tally only runs when background hydration is configured.
        let hydrate = HydrationConfig {
            source: Box::new(counting(vec![1u8; size])),
            invalidations: None,
        };
        let cache = CachingSource::new(
            Box::new(counting(vec![1u8; size])),
            dir.clone(),
            None,
            None,
            Some(hydrate),
        );

        let mut n = node(size as u64, "etag-1");
        for fid in 1..=(MAX_SEEN as u64 + 2) {
            n.file_id = Some(fid);
            // Offset > 0: no spill file, so the read counts toward hydration.
            assert_eq!(cache.read(&n, 4096, 1024).unwrap().len(), 1024);
        }

        let len = cache.seen.lock().unwrap().len();
        assert!(
            len <= MAX_SEEN,
            "the read tally must stay bounded, holds {len} entries"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hydrate_trigger_separates_opens_from_peeks() {
        // Large file: the floor (not a fraction), so a single open — which reads
        // well past the floor — caches it, while a tiny sniff does not.
        let big = 4u64 * 1024 * 1024 * 1024; // 4 GiB
        assert_eq!(hydrate_trigger(big), HYDRATE_FLOOR);
        assert!(
            64 * 1024 < hydrate_trigger(big),
            "a 64 KiB sniff does not hydrate"
        );

        // Small file: only reading (nearly) all of it hydrates; a sniff does not.
        let small = 100 * 1024;
        assert_eq!(hydrate_trigger(small), small);
        assert!(
            4096 < hydrate_trigger(small),
            "a 4 KiB sniff of it does not"
        );

        // Mid file: capped at the floor.
        assert_eq!(hydrate_trigger(2 * 1024 * 1024), HYDRATE_FLOOR);
    }

    #[test]
    fn pinned_reads_come_from_cache() {
        let dir = std::env::temp_dir().join(format!("wusel-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Counting {
            calls: calls.clone(),
            data: b"hello".to_vec(),
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), None, None, None);
        let n = node(5, "etag-1");

        // Pinning is the explicit "keep offline" request that fills the cache.
        cache.pin_file(&n).unwrap();
        let after_pin = calls.load(Ordering::SeqCst);
        assert!(after_pin >= 1, "pinning fetches the file once");

        // Reads now come from disk, inner untouched.
        assert_eq!(cache.read(&n, 0, 5).unwrap(), b"hello");
        assert_eq!(cache.read(&n, 1, 3).unwrap(), b"ell");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            after_pin,
            "a pinned file is served from cache → no network"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn changed_etag_invalidates() {
        let dir = std::env::temp_dir().join(format!("wusel-cache-inval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Counting {
            calls: calls.clone(),
            data: b"world".to_vec(),
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), None, None, None);

        // A stored (written) blob is served from cache while its ETag matches.
        cache.store(&node(5, "etag-1"), b"world", "etag-1").unwrap();
        cache.read(&node(5, "etag-1"), 0, 5).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a fresh cached blob is served without the network"
        );
        // Same file, new ETag → the cached copy is stale, so read falls through live.
        cache.read(&node(5, "etag-2"), 0, 5).unwrap();
        assert!(
            calls.load(Ordering::SeqCst) > 0,
            "a changed ETag invalidates the cached copy"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lru_eviction_respects_size_budget() {
        use std::time::{Duration, SystemTime};
        let dir = std::env::temp_dir().join(format!("wusel-cache-evict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Budget 150 B, three 100 B blobs with staggered mtimes (1 oldest, 3 newest).
        let inner = Counting {
            calls: Arc::new(AtomicUsize::new(0)),
            data: Vec::new(),
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), Some(150), None, None);
        for (name, secs_ago) in [("1", 30u64), ("2", 20), ("3", 10)] {
            let blob = dir.join(name);
            std::fs::write(&blob, vec![0u8; 100]).unwrap();
            std::fs::write(etag_path(&blob), "e").unwrap();
            let f = std::fs::OpenOptions::new().write(true).open(&blob).unwrap();
            f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
                .unwrap();
        }

        cache.enforce_budget();

        // 300 B > 150 B → evict oldest first: 1 then 2 (200→100 ≤ 150), keep 3.
        assert!(!dir.join("1").exists(), "oldest blob must be evicted");
        assert!(!dir.join("1.etag").exists(), "its ETag sidecar goes too");
        assert!(
            !dir.join("2").exists(),
            "second-oldest blob must be evicted"
        );
        assert!(dir.join("3").exists(), "newest blob stays within budget");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_file_bypasses_cache() {
        let dir =
            std::env::temp_dir().join(format!("wusel-cache-oversized-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Counting {
            calls: calls.clone(),
            data: b"hello".to_vec(),
        };
        // Budget 4 bytes; the 5-byte file is over budget. Reads never cache in any
        // case, so it must certainly never be written here.
        let cache = CachingSource::new(Box::new(inner), dir.clone(), Some(4), None, None);
        let n = node(5, "etag-1");

        assert_eq!(cache.read(&n, 0, 5).unwrap(), b"hello");
        assert!(
            !dir.join("42").exists(),
            "an oversized file must not be written to the cache"
        );
        let after_first = calls.load(Ordering::SeqCst);

        // A later read is served live again — nothing was cached to hit.
        assert_eq!(cache.read(&n, 1, 3).unwrap(), b"ell");
        assert!(
            calls.load(Ordering::SeqCst) > after_first,
            "each read of an oversized file goes to the live source"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_id_bypasses_cache() {
        let dir = std::env::temp_dir().join(format!("wusel-cache-nofid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Counting {
            calls: calls.clone(),
            data: b"hello".to_vec(),
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), None, None, None);
        let mut n = node(5, "etag-1");
        n.file_id = None; // no stable key → must not be cached

        assert_eq!(cache.read(&n, 0, 5).unwrap(), b"hello");
        assert!(
            std::fs::read_dir(&dir).unwrap().next().is_none(),
            "a file without a file id must not be written to the cache"
        );
        let after_first = calls.load(Ordering::SeqCst);
        cache.read(&n, 0, 5).unwrap();
        assert!(
            calls.load(Ordering::SeqCst) > after_first,
            "each read goes to the live source"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pinned_blob_survives_eviction_even_when_oversized() {
        let dir = std::env::temp_dir().join(format!("wusel-cache-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Counting {
            calls: calls.clone(),
            data: b"hello".to_vec(),
        };
        // Budget 4 bytes; the pinned 5-byte file is over budget yet must be kept.
        let cache = CachingSource::new(Box::new(inner), dir.clone(), Some(4), None, None);

        cache.pin_file(&node(5, "etag-1")).unwrap();
        assert!(
            dir.join("42").exists(),
            "pinning caches the file despite the budget"
        );
        assert!(dir.join("42.pin").exists(), "pin marker written");

        // An unpinned, older, oversized blob that eviction should remove.
        std::fs::write(dir.join("99"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("99.etag"), "e").unwrap();

        cache.enforce_budget();
        assert!(dir.join("42").exists(), "pinned blob is never evicted");
        assert!(!dir.join("99").exists(), "unpinned blob is evicted");

        // Unpinning drops the marker, making it evictable again.
        cache.unpin_file(42);
        assert!(!dir.join("42.pin").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A source that serves a large virtual file (byte at position p = p % 251),
    /// without ever holding it whole — for exercising streaming hydration.
    struct Big {
        size: u64,
    }
    impl ContentSource for Big {
        fn read(&self, _node: &NodeRow, offset: u64, len: u32) -> Result<Vec<u8>> {
            let n = std::cmp::min(len as u64, self.size.saturating_sub(offset)) as usize;
            Ok((0..n).map(|i| ((offset + i as u64) % 251) as u8).collect())
        }
    }

    #[test]
    fn hydrate_to_streams_the_full_file_across_chunks() {
        let dir = std::env::temp_dir().join(format!("wusel-hydrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Larger than one fetch chunk, so the streaming loop runs several times.
        let size = (FETCH_CHUNK as u64) * 2 + 1234;
        let src = Big { size };
        let dest = dir.join("base");
        src.hydrate_to(&node(size, "e"), &dest).unwrap();

        let got = std::fs::read(&dest).unwrap();
        assert_eq!(got.len() as u64, size, "the whole file must be written");
        // Spot-check the pattern, including at and beyond a chunk boundary.
        assert_eq!(got[0], 0);
        assert_eq!(
            got[FETCH_CHUNK as usize],
            ((FETCH_CHUNK as u64) % 251) as u8
        );
        assert_eq!(got[size as usize - 1], (((size - 1) % 251) as u8));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A source that serves `ok_reads` reads and fails afterwards — for
    /// exercising the read path's error branches (a dropped connection mid-run).
    struct FailAfter {
        ok_reads: AtomicUsize,
        data: Vec<u8>,
    }
    impl ContentSource for FailAfter {
        fn read(&self, _node: &NodeRow, offset: u64, len: u32) -> Result<Vec<u8>> {
            if self.ok_reads.load(Ordering::SeqCst) == 0 {
                return Err(crate::Error::Other("connection reset".into()));
            }
            self.ok_reads.fetch_sub(1, Ordering::SeqCst);
            let start = (offset as usize).min(self.data.len());
            let end = std::cmp::min(start + len as usize, self.data.len());
            Ok(self.data[start..end].to_vec())
        }
    }

    /// The names of all readahead spill files left in `dir` — so a leak assertion
    /// can name the orphans instead of just failing.
    fn spill_files(dir: &Path) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        rd.flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "ra"))
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect()
    }

    #[test]
    fn a_failed_read_leaves_no_spill_file_behind() {
        let dir = std::env::temp_dir().join(format!("wusel-cache-spill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let size: usize = 4096;
        let inner = FailAfter {
            ok_reads: AtomicUsize::new(1), // the first read works, the next fails
            data: vec![9u8; size],
        };
        let cache = CachingSource::new(Box::new(inner), dir.clone(), None, None, None);
        let n = node(size as u64, "etag-1");

        // A run from byte 0 opens a spill file; the follow-up read fails, so the
        // window is dropped without ever completing.
        assert_eq!(cache.read(&n, 0, 512).unwrap().len(), 512);
        assert!(cache.read(&n, 512, 512).is_err(), "the second read fails");

        // Spill files carry an extension, so eviction never counts or removes
        // them: one orphan per failed attempt would grow the cache dir forever.
        assert!(
            spill_files(&dir).is_empty(),
            "the spill must be cleaned up on the error path, leftovers: {:?}",
            spill_files(&dir)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A source that is slow enough to keep concurrent readers overlapping.
    struct Slow {
        calls: Arc<AtomicUsize>,
        data: Vec<u8>,
    }
    impl ContentSource for Slow {
        fn read(&self, _node: &NodeRow, offset: u64, len: u32) -> Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(50));
            let start = (offset as usize).min(self.data.len());
            let end = std::cmp::min(start + len as usize, self.data.len());
            Ok(self.data[start..end].to_vec())
        }
    }

    #[test]
    fn single_flight_coalesces_concurrent_pins() {
        let dir = std::env::temp_dir().join(format!("wusel-cache-flight-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Slow {
            calls: calls.clone(),
            data: b"hello".to_vec(),
        };
        let cache = Arc::new(CachingSource::new(
            Box::new(inner),
            dir.clone(),
            None,
            None,
            None,
        ));
        let n = node(5, "etag-1");

        // Eight pins of the same cold file at once (pinning is the only read-side
        // path that downloads a whole file, so single-flight guards it).
        std::thread::scope(|s| {
            for _ in 0..8 {
                let cache = cache.clone();
                let n = n.clone();
                s.spawn(move || {
                    cache.pin_file(&n).unwrap();
                });
            }
        });

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "concurrent pins must share a single download"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
