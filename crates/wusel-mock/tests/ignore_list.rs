// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for the ignore list: an ephemeral editor/OS file (matched by an
//! ignore pattern) is kept purely local — it never reaches the server — yet is
//! fully usable through the mount. And an ignored temp file renamed onto a real
//! name is "promoted": its content is uploaded under the new name (the atomic-
//! save pattern of office suites).

mod common;

use wusel_core::state::ROOT_INODE;

fn engine_for(addr: &str, fixture: &std::path::Path) -> common::Engine {
    let _ = fixture; // fixture is served by the mock; the engine talks over HTTP
    common::Engine::start(addr)
}

#[test]
fn ignored_files_stay_local_and_promote_on_rename() {
    let base = std::env::temp_dir().join(format!("wusel-mock-ignore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let engine = engine_for(&addr, &fixture);

    // 1. A LibreOffice lock file: kept purely local through its whole lifecycle.
    let lock = engine.create(ROOT_INODE, ".~lock.notes.odt#").unwrap();
    engine.write(lock.inode, 0, b"lockdata").unwrap();
    engine.flush(lock.inode).unwrap();
    assert!(
        !fixture.join(".~lock.notes.odt#").exists(),
        "an ignored file must never be uploaded"
    );
    // Still fully usable locally: reads come from the buffer.
    assert_eq!(engine.read(lock.inode, 0, 8).unwrap(), b"lockdata");
    // Deleting it must not error (nothing to DELETE on the server).
    engine.remove(ROOT_INODE, ".~lock.notes.odt#").unwrap();
    assert!(!fixture.join(".~lock.notes.odt#").exists());

    // 2. Promotion: an ignored temp file renamed onto a real document uploads.
    let tmp = engine.create(ROOT_INODE, "scratch.tmp").unwrap();
    engine.write(tmp.inode, 0, b"the document").unwrap();
    assert!(
        !fixture.join("scratch.tmp").exists(),
        "the temp file itself is never uploaded"
    );
    engine
        .rename(ROOT_INODE, "scratch.tmp", ROOT_INODE, "notes.odt")
        .unwrap();
    // The promotion upload is scheduled, not performed inside the rename. A
    // failed upload must not fail the rename — the local rename is already
    // committed, and reporting failure would tell the kernel it never happened
    // — so the rename returns and the document appears a moment later.
    let promoted = fixture.join("notes.odt");
    for _ in 0..100 {
        if promoted.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        std::fs::read(fixture.join("notes.odt")).unwrap(),
        b"the document",
        "renaming an ignored temp onto a real name promotes (uploads) its content"
    );
    assert!(
        !fixture.join("scratch.tmp").exists(),
        "the temp name never materialises on the server"
    );

    std::fs::remove_dir_all(&base).ok();
}
