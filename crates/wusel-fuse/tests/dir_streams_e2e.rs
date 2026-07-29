// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Many concurrent directory handles over a real FUSE mount.
//!
//! `opendir` keeps a listing snapshot per open stream, but only up to
//! `MAX_DIR_STREAMS` of them — otherwise N handles on one huge directory cost N
//! copies of its names. Beyond the cap a handle is served without a snapshot,
//! from a listing built per chunk. That fallback must be as correct as the
//! snapshot path: every one of these streams still lists the whole directory,
//! and no handle — registered or not — may make `readdir`/`releasedir` panic.
//!
//! Needs `/dev/fuse`, so it runs only on Linux (the podman container).
#![cfg(target_os = "linux")]

mod common;

use std::ffi::{CStr, CString};

/// Drain an open directory stream into its entry names. (A twin of the helper
/// in `rewinddir_e2e.rs`: each test binary compiles `common` on its own, so a
/// shared copy there would be dead code in the binaries that do not read
/// directories by hand.)
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
        names.push(
            CStr::from_ptr((*ent).d_name.as_ptr())
                .to_string_lossy()
                .into_owned(),
        );
    }
}

#[test]
fn more_open_handles_than_the_snapshot_cap_all_list_the_directory() {
    let m = common::MountFixture::start("dirstreams");
    assert!(m.fixture.join("Notes.txt").is_file(), "fixture tree exists");
    let cpath = CString::new(m.mnt.to_str().unwrap()).unwrap();

    // Comfortably more than MAX_DIR_STREAMS (64), all open at once, so the
    // later ones are served without a snapshot.
    const HANDLES: usize = 80;
    let mut dirs = Vec::with_capacity(HANDLES);
    for i in 0..HANDLES {
        let dirp = unsafe { libc::opendir(cpath.as_ptr()) };
        assert!(!dirp.is_null(), "opendir #{i} on the mount failed");
        dirs.push(dirp);
    }

    // Start every stream before draining any: that is what makes them
    // concurrent traversals rather than a sequence of open/close pairs.
    for &dirp in &dirs {
        assert!(!unsafe { libc::readdir(dirp) }.is_null(), "first entry");
    }

    for (i, dirp) in dirs.into_iter().enumerate() {
        // The first entry is already consumed above, so this is the remainder.
        let names = unsafe { read_all(dirp) };
        unsafe { libc::closedir(dirp) };
        assert!(
            names.iter().any(|n| n == "Notes.txt"),
            "handle #{i} lost an entry: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "Sub Folder"),
            "handle #{i} lost the subdirectory: {names:?}"
        );
    }
}
