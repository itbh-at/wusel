// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A measurement, not a feature test: **when a process blocked in `read()` is
//! killed, does the kernel deliver `FLUSH`/`RELEASE` while our read reply is
//! still outstanding?**
//!
//! Why it matters. `fuser` exposes no `interrupt` callback (its `Filesystem`
//! trait has 41 methods and none of them is it; the low-level parser exists but
//! answers `ENOSYS`), so we never see `FUSE_INTERRUPT`. If a reader gives up —
//! the user hits Ctrl-C, the process is killed — the only way we could learn of
//! it is the file-descriptor teardown, i.e. `flush` and `release`. Whether that
//! arrives *while the read is still running* decides whether cancelling an
//! in-flight transfer is possible at all without patching `fuser`. On a
//! throttled or metered link that is the difference between stopping a
//! pointless 2 GB download and paying for it. See the _Cancellation_ section of
//! the Concurrency design page.
//!
//! The probe deliberately does **not** use wusel. The question is about `fuser`
//! and the kernel, so a minimal filesystem isolates it. Its `read` replies from
//! another thread and returns immediately — the shape wusel's read path takes
//! once reads are offloaded. That shape is essential: with today's inline,
//! blocking `read` the dispatch loop is *inside* the call, so no other callback
//! could arrive anyway and the measurement would answer a different question.

#![cfg(target_os = "linux")]

use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyEmpty, ReplyEntry, ReplyOpen,
    Request,
};

/// How long the probe holds a read before answering. Long enough that the kill
/// and the inspection both happen comfortably inside the window.
const SLOW_READ: Duration = Duration::from_secs(6);
/// When the reader is killed, measured from the start of the read.
const KILL_AFTER: Duration = Duration::from_secs(1);
/// When the log is inspected — before the reply lands, which is the whole point.
const INSPECT_AFTER: Duration = Duration::from_secs(3);

const FILE_INO: u64 = 2;
const FILE_SIZE: u64 = 64 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(1);

/// Callback log, timestamped relative to the mount.
#[derive(Clone)]
struct Log {
    t0: Instant,
    entries: Arc<Mutex<Vec<(Duration, String)>>>,
}

impl Log {
    fn new() -> Self {
        Self {
            t0: Instant::now(),
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn note(&self, what: impl Into<String>) {
        let at = self.t0.elapsed();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((at, what.into()));
    }
    fn snapshot(&self) -> Vec<(Duration, String)> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn saw(&self, what: &str) -> bool {
        self.snapshot().iter().any(|(_, w)| w.starts_with(what))
    }
}

fn attr(ino: u64, kind: FileType, size: u64) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size,
        blocks: size.div_ceil(512),
        atime: SystemTime::UNIX_EPOCH,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
        kind,
        perm: if kind == FileType::Directory {
            0o755
        } else {
            0o644
        },
        nlink: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// One file, `big`, whose reads are answered late and from another thread.
struct ProbeFs {
    log: Log,
}

impl Filesystem for ProbeFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if parent.0 == 1 && name == "big" {
            reply.entry(
                &TTL,
                &attr(FILE_INO, FileType::RegularFile, FILE_SIZE),
                Generation(0),
            );
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match ino.0 {
            1 => reply.attr(&TTL, &attr(1, FileType::Directory, 0)),
            FILE_INO => reply.attr(&TTL, &attr(FILE_INO, FileType::RegularFile, FILE_SIZE)),
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.log.note("open");
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.log
            .note(format!("read start (offset {offset}, {size} B)"));
        let log = self.log.clone();
        // The reply object is `Send` and carries its own request id, so it can
        // be answered later, from anywhere, out of order. Returning now frees
        // the dispatch loop — exactly what makes the question meaningful.
        std::thread::spawn(move || {
            std::thread::sleep(SLOW_READ);
            log.note("read reply sent");
            reply.data(&vec![7u8; size as usize]);
        });
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lo: LockOwner,
        reply: ReplyEmpty,
    ) {
        self.log.note("flush");
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.log.note("release");
        reply.ok();
    }
}

#[test]
fn does_the_kernel_report_a_killed_reader_while_the_read_is_outstanding() {
    let mnt = std::env::temp_dir().join(format!("wusel-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&mnt);
    std::fs::create_dir_all(&mnt).unwrap();

    let log = Log::new();
    // 0.18 takes a structured `Config`; it is `#[non_exhaustive]`, so it must be
    // built from `default()` and mutated rather than written as a literal.
    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("wusel-probe".into())];
    let session = fuser::spawn_mount(ProbeFs { log: log.clone() }, &mnt, &config)
        .expect("mount the probe filesystem (needs /dev/fuse — run in the container)");

    // Wait until the mount answers, so the timings below start from a live mount.
    let mut ready = false;
    for _ in 0..50 {
        if std::fs::metadata(mnt.join("big")).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(ready, "the probe mount never came up");

    // A reader that will block in read() for SLOW_READ.
    let mut reader = std::process::Command::new("cat")
        .arg(mnt.join("big"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the reader");

    std::thread::sleep(KILL_AFTER);
    assert!(
        log.saw("read start"),
        "the reader never reached a read — nothing to measure"
    );

    reader.kill().expect("kill the reader");
    let _ = reader.wait();

    // Inspect *before* the reply lands: anything present now arrived while the
    // read was still outstanding.
    std::thread::sleep(INSPECT_AFTER - KILL_AFTER);
    let during = log.snapshot();
    let flush_during = log.saw("flush");
    let release_during = log.saw("release");

    // Let the reply land, then look again for contrast.
    std::thread::sleep(SLOW_READ);
    let after = log.snapshot();

    drop(session);
    let _ = std::fs::remove_dir_all(&mnt);

    eprintln!("\n=== callbacks while the read was still outstanding ===");
    for (at, what) in &during {
        eprintln!("  {:>6} ms  {what}", at.as_millis());
    }
    eprintln!("=== full log, after the reply ===");
    for (at, what) in &after {
        eprintln!("  {:>6} ms  {what}", at.as_millis());
    }
    eprintln!(
        "\nRESULT: with a killed reader and a read still in flight, \
         flush={flush_during}, release={release_during}\n\
         → cancelling an in-flight transfer via the descriptor teardown is {}.\n",
        if flush_during || release_during {
            "POSSIBLE"
        } else {
            "NOT possible — only a real FUSE interrupt would reach it"
        }
    );

    assert!(
        after.iter().any(|(_, w)| w.starts_with("read start")),
        "no read was recorded at all"
    );

    // Measured on Linux 6.x with fuser 0.14: `flush` arrives the moment the
    // reader dies, while the reads are still outstanding — `release` does not,
    // the kernel holds it back until every pending request has been answered.
    //
    // These two assertions are a regression guard, not a specification. The
    // cancellation design rests on exactly this: hook `flush`, never `release`.
    // If one of them fails, the kernel or fuser changed and the design needs
    // revisiting — the test is not wrong, the assumption is.
    assert!(
        flush_during,
        "flush no longer arrives while a read is outstanding — cancelling an \
         abandoned transfer through the descriptor teardown would stop working"
    );
    assert!(
        !release_during,
        "release now arrives while a read is outstanding — the design may hook \
         it after all, which would be simpler; revisit the Cancellation section"
    );
}
