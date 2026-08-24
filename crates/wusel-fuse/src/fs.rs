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
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    ReplyXattr, Request as Request_, TimeOrNow, WriteFlags,
};

use std::sync::Arc;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use wusel_core::provider::{Invalidation, Provider};
use wusel_core::runtime::{Pools, Substrate};
use wusel_core::state::NodeRow;
use wusel_fsm::{Intent, ObjectId, Request, RequestId};

use crate::dispatch::{spawn_pump, Pending, PumpContext, Replies};

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
pub(crate) const TTL: Duration = Duration::from_secs(1);
pub(crate) const GENERATION: Generation = Generation(0);

/// The single xattr we expose: a file's availability state (`online-only` /
/// `cached` / `pinned` / `modified`), read by file-manager extensions to draw
/// per-file emblems. See [`wusel_core::provider::FileState`].
pub(crate) const STATE_XATTR: &str = "user.wusel.state";

/// Reply to an xattr get/list following the kernel's two-call protocol: a
/// `size == 0` probe asks only for the length; a sized call copies the bytes if
/// they fit, else `ERANGE`.
pub(crate) fn reply_xattr(reply: ReplyXattr, value: &[u8], size: u32) {
    if size == 0 {
        reply.size(value.len() as u32);
    } else if (size as usize) < value.len() {
        reply.error(Errno::ERANGE);
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
pub(crate) const MARKERS: [(u64, &str); 2] =
    [(u64::MAX, ".trackerignore"), (u64::MAX - 1, ".nomedia")];

fn marker_name(ino: u64) -> Option<&'static str> {
    MARKERS.iter().find(|(i, _)| *i == ino).map(|&(_, n)| n)
}
fn marker_inode(name: &str) -> Option<u64> {
    MARKERS.iter().find(|(_, n)| *n == name).map(|&(i, _)| i)
}

/// Attributes of a synthetic marker: a 0-byte, read-only regular file.
fn marker_attr(ino: u64) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
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

/// Per-open-directory listing snapshots — pure *frontend* state, so it stays in
/// the adapter rather than behind the owner thread.
///
/// A directory stream arrives as several `readdir` chunks indexed by offset;
/// recomputing the listing per chunk would let a background revalidation between
/// two chunks shift the offsets and skip or duplicate entries. One snapshot per
/// *traversal* keeps a stream internally consistent — it is taken when the stream
/// starts (`readdir` at offset 0, see there) and reused for every continuation
/// chunk. Capped at [`MAX_DIR_STREAMS`] entries, so the copies cannot pile up.
///
/// `readdir`/`opendir`/`releasedir` mutate this through `&self`, so it needs
/// interior mutability: a small `Mutex`, held only for map lookups — never across
/// an owner round-trip or any I/O — so it stays uncontended even once Etappe 6
/// turns the dispatch threads up.
pub(crate) struct DirStreams {
    streams: std::collections::HashMap<u64, Vec<(u64, FileType, String)>>,
    /// Last file handle handed out by `opendir`. Starts at 0 so the first handle
    /// is 1 — `fh == 0` stays free to mean "no registered stream" (readdir then
    /// falls back to a throwaway listing).
    next_fh: u64,
}

impl DirStreams {
    /// Keep a listing for a stream's continuation chunks.
    pub(crate) fn remember(&mut self, fh: u64, entries: Vec<(u64, FileType, String)>) {
        // Only for handles we actually registered: past the cap, and for the
        // `fh == 0` case where no `releasedir` will ever arrive, storing would
        // leak exactly what the cap exists to prevent.
        if self.streams.contains_key(&fh) {
            self.streams.insert(fh, entries);
        }
    }

    /// The listing a continuation chunk should be served from.
    pub(crate) fn snapshot(&self, fh: u64) -> Option<&[(u64, FileType, String)]> {
        self.streams.get(&fh).map(Vec::as_slice)
    }
}

/// The FUSE adapter.
///
/// It carries no engine logic and no engine state. A callback is translated
/// into an [`Intent`], its reply is parked, and the dispatch thread returns —
/// everything after that happens on the substrate's threads. What remains here
/// is genuinely frontend-only: the directory-stream snapshots and the synthetic
/// marker files, neither of which the engine has ever heard of.
pub struct NcFs {
    substrate: Substrate,
    replies: Arc<Replies>,
    dirs: Arc<Mutex<DirStreams>>,
    /// Reads still outstanding, by the file handle that issued them.
    ///
    /// This is what makes cancellation possible at all. `fuser` exposes no
    /// interrupt callback, so a reader that dies can only be noticed through the
    /// descriptor teardown — and the kernel delivers `flush` for it immediately
    /// while holding `release` back until every outstanding read is answered. A
    /// `flush` on a handle that still has reads open therefore means nobody
    /// wants them any more.
    reads: Arc<Mutex<HashMap<u64, Vec<RequestId>>>>,
    /// Last handle handed out by `open`. Real handles, not a constant: without
    /// them there is no telling one reader's outstanding work from another's.
    next_fh: AtomicU64,
    /// Expose the synthetic indexer-exclusion markers at the root (opt-in).
    markers: bool,
}

impl NcFs {
    /// Lock the frontend directory-stream state, tolerating a poisoned mutex (a
    /// panicking callback must not wedge every later one).
    fn dirs(&self) -> std::sync::MutexGuard<'_, DirStreams> {
        self.dirs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Park a reply and hand the work to the machine.
    ///
    /// A submission that cannot be delivered answers straight away: a request
    /// nobody will ever complete is worse than an error, because the caller
    /// waits for ever with nothing to point at.
    fn go(&self, pending: Pending, object: u64, intent: Intent) -> Option<RequestId> {
        let id = self.replies.park(pending);
        let request = Request {
            id,
            object: ObjectId(object),
            intent,
        };
        if self.substrate.submit(request).is_err() {
            if let Some(p) = self.replies.take(id) {
                p.fail(Errno::EIO);
            }
            return None;
        }
        Some(id)
    }

    /// Give up whatever this handle still has in flight.
    ///
    /// Requests already answered are simply not found — the machine says so,
    /// rather than us keeping a second account of what is still live.
    fn abandon_reads(&self, fh: u64) {
        let ids = {
            let mut reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
            reads.get_mut(&fh).map(std::mem::take).unwrap_or_default()
        };
        for id in ids {
            let _ = self.substrate.abandon(id);
        }
    }

    /// The same, for a write — whose bytes travel beside the request.
    fn go_write(&self, pending: Pending, object: u64, intent: Intent, data: Vec<u8>) {
        let id = self.replies.park(pending);
        let request = Request {
            id,
            object: ObjectId(object),
            intent,
        };
        if self.substrate.submit_write(request, data).is_err() {
            if let Some(p) = self.replies.take(id) {
                p.fail(Errno::EIO);
            }
        }
    }
}

/// A freedesktop "top directory" trash name — `.Trash` or `.Trash-<uid>`. Such a
/// directory at the mount root is where a file manager would move files "to the
/// wastebasket". We refuse to host one so that deletion goes straight to the
/// server (and Nextcloud's own trash), instead of a `.Trash-<uid>` folder
/// appearing in the user's cloud and syncing to every device. See the desktop
/// integration docs.
pub(crate) fn is_trash_name(name: &str) -> bool {
    name == ".Trash" || name.starts_with(".Trash-")
}

/// Append one chunk of a directory listing to the reply, starting at `start`.
pub(crate) fn serve_chunk(
    entries: &[(u64, FileType, String)],
    start: usize,
    reply: &mut ReplyDirectory,
) {
    for (i, (inode, kind, name)) in entries.iter().enumerate().skip(start) {
        // `offset` is the *next* entry to read; hence i + 1.
        if reply.add(INodeNo(*inode), (i + 1) as u64, *kind, name) {
            break; // reply buffer full
        }
    }
}

impl Filesystem for NcFs {
    fn getattr(&self, _req: &Request_, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino = ino.0;
        if self.markers && marker_name(ino).is_some() {
            return reply.attr(&TTL, &marker_attr(ino));
        }
        self.go(Pending::Attr(reply), ino, Intent::Stat);
    }

    fn lookup(&self, _req: &Request_, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            return reply.error(Errno::EINVAL);
        };
        if self.markers && parent.0 == wusel_core::state::ROOT_INODE {
            if let Some(ino) = marker_inode(name) {
                return reply.entry(&TTL, &marker_attr(ino), GENERATION);
            }
        }
        // A trash directory at the root is hidden from listings; make a direct
        // lookup agree, so a file manager cannot stat or traverse into a
        // pre-existing `.Trash-<uid>` either. Together they make it absent.
        if parent.0 == wusel_core::state::ROOT_INODE && is_trash_name(name) {
            return reply.error(Errno::ENOENT);
        }
        self.go(
            Pending::Entry(reply),
            parent.0,
            Intent::Lookup {
                name: name.to_string(),
            },
        );
    }

    /// Open a directory stream: hand out a fresh handle and register it. The
    /// listing itself is taken by `readdir` when the stream starts, which is
    /// also where a `rewinddir` lands — snapshotting here as well would build
    /// every listing twice per `ls`.
    fn opendir(&self, _req: &Request_, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let mut dirs = self.dirs();
        dirs.next_fh += 1;
        let fh = dirs.next_fh;
        if dirs.streams.len() < MAX_DIR_STREAMS {
            dirs.streams.insert(fh, Vec::new());
        } else {
            tracing::debug!(
                open_streams = dirs.streams.len(),
                "directory-stream cap reached — serving this handle without a snapshot"
            );
        }
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request_,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        // A continuation chunk is served from the snapshot this stream started
        // with. That is the whole point of taking one: a background refresh
        // between two chunks must not shift the offsets and thereby skip or
        // duplicate entries.
        if start > 0 {
            let dirs = self.dirs();
            if let Some(entries) = dirs.snapshot(fh.0) {
                let entries = entries.to_vec();
                drop(dirs);
                serve_chunk(&entries, start, &mut reply);
                return reply.ok();
            }
        }
        // Offset 0 is where a stream begins *and* where `rewinddir` lands, so
        // it takes a fresh listing — without which a long-lived directory
        // handle would go blind to everything that arrives later.
        self.go(
            Pending::Dir {
                reply,
                ino: ino.0,
                fh: fh.0,
                start,
            },
            ino.0,
            Intent::Enumerate,
        );
    }

    /// Close a directory stream: drop its snapshot.
    fn releasedir(
        &self,
        _req: &Request_,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.dirs().streams.remove(&fh.0);
        reply.ok();
    }

    /// Nothing to set up per open, and nothing to ask anybody: answering here
    /// keeps the machine out of the one callback that has no work to do.
    fn open(&self, _req: &Request_, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request_,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let ino = ino.0;
        if self.markers && marker_name(ino).is_some() {
            return reply.data(&[]); // synthetic markers are always empty
        }
        // Remembered against the handle that issued it: a `flush` on that handle
        // is how a dead reader announces itself, and this is the list it kills.
        if let Some(id) = self.go(
            Pending::Data(reply),
            ino,
            Intent::Fetch { offset, len: size },
        ) {
            self.reads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(fh.0)
                .or_default()
                .push(id);
        }
    }

    /// Filesystem statistics. We are a virtual, server-backed filesystem, so we
    /// advertise a large capacity with plenty free — otherwise applications that
    /// check available space before saving would see zero and refuse.
    fn statfs(&self, _req: &Request_, _ino: INodeNo, reply: ReplyStatfs) {
        const BSIZE: u32 = 512;
        const TOTAL_BLOCKS: u64 = 1 << 41; // ~1 PiB at 512-byte blocks
        const TOTAL_INODES: u64 = 1 << 32;
        reply.statfs(
            TOTAL_BLOCKS,
            TOTAL_BLOCKS,
            TOTAL_BLOCKS,
            TOTAL_INODES,
            TOTAL_INODES,
            BSIZE,
            255,
            BSIZE,
        );
    }

    fn getxattr(&self, _req: &Request_, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        if name != OsStr::new(STATE_XATTR) {
            return reply.error(Errno::ENODATA);
        }
        let ino = ino.0;
        if self.markers && marker_name(ino).is_some() {
            // A fabrication has no availability state to report — the same
            // answer a real, unpinned directory gets.
            return reply.error(Errno::ENODATA);
        }
        self.go(Pending::Xattr { reply, size }, ino, Intent::State);
    }

    fn listxattr(&self, _req: &Request_, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let ino = ino.0;
        if self.markers && marker_name(ino).is_some() {
            return reply_xattr(reply, &[], size);
        }
        self.go(Pending::XattrList { reply, size }, ino, Intent::State);
    }

    fn write(
        &self,
        _req: &Request_,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let len = data.len() as u32;
        self.go_write(
            Pending::Written(reply),
            ino.0,
            Intent::Write { offset, len },
            data.to_vec(),
        );
    }

    fn create(
        &self,
        _req: &Request_,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name) = name.to_str() else {
            return reply.error(Errno::EINVAL);
        };
        if parent.0 == wusel_core::state::ROOT_INODE && is_trash_name(name) {
            return reply.error(Errno::EACCES);
        }
        self.go(
            Pending::Created(reply),
            parent.0,
            Intent::Materialise {
                name: name.to_string(),
                dir: false,
            },
        );
    }

    fn mkdir(
        &self,
        _req: &Request_,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            return reply.error(Errno::EINVAL);
        };
        // Refuse the file manager's attempt to create a `.Trash-<uid>` at the
        // mount root: no top-directory trash here means "move to trash" falls
        // back to a real delete (which lands in Nextcloud's own trash).
        if parent.0 == wusel_core::state::ROOT_INODE && is_trash_name(name) {
            return reply.error(Errno::EACCES);
        }
        self.go(
            Pending::Entry(reply),
            parent.0,
            Intent::Materialise {
                name: name.to_string(),
                dir: true,
            },
        );
    }

    fn unlink(&self, _req: &Request_, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, reply);
    }

    fn rmdir(&self, _req: &Request_, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, reply);
    }

    fn rename(
        &self,
        _req: &Request_,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            return reply.error(Errno::EINVAL);
        };
        // Do not let a rename create a top-directory trash at the root either.
        if newparent.0 == wusel_core::state::ROOT_INODE && is_trash_name(newname) {
            return reply.error(Errno::EACCES);
        }
        self.go(
            Pending::Empty(reply),
            parent.0,
            Intent::Move {
                from_name: name.to_string(),
                to_parent: ObjectId(newparent.0),
                to_name: newname.to_string(),
            },
        );
    }

    fn setattr(
        &self,
        _req: &Request_,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // Signed conversion: a pre-epoch timestamp (`touch -t 196912…`, archive
        // tools restoring old mtimes) must stay negative rather than collapse
        // to 1970-01-01.
        let mtime = mtime.map(|t| match t {
            TimeOrNow::SpecificTime(t) => unix_from_system_time(t),
            TimeOrNow::Now => unix_from_system_time(SystemTime::now()),
        });
        self.go(Pending::Attr(reply), ino.0, Intent::SetAttr { size, mtime });
    }

    fn flush(
        &self,
        _req: &Request_,
        ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // Measured, not assumed: the kernel delivers this for a killed reader
        // while its reads are still open. Whatever this handle still has running
        // has nobody waiting for it — and over a metered link that is the user's
        // bandwidth, not merely tidiness.
        self.abandon_reads(fh.0);
        self.go(Pending::Empty(reply), ino.0, Intent::Publish);
    }

    fn fsync(
        &self,
        _req: &Request_,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.go(Pending::Empty(reply), ino.0, Intent::Publish);
    }

    fn release(
        &self,
        _req: &Request_,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // The handle is finished with, so nothing it started can still be wanted.
        self.abandon_reads(fh.0);
        self.reads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&fh.0);
        self.go(Pending::Empty(reply), ino.0, Intent::Publish);
    }
}

impl NcFs {
    /// Shared body for `unlink` and `rmdir` — one operation, so one script.
    fn remove(&self, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            return reply.error(Errno::EINVAL);
        };
        self.go(
            Pending::Empty(reply),
            parent.0,
            Intent::Remove {
                name: name.to_string(),
            },
        );
    }
}

/// Build FUSE attributes from a state row.
pub(crate) fn to_attr(node: &NodeRow) -> FileAttr {
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
        ino: INodeNo(node.inode),
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

    // fuser 0.18 takes a structured `Config` instead of a `&[MountOption]`. It is
    // `#[non_exhaustive]`, so it must be built from `default()` and mutated, not a
    // struct literal.
    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("wusel".into())];
    // NOTE: `x-gvfs-notrash` is deliberately NOT passed here. fuser puts a
    // `CUSTOM` option into the kernel mount-data string, and the FUSE kernel
    // rejects the unknown option — the mount then never comes up (proved by the
    // atomic-save mount-e2e). `x-gvfs-notrash` is a userspace mount option and
    // must reach GIO via fstab / utab, not the kernel; that is an install-time
    // integration, tracked separately. The filesystem-side refusal of trash
    // directories (see `is_trash_name` / `is_trash_path`) is the desktop-agnostic
    // mechanism and needs no mount option.
    // Concurrency (Etappe 6): run this many FUSE dispatch threads, so independent
    // operations are served in parallel. Left at 1 by default (single-threaded,
    // the pre-concurrency behaviour); a value > 1 is a deliberate config choice.
    // The engine runtime is already sized to match (see `Provider::new`), and all
    // shared frontend state is behind locks / the owner thread, so this is safe to
    // turn up. `n_threads` is only set above 1 — leaving it unset keeps fuser's
    // own single-threaded default.
    let dispatch_threads = provider.dispatch_threads();
    if dispatch_threads > 1 {
        config.n_threads = Some(dispatch_threads);
    }

    // Take the syncer's kernel-invalidation stream before moving the provider in;
    // a background thread turns each event into a FUSE notification so a file
    // manager sitting in a directory sees server-side add/removes without a manual
    // refresh. We use `Session` (not `mount`) so we can get its `Notifier`.
    let invalidations = provider.take_invalidations();
    let markers = provider.exclude_from_indexers();
    // Capture what the drain thread needs before the provider moves into the
    // session: the desktop backend (to push per-file emblem refreshes) and the
    // mountpoint (to turn a remote path into the on-disk path the desktop knows).
    let desktop = provider.desktop();
    let mount_root = mountpoint.to_path_buf();
    // The substrate: the deciding thread, the database readers and writer, and
    // the network and file pools. The Provider hands over its own parts rather
    // than having us assemble them from pieces we would have to be told about.
    let ctx = provider.substrate_context();
    let pools = Pools {
        db_readers: dispatch_threads.max(2),
        net: dispatch_threads.max(4),
        file: dispatch_threads.max(2),
    };
    let (substrate, answers) = Substrate::start(&ctx, pools)?;

    let replies = Arc::new(Replies::new());

    // Serve the engine's internal state on a per-user socket, so `wusel doctor`
    // can read what the mount is doing — the stuck flow, the parked replies —
    // without a debugger. Best-effort and name-free; a socket that will not bind
    // never stops the mount. Take the handle now, before the substrate moves
    // into the session below. `_diag_socket` lives to the end of `mount`, which
    // removes the socket file when the mount ends.
    let _diag_socket = crate::diag::DiagSocket::bind(
        wusel_core::config::diag_socket_for_mount(mountpoint),
        substrate.diag_handle(),
        Arc::clone(&replies),
    );
    let dirs = Arc::new(Mutex::new(DirStreams {
        streams: std::collections::HashMap::new(),
        next_fh: 0,
    }));
    // The reply pump runs on its own thread and not on the deciding one:
    // `reply.data()` writes to /dev/fuse, which is I/O, and the decider does
    // none by construction.
    let _pump = spawn_pump(
        answers,
        Arc::clone(&replies),
        PumpContext {
            dirs: Arc::clone(&dirs),
            markers,
        },
    );

    // The Provider is no longer on the request path — the substrate is — but it
    // still owns the background syncer and the revalidator, so it has to
    // outlive the session rather than be dropped here.
    let _engine = provider;

    let fs = NcFs {
        substrate,
        replies,
        dirs,
        reads: Arc::new(Mutex::new(HashMap::new())),
        next_fh: AtomicU64::new(0),
        markers,
    };
    let mut session = fuser::Session::new(fs, mountpoint, &config)?;

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
        // The kernel notifier is deliberately not taken here: its two calls are
        // disabled below (see the note). Re-add `session.notifier()` when they
        // come back.
        std::thread::Builder::new()
            .name("wusel-fuse-inval".into())
            .spawn(move || {
                while let Ok(inv) = rx.recv() {
                    // NOTE (under investigation): the kernel-cache invalidations
                    // below — `notify_inval_entry` / `notify_inval_inode` — are
                    // the one thing this mount does that an ordinary filesystem
                    // never does, and they are the remaining suspect for a
                    // reported freeze: navigating out of a directory and back
                    // leaves Nautilus showing a blank view, with the daemon never
                    // asked to re-read. A busy shared server fires these
                    // constantly (every push → sync walk), so they land while the
                    // user is navigating.
                    //
                    // They are disabled here to confirm that and to give a usable
                    // build. What is lost is only *live* appearance of add/removes
                    // in an already-open window — the kernel picks them up on its
                    // own one-second attribute/entry TTL, and on the next reload.
                    // The desktop emblem refresh (`file_changed`) is kept: it goes
                    // through the file manager's own extension, not the kernel, so
                    // it cannot cause this.
                    match inv {
                        Invalidation::Entry { path, .. } => {
                            desktop.file_changed(&mount_root.join(&path).to_string_lossy());
                        }
                        Invalidation::Content { path, .. } => {
                            desktop.file_changed(&mount_root.join(&path).to_string_lossy());
                        }
                    }
                }
            })
            .expect("spawn fuse invalidation thread");
    }
    // `_teardown` drops after this returns — including on the `?` — so the
    // mountpoint is released whether the session ended by unmount or by error.
    // `run` consumes the session, so `NcFs` drops when it returns, which stops
    // the substrate: its threads are joined by that drop, and the reply pump
    // ends when the answer channel closes with them.
    session.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // (The old `dir_offset` negative-rejection tests are gone with the helper:
    // fuser 0.18 types the readdir offset as `u64`, so a negative value is now
    // impossible at the type level rather than rejected at runtime. The readdir
    // end-to-end coverage lives in `tests/rewinddir_e2e.rs`.)

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
