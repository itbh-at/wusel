// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `touch challenges.adoc` when the answer to its upload is lost must leave one
//! empty file — not that file plus a "conflicted copy" of it.
//!
//! The same `touch` as in `touch_relist_fails` (create, stamp the time, close as
//! `flush` + `release`), with the other failure: the empty upload lands, but its
//! answer never comes back (the mock's `*.lost-once.*` marker — in the field a
//! gateway timeout after the server had committed). The second publish sends
//! the same empty buffer again, and must not park it as a copy of itself.
//!
//! Own test binary: it sets the process-global upload retry interval.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn touching_a_new_file_whose_answer_is_lost_leaves_one_empty_file() {
    let base = std::env::temp_dir().join(format!("wusel-mock-touch-lost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);
    std::env::set_var("WUSEL_UPLOAD_RETRY_SECS", "99999");

    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);

    // touch challenges.lost-once.adoc
    let name = "challenges.lost-once.adoc";
    let f = engine.create(ROOT_INODE, name).expect("open(O_CREAT)");
    engine.set_mtime(f.inode, 1_790_000_000).expect("utimensat");
    engine.flush(f.inode).expect("close: flush");
    // The empty upload lands; its answer is lost.
    engine.wait_until_upload_attempted(f.inode);
    engine.flush(f.inode).expect("close: release");
    engine.wait_for_uploads();

    let entries: Vec<String> = std::fs::read_dir(&fixture)
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

    std::fs::remove_dir_all(&base).ok();
}
