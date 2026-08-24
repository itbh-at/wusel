// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The mount refuses to host a freedesktop wastebasket, so "Move to Trash" in a
//! file manager falls back to a real delete (which lands in Nextcloud's own
//! server trash) instead of a `.Trash-<uid>` folder appearing in the cloud and
//! syncing to every device.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn the_mount_refuses_to_host_a_desktop_trash() {
    let base = std::env::temp_dir().join(format!("wusel-mock-notrash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    // A trash directory that already exists on the server (another client, or
    // Windows) — the case the mount-root guard cannot see.
    std::fs::create_dir_all(fixture.join(".Trash-1000/files")).unwrap();
    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();
    let mut engine = common::Engine::start(&addr);

    // Creating a top-directory trash is refused …
    assert!(
        engine.mkdir(ROOT_INODE, ".Trash-1000-new").is_err(),
        "a new top-level trash directory is refused"
    );
    // … while an ordinary directory is fine.
    assert!(
        engine.mkdir(ROOT_INODE, "Documents").is_ok(),
        "an ordinary directory is unaffected"
    );

    // And placing a file inside the *pre-existing* trash directory is refused,
    // so the mount is never used to populate a wastebasket.
    let trash = engine
        .resolve(".Trash-1000")
        .unwrap()
        .expect("the server-side trash directory is visible");
    assert!(
        engine.create(trash.inode, "deleted.txt").is_err(),
        "writing into a trash directory is refused"
    );

    // The way a file manager actually empties a file into the wastebasket:
    // rename it into the pre-existing `.Trash-1000/files/`. This is the path the
    // create guard above does *not* cover, and the one testers hit when a
    // `.Trash-1000` already exists on the server — it must be refused too.
    let files = engine
        .resolve(".Trash-1000/files")
        .unwrap()
        .expect("the trash's files/ subdirectory is visible");
    let victim = engine
        .create(ROOT_INODE, "victim.txt")
        .expect("an ordinary file is created");
    assert!(
        engine
            .rename(ROOT_INODE, "victim.txt", files.inode, "victim.txt")
            .is_err(),
        "moving a file into a pre-existing trash directory is refused"
    );
    // The file stays where it was — the refused move did not lose it.
    assert!(
        engine.lookup(ROOT_INODE, "victim.txt").is_some(),
        "the refused move left the file in place"
    );
    let _ = victim;

    std::fs::remove_dir_all(&base).ok();
}
