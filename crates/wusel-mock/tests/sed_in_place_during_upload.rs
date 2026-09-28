// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `sed -i`, and every other write-temporary-then-rename save: the new content
//! goes into a fresh temporary, which is closed and then renamed onto the file.
//! The temporary's first upload is still on its way when the rename lands.
//!
//! The file must end up holding the new content, and neither the temporary nor
//! a conflicted copy may be left on the server. Before the rename waited for
//! that upload, the temporary landed under its own name and stayed there.
//!
//! Own test binary: it sets the process-global mock PUT delay.

mod common;

use std::time::Duration;
use wusel_core::state::ROOT_INODE;

#[test]
fn a_temporary_renamed_onto_a_file_mid_upload_replaces_it_and_leaves_nothing() {
    let base = std::env::temp_dir().join(format!("wusel-mock-sed-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("s"), b"alpha").unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);

    // sed looks at the file first, then writes its replacement to a temporary.
    assert!(engine.lookup(ROOT_INODE, "s").is_some(), "the file exists");
    let tmp = engine.create(ROOT_INODE, "sedAbC123").expect("create");
    engine.write(tmp.inode, 0, b"beta").expect("write");

    // Hold the PUT so the temporary's upload is still in flight at the rename.
    std::env::set_var("WUSEL_MOCK_PUT_DELAY_MS", "1000");
    engine.flush(tmp.inode).expect("flush");
    std::thread::sleep(Duration::from_millis(300));

    engine
        .rename(ROOT_INODE, "sedAbC123", ROOT_INODE, "s")
        .expect("rename onto the file");

    engine.wait_for_uploads();
    std::env::remove_var("WUSEL_MOCK_PUT_DELAY_MS");

    let names: Vec<String> = std::fs::read_dir(&fixture)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n.starts_with("sed")),
        "the temporary was left behind on the server: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("conflicted copy")),
        "the save spawned a spurious conflicted copy: {names:?}"
    );
    assert_eq!(
        std::fs::read(fixture.join("s")).unwrap(),
        b"beta",
        "the file must hold the new content on the server"
    );
    let s = engine
        .lookup(ROOT_INODE, "s")
        .expect("the file is still there");
    assert_eq!(
        engine.read(s.inode, 0, 16).expect("readable"),
        b"beta",
        "and read back the new content under its name"
    );

    std::fs::remove_dir_all(&base).ok();
}
