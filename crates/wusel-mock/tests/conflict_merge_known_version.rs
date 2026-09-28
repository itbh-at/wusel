// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The text merge when Wusel already *knows* about the server's change.
//!
//! With `notify_push` this is the normal case, not the edge: the server edits
//! the file while our write buffer is open, the push arrives, and the sync walk
//! records the new version before our upload meets its 412. The merge base is
//! the version the buffer was started from — not whatever the state database
//! says now. Looking it up by the database's version finds no base and turns a
//! clean merge into a conflicted copy.

mod common;

use wusel_core::config::Account;

#[test]
fn disjoint_edits_merge_after_the_syncer_recorded_the_server_version() {
    let base = std::env::temp_dir().join(format!("wusel-mock-merge-known-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    let backing = fixture.join("doc.txt");
    std::fs::write(&backing, b"line1\nline2\nline3\n").unwrap();

    common::xdg_sandbox(&base);

    let account = Account::new("default");
    std::fs::create_dir_all(account.config_path().parent().unwrap()).unwrap();
    std::fs::write(account.config_path(), "[sync]\ntext_merge = true\n").unwrap();

    let mock = common::Mock::serve(&fixture);
    let mut engine = common::Engine::start(&mock.addr);

    let node = engine
        .provider()
        .resolve("doc.txt")
        .unwrap()
        .expect("doc.txt");
    // Local edit on line 1 — the buffer is based on the original version.
    engine.write(node.inode, 0, b"LINE1").unwrap();

    // Server edit on line 3, and the syncer learns of it before we upload.
    std::fs::write(&backing, b"line1\nline2\nLINE3\n").unwrap();
    engine.wait_until_stale("doc.txt", &node.etag);

    engine.flush(node.inode).unwrap();
    engine.wait_for_uploads();

    assert_eq!(std::fs::read(&backing).unwrap(), b"LINE1\nline2\nLINE3\n");
    let has_copy = std::fs::read_dir(&fixture)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains("conflicted copy"));
    assert!(!has_copy, "a clean merge must not leave a conflicted copy");

    std::fs::remove_dir_all(&base).ok();
}
