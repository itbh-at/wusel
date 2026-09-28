// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! What the mount does with the attributes Nextcloud cannot store — owners,
//! extended attributes, locks — through a real mount.
//!
//! None of these is implemented by Wusel; the kernel's handling of what a FUSE
//! filesystem leaves out is the behaviour. This pins that behaviour down,
//! because the reference page ("File attributes in the mount") promises it and
//! tools rely on it: `cp -a` and `rsync -a` try all of them.
//!
//! Linux only, and needs `/dev/fuse` — it runs in the container.

mod common;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[test]
fn owners_xattrs_and_locks_behave_as_documented() {
    let m = common::MountFixture::start("attributes");
    let notes = m.mnt.join("Notes.txt");
    let c = CString::new(notes.as_os_str().as_bytes()).unwrap();

    // chown: accepted, and the owner stays the user running the mount.
    let before = std::fs::metadata(&notes).unwrap();
    let rc = unsafe { libc::chown(c.as_ptr(), before.uid() + 1, before.gid() + 1) };
    assert_eq!(rc, 0, "chown is accepted (errno {})", errno());
    let after = std::fs::metadata(&notes).unwrap();
    assert_eq!(
        (after.uid(), after.gid()),
        (before.uid(), before.gid()),
        "and changes nothing"
    );

    // Extended attributes: "this filesystem has none", which is EOPNOTSUPP.
    let name = CString::new("user.test").unwrap();
    let rc = unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), b"v".as_ptr().cast(), 1, 0) };
    assert_eq!(
        (rc, errno()),
        (-1, libc::EOPNOTSUPP),
        "setxattr is not supported"
    );
    let mut buf = [0u8; 16];
    let rc = unsafe {
        libc::getxattr(
            c.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    assert_eq!(
        (rc, errno()),
        (-1, libc::EOPNOTSUPP),
        "getxattr is not supported"
    );

    // flock: works, locally — a second open file sees the first one's lock.
    let a = std::fs::File::open(&notes).unwrap();
    let b = std::fs::File::open(&notes).unwrap();
    assert_eq!(unsafe { libc::flock(a.as_raw_fd(), libc::LOCK_EX) }, 0);
    let rc = unsafe { libc::flock(b.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(
        (rc, errno()),
        (-1, libc::EWOULDBLOCK),
        "a held lock blocks a second one"
    );
    assert_eq!(unsafe { libc::flock(a.as_raw_fd(), libc::LOCK_UN) }, 0);
    assert_eq!(
        unsafe { libc::flock(b.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "and a released one no longer does"
    );
}
