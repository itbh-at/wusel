// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The per-file state a file manager draws an emblem from follows the async
//! upload: a committed change on its way to the server reads `Uploading`, and
//! one whose upload failed for good reads `SyncError`.

mod common;

use wusel_core::provider::FileState;

#[test]
fn the_emblem_follows_the_upload() {
    let base = std::env::temp_dir().join(format!("wusel-mock-emblem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("doc.fail-perm"), b"one").unwrap();
    std::fs::write(fixture.join("doc.fail-once"), b"two").unwrap();

    common::xdg_sandbox(&base);
    // Suppress automatic retries, so the transient case stays visibly in flight
    // rather than landing before the test can look.
    std::env::set_var("WUSEL_UPLOAD_RETRY_SECS", "99999");

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();
    let mut engine = common::Engine::start(&addr);

    // A permanent failure (403): the upload is parked, and the emblem says so.
    let perm = engine.resolve("doc.fail-perm").unwrap().expect("exists");
    engine.write(perm.inode, 0, b"XXX").unwrap();
    engine.flush(perm.inode).unwrap();
    engine.wait_for_uploads(); // settles as an error
    assert_eq!(
        engine.upload_state(perm.inode),
        Some(wusel_core::state::UploadState::Error)
    );
    assert_eq!(
        engine.state(perm.inode),
        Some(FileState::SyncError),
        "a parked upload reads as a sync error"
    );

    // A transient failure (500) with retries suppressed: the change is still on
    // its way, so the emblem says "uploading".
    let once = engine.resolve("doc.fail-once").unwrap().expect("exists");
    engine.write(once.inode, 0, b"YYY").unwrap();
    engine.flush(once.inode).unwrap();
    engine.wait_until_upload_attempted(once.inode);
    assert_eq!(
        engine.state(once.inode),
        Some(FileState::Uploading),
        "a change on its way to the server reads as uploading"
    );

    std::fs::remove_dir_all(&base).ok();
}
