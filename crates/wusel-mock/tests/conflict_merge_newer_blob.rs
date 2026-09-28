// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A merge must never use the server's *new* version as its base.
//!
//! Once the syncer has recorded the server's change, a reader can leave that new
//! version in the cache — and a base looked up by the database's version finds
//! it. Base and theirs are then identical, `diffy` returns ours unchanged, and
//! the upload silently discards the server's edit. The base must be the version
//! the write buffer was started from; with that gone from the cache there is
//! nothing to merge against, and the conflict becomes a conflicted copy.

mod common;

use wusel_core::config::Account;

#[test]
fn a_cached_server_version_is_never_taken_as_the_merge_base() {
    let base = std::env::temp_dir().join(format!("wusel-mock-merge-newer-{}", std::process::id()));
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
    engine.write(node.inode, 0, b"LINE1").unwrap();

    let theirs = b"line1\nline2\nLINE3\n";
    std::fs::write(&backing, theirs).unwrap();
    engine.wait_until_stale("doc.txt", &node.etag);

    // The server's new version lands in the cache, as a reader would leave it.
    let now = engine.provider().resolve("doc.txt").unwrap().unwrap();
    let blobs = account.blob_cache_dir();
    let blob = blobs.join(now.file_id.expect("file id").to_string());
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::write(&blob, theirs).unwrap();
    std::fs::write(blob.with_extension("etag"), &now.etag).unwrap();

    engine.flush(node.inode).unwrap();
    engine.wait_for_uploads();

    assert_eq!(
        std::fs::read(&backing).unwrap(),
        theirs,
        "the server's edit must survive"
    );
    let copy = std::fs::read_dir(&fixture)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().contains("conflicted copy"))
        .expect("our edit is parked as a conflicted copy");
    assert_eq!(
        std::fs::read(copy.path()).unwrap(),
        b"LINE1\nline2\nline3\n"
    );

    std::fs::remove_dir_all(&base).ok();
}
