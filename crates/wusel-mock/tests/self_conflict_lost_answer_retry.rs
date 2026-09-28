// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A retry of an upload whose answer was lost must not leave a "conflicted
//! copy" of the very same bytes beside the file.
//!
//! The upload lands, its answer is lost (the mock's `*.lost-once.*` marker), and
//! the client retries the unchanged content. That retry asserts "must not exist"
//! and collides with the file its own first attempt created. Nothing differs
//! between the two versions, so there is nothing to keep apart — the field
//! report was a duplicate `… (conflicted copy …).zip` after zipping on a flaky
//! connection.
//!
//! Own test binary: it sets the process-global upload retry interval.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn retrying_an_upload_whose_answer_was_lost_makes_no_copy_of_identical_bytes() {
    let base = std::env::temp_dir().join(format!("wusel-mock-lost-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);
    std::env::set_var("WUSEL_UPLOAD_RETRY_SECS", "99999");

    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);

    let f = engine
        .create(ROOT_INODE, "archive.lost-once.zip")
        .expect("create");
    engine.write(f.inode, 0, b"PK-bytes").expect("write");
    engine.flush(f.inode).expect("flush");
    // It landed; the answer was lost, so the upload is still owed.
    engine.wait_until_upload_attempted(f.inode);

    // The retry — the same bytes again.
    engine.flush(f.inode).expect("retry");
    engine.wait_for_uploads();

    let names: Vec<String> = std::fs::read_dir(&fixture)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n.contains("conflicted copy")),
        "a copy of identical bytes was made: {names:?}"
    );
    assert_eq!(
        std::fs::read(fixture.join("archive.lost-once.zip")).unwrap(),
        b"PK-bytes"
    );

    std::fs::remove_dir_all(&base).ok();
}
