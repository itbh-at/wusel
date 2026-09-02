// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Real FUSE mount end-to-end: `statfs` on the mount must eventually report the
//! server's *real* storage quota, not the fallback placeholder — the frontend
//! (`fs.rs`), the substrate (`Substrate::quota`) and the cache
//! (`QuotaCache`/`WebDavClient::quota`) all have their own unit/HTTP-level
//! coverage, but only a real kernel mount proves the whole chain is actually
//! wired together. Needs `/dev/fuse`, so it runs only on Linux (the podman
//! container); it is a no-op elsewhere.
#![cfg(target_os = "linux")]

mod common;

use std::ffi::CString;

/// The fixture `MountFixture::start` seeds: `Notes.txt` ("hello"),
/// `Sub Folder/deep.txt` ("nested") and the hidden `.Trash-1000/files/old.txt`
/// ("gone") — wusel-mock's `quota-used-bytes` mirrors the served tree's real
/// size, trash included, so this is *all* of it, not just what the mount shows.
const USED: u64 = 5 + 6 + 4;
/// wusel-mock's fixed `quota-available-bytes` (see `entry_xml` in wusel-mock).
const AVAILABLE: u64 = 1_000_000_000;
/// Matches `fs.rs`'s `statfs` block size.
const BSIZE: u64 = 512;

#[test]
fn statfs_reflects_the_servers_real_quota() {
    let m = common::MountFixture::start("statfs-quota");
    let cpath = CString::new(m.mnt.to_str().unwrap()).unwrap();

    let stat = || -> libc::statvfs {
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut st) };
        assert_eq!(rc, 0, "statvfs syscall failed");
        st
    };

    let expected_blocks = (USED + AVAILABLE).div_ceil(BSIZE);
    let expected_free = AVAILABLE.div_ceil(BSIZE);

    // The very first `statfs` after mount can still see the placeholder: the
    // real quota is fetched in the background (see `QuotaCache`) so it never
    // blocks the call that triggers it. Poll rather than assert on the first
    // reading.
    common::eventually("statfs settles on the server's real quota", || {
        stat().f_blocks == expected_blocks
    });

    let st = stat();
    assert_eq!(st.f_blocks, expected_blocks, "total = used + available");
    assert_eq!(st.f_bfree, expected_free);
    assert_eq!(st.f_bavail, expected_free);
    assert_eq!(st.f_bsize, BSIZE);

    // `m` drops here: unmount + cleanup.
}
