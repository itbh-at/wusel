// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Real FUSE mount end-to-end: mount `wusel` (engine + FUSE) against the
//! in-process `wusel-mock` WebDAV server and drive it through the kernel — `ls`,
//! `cat`, `stat`, `statfs`, plus writing (create, overwrite, mkdir, rename,
//! unlink) — then unmount. Needs `/dev/fuse`, so it runs only on Linux (the
//! podman container); it is a no-op elsewhere.
#![cfg(target_os = "linux")]

mod common;

use std::ffi::CString;

#[test]
fn mount_lists_reads_and_reports_statfs() {
    let m = common::MountFixture::start("e2e");
    let (mnt, fixture) = (m.mnt.clone(), m.fixture.clone());

    // ls: the tree is visible.
    let names: Vec<_> = std::fs::read_dir(&mnt)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"Notes.txt".to_string()),
        "root lists Notes.txt"
    );
    assert!(
        names.contains(&"Sub Folder".to_string()),
        "root lists the subdir"
    );

    // stat + cat: metadata and content come through the kernel.
    assert_eq!(std::fs::metadata(mnt.join("Notes.txt")).unwrap().len(), 5);
    assert_eq!(std::fs::read(mnt.join("Notes.txt")).unwrap(), b"hello");
    assert_eq!(
        std::fs::read(mnt.join("Sub Folder/deep.txt")).unwrap(),
        b"nested"
    );

    // statfs: df must see a non-empty, non-full filesystem (so apps do not balk).
    let cpath = CString::new(mnt.to_str().unwrap()).unwrap();
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut st) };
    assert_eq!(rc, 0, "statvfs syscall failed");
    assert!(st.f_blocks > 0, "statfs reports zero total blocks");
    assert!(st.f_bavail > 0, "statfs reports no free space");

    // --- writing through the kernel ---

    // Create a new file: the write reaches the server (asynchronously) and reads
    // back from the mount immediately (read-your-writes, served from the buffer).
    std::fs::write(mnt.join("new.txt"), b"created").unwrap();
    assert_eq!(std::fs::read(mnt.join("new.txt")).unwrap(), b"created");
    common::eventually("new.txt on the server", || {
        std::fs::read(fixture.join("new.txt")).unwrap_or_default() == b"created"
    });

    // Overwrite an existing file (open O_TRUNC → write → flush).
    std::fs::write(mnt.join("Notes.txt"), b"OVERWRITTEN").unwrap();
    common::eventually("Notes.txt overwritten on the server", || {
        std::fs::read(fixture.join("Notes.txt")).unwrap_or_default() == b"OVERWRITTEN"
    });

    // mkdir (created on the server synchronously), then rename and unlink. The
    // rename's server effect follows the upload it moves, so it is waited for.
    std::fs::create_dir(mnt.join("NewDir")).unwrap();
    assert!(fixture.join("NewDir").is_dir());
    std::fs::rename(mnt.join("new.txt"), mnt.join("renamed.txt")).unwrap();
    common::eventually("rename reflected on the server", || {
        fixture.join("renamed.txt").is_file() && !fixture.join("new.txt").exists()
    });
    std::fs::remove_file(mnt.join("renamed.txt")).unwrap();
    common::eventually("unlink reflected on the server", || {
        !fixture.join("renamed.txt").exists()
    });

    // `m` drops here: unmount + cleanup.
}
