// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! POSIX `rewinddir` semantics over a real FUSE mount.
//!
//! `rewinddir()` must make the stream "refer to the current state of the
//! directory" (POSIX). glibc implements it as `lseek(fd, 0, SEEK_SET)` on the
//! *same* descriptor — no second `opendir` — so the kernel re-issues
//! `FUSE_READDIR` with `offset == 0` and the same file handle. A long-lived
//! dirfd (a watcher, an indexer, a file manager holding a directory open) that
//! polls with `rewinddir` must therefore see entries that appeared after its
//! `opendir`.
//!
//! Needs `/dev/fuse`, so it runs only on Linux (the podman container).
#![cfg(target_os = "linux")]

mod common;

use std::ffi::{CStr, CString};

/// Drain an open directory stream into its entry names (glibc `readdir` until
/// it returns NULL = end of directory).
///
/// # Safety
/// `dirp` must be a live stream from `opendir`.
unsafe fn read_all(dirp: *mut libc::DIR) -> Vec<String> {
    let mut names = Vec::new();
    loop {
        let ent = libc::readdir(dirp);
        if ent.is_null() {
            return names;
        }
        let name = CStr::from_ptr((*ent).d_name.as_ptr());
        names.push(name.to_string_lossy().into_owned());
    }
}

#[test]
fn rewinddir_sees_an_entry_added_after_opendir() {
    let m = common::MountFixture::start("rewinddir");

    // A long-lived directory handle, as a watcher or indexer would hold it.
    let cpath = CString::new(m.mnt.to_str().unwrap()).unwrap();
    let dirp = unsafe { libc::opendir(cpath.as_ptr()) };
    assert!(!dirp.is_null(), "opendir on the mount failed");

    let first = unsafe { read_all(dirp) };
    assert!(
        first.iter().any(|n| n == "Notes.txt"),
        "the first pass lists the fixture: {first:?}"
    );
    assert!(
        !first.iter().any(|n| n == "added.txt"),
        "the new entry must not exist yet: {first:?}"
    );

    // Make a new entry appear in that directory. We create it through the mount
    // (a second descriptor on the same filesystem) rather than in the server
    // fixture: that updates the engine's state synchronously, so the test does
    // not depend on the directory-revalidation TTL. What is under test is the
    // *open handle* — it must not keep serving its `opendir`-time snapshot,
    // whichever side the change came from.
    std::fs::write(m.mnt.join("added.txt"), b"new").unwrap();
    assert!(
        m.fixture.join("added.txt").exists(),
        "the new entry reached the server"
    );

    unsafe { libc::rewinddir(dirp) };
    let second = unsafe { read_all(dirp) };
    unsafe { libc::closedir(dirp) };

    assert!(
        second.iter().any(|n| n == "added.txt"),
        "rewinddir must expose the current directory state, but the stream still \
         served its opendir snapshot: {second:?}"
    );
    assert!(
        second.iter().any(|n| n == "Notes.txt"),
        "the refreshed listing still holds the old entries: {second:?}"
    );
}
