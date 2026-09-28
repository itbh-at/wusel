// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The sync walk asks the root's ETag first (Depth 0) and stops there when the
//! tree has not changed since the last walk — no Depth-1 listing of the root.
//! A change after such a skip is still found.

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use wusel_mock::{ROOT_ETAG_PROBES, ROOT_LISTINGS};

/// Wait until `counter` reaches `n`, or fail naming what never happened.
fn wait_for(counter: &std::sync::atomic::AtomicUsize, n: usize, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while counter.load(Ordering::SeqCst) < n {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_unchanged_root_skips_the_listing_and_a_later_change_is_still_found() {
    let base = std::env::temp_dir().join(format!("wusel-mock-rootetag-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("A")).unwrap();
    std::fs::write(fixture.join("A").join("keep.txt"), b"k").unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let mut engine = common::Engine::start(&mock.addr);
    let a = engine.resolve("A").unwrap().expect("A");
    // List A now, so finding `new.txt` later is the walk's doing — the walk only
    // descends into directories already listed.
    assert_eq!(engine.list_dir(a.inode).len(), 1);
    let sync = engine.sync_trigger();

    // First walk: nothing recorded yet, so it lists the root.
    let listings = ROOT_LISTINGS.load(Ordering::SeqCst);
    sync();
    wait_for(&ROOT_ETAG_PROBES, 1, "the first root ETag probe");
    wait_for(
        &ROOT_LISTINGS,
        listings + 1,
        "the first walk's root listing",
    );

    // Second trigger, nothing changed. Waiting for its probe keeps it from
    // coalescing with the third; the settle time lets it finish deciding.
    sync();
    wait_for(&ROOT_ETAG_PROBES, 2, "the second root ETag probe");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        ROOT_LISTINGS.load(Ordering::SeqCst),
        listings + 1,
        "an unchanged root must not be listed again"
    );

    // A change deep enough to need the walk, then a third trigger.
    std::fs::write(fixture.join("A").join("new.txt"), b"n").unwrap();
    sync();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !engine.list_dir(a.inode).iter().any(|n| n.name == "new.txt") {
        assert!(
            Instant::now() < deadline,
            "the change after a skip was never found"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(ROOT_LISTINGS.load(Ordering::SeqCst), listings + 2);

    std::fs::remove_dir_all(&base).ok();
}
