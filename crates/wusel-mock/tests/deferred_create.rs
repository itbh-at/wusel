// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for deferred creation: `create` must not touch the server; the
//! file is materialised on the first flush. A file created and deleted before
//! any flush (an editor probe/temp file) never contacts the server at all.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn create_defers_upload_until_flush_and_temp_files_never_hit_the_server() {
    let base = std::env::temp_dir().join(format!("wusel-mock-defer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let engine = common::Engine::start(&addr);

    // 1. Deferred create: nothing on the server yet.
    let node = engine.create(ROOT_INODE, "new.txt").unwrap();
    assert!(
        !fixture.join("new.txt").exists(),
        "create must not upload anything"
    );
    // Readable before any flush — from the scratch — and still empty.
    assert!(engine.read(node.inode, 0, 16).unwrap().is_empty());

    // 2. Write, then flush → the file appears on the server with its content.
    engine.write(node.inode, 0, b"hello").unwrap();
    assert_eq!(engine.read(node.inode, 0, 16).unwrap(), b"hello");
    assert!(!fixture.join("new.txt").exists(), "still local until flush");
    engine.flush(node.inode).unwrap();
    engine.wait_for_uploads();
    assert_eq!(std::fs::read(fixture.join("new.txt")).unwrap(), b"hello");

    // 3. A file created and deleted before any flush never reaches the server —
    //    and delete must not error (there is nothing to DELETE remotely).
    let tmp = engine.create(ROOT_INODE, ".new.txt.swp").unwrap();
    engine.write(tmp.inode, 0, b"swapdata").unwrap();
    engine.remove(ROOT_INODE, ".new.txt.swp").unwrap();
    assert!(
        !fixture.join(".new.txt.swp").exists(),
        "a temp file created and deleted before flush must never materialise"
    );

    std::fs::remove_dir_all(&base).ok();
}
