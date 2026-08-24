// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Conflict handling — the default (no text merge): when the server changed
//! under us, the upload is rejected (If-Match 412) and our local edit is saved
//! as a "conflicted copy" while the server version stays.

mod common;

use std::sync::{Arc, Mutex};

use wusel_core::desktop::{Desktop, Notice, Status};

/// A test OS-integration backend that just records what the engine reports.
#[derive(Default)]
struct Recorder {
    notices: Mutex<Vec<Notice>>,
    statuses: Mutex<Vec<Status>>,
}
impl Desktop for Recorder {
    fn notify(&self, n: &Notice) {
        self.notices.lock().unwrap().push(n.clone());
    }
    fn set_status(&self, s: Status) {
        self.statuses.lock().unwrap().push(s);
    }
}

#[test]
fn conflict_saves_a_copy_and_keeps_the_server_version() {
    let base = std::env::temp_dir().join(format!("wusel-mock-conflict-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    let backing = fixture.join("note.txt");
    std::fs::write(&backing, b"server-original").unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    // Plug in a recording OS-integration backend (the swappable Desktop seam).
    let recorder = Arc::new(Recorder::default());
    // Before the substrate starts: its workers take a copy of this seam.
    let mut engine = common::Engine::start_with(&addr, Some(recorder.clone()));

    let node = engine
        .provider()
        .resolve("note.txt")
        .unwrap()
        .expect("note.txt");
    engine.truncate(node.inode, 0).unwrap();
    engine.write(node.inode, 0, b"LOCAL-EDIT").unwrap();

    // The server changes under us before we flush.
    std::fs::write(&backing, b"SERVER-CHANGED").unwrap();

    engine.flush(node.inode).unwrap();
    engine.wait_for_uploads();

    // The server version stays at the original path.
    assert_eq!(std::fs::read(&backing).unwrap(), b"SERVER-CHANGED");

    // A conflicted copy holds our local edit.
    let copy = std::fs::read_dir(&fixture)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("note (conflicted copy") && n.ends_with(").txt"))
        .expect("a conflicted copy was created");
    assert_eq!(std::fs::read(fixture.join(copy)).unwrap(), b"LOCAL-EDIT");

    // The engine reported the conflict through the Desktop seam: a ConflictCopy
    // notice, and a Syncing→Idle status transition around the upload.
    let notices = recorder.notices.lock().unwrap();
    assert!(
        notices
            .iter()
            .any(|n| matches!(n, Notice::ConflictCopy { path, .. } if path == "note.txt")),
        "a ConflictCopy notice must be raised, got {notices:?}"
    );
    let statuses = recorder.statuses.lock().unwrap();
    assert!(statuses.contains(&Status::Syncing), "status went Syncing");
    assert_eq!(statuses.last(), Some(&Status::Idle), "and back to Idle");

    std::fs::remove_dir_all(&base).ok();
}
