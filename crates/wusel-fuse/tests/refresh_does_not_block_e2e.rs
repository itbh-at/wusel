// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A background refresh must not put the next caller behind itself.
//!
//! Measured on a real desktop before this held: `ls` took 20 ms, the next `ls`
//! took 2.9 seconds, and the one after that 20 ms again. The middle one had
//! queued behind the PROPFIND the first one asked for — the refresh claimed the
//! directory, and everything keyed on that directory (a second listing, and the
//! `lookup` a file manager makes for every visible entry) waited for it.
//!
//! Here the same shape is produced deliberately: a one-second revalidation
//! interval and a PROPFIND the mock holds for a second and a half, so a listing
//! that waits for the refresh is unmistakable in the timing.
//!
//! Linux only, and needs `/dev/fuse` — it runs in the container.

mod common;

use std::time::{Duration, Instant};

/// Generous, because it is not measuring speed: anything below it means the
/// listing was served locally, anything near the PROPFIND delay means it
/// waited. The two are an order of magnitude apart on purpose.
const SERVED_LOCALLY: Duration = Duration::from_millis(600);
const PROPFIND_DELAY_MS: u64 = 1500;

#[test]
fn a_listing_is_not_delayed_by_the_refresh_it_triggers() {
    // Both must be set before the mount and the mock start.
    std::env::set_var("WUSEL_TEST_REVALIDATE_SECS", "1");
    std::env::set_var(
        "WUSEL_MOCK_PROPFIND_DELAY_MS",
        PROPFIND_DELAY_MS.to_string(),
    );

    let m = common::MountFixture::start("refreshblock");

    // Load the listing once, so the directory's children are known.
    let entries = std::fs::read_dir(&m.mnt).unwrap().count();
    assert!(entries > 0, "the fixture has entries");

    // Let it go stale.
    std::thread::sleep(Duration::from_millis(1200));

    // This one is served from what we have — and asks for a refresh.
    let first = Instant::now();
    let n1 = std::fs::read_dir(&m.mnt).unwrap().count();
    let first = first.elapsed();
    assert_eq!(n1, entries);

    // And this one arrives while that refresh is still in flight. It must not
    // wait for it: we already have an answer, and the whole point of refreshing
    // in the background is that nobody is blocked by it.
    let second = Instant::now();
    let n2 = std::fs::read_dir(&m.mnt).unwrap().count();
    let second = second.elapsed();
    assert_eq!(n2, entries);

    assert!(
        second < SERVED_LOCALLY,
        "the second listing waited for the background refresh: {second:?} \
         (the refresh takes {PROPFIND_DELAY_MS} ms; the first listing took {first:?})"
    );
}
