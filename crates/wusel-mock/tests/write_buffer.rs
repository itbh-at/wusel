// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for the write buffer (strategy B): a `Provider` writes into an
//! existing file's scratch and flushes; we check the upload reached the server
//! (the wusel-mock backing file changed) and the read cache is coherent afterwards.

mod common;

#[test]
fn partial_write_uploads_and_stays_coherent() {
    let base = std::env::temp_dir().join(format!("wusel-mock-wbuf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    let backing = fixture.join("note.txt");
    std::fs::write(&backing, b"abcdef").unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let mut engine = common::Engine::start(&addr);

    let node = engine
        .provider()
        .resolve("note.txt")
        .unwrap()
        .expect("note.txt exists");

    // Overwrite two bytes in the middle — the base content must be preserved.
    assert_eq!(engine.write(node.inode, 2, b"XY").unwrap(), 2);
    engine.flush(node.inode).unwrap();
    engine.wait_for_uploads();

    // The upload reached the server: the mock's backing file changed.
    assert_eq!(std::fs::read(&backing).unwrap(), b"abXYef");

    // The read cache is coherent with the uploaded content (served locally).
    assert_eq!(engine.read(node.inode, 0, 6).unwrap(), b"abXYef");

    std::fs::remove_dir_all(&base).ok();
}
