// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Nodes Nextcloud cannot store — symbolic links, hard links, FIFOs, sockets,
//! devices — are refused with `EPERM` through a real mount.
//!
//! `EPERM` is what `symlink(2)`, `link(2)` and `mknod(2)` name for "the
//! filesystem does not support this kind of node", and what tools test for:
//! `npm --no-bin-links` is the documented answer to it, and `git` probes
//! `symlink` once and falls back. `ENOSYS` ("function not implemented") reads
//! as a broken mount instead.
//!
//! Linux only, and needs `/dev/fuse` — it runs in the container.

mod common;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn errno_of(result: std::io::Result<()>) -> Option<i32> {
    result.err().and_then(|e| e.raw_os_error())
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
}

#[test]
fn nodes_the_server_cannot_store_are_refused_with_eperm() {
    let m = common::MountFixture::start("links");
    let notes = m.mnt.join("Notes.txt");
    let mut broken: Vec<String> = Vec::new();

    let symlink = errno_of(std::os::unix::fs::symlink("Notes.txt", m.mnt.join("sym")));
    if symlink != Some(libc::EPERM) {
        broken.push(format!("symlink: errno {symlink:?}, want EPERM"));
    }

    let hard = errno_of(std::fs::hard_link(&notes, m.mnt.join("hard")));
    if hard != Some(libc::EPERM) {
        broken.push(format!("link: errno {hard:?}, want EPERM"));
    }

    let fifo = unsafe { libc::mkfifo(cstr(&m.mnt.join("fifo")).as_ptr(), 0o644) };
    let fifo = (fifo != 0).then(|| std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    if fifo != Some(libc::EPERM) {
        broken.push(format!("mkfifo: errno {fifo:?}, want EPERM"));
    }

    // Nothing was left behind by any of them.
    for name in ["sym", "hard", "fifo"] {
        if m.mnt.join(name).symlink_metadata().is_ok() {
            broken.push(format!("{name} exists after being refused"));
        }
    }

    assert!(broken.is_empty(), "broken:\n  {}", broken.join("\n  "));
}
