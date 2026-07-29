// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The mount is released when `wusel_fuse::mount()` returns.
//!
//! `fuser` ties the unmount to dropping the `Mount`, which lives behind an
//! `Arc<Mutex<Option<Mount>>>` shared with every `SessionUnmounter` — dropping
//! the `Session` does *not* take it. Our signal-unmount thread holds such a
//! clone, so for as long as it lives the `Mount` cannot be dropped: the mount
//! would survive the call, and a session that ends by itself (a kernel
//! connection error) would leave a `Transport endpoint is not connected` stump
//! behind that only `fusermount3 -u` clears.
//!
//! The observable trace of that leak is the session's own `/dev/fuse`
//! descriptor: `Mount` owns it, so it stays open exactly as long as the `Mount`
//! does. This test therefore counts the process's `/dev/fuse` descriptors
//! around a full mount/unmount cycle.
//!
//! Needs `/dev/fuse`, so it runs only on Linux (the podman container).
#![cfg(target_os = "linux")]

mod common;

use std::path::Path;

/// How many of this process's descriptors point at `/dev/fuse`.
fn open_fuse_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("/proc/self/fd is readable")
        .flatten()
        .filter(|e| std::fs::read_link(e.path()).is_ok_and(|t| t == Path::new("/dev/fuse")))
        .count()
}

/// Is `path` currently a mountpoint according to the kernel?
fn is_mounted(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    text.lines()
        .filter_map(|line| line.split(' ').nth(4))
        .any(|mp| Path::new(mp) == path)
}

#[test]
fn the_mount_is_released_when_the_mount_call_returns() {
    let before = open_fuse_fds();

    let m = common::MountFixture::start("unmount");
    let mnt = m.mnt.clone();
    // Sanity: the fixture really is served through the kernel.
    assert!(m.fixture.join("Notes.txt").is_file(), "fixture tree exists");
    assert!(is_mounted(&mnt), "the mountpoint is in the mount table");
    assert!(
        open_fuse_fds() > before,
        "a live mount holds /dev/fuse open (before: {before})"
    );

    // Unmounts and joins the mount thread — i.e. `wusel_fuse::mount()` has
    // returned by the time this is done.
    drop(m);

    assert!(
        !is_mounted(&mnt),
        "the mountpoint is gone from the mount table"
    );
    // Poll rather than assert once: the kernel-notification thread holds a
    // second, duplicated descriptor and closes it when its channel disconnects,
    // a moment after `mount()` has returned. The session's own descriptor —
    // the one the leak keeps — never goes away on its own, so a bounded wait
    // still fails the leak.
    for _ in 0..100 {
        if open_fuse_fds() == before {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!(
        "a /dev/fuse descriptor is still open after mount() returned (before: {before}, \
         now: {}) — the Mount was never dropped, so nothing tears the mount down on an \
         exit path other than the signal one",
        open_fuse_fds()
    );
}
