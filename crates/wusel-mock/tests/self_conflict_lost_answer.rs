// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `cat > f` when the answer to the first upload is lost: the new file must end
//! up holding what was written, not an empty file beside a "conflicted copy".
//!
//! The same shell pattern as in `self_conflict_failed_relist`, with a different
//! failure: the early, **empty** upload lands on the server, but its answer never
//! comes back (here: injected, via the mock's `*.lost-once.*` marker — in the field
//! a gateway timeout after the server had committed). The client cannot tell
//! that the file now exists, so the content upload that follows asserts "must
//! not exist" and collides — with our own empty file.
//!
//! Own test binary: it sets the process-global upload retry interval.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn content_written_after_an_upload_whose_answer_was_lost_is_not_a_conflict() {
    let base = std::env::temp_dir().join(format!("wusel-mock-lost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);
    std::env::set_var("WUSEL_UPLOAD_RETRY_SECS", "99999");

    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);

    // The shell's early close: created and flushed while still empty. The upload
    // lands; its answer is lost.
    let f = engine
        .create(ROOT_INODE, "package.lost-once.json")
        .expect("create");
    engine
        .flush(f.inode)
        .expect("flush of the still-empty file");
    engine.wait_until_upload_attempted(f.inode);

    // The program writes the content and closes.
    engine.write(f.inode, 0, b"{}\n").expect("write");
    engine.flush(f.inode).expect("flush of the content");
    engine.wait_for_uploads();

    let names: Vec<String> = std::fs::read_dir(&fixture)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            format!("{} ({size} B)", e.file_name().to_string_lossy())
        })
        .collect();
    assert!(
        !names.iter().any(|n| n.contains("conflicted copy")),
        "our own earlier upload was taken for somebody else's file: {names:?}"
    );
    assert_eq!(
        std::fs::read(fixture.join("package.lost-once.json")).unwrap(),
        b"{}\n",
        "the file must hold what was written, not stay empty"
    );

    std::fs::remove_dir_all(&base).ok();
}
