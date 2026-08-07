// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `[sync] open_pinned = "offline"`: opening an out-of-date pinned file serves
//! the copy that is already here.
//!
//! The default is the other way round, and rightly so on a desk: the local copy
//! no longer matches, so read the current one. On a train it is wrong — a pin
//! exists so the file is *there*, and a hotel connection can make "there" cost
//! more than the outdated copy is worth. This setting hands that judgement to
//! the person paying for the connection.
//!
//! The stale copy is never served silently; the notification is asserted here
//! too, because an application that opens it and saves produces a conflict
//! nobody saw coming.

mod common;

use std::sync::{Arc, Mutex};
use wusel_core::desktop::{Desktop, Notice};

/// Records what the user would have been shown.
#[derive(Default)]
struct Notices(Mutex<Vec<String>>);

impl Desktop for Notices {
    fn set_status(&self, _status: wusel_core::desktop::Status) {}

    fn notify(&self, notice: &Notice) {
        if let Notice::StaleCopyServed { path, reason } = notice {
            assert_eq!(
                *reason,
                wusel_core::desktop::Stale::ByChoice,
                "the setting served this, not a missing server — and the two \
                 messages give different advice"
            );
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(path.clone());
        }
    }
}

#[test]
fn an_outdated_pinned_file_opens_from_disk_and_says_so() {
    let base = std::env::temp_dir().join(format!("wusel-mock-openpin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("Notes.txt"), b"the offline version").unwrap();
    common::xdg_sandbox(&base);

    let config_dir = wusel_core::config::Account::new("default").config_dir();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[sync]\nopen_pinned = \"offline\"\n",
    )
    .unwrap();

    let mock = common::Mock::serve(&fixture);
    let notices = Arc::new(Notices::default());
    let mut engine = common::Engine::start_with(&mock.addr, Some(notices.clone()));

    // A second file, NOT pinned but cached, so we can prove the setting does not
    // touch it: an unpinned copy promises nothing, and serving it outdated would
    // be a bug rather than a policy.
    std::fs::write(fixture.join("Other.txt"), b"the cached version").unwrap();
    engine.list_dir(wusel_core::state::ROOT_INODE);
    let other = engine.resolve("Other.txt").unwrap().unwrap();
    let _ = engine.read(other.inode, 0, 64); // read once → cached

    // Pin the first, so a complete local copy exists and is protected.
    engine.pin("Notes.txt").expect("pin the file");
    let node = engine.resolve("Notes.txt").unwrap().unwrap();

    // Both server copies move on; the engine learns of it the way it really
    // does — a push, then the walk that reconciles the new versions.
    std::fs::write(
        fixture.join("Notes.txt"),
        b"a much newer version from the web",
    )
    .unwrap();
    std::fs::write(fixture.join("Other.txt"), b"a newer cached version").unwrap();
    engine.wait_until_stale("Notes.txt", &node.etag);

    let served = engine.read(node.inode, 0, 64).expect("read the file");
    assert_eq!(
        String::from_utf8_lossy(&served),
        "the offline version",
        "the copy on disk is what an `offline` open serves"
    );

    let told = notices.0.lock().unwrap().clone();
    assert_eq!(
        told,
        vec!["Notes.txt".to_string()],
        "handing out outdated bytes is announced, exactly once — and only for \
         the pinned file, never the unpinned one"
    );

    // The unpinned file is not the setting's business: it serves the current
    // version, live, and stays writable. `open_pinned` is about pins only.
    let other = engine.resolve("Other.txt").unwrap().unwrap();
    let fresh = engine
        .read(other.inode, 0, 64)
        .expect("read the unpinned file");
    assert_eq!(
        String::from_utf8_lossy(&fresh),
        "a newer cached version",
        "an unpinned file goes live when stale, whatever open_pinned says"
    );
    assert!(
        wusel_core::model::is_writable(&other.permissions, false),
        "and stays writable"
    );

    // And it is read-only while it is outdated. An edit here would be based on
    // bytes the user never saw — the buffer is seeded from the server — and
    // would replace the newer version without raising a conflict.
    let refused = engine.write(node.inode, 0, b"an edit of the old version");
    assert_eq!(
        refused,
        Err(wusel_fsm::Outcome::Failed(wusel_fsm::Failure::NotWritable)),
        "an outdated offline copy is read-only, whatever made it outdated"
    );

    std::fs::remove_dir_all(&base).ok();
}
