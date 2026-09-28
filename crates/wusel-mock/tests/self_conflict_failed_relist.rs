// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `cat > f` under a struggling server: the new file must end up holding what
//! was written, not an empty file beside a "conflicted copy" of the content.
//!
//! A shell redirection opens the file, `dup2`s the descriptor and closes the
//! original before the program writes a byte — and that close is a flush, so an
//! **empty** first upload goes out. It lands. The listing that should then give
//! the new file its server id fails (here: injected, via the mock's
//! `*.relist-fails` directory). The content is written and flushed next. The
//! file is on the server — as our own empty version — so this second upload must
//! not be treated as colliding with somebody else's file.
//!
//! Own test binary: it sets the process-global upload retry interval.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn content_written_after_a_landed_upload_whose_relist_failed_is_not_a_conflict() {
    let base = std::env::temp_dir().join(format!("wusel-mock-relist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    let dir = fixture.join("work.relist-fails");
    std::fs::create_dir_all(&dir).unwrap();

    common::xdg_sandbox(&base);
    // No background retry: the steps below decide what is sent, and when.
    std::env::set_var("WUSEL_UPLOAD_RETRY_SECS", "99999");

    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);

    let work = engine
        .lookup(ROOT_INODE, "work.relist-fails")
        .expect("the directory exists");
    let _ = engine.list(work.inode); // listed before anything is uploaded into it

    // The shell's early close: the file is created and flushed while still empty.
    let f = engine.create(work.inode, "README.adoc").expect("create");
    engine
        .flush(f.inode)
        .expect("flush of the still-empty file");
    // That empty upload lands; the relist behind it fails.
    engine.wait_until_upload_attempted(f.inode);

    // Now the program writes the content and closes.
    engine.write(f.inode, 0, b"= Title\n").expect("write");
    engine.flush(f.inode).expect("flush of the content");
    engine.wait_for_uploads();

    // Name and size, so a failure shows the empty original beside the copy.
    let names: Vec<String> = std::fs::read_dir(&dir)
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
        std::fs::read(dir.join("README.adoc")).unwrap(),
        b"= Title\n",
        "the file must hold what was written, not stay empty"
    );

    std::fs::remove_dir_all(&base).ok();
}
