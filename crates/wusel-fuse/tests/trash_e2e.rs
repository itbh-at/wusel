// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The mount gives a freedesktop wastebasket no place to live: an existing
//! `.Trash-<uid>` on the server is hidden (not listed, not resolvable), and a
//! fresh one cannot be created. A file manager therefore sees no trash to move
//! deletions into and no way to make one, so "Move to Trash" falls back to a
//! real delete — which lands in Nextcloud's own server-side trash rather than a
//! `.Trash-1000` folder syncing to every device.
//!
//! Needs `/dev/fuse`, so it runs only on Linux (the podman container).
#![cfg(target_os = "linux")]

mod common;

use std::io::ErrorKind;

#[test]
fn a_desktop_trash_is_hidden_and_cannot_be_created() {
    // The fixture already carries a server-side `.Trash-1000/files/old.txt`
    // (seeded in the shared harness), standing in for a wastebasket another
    // client left behind.
    let m = common::MountFixture::start("trash");

    // Hidden from the root listing …
    let names: Vec<_> = std::fs::read_dir(&m.mnt)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"Notes.txt".to_string()),
        "the ordinary tree is listed: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with(".Trash")),
        "the pre-existing trash directory must not be listed: {names:?}"
    );

    // … and not resolvable either: a direct stat behaves as if it is absent.
    let err = std::fs::metadata(m.mnt.join(".Trash-1000")).unwrap_err();
    assert_eq!(
        err.kind(),
        ErrorKind::NotFound,
        "stat of the hidden trash must report it as absent, got {err:?}"
    );

    // A fresh wastebasket cannot be created at the root — both the classic
    // `.Trash-<uid>` and the shared `.Trash` spelling are refused …
    for name in [".Trash-1000", ".Trash"] {
        let err = std::fs::create_dir(m.mnt.join(name)).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::PermissionDenied,
            "creating {name} must be refused, got {err:?}"
        );
    }

    // … while an ordinary directory is unaffected.
    std::fs::create_dir(m.mnt.join("Documents")).unwrap();
    assert!(m.mnt.join("Documents").is_dir());
}
