// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! FUSE filesystem: a thin adapter that translates kernel callbacks into
//! [`wusel_core::provider::Provider`] calls. It carries no engine logic of its own —
//! the Provider owns the state, the WebDAV client and the sync↔async bridge.
//!
//! Reads (list/stat/read) and writes (write/create/mkdir/unlink/rmdir/rename/
//! truncate, upload on flush) all delegate to the Provider.

use std::ffi::OsStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
};

use wusel_core::provider::{Invalidation, Provider};
use wusel_core::state::NodeRow;

/// Map an engine error to an errno for the kernel.
fn errno(e: &wusel_core::Error) -> i32 {
    match e {
        wusel_core::Error::Denied => libc::EACCES,
        // The object vanished on the server — the network-filesystem idiom for
        // "this handle no longer refers to anything".
        wusel_core::Error::NotFound => libc::ESTALE,
        _ => libc::EIO,
    }
}

/// Unix seconds → `SystemTime`, both signs. `SystemTime` has no signed
/// constructor, but it does represent pre-1970 instants as `UNIX_EPOCH -
/// duration` — so a file dated before 1970 (rare, but servers do report them)
/// keeps its real timestamp instead of collapsing to the epoch.
fn system_time_from_unix(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        // `unsigned_abs` handles i64::MIN, whose plain `abs()` would overflow.
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

/// `SystemTime` → unix seconds, both signs — the inverse of
/// [`system_time_from_unix`]. `duration_since(UNIX_EPOCH)` errs for pre-epoch
/// instants; the error carries the backwards distance, which is our negative
/// timestamp.
fn unix_from_system_time(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// How long the kernel may cache our attribute/entry answers. Short for now;
/// active invalidation on remote changes comes later (see architecture docs).
const TTL: Duration = Duration::from_secs(1);
const GENERATION: u64 = 0;

/// The single xattr we expose: a file's availability state (`online-only` /
/// `cached` / `pinned` / `modified`), read by file-manager extensions to draw
/// per-file emblems. See [`wusel_core::provider::FileState`].
const STATE_XATTR: &str = "user.wusel.state";

/// Reply to an xattr get/list following the kernel's two-call protocol: a
/// `size == 0` probe asks only for the length; a sized call copies the bytes if
/// they fit, else `ERANGE`.
fn reply_xattr(reply: ReplyXattr, value: &[u8], size: u32) {
    if size == 0 {
        reply.size(value.len() as u32);
    } else if (size as usize) < value.len() {
        reply.error(libc::ERANGE);
    } else {
        reply.data(value);
    }
}

/// Synthetic, local-only marker files exposed at the mount root when
/// `exclude_from_indexers` is on. GNOME Tracker/LocalSearch skips any directory
/// containing one of these — and its whole subtree — so the desktop file
/// indexer never walks the mount. They are pure FUSE fabrications: never in the
/// state, never uploaded to Nextcloud. The reserved inodes sit far above any
/// real SQLite rowid, so they never collide. (KDE Baloo ignores markers; it
/// needs a config exclude instead — a later addition.)
const MARKERS: [(u64, &str); 2] = [(u64::MAX, ".trackerignore"), (u64::MAX - 1, ".nomedia")];

fn marker_name(ino: u64) -> Option<&'static str> {
    MARKERS.iter().find(|(i, _)| *i == ino).map(|&(_, n)| n)
}
fn marker_inode(name: &str) -> Option<u64> {
    MARKERS.iter().find(|(_, n)| *n == name).map(|&(i, _)| i)
}

/// Attributes of a synthetic marker: a 0-byte, read-only regular file.
fn marker_attr(ino: u64) -> FileAttr {
    FileAttr {
        ino,
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::RegularFile,
        perm: 0o444,
        nlink: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

pub struct NcFs {
    provider: Provider,
    /// Expose the synthetic indexer-exclusion markers at the root (opt-in).
    markers: bool,
    /// Per-open-directory listing snapshots, keyed by the file handle we hand
    /// out in `opendir`. A directory stream arrives as several `readdir`
    /// chunks indexed by offset; recomputing the listing per chunk would let a
    /// background revalidation between two chunks shift the offsets and skip
    /// or duplicate entries. Snapshotting once per `opendir` keeps one stream
    /// internally consistent. FUSE requests are served on a single thread
    /// (like everything else in this adapter), so a plain map needs no lock.
    dir_streams: std::collections::HashMap<u64, Vec<(u64, FileType, String)>>,
    /// Last file handle handed out by `opendir`. Starts at 0 so the first
    /// handle is 1 — `fh == 0` stays free to mean "no opendir snapshot"
    /// (readdir then falls back to a fresh listing).
    next_dir_fh: u64,
}

impl Filesystem for NcFs {
    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        if self.markers && marker_name(ino).is_some() {
            return reply.attr(&TTL, &marker_attr(ino));
        }
        match self.provider.node(ino) {
            Ok(Some(node)) => reply.attr(&TTL, &to_attr(&node)),
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => {
                tracing::error!(%e, ino, "getattr failed");
                reply.error(libc::EIO);
            }
        }
    }

    fn lookup(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            return reply.error(libc::EINVAL);
        };
        if self.markers && parent == wusel_core::state::ROOT_INODE {
            if let Some(ino) = marker_inode(name) {
                return reply.entry(&TTL, &marker_attr(ino), GENERATION);
            }
        }
        match self.provider.lookup(parent, name) {
            Ok(Some(child)) => reply.entry(&TTL, &to_attr(&child), GENERATION),
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => {
                tracing::error!(%e, parent, name, "lookup failed");
                reply.error(errno(&e));
            }
        }
    }

    /// Open a directory stream: snapshot the listing once and key it by a fresh
    /// file handle, so every `readdir` chunk of this stream serves from the
    /// same listing (see the `dir_streams` field for the why).
    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.dir_entries(ino) {
            Ok(entries) => {
                self.next_dir_fh += 1;
                let fh = self.next_dir_fh;
                self.dir_streams.insert(fh, entries);
                reply.opened(fh, 0);
            }
            Err(errno) => reply.error(errno),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        // Serve from the snapshot taken in `opendir`. An unknown fh (e.g. 0,
        // when no opendir preceded) falls back to a fresh listing — correct for
        // that one chunk, just without the cross-chunk stability guarantee.
        let fresh = if self.dir_streams.contains_key(&fh) {
            None
        } else {
            match self.dir_entries(ino) {
                Ok(entries) => Some(entries),
                Err(errno) => return reply.error(errno),
            }
        };
        let entries = fresh.as_ref().unwrap_or_else(|| &self.dir_streams[&fh]);

        for (i, (inode, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            // `offset` is the *next* entry to read; hence i + 1.
            if reply.add(*inode, (i + 1) as i64, *kind, name) {
                break; // reply buffer full
            }
        }
        reply.ok();
    }

    /// Close a directory stream: drop its `opendir` snapshot.
    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        self.dir_streams.remove(&fh);
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        // Nothing to set up per open — but this is the one low-frequency spot
        // that narrates *which* files applications touch: cache-served reads
        // never hit the network and would otherwise be invisible in the log.
        if let Ok(Some(node)) = self.provider.node(ino) {
            tracing::debug!(path = %node.path, "open");
        }
        reply.opened(0, 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if self.markers && marker_name(ino).is_some() {
            return reply.data(&[]); // synthetic markers are always empty
        }
        // FUSE carries the offset as i64, but a negative offset is never valid
        // for a regular file — reject it instead of silently acting on 0.
        if offset < 0 {
            return reply.error(libc::EINVAL);
        }
        match self.provider.read(ino, offset as u64, size) {
            Ok(bytes) => reply.data(&bytes),
            // A file deleted on the server is expected, not an error: log quietly
            // and return a stale-handle errno so the reader stops.
            Err(e @ wusel_core::Error::NotFound) => {
                tracing::debug!(ino, offset, size, "read: file gone on the server");
                reply.error(errno(&e));
            }
            Err(e) => {
                tracing::error!(%e, ino, offset, size, "read failed");
                reply.error(errno(&e));
            }
        }
    }

    /// Filesystem statistics (`statfs`/`df`). We are a virtual, server-backed FS,
    /// so we advertise a large capacity with plenty free — otherwise apps that
    /// check available space before opening/saving would see zero and refuse.
    /// (Reporting the real Nextcloud quota is a later refinement.)
    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        const BSIZE: u32 = 512;
        const TOTAL_BLOCKS: u64 = 1 << 41; // ~1 PiB at 512-byte blocks
        const TOTAL_INODES: u64 = 1 << 32;
        reply.statfs(
            TOTAL_BLOCKS, // blocks
            TOTAL_BLOCKS, // bfree
            TOTAL_BLOCKS, // bavail
            TOTAL_INODES, // files
            TOTAL_INODES, // ffree
            BSIZE,        // bsize
            255,          // namelen
            BSIZE,        // frsize
        );
    }

    /// Serve our one xattr, `user.wusel.state`, so file-manager extensions can
    /// draw per-file emblems. `ENODATA` for any other name, or when the inode has
    /// no state (an unpinned directory). Network-free — safe for the storms of
    /// getxattr a file manager issues while drawing a folder.
    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        if name != OsStr::new(STATE_XATTR) {
            return reply.error(libc::ENODATA);
        }
        match self.provider.file_state(ino) {
            Ok(Some(state)) => reply_xattr(reply, state.as_xattr().as_bytes(), size),
            Ok(None) => reply.error(libc::ENODATA),
            Err(e) => {
                tracing::error!(%e, ino, "getxattr: file_state failed");
                reply.error(libc::EIO);
            }
        }
    }

    /// List the xattrs of `ino`: just `user.wusel.state`, and only when the
    /// inode actually has a state (so `getfattr -d` on an unpinned directory
    /// shows nothing rather than an empty-valued attribute).
    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: ReplyXattr) {
        let mut list = Vec::new();
        if matches!(self.provider.file_state(ino), Ok(Some(_))) {
            list.extend_from_slice(STATE_XATTR.as_bytes());
            list.push(0); // the kernel expects a NUL-separated, NUL-terminated list
        }
        reply_xattr(reply, &list, size);
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        // As in `read`: a negative offset is invalid, not "offset 0".
        if offset < 0 {
            return reply.error(libc::EINVAL);
        }
        match self.provider.write(ino, offset as u64, data) {
            Ok(written) => reply.written(written),
            Err(e) => {
                tracing::error!(%e, ino, "write failed");
                reply.error(errno(&e));
            }
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name) = name.to_str() else {
            return reply.error(libc::EINVAL);
        };
        match self.provider.create(parent, name) {
            Ok(node) => reply.created(&TTL, &to_attr(&node), GENERATION, 0, 0),
            Err(e) => {
                tracing::error!(%e, parent, name, "create failed");
                reply.error(errno(&e));
            }
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            return reply.error(libc::EINVAL);
        };
        match self.provider.mkdir(parent, name) {
            Ok(node) => reply.entry(&TTL, &to_attr(&node), GENERATION),
            Err(e) => {
                tracing::error!(%e, parent, name, "mkdir failed");
                reply.error(errno(&e));
            }
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, reply);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, reply);
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            return reply.error(libc::EINVAL);
        };
        match self.provider.rename(parent, name, newparent, newname) {
            Ok(()) => reply.ok(),
            Err(e) => {
                tracing::error!(%e, parent, name, "rename failed");
                reply.error(errno(&e));
            }
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // We honour truncation (`size`) and mtime (propagated on the next
        // upload as X-OC-Mtime); other attrs are accepted as no-ops.
        if let Some(size) = size {
            if let Err(e) = self.provider.truncate(ino, size) {
                tracing::error!(%e, ino, size, "truncate failed");
                return reply.error(errno(&e));
            }
        }
        if let Some(mtime) = mtime {
            // Signed conversion: a pre-epoch timestamp (`touch -t 196912...`,
            // archive tools restoring old mtimes) must stay negative rather
            // than collapse to 0 = 1970-01-01.
            let secs = match mtime {
                TimeOrNow::SpecificTime(t) => unix_from_system_time(t),
                TimeOrNow::Now => unix_from_system_time(SystemTime::now()),
            };
            if let Err(e) = self.provider.set_mtime(ino, secs) {
                tracing::error!(%e, ino, "set mtime failed");
                return reply.error(errno(&e));
            }
        }
        match self.provider.node(ino) {
            Ok(Some(node)) => reply.attr(&TTL, &to_attr(&node)),
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        self.upload(ino, reply);
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.upload(ino, reply);
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.upload(ino, reply);
    }
}

impl NcFs {
    /// Build the full listing for one directory stream: `.` and `..`, the
    /// provider's children, plus the synthetic root markers. Called once per
    /// `opendir` (and as the `readdir` fallback for an unknown fh); errors map
    /// straight to the errno the caller replies with.
    fn dir_entries(&mut self, ino: u64) -> Result<Vec<(u64, FileType, String)>, i32> {
        // The directory's own inode + parent, for `.` and `..`.
        let node = match self.provider.node(ino) {
            Ok(Some(n)) if n.is_dir => n,
            Ok(Some(_)) => return Err(libc::ENOTDIR),
            Ok(None) => return Err(libc::ENOENT),
            Err(e) => {
                tracing::error!(%e, ino, "readdir: node lookup failed");
                return Err(errno(&e));
            }
        };
        let children = match self.provider.list_dir(ino) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(%e, path = %node.path, "readdir failed");
                return Err(errno(&e));
            }
        };
        // One log line per listing (built once per stream, not per readdir
        // chunk). Cached listings produce no PROPFIND line, so this is their
        // only trace.
        tracing::debug!(path = %node.path, entries = children.len(), "readdir");

        let mut entries: Vec<(u64, FileType, String)> = Vec::with_capacity(children.len() + 2);
        entries.push((ino, FileType::Directory, ".".to_string()));
        entries.push((node.parent, FileType::Directory, "..".to_string()));
        for c in children {
            let kind = if c.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            entries.push((c.inode, kind, c.name));
        }
        // Synthetic indexer-exclusion markers, root only (a root marker excludes
        // the whole subtree). Never in the listing from the server.
        if self.markers && ino == wusel_core::state::ROOT_INODE {
            for (mino, mname) in MARKERS {
                entries.push((mino, FileType::RegularFile, mname.to_string()));
            }
        }
        Ok(entries)
    }

    /// Shared body for `unlink`/`rmdir`.
    fn remove(&mut self, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            return reply.error(libc::EINVAL);
        };
        match self.provider.remove(parent, name) {
            Ok(()) => reply.ok(),
            Err(e) => {
                tracing::error!(%e, parent, name, "remove failed");
                reply.error(errno(&e));
            }
        }
    }

    /// Shared body for `flush`/`fsync`/`release`: upload if dirty.
    fn upload(&mut self, ino: u64, reply: ReplyEmpty) {
        match self.provider.flush(ino) {
            Ok(()) => reply.ok(),
            Err(e) => {
                tracing::error!(%e, ino, "flush failed");
                reply.error(errno(&e));
            }
        }
    }
}

/// Build FUSE attributes from a state row.
fn to_attr(node: &NodeRow) -> FileAttr {
    let t = system_time_from_unix(node.mtime);
    // Reflect the server's permissions: drop the write bits when the entry is not
    // writable (a read-only share or group folder), so the mode matches what the
    // Provider will actually allow on write.
    let writable = node.is_writable();
    let (kind, perm, nlink) = match (node.is_dir, writable) {
        (true, true) => (FileType::Directory, 0o755, 2),
        (true, false) => (FileType::Directory, 0o555, 2),
        (false, true) => (FileType::RegularFile, 0o644, 1),
        (false, false) => (FileType::RegularFile, 0o444, 1),
    };
    FileAttr {
        ino: node.inode,
        size: node.size,
        blocks: node.size.div_ceil(512),
        atime: t,
        mtime: t,
        ctime: t,
        crtime: t,
        kind,
        perm,
        nlink,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// Set by the SIGINT/SIGTERM handler; the unmount thread polls it. An
/// `AtomicBool` store is one of the few async-signal-safe operations, so the
/// handler itself does nothing else.
static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Unmount cleanly on Ctrl-C / `systemctl --user stop`.
///
/// `fuser` unmounts when the `Session` is dropped, but the default action for
/// SIGINT/SIGTERM terminates the process before that runs, leaving a dangling
/// `ENOTCONN` mount that needs a manual `fusermount3 -u`. We therefore catch
/// both signals (the handler only flags a shutdown) and let a small thread turn
/// that flag into an unmount, which makes the blocking `Session::run` return so
/// `Session::drop` can clean up the mountpoint.
///
/// We deliberately do *not* use `MountOption::AutoUnmount`: `fuser` implements it
/// via a `fusermount` helper that forces `allow_other`, which in turn requires
/// `user_allow_other` in `/etc/fuse.conf` and would expose a personal cloud
/// mount to every local user. Owning the unmount ourselves avoids both.
fn unmount_on_signal(mut unmounter: fuser::SessionUnmounter) {
    // SAFETY: `on_signal` only stores to an atomic — async-signal-safe. glibc's
    // `signal` re-arms the handler and restarts syscalls (BSD semantics), which
    // is fine: we drive the unmount from the thread below, not from EINTR.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    std::thread::Builder::new()
        .name("wusel-fuse-unmount".into())
        .spawn(move || {
            while !SHUTDOWN.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(200));
            }
            tracing::info!("signal received — unmounting");
            // An error here (EINVAL/ENOENT) just means it was already unmounted,
            // e.g. via `fusermount3 -u`; nothing left to do.
            let _ = unmounter.unmount();
        })
        .expect("spawn fuse unmount thread");
}

/// Mounts the filesystem at `mountpoint` (blocks until unmount).
pub fn mount(mountpoint: &std::path::Path, mut provider: Provider) -> anyhow::Result<()> {
    tracing::info!(mountpoint = %mountpoint.display(), "mounting wusel");

    let options = vec![MountOption::FSName("wusel".into())];

    // Take the syncer's kernel-invalidation stream before moving the provider in;
    // a background thread turns each event into a FUSE notification so a file
    // manager sitting in a directory sees server-side add/removes without a manual
    // refresh. We use `Session` (not `mount2`) so we can get its `Notifier`.
    let invalidations = provider.take_invalidations();
    let markers = provider.exclude_from_indexers();
    // Capture what the drain thread needs before the provider moves into the
    // session: the desktop backend (to push per-file emblem refreshes) and the
    // mountpoint (to turn a remote path into the on-disk path the desktop knows).
    let desktop = provider.desktop();
    let mount_root = mountpoint.to_path_buf();
    let fs = NcFs {
        provider,
        markers,
        dir_streams: std::collections::HashMap::new(),
        next_dir_fh: 0,
    };
    let mut session = fuser::Session::new(fs, mountpoint, &options)?;

    // Without AutoUnmount we own the teardown: unmount on SIGINT/SIGTERM.
    unmount_on_signal(session.unmount_callable());
    if let Some(rx) = invalidations {
        let notifier = session.notifier();
        std::thread::Builder::new()
            .name("wusel-fuse-inval".into())
            .spawn(move || {
                while let Ok(inv) = rx.recv() {
                    match inv {
                        Invalidation::Entry { parent, name, path } => {
                            // Drop the kernel's cached dentry so add/removes show;
                            // ENOENT just means nothing was cached, which is fine.
                            let _ = notifier.inval_entry(parent, OsStr::new(&name));
                            // Push the on-disk path to the desktop so a file
                            // manager re-reads this file's emblem live.
                            desktop.file_changed(&mount_root.join(&path).to_string_lossy());
                        }
                    }
                }
            })
            .expect("spawn fuse invalidation thread");
    }
    session.run()?;
    Ok(())
}
