// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! "Update now": bring an outdated offline copy back in step, on request.
//!
//! What is asserted here is the contract, not the timing. Background hydration
//! may re-fetch a stale blob at any moment on its own, so asserting *that a
//! fetch happened* would be a race against the code under test — and a test
//! that has to win such a race says nothing about either side. The contract
//! does not race: it refuses paths that are not pinned, it reports nothing to
//! do when the copy is current, and it never drops the eviction marker.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn updating_refuses_what_it_does_not_promise_and_is_idempotent() {
    let base = std::env::temp_dir().join(format!("wusel-mock-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("kept.txt"), b"keep me offline").unwrap();
    std::fs::write(fixture.join("loose.txt"), b"ordinary file").unwrap();

    common::xdg_sandbox(&base);
    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();
    let mut engine = common::Engine::start(&addr);

    let kept = engine.resolve("kept.txt").unwrap().expect("kept.txt");
    engine.pin("kept.txt").unwrap();

    // A pin promises the file is there when the server is not. Nothing else
    // makes that promise, so nothing else has anything to keep up to date —
    // and offering to "update" an ordinary file would imply a guarantee it does
    // not have.
    let refused = engine.provider().refresh("loose.txt");
    assert!(
        refused.is_err(),
        "updating an unpinned file must be refused, not silently pinned"
    );

    // A pinned copy that is already current: nothing to fetch, and saying so is
    // the answer rather than a redundant download.
    assert_eq!(
        engine.provider().refresh("kept.txt").unwrap(),
        0,
        "a copy already in step is not fetched again"
    );

    // The eviction marker survives an update. This is why it is its own verb:
    // unpin-then-pin would drop the marker first, so a re-download that failed
    // would leave the file outdated *and* unprotected.
    let blobs = wusel_core::config::Account::new("default").blob_cache_dir();
    let marker = blobs.join(format!("{}.pin", kept.file_id.expect("a server file id")));
    assert!(marker.is_file(), "the file was pinned to begin with");
    let _ = engine.provider().refresh("kept.txt");
    assert!(
        marker.is_file(),
        "and it is still protected after an update"
    );

    // The whole account is a legitimate target: it is where a pin can also sit.
    engine.pin("").unwrap();
    assert!(
        engine.provider().refresh("").is_ok(),
        "the account-wide pin can be brought up to date too"
    );
    let _ = ROOT_INODE;

    drop(engine);
    std::fs::remove_dir_all(&base).ok();
}
