// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A pin is a promise, and the emblem has to report the promise as it *stands*.
//!
//! `pinned` used to mean nothing more than "a pin covers this path", so a file
//! the bytes had never reached was drawn exactly like one sitting on the disk.
//! That is the one reading a user acts on before losing the network, and it was
//! wrong for every file a directory pin (or an account-wide one) covered
//! without ever fetching: the pin is made once, the files keep arriving.
//!
//! Here the copy is taken away from under a pinned file, which is the same
//! situation from the other side and needs no waiting for a server to grow one.

mod common;

use wusel_core::config::Account;
use wusel_core::provider::FileState;

#[test]
fn a_pinned_file_with_no_local_copy_is_pending_and_update_fetches_it() {
    let base = std::env::temp_dir().join(format!("wusel-mock-pending-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("keep")).unwrap();
    std::fs::write(fixture.join("keep/promised.txt"), b"kept offline").unwrap();

    common::xdg_sandbox(&base);
    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();
    let mut engine = common::Engine::start(&addr);

    // A directory pin, the way a user makes one: it covers what is in there now
    // and whatever turns up later.
    engine.pin("keep").unwrap();
    let dir = engine.resolve("keep").unwrap().expect("keep");
    let file = engine.resolve("keep/promised.txt").unwrap().expect("file");
    assert_eq!(
        engine.state(file.inode),
        Some(FileState::Pinned),
        "pinning fetches, so the copy is here and the promise is kept"
    );

    // Take the copy away without touching the pin — a cleared cache, a lost
    // disk, or (the everyday case) a file the pin covers that was never
    // fetched at all.
    let blob = Account::new("default")
        .blob_cache_dir()
        .join(file.file_id.expect("the mock assigns file ids").to_string());
    std::fs::remove_file(&blob).expect("the pinned blob is where the cache keeps it");

    assert_eq!(
        engine.state(file.inode),
        Some(FileState::PinnedPending),
        "promised but not here: it still costs the network to open, and the \
         emblem must not claim otherwise"
    );
    // The directory's own pin is untouched by any of this: it has no content of
    // its own to be here or missing.
    assert_eq!(engine.state(dir.inode), Some(FileState::Pinned));

    // And the repair is the ordinary one. `update` used to look only for a copy
    // that was here and outdated, so it walked straight past a missing one and
    // reported nothing to do.
    assert_eq!(
        engine.provider().refresh("keep").unwrap(),
        1,
        "update fetches what the pin promised and did not deliver"
    );
    assert_eq!(engine.state(file.inode), Some(FileState::Pinned));

    std::fs::remove_dir_all(&base).ok();
}
