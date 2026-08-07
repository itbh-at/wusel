// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Regression: an **unknown** ETag must not be mistaken for "the file does not
//! exist yet".
//!
//! A server (or a reverse proxy) may answer a `PUT` without an `ETag` header —
//! the mock's `*.no-etag*` marker stages exactly that. The node's stored ETag is
//! then empty, which used to be the same value that marks a deferred create, so
//! the next save sent `If-None-Match: *`. The file plainly exists, the server
//! answered 412, and the user's own document was filed away as a "conflicted
//! copy" — on every save, forever.
//!
//! The distinction that has to hold: no ETag *and* no server identity → a create
//! (`If-None-Match: *`); no ETag but the file exists → send no precondition at
//! all, because neither one would be true.

mod common;

use std::sync::{Arc, Mutex};

use wusel_core::desktop::{Desktop, Notice, Status};

/// Records what the engine reports through the OS-integration seam.
#[derive(Default)]
struct Recorder {
    notices: Mutex<Vec<Notice>>,
}
impl Desktop for Recorder {
    fn notify(&self, n: &Notice) {
        self.notices.lock().unwrap().push(n.clone());
    }
    fn set_status(&self, _s: Status) {}
}

/// Every entry in `dir` whose name marks it as a conflicted copy.
fn conflict_copies(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("conflicted copy"))
        .collect()
}

#[test]
fn repeated_saves_survive_a_server_that_sends_no_etag() {
    let base = std::env::temp_dir().join(format!("wusel-mock-noetag-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    // The `.no-etag` marker makes the mock answer PUT/MOVE without an ETag
    // header — the whole point of this fixture.
    let backing = fixture.join("note.no-etag.txt");
    std::fs::write(&backing, b"server-original").unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let recorder = Arc::new(Recorder::default());
    // Before the substrate starts: its workers take a copy of this seam.
    let mut engine = common::Engine::start_with(&addr, Some(recorder.clone()));

    let node = engine
        .resolve("note.no-etag.txt")
        .unwrap()
        .expect("note.no-etag.txt");
    let file_id = node.file_id.expect("the fixture exists server-side");

    // First save: a normal conditional upload against the ETag we listed. It
    // succeeds, but the answer carries no ETag — so we lose track of the version.
    engine.truncate(node.inode, 0).unwrap();
    engine.write(node.inode, 0, b"first-edit").unwrap();
    engine.flush(node.inode).unwrap();
    engine.wait_for_uploads();
    assert_eq!(std::fs::read(&backing).unwrap(), b"first-edit");

    let after_first = engine.stat(node.inode).expect("node survives");
    assert!(
        after_first.etag.is_empty(),
        "precondition of this test: with no ETag in the answer the stored ETag \
         is empty, got {:?}",
        after_first.etag
    );
    assert_eq!(
        after_first.file_id,
        Some(file_id),
        "the file still has its server identity — it exists remotely"
    );

    // Second save of the very same file. Nothing changed on the server; this
    // must simply overwrite it.
    engine.truncate(node.inode, 0).unwrap();
    engine.write(node.inode, 0, b"second-edit").unwrap();
    engine.flush(node.inode).unwrap();
    engine.wait_for_uploads();

    assert_eq!(
        conflict_copies(&fixture),
        Vec::<String>::new(),
        "an unknown ETag is not a conflict — no conflicted copy may be created"
    );
    assert_eq!(
        std::fs::read(&backing).unwrap(),
        b"second-edit",
        "the second save must reach the server file itself"
    );
    let notices = recorder.notices.lock().unwrap();
    assert!(
        !notices
            .iter()
            .any(|n| matches!(n, Notice::ConflictCopy { .. })),
        "no conflict must be reported to the user, got {notices:?}"
    );
    drop(notices);

    // The node is sane afterwards: still the same server file, correct size.
    let after_second = engine.stat(node.inode).expect("node survives");
    assert_eq!(after_second.file_id, Some(file_id));
    assert_eq!(after_second.size, b"second-edit".len() as u64);

    std::fs::remove_dir_all(&base).ok();
}
