// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A file changed on the server has to be announced, not merely noticed.
//!
//! The kernel caches a FUSE file's pages and attributes for as long as the
//! frontend's TTL allows. When somebody edits a file in the Nextcloud web
//! interface, nothing in that chain knows — so a re-read served what the kernel
//! remembered until the TTL ran out. Right by the grace of a timer.
//!
//! The syncer sees the ETag move. What was missing is that it said so only for
//! entries *added and removed*, never for one whose contents changed. This
//! holds that line at the level where the decision is made; the frontend turns
//! the announcement into `notify_inval_inode`.
//!
//! Not a test for inotify: FUSE's reverse invalidation produces no fsnotify
//! events, so an editor *watching* a file is still not woken by itself. What is
//! asserted here is that everything downstream is told at all.

mod common;

use std::time::Duration;
use wusel_core::provider::Invalidation;

#[test]
fn a_file_changed_on_the_server_is_announced() {
    let base = std::env::temp_dir().join(format!("wusel-mock-srvchange-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("Notes.txt"), b"hello").unwrap();
    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let mut engine = common::Engine::start(&mock.addr);
    let invalidations = engine.take_invalidations().expect("the channel exists");

    // Load the root, so the syncer has something to compare against.
    let root = engine.list_dir(wusel_core::state::ROOT_INODE);
    assert!(root.iter().any(|n| n.name == "Notes.txt"));
    let before = engine.resolve("Notes.txt").unwrap().unwrap();

    // Somebody edits it in the web interface. Longer than the original, so a
    // stale size would be as visible as stale bytes.
    std::fs::write(fixture.join("Notes.txt"), b"edited in the web interface\n").unwrap();

    // What a notify_push event does. Without one the syncer never walks — that
    // is by design, and it is why this test gives the push itself. Repeatedly,
    // because a push arrives per change and a single one racing the syncer's
    // readiness would test the timing rather than the announcement.
    let push = engine.sync_trigger();
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let announced = loop {
        push();
        match invalidations.recv_timeout(Duration::from_millis(500)) {
            Ok(inv) => {
                if let Invalidation::Content { object, .. } = &inv {
                    if *object == before.inode {
                        break inv;
                    }
                }
                seen.push(inv);
            }
            Err(_) => assert!(
                std::time::Instant::now() < deadline,
                "a changed file was never announced; the syncer said only: {seen:?}"
            ),
        }
    };

    match announced {
        Invalidation::Content { object, path } => {
            assert_eq!(object, before.inode, "the object the frontend has cached");
            assert_eq!(path, "Notes.txt", "and the path the desktop knows it by");
        }
        other => panic!("wrong announcement: {other:?}"),
    }

    std::fs::remove_dir_all(&base).ok();
}
