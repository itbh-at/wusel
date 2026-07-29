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
///
/// Both directions **saturate**. The distance is a `Duration`, i.e. unsigned
/// and up to `u64::MAX` seconds, while our timestamp is an `i64`: a plain
/// `as i64` would wrap. An application can hand us exactly that through
/// `utimensat` with `tv_sec = i64::MIN` — the backwards distance is then 2^63
/// seconds, `as i64` wraps to `i64::MIN`, and negating *that* panics in a debug
/// build (and wraps silently in release). Clamping to the `i64` ends keeps the
/// timestamp as close to the request as it can be represented.
fn unix_from_system_time(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        // `-i64::MIN` has no positive counterpart, so the negation is done on
        // the checked value: anything past it clamps to `i64::MIN`.
        Err(e) => i64::try_from(e.duration().as_secs()).map_or(i64::MIN, |s| -s),
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

/// Where a `readdir` chunk starts in the listing.
///
/// FUSE carries the offset as `i64`, and as in `read`/`write` a negative one is
/// never valid. It has to be rejected explicitly: `offset as usize` would wrap
/// it into a huge number, `skip` would swallow the whole listing, and the reply
/// would be an empty — i.e. "end of directory" — answer to a bogus request.
fn dir_offset(offset: i64) -> Result<usize, i32> {
    if offset < 0 {
        Err(libc::EINVAL)
    } else {
        Ok(offset as usize)
    }
}

/// How many directory streams may keep a snapshot at the same time.
///
/// A snapshot is a full copy of one directory's entries, so N open handles on
/// the same huge directory cost N copies of its names — an amplification with
/// no natural bound, since a process may hold as many dirfds as its `RLIMIT_NOFILE`
/// allows. Past the cap a stream is simply not registered and `readdir` falls
/// back to a fresh listing per chunk (see there): correct, and what every
/// unknown handle already got, only without the intra-stream stability the
/// snapshot buys. Real workloads stay far below — a file manager, an indexer
/// and a shell together hold a handful of directories open, not sixty.
const MAX_DIR_STREAMS: usize = 64;

/// The listing served when a handle has no snapshot and none could be built —
/// unreachable by construction, and an empty listing (i.e. "end of directory")
/// is the harmless answer if it ever were reached.
const NO_ENTRIES: &[(u64, FileType, String)] = &[];

pub struct NcFs {
    provider: Provider,
    /// Expose the synthetic indexer-exclusion markers at the root (opt-in).
    markers: bool,
    /// Per-open-directory listing snapshots, keyed by the file handle we hand
    /// out in `opendir`. A directory stream arrives as several `readdir`
    /// chunks indexed by offset; recomputing the listing per chunk would let a
    /// background revalidation between two chunks shift the offsets and skip
    /// or duplicate entries. One snapshot per *traversal* keeps a stream
    /// internally consistent — it is taken when the stream starts (`readdir`
    /// at offset 0, see there) and reused for every continuation chunk.
    /// FUSE requests are served on a single thread (like everything else in
    /// this adapter), so a plain map needs no lock. Capped at
    /// [`MAX_DIR_STREAMS`] entries, so the copies cannot pile up unbounded.
    dir_streams: std::collections::HashMap<u64, Vec<(u64, FileType, String)>>,
    /// Last file handle handed out by `opendir`. Starts at 0 so the first
    /// handle is 1 — `fh == 0` stays free to mean "no registered stream"
    /// (readdir then falls back to a throwaway listing).
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

    /// Open a directory stream: hand out a fresh file handle and register the
    /// stream. The listing itself is *not* taken here — `readdir` takes it when
    /// the stream starts (offset 0), which is also where a `rewinddir` lands
    /// (see `readdir`). Snapshotting here as well would only build every
    /// listing twice per `ls`.
    ///
    /// Beyond [`MAX_DIR_STREAMS`] open streams the handle is still valid but
    /// stays unregistered: the open must not fail, it just loses the snapshot
    /// and is served like any unknown handle.
    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        self.next_dir_fh += 1;
        let fh = self.next_dir_fh;
        if self.dir_streams.len() < MAX_DIR_STREAMS {
            self.dir_streams.insert(fh, Vec::new());
        } else {
            tracing::debug!(
                open_streams = self.dir_streams.len(),
                "directory-stream cap reached — serving this handle without a snapshot"
            );
        }
        reply.opened(fh, 0);
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let start = match dir_offset(offset) {
            Ok(start) => start,
            Err(errno) => return reply.error(errno),
        };

        // A stream starting at offset 0 gets a fresh listing; its continuation
        // chunks (offset > 0) are served from that snapshot.
        //
        // Both halves matter. The snapshot keeps *one* traversal consistent: a
        // background revalidation landing between two chunks must not shift the
        // offsets and thereby skip or duplicate entries. Refreshing at offset 0
        // is what POSIX demands of `rewinddir` — the stream must then "refer to
        // the current state of the directory". glibc implements `rewinddir` as
        // `lseek(fd, 0, SEEK_SET)` on the *same* descriptor, so the kernel
        // re-issues READDIR at offset 0 with the same `fh` and never a second
        // OPENDIR. Without the refresh, a long-lived dirfd (a watcher, an
        // indexer, a file manager sitting in the directory) would keep serving
        // its opendir-time listing for the lifetime of that handle — blind to
        // every later addition, even after push invalidation fired.
        //
        // An unknown fh — 0 when no opendir preceded, or a handle past
        // [`MAX_DIR_STREAMS`] — gets a throwaway listing rather than an entry
        // in the map: for fh 0 no `releasedir` will ever arrive, so storing one
        // would leak, and past the cap storing is exactly what we are avoiding.
        let mut throwaway = None;
        if self.dir_streams.contains_key(&fh) {
            if start == 0 {
                match self.dir_entries(ino) {
                    Ok(entries) => {
                        self.dir_streams.insert(fh, entries);
                    }
                    Err(errno) => return reply.error(errno),
                }
            }
        } else {
            match self.dir_entries(ino) {
                Ok(entries) => throwaway = Some(entries),
                Err(errno) => return reply.error(errno),
            }
        }
        // `get`, never `self.dir_streams[&fh]`: indexing panics on a missing
        // key, and a panic in a FUSE callback takes the whole mount down. The
        // key is present by construction here — that is precisely why it must
        // not be enforced by a panic.
        let entries: &[(u64, FileType, String)] = match &throwaway {
            Some(entries) => entries,
            None => self.dir_streams.get(&fh).map_or(NO_ENTRIES, Vec::as_slice),
        };

        for (i, (inode, kind, name)) in entries.iter().enumerate().skip(start) {
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
/// `fuser` unmounts when the `Mount` is dropped, but the default action for
/// SIGINT/SIGTERM terminates the process before that runs, leaving a dangling
/// `ENOTCONN` mount that needs a manual `fusermount3 -u`. We therefore catch
/// both signals (the handler only flags a shutdown) and let a small thread turn
/// that flag into an unmount, which makes the blocking `Session::run` return so
/// [`Teardown`] can clean up the mountpoint.
///
/// The thread also stops once `finished` is set — the session ended on its own
/// and [`Teardown`] does the unmount. That matters for more than tidiness: a
/// `SessionUnmounter` holds a *clone* of the `Arc<Mutex<Option<Mount>>>`, so a
/// thread parked in this loop forever would keep the mount alive no matter what
/// the rest of the process does.
///
/// We deliberately do *not* use `MountOption::AutoUnmount`: `fuser` implements it
/// via a `fusermount` helper that forces `allow_other`, which in turn requires
/// `user_allow_other` in `/etc/fuse.conf` and would expose a personal cloud
/// mount to every local user. Owning the unmount ourselves avoids both.
fn unmount_on_signal(
    mut unmounter: fuser::SessionUnmounter,
    finished: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
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
                if finished.load(std::sync::atomic::Ordering::SeqCst) {
                    return; // the session loop returned — nothing left to unmount
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            tracing::info!("signal received — unmounting");
            // An error here (EINVAL/ENOENT) just means it was already unmounted,
            // e.g. via `fusermount3 -u`; nothing left to do.
            let _ = unmounter.unmount();
        })
        .expect("spawn fuse unmount thread");
}

/// Tears the kernel mount down when [`mount`] returns — on *every* path out of
/// it, which is the whole point of it being a `Drop` type.
///
/// `fuser` ties the unmount to dropping the `Mount`, which lives behind an
/// `Arc<Mutex<Option<Mount>>>` shared with every `SessionUnmounter`; dropping
/// the `Session` does **not** take it. So as long as the signal thread holds
/// its clone, nothing unmounts on its own — and a session that ends by itself
/// (`Session::run` returning an error: a kernel connection error, a failed read
/// from `/dev/fuse`) would leave the mountpoint attached as a
/// `Transport endpoint is not connected` stump. systemd then restarts the unit,
/// the new instance finds the stale mount, refuses, and three of those in 60 s
/// put the unit in `failed` — with the user left to run `fusermount3 -u` by
/// hand. Taking the `Mount` here, unconditionally, is what keeps a crash of the
/// session from becoming a broken desktop.
struct Teardown {
    unmounter: fuser::SessionUnmounter,
    finished: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Teardown {
    fn drop(&mut self) {
        // First let the signal thread go: it is only there to turn a signal
        // into an unmount, and we are already unmounting.
        self.finished
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // An error means it was already unmounted (the signal path got there
        // first, or someone ran `fusermount3 -u`) — nothing left to do.
        let _ = self.unmounter.unmount();
    }
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

    // Without AutoUnmount we own the teardown, twice over: `Teardown` drops the
    // kernel mount however `mount()` returns, and the signal thread turns a
    // SIGINT/SIGTERM into the same unmount while the session is still running.
    //
    // `SHUTDOWN` and the signal dispositions are process-global, so a *second*
    // mount in one process would otherwise start out already flagged. Clearing
    // it here, before the handlers go in, keeps sequential mounts working; two
    // *concurrent* mounts in one process would still share the flag, which the
    // CLI never does (one mount per daemon).
    SHUTDOWN.store(false, std::sync::atomic::Ordering::SeqCst);
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _teardown = Teardown {
        unmounter: session.unmount_callable(),
        finished: finished.clone(),
    };
    unmount_on_signal(session.unmount_callable(), finished);
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
    // `_teardown` drops after this returns — including on the `?` — so the
    // mountpoint is released whether the session ended by unmount or by error.
    session.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A negative offset must become EINVAL, not a silent "end of directory".
    /// The kernel never sends one, but the FUSE wire format allows it, and this
    /// is the same contract `read`/`write` already enforce. (The `readdir`
    /// callback itself needs a live mount to drive — the end-to-end coverage
    /// lives in `tests/rewinddir_e2e.rs`.)
    #[test]
    fn negative_readdir_offsets_are_rejected() {
        assert_eq!(dir_offset(-1), Err(libc::EINVAL));
        assert_eq!(dir_offset(i64::MIN), Err(libc::EINVAL));
    }

    #[test]
    fn non_negative_readdir_offsets_pass_through() {
        assert_eq!(dir_offset(0), Ok(0));
        assert_eq!(dir_offset(42), Ok(42));
    }

    /// The timestamp conversion must survive both ends of the `i64` range.
    /// `utimensat(…, tv_sec = i64::MIN)` reaches `setattr` as exactly this
    /// `SystemTime`; the backwards distance is then 2^63 seconds, which does
    /// not fit a positive `i64` — the old `-(secs as i64)` wrapped to
    /// `i64::MIN` and panicked on the negation in a debug build.
    #[test]
    fn extreme_timestamps_saturate_instead_of_overflowing() {
        assert_eq!(
            unix_from_system_time(system_time_from_unix(i64::MIN)),
            i64::MIN
        );
        assert_eq!(
            unix_from_system_time(system_time_from_unix(i64::MAX)),
            i64::MAX
        );
    }

    /// Ordinary and just-pre-epoch values still round-trip exactly — the
    /// saturation must not cost precision anywhere a real file lives.
    #[test]
    fn ordinary_timestamps_round_trip() {
        for secs in [0, 1, -1, 1_700_000_000, -2_208_988_800] {
            assert_eq!(unix_from_system_time(system_time_from_unix(secs)), secs);
        }
    }
}
