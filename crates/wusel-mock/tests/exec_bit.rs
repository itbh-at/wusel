// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The executable bit, as the engine keeps it: in the local state, per file,
//! through the ways a file is created, saved and renamed — and never on the
//! server.
//!
//! One test binary, one engine: the harness points the XDG dirs at a sandbox,
//! which is process-global.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn the_executable_bit_is_local_state_that_follows_the_file() {
    let base = std::env::temp_dir().join(format!("wusel-mock-exec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("dir")).unwrap();
    std::fs::write(fixture.join("tool.sh"), b"#!/bin/sh\n").unwrap();

    common::xdg_sandbox(&base);
    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);
    engine.list(ROOT_INODE);

    // A file from the server is not executable, and chmod +x makes it so.
    let tool = engine.lookup(ROOT_INODE, "tool.sh").expect("tool.sh");
    assert!(!tool.exec, "nothing arrives executable from the server");
    assert!(engine.set_exec(tool.inode, true).expect("chmod +x").exec);
    assert!(!engine.set_exec(tool.inode, false).expect("chmod -x").exec);

    // A file created executable stays so through its upload and the re-listing
    // behind it, which gives the row its file id.
    let born = engine
        .create_exec(ROOT_INODE, "born.sh")
        .expect("create 0755");
    assert!(born.exec, "created with mode 0755");
    engine.write(born.inode, 0, b"#!/bin/sh\n").expect("write");
    engine.flush(born.inode).expect("close");
    engine.wait_for_uploads();
    let born = engine.stat(born.inode).expect("born.sh after upload");
    assert!(born.file_id.is_some(), "the upload identified it");
    assert!(born.exec, "the x bit survives the upload");

    // A rename keeps it.
    engine
        .rename(ROOT_INODE, "born.sh", ROOT_INODE, "moved.sh")
        .expect("rename");
    assert!(engine.lookup(ROOT_INODE, "moved.sh").unwrap().exec);

    // An atomic save — write a temporary, chmod it, rename it over the file —
    // leaves the saved file executable, as `sed -i` and editors do.
    let temp = engine.create(ROOT_INODE, "sedAbC123").expect("temporary");
    engine
        .write(temp.inode, 0, b"#!/bin/sh\n# edited\n")
        .unwrap();
    engine.flush(temp.inode).unwrap();
    engine
        .set_exec(temp.inode, true)
        .expect("fchmod the temporary");
    engine
        .rename(ROOT_INODE, "sedAbC123", ROOT_INODE, "moved.sh")
        .expect("rename over");
    engine.wait_for_uploads();
    let saved = engine.lookup(ROOT_INODE, "moved.sh").expect("moved.sh");
    assert!(saved.exec, "the x bit survives an atomic save");
    assert_eq!(
        std::fs::read(fixture.join("moved.sh")).unwrap(),
        b"#!/bin/sh\n# edited\n",
        "and the save itself reached the server"
    );

    // A directory's mode is not ours: accepted, and nothing recorded.
    let dir = engine.lookup(ROOT_INODE, "dir").expect("dir");
    assert!(!engine.set_exec(dir.inode, true).expect("chmod a dir").exec);

    std::fs::remove_dir_all(&base).ok();
}
