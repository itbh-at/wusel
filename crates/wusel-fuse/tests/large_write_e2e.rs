// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Reproduction: copy a large file into the mount the way a user does — open,
//! write it in many kernel-sized chunks, close — and it must reach the server
//! byte-for-byte. This exercises what the mock-level tests cannot: the kernel's
//! page-cache write-back, which can issue several `FUSE_WRITE`s for the same
//! inode concurrently to a multi-threaded mount, plus the asynchronous
//! chunked upload behind `flush`.
//!
//! Needs `/dev/fuse`, so it runs only on Linux (the podman container).
#![cfg(target_os = "linux")]

mod common;

use std::io::Write;

#[test]
fn a_large_file_copied_through_the_kernel_reaches_the_server() {
    let m = common::MountFixture::start("largewrite");

    // 20 MiB — several 4 MiB chunks — with position-dependent bytes so any
    // mis-ordered write shows up as a content mismatch, not just a size change.
    let total = 20 * 1024 * 1024usize;
    let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();

    // Write it as a copy would: a single stream in kernel-sized pieces, then
    // close (which flushes).
    let path = m.mnt.join("big.bin");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        let mut off = 0usize;
        while off < total {
            let end = (off + 128 * 1024).min(total);
            f.write_all(&data[off..end]).unwrap();
            off = end;
        }
        f.sync_all().unwrap();
    }

    // Read-your-writes from the mount: the size is correct immediately, served
    // from the local buffer even while the upload is still in flight.
    assert_eq!(
        std::fs::metadata(&path).unwrap().len() as usize,
        total,
        "the mount reports the wrong size for the just-written file"
    );

    // The upload is asynchronous; wait for the bytes to land on the server, then
    // check them byte-for-byte.
    common::eventually("big.bin reached the server intact", || {
        std::fs::read(m.fixture.join("big.bin"))
            .map(|b| b.len() == total && b == data)
            .unwrap_or(false)
    });
}
