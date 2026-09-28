// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The boundary of the "our own earlier attempt" settlement: a new file that
//! collides with somebody else's file of the same name — different, non-empty
//! content — is still a real conflict. Their file stays; ours is parked beside
//! it as a conflicted copy.
//!
//! Only identical bytes, or an empty file where we were creating one, may be
//! settled without a copy. This guards that the settlement does not swallow a
//! genuine collision.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn a_new_file_colliding_with_a_foreign_file_still_keeps_both() {
    let base = std::env::temp_dir().join(format!("wusel-mock-foreign-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);

    // Ours: created and written locally, not yet flushed.
    let f = engine.create(ROOT_INODE, "note.txt").expect("create");
    engine.write(f.inode, 0, b"mine").expect("write");

    // Somebody else creates the same name on the server first.
    std::fs::write(fixture.join("note.txt"), b"theirs!").unwrap();

    engine.flush(f.inode).expect("flush");
    engine.wait_for_uploads();

    assert_eq!(
        std::fs::read(fixture.join("note.txt")).unwrap(),
        b"theirs!",
        "their file must stay as it is"
    );
    let copy = std::fs::read_dir(&fixture)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().contains("conflicted copy"))
        .expect("ours is parked as a conflicted copy");
    assert_eq!(std::fs::read(copy).unwrap(), b"mine");

    std::fs::remove_dir_all(&base).ok();
}
