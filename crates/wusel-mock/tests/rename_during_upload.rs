// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Renaming a freshly created file while its first upload is still in flight
//! must not leave a ghost at the old name, spawn a conflicted copy, or lose the
//! content. This is the `mv n1 n2` / `sed -i` pattern: create + write + close
//! schedules a deferred-create upload, and the rename lands before that upload
//! reaches the server.
//!
//! The upload reads the node's path live at PUT time, while `rename` rewrites
//! that same path on a different scheduler object with no lock between them. We
//! make the race deterministic by holding the PUT on the mock (the upload has
//! already captured the old path) and committing the rename inside that window.

mod common;

use std::time::Duration;
use wusel_core::state::ROOT_INODE;

#[test]
fn renaming_a_new_file_mid_upload_keeps_one_file_at_the_new_name() {
    let base = std::env::temp_dir().join(format!("wusel-mock-mv-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);

    // `echo q > n1`: deferred create + one write, nothing on the server yet.
    let node = engine.create(ROOT_INODE, "n1").expect("create");
    engine.write(node.inode, 0, b"q").expect("write");

    // Hold every PUT for a second. The flush's upload enters `upload()`, reads
    // the path ("n1"), then parks in the PUT — long enough for the rename to
    // commit the new path underneath it.
    std::env::set_var("WUSEL_MOCK_PUT_DELAY_MS", "1000");
    engine.flush(node.inode).expect("flush"); // returns at commit; upload runs on

    // Let the net worker actually enter the (now sleeping) PUT with path "n1".
    std::thread::sleep(Duration::from_millis(300));

    // `mv n1 n2` while that upload sleeps.
    engine
        .rename(ROOT_INODE, "n1", ROOT_INODE, "n2")
        .expect("rename");

    engine.wait_for_uploads();
    std::env::remove_var("WUSEL_MOCK_PUT_DELAY_MS");

    let names: Vec<String> = std::fs::read_dir(&fixture)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();

    assert!(
        !names.iter().any(|n| n == "n1"),
        "a ghost of the pre-rename name was left on the server: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("conflicted copy")),
        "the rename spawned a spurious conflicted copy: {names:?}"
    );
    assert!(
        fixture.join("n2").exists(),
        "the content never landed under the new name: {names:?}"
    );
    assert_eq!(
        std::fs::read(fixture.join("n2")).unwrap(),
        b"q",
        "the renamed file must hold its content on the server"
    );
    assert_eq!(
        engine.read(node.inode, 0, 16).expect("still readable"),
        b"q",
        "the file must stay readable (no EIO) after the rename"
    );

    std::fs::remove_dir_all(&base).ok();
}
