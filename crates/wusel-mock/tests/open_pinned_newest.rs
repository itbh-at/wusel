// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The default, `open_pinned = "newest"`: opening an out-of-date pinned file
//! fetches the current version and the file stays writable.
//!
//! This is the regression guard for everyone. Someone who never touched the
//! setting must see exactly the behaviour that existed before it: the local copy
//! no longer matches, so read the current one, and it can be edited. No
//! configuration is written here on purpose — the default is what is under test.

mod common;

use std::sync::{Arc, Mutex};
use wusel_core::desktop::{Desktop, Notice};

/// Records every stale-copy notice, so we can assert none fired.
#[derive(Default)]
struct Notices(Mutex<Vec<String>>);

impl Desktop for Notices {
    fn set_status(&self, _s: wusel_core::desktop::Status) {}
    fn notify(&self, notice: &Notice) {
        if let Notice::StaleCopyServed { path, .. } = notice {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(path.clone());
        }
    }
}

#[test]
fn the_default_fetches_the_current_version_and_stays_writable() {
    let base = std::env::temp_dir().join(format!("wusel-mock-opennew-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("Notes.txt"), b"the offline version").unwrap();
    common::xdg_sandbox(&base);
    // No [sync] open_pinned — the default is exactly what we are checking.

    let mock = common::Mock::serve(&fixture);
    let notices = Arc::new(Notices::default());
    let mut engine = common::Engine::start_with(&mock.addr, Some(notices.clone()));

    engine.pin("Notes.txt").expect("pin the file");
    let node = engine.resolve("Notes.txt").unwrap().unwrap();

    std::fs::write(
        fixture.join("Notes.txt"),
        b"a much newer version from the web",
    )
    .unwrap();
    engine.wait_until_stale("Notes.txt", &node.etag);
    let node = engine.resolve("Notes.txt").unwrap().unwrap();

    let served = engine.read(node.inode, 0, 64).expect("read the file");
    assert_eq!(
        String::from_utf8_lossy(&served),
        "a much newer version from the web",
        "the default serves the current version, not the outdated local copy"
    );

    assert!(
        engine.write(node.inode, 0, b"an ordinary edit").is_ok(),
        "and the file is writable — the read-only rule is the setting's, not the default's"
    );

    assert!(
        notices.0.lock().unwrap().is_empty(),
        "nothing outdated was served, so nothing was announced"
    );

    std::fs::remove_dir_all(&base).ok();
}
