// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The synchronous fallback: with `[sync] upload = sync`, `flush` waits for the
//! upload, so the file is on the server before `close()` returns — the
//! pre-async behaviour, kept for anyone who wants a save to mean "on the server".

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn synchronous_upload_reaches_the_server_before_flush_returns() {
    let base = std::env::temp_dir().join(format!("wusel-mock-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);
    // Configure this account for synchronous write-back before the engine reads
    // its settings.
    let cfg_dir =
        std::path::PathBuf::from(std::env::var("XDG_CONFIG_HOME").unwrap()).join("wusel");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "[sync]\nupload = \"sync\"\n").unwrap();

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();
    let engine = common::Engine::start(&addr);

    let node = engine.create(ROOT_INODE, "new.txt").unwrap();
    engine.write(node.inode, 0, b"hello").unwrap();
    assert!(
        !fixture.join("new.txt").exists(),
        "nothing on the server until flush — the create is deferred"
    );

    // Synchronous: this call returns only once the upload has landed. No
    // wait_for_uploads — the assertion right after proves it was synchronous.
    engine.flush(node.inode).expect("flush uploads and waits");
    assert_eq!(
        std::fs::read(fixture.join("new.txt")).unwrap(),
        b"hello",
        "a synchronous flush put the file on the server before returning"
    );
    assert_eq!(
        engine.upload_state(node.inode),
        None,
        "and nothing is left owed"
    );

    std::fs::remove_dir_all(&base).ok();
}
