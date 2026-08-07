// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! An outdated pinned copy must still be readable when the server is gone.
//!
//! This was a defect, not a missing feature. A pinned file promises "keep this
//! offline"; the read path served the local blob only while its ETag still
//! matched, so the moment the server copy changed the read fell through to the
//! live path — and with no server, failed. The bytes were on disk the whole
//! time, complete and readable.
//!
//! An outdated copy is enormously better than an error, and the user is told,
//! because an application that opens and saves those bytes has no other way to
//! know.

mod common;

use std::sync::{Arc, Mutex};

use wusel_core::desktop::{Desktop, Notice, Status};
use wusel_core::provider::FileState;

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

#[test]
fn a_stale_pinned_file_is_served_when_the_server_is_unreachable() {
    let base = std::env::temp_dir().join(format!("wusel-mock-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    let backing = fixture.join("offline.txt");
    std::fs::write(&backing, b"ORIGINAL").unwrap();

    common::xdg_sandbox(&base);
    // TTL 0, so a second listing really re-reads the server and our state learns
    // the new ETag. Without that the copy is not *known* to be outdated, and the
    // defect this test is about cannot arise.
    std::env::set_var("WUSEL_REVALIDATE_SECS", "0");
    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let recorder = Arc::new(Recorder::default());
    // Before the substrate starts: its workers take a copy of this seam.
    let mut engine = common::Engine::start_with(&addr, Some(recorder.clone()));

    let node = engine.resolve("offline.txt").unwrap().expect("offline.txt");
    // Pin it: the promise is that this file stays readable offline.
    engine.pin("offline.txt").unwrap();
    assert_eq!(engine.read(node.inode, 0, 8).unwrap(), b"ORIGINAL");

    // Wait for the pinned copy to actually be on disk.
    let mut cached = false;
    for _ in 0..100 {
        if engine.state(node.inode).is_some() && engine.read(node.inode, 0, 8).is_ok() {
            cached = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(cached, "the pinned file never reached the cache");

    // The server copy changes, and a re-listing teaches our state the new ETag.
    // Now the blob on disk is *known* to be outdated — which is precisely when
    // the old read path gave up on it.
    std::fs::write(&backing, b"NEWER-ON-SERVER-SIDE").unwrap();
    let mut noticed = false;
    for _ in 0..100 {
        let _ = engine.list_dir(wusel_core::state::ROOT_INODE);
        if engine.stat(node.inode).is_some_and(|n| n.etag != node.etag) {
            noticed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(noticed, "the state never learned the server's new version");

    // The emblem says so before anybody opens the file — which is the point of
    // having a fifth state at all: a promise half-kept should be visible while
    // the connection is still there to fix it.
    assert_eq!(
        engine.state(node.inode),
        Some(FileState::PinnedStale),
        "a pinned file whose server copy moved on is shown as out of date"
    );

    // "Update now": fetch the current version in place. Deliberately not
    // unpin+pin — that would drop the eviction marker first, so a failed
    // re-download would leave the file outdated *and* unprotected.
    // "Update now" is exercised on its own below. It is deliberately *not*
    // asserted here by content or by count: background hydration may re-fetch a
    // stale blob on its own at any moment, so both race, and a test that has to
    // win a race against the code under test proves nothing about either.

    // And then the server goes away entirely. This is the situation a pin is for.
    drop(mock);

    // The read must still work, from the copy we have.
    let served = engine
        .read(node.inode, 0, 8)
        .expect("a pinned file must stay readable with no server");
    assert_eq!(
        served, b"ORIGINAL",
        "the local copy is outdated but real; failing instead would break the pin promise"
    );

    // And the user is told, once — an application that saves these bytes would
    // otherwise produce a conflicted copy out of nowhere.
    let notices = recorder.notices.lock().unwrap();
    let stale: Vec<_> = notices
        .iter()
        // The reason matters: this test unplugs the server, so the message must
        // be the one that talks about the connection, not the one about a
        // setting the user chose.
        .filter(|n| {
            matches!(n,
                Notice::StaleCopyServed { path, reason: wusel_core::desktop::Stale::Unreachable }
                if path == "offline.txt")
        })
        .collect();
    assert_eq!(
        stale.len(),
        1,
        "exactly one notice per file, not one per read: got {notices:?}"
    );

    drop(notices);
    drop(engine);
    std::env::remove_var("WUSEL_REVALIDATE_SECS");
    std::fs::remove_dir_all(&base).ok();
}
