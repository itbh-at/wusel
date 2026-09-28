// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `touch parts/moderation.adoc` under a struggling server must leave one empty
//! file — not that file plus a "conflicted copy" of it (the field report even
//! saw two, a second apart).
//!
//! What `touch` does to a file that does not exist yet: `open(O_CREAT)`, an
//! `utimensat` to stamp the time, and a close — which the kernel delivers as
//! **two** publishes, `flush` and then `release`, both of the same empty buffer.
//! The first one's upload lands; the listing that should give the new file its
//! server id fails (the mock's `*.relist-fails` directory). The second publish
//! must then treat the file as ours on the server, not assert that it does not
//! exist and collide with it.
//!
//! Own test binary: it sets the process-global upload retry interval.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn touching_a_new_file_whose_relist_fails_leaves_one_empty_file() {
    let base = std::env::temp_dir().join(format!("wusel-mock-touch-relist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    let dir = fixture.join("parts.relist-fails");
    std::fs::create_dir_all(&dir).unwrap();

    common::xdg_sandbox(&base);
    std::env::set_var("WUSEL_UPLOAD_RETRY_SECS", "99999");

    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);

    let parts = engine
        .lookup(ROOT_INODE, "parts.relist-fails")
        .expect("the directory exists");
    let _ = engine.list(parts.inode);

    // touch parts.relist-fails/moderation.adoc
    let f = engine
        .create(parts.inode, "moderation.adoc")
        .expect("open(O_CREAT)");
    engine.set_mtime(f.inode, 1_790_000_000).expect("utimensat");
    engine.flush(f.inode).expect("close: flush");
    // The empty upload lands; the relist behind it fails.
    engine.wait_until_upload_attempted(f.inode);
    engine.flush(f.inode).expect("close: release");
    engine.wait_for_uploads();

    assert_touched(&dir, "moderation.adoc");
    std::fs::remove_dir_all(&base).ok();
}

/// Exactly one entry in `dir`: `name`, empty. Lists name and size on failure,
/// so a conflicted copy beside the file is visible in the message.
fn assert_touched(dir: &std::path::Path, name: &str) {
    let entries: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            format!("{} ({size} B)", e.file_name().to_string_lossy())
        })
        .collect();
    assert_eq!(
        entries,
        vec![format!("{name} (0 B)")],
        "touch must leave exactly the one empty file"
    );
}
