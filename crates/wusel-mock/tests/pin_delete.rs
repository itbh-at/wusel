// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end: deleting a *pinned* file must take its pin record and its
//! on-disk eviction marker with it.
//!
//! A surviving `.pin` marker is permanent damage: eviction skips pinned blobs
//! *and* does not count them against the budget, so those bytes leave the cache
//! budget for good — and a later file that happens to reuse the same file id
//! would look pinned to us. Only a real DELETE round-trip shows this, hence an
//! e2e against `wusel-mock` rather than a unit test.

mod common;

use wusel_core::config::Account;
use wusel_core::state::ROOT_INODE;

#[test]
fn deleting_a_pinned_file_drops_its_pin_and_its_eviction_marker() {
    let base = std::env::temp_dir().join(format!("wusel-mock-pin-del-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    // The mock serves a real directory, so the fixture *is* the server state:
    // one small file, so the pin below has something to fetch.
    let backing = fixture.join("Notes.txt");
    std::fs::write(&backing, b"hello").unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let account = Account::new("default");
    let mut engine = common::Engine::start(&addr);

    // Pin the file: it is fetched whole and marked "keep offline" on disk.
    assert_eq!(engine.pin("Notes.txt").unwrap(), 1);
    // The cache is keyed by the server's file id, which the mock derives from
    // the path — so ask the state for it instead of hard-coding one.
    let file_id = engine
        .resolve("Notes.txt")
        .unwrap()
        .expect("Notes.txt exists")
        .file_id
        .expect("the mock serves file ids");
    let blobs = account.blob_cache_dir();
    assert!(
        blobs.join(file_id.to_string()).exists(),
        "the pinned blob is cached"
    );
    assert!(
        blobs.join(format!("{file_id}.pin")).exists(),
        "with its eviction marker"
    );

    // Deleting the file must take both the pin record and the marker with it.
    engine.remove(ROOT_INODE, "Notes.txt").unwrap();
    assert!(!backing.exists(), "the DELETE reached the server");
    assert!(
        engine.pins().unwrap().is_empty(),
        "the pin record went with the file: {:?}",
        engine.pins().unwrap()
    );
    assert!(
        !blobs.join(format!("{file_id}.pin")).exists(),
        "the eviction marker went with the file"
    );

    std::fs::remove_dir_all(&base).ok();
}
