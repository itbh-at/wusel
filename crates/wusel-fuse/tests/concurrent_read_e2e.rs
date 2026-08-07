// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A read in flight must not block unrelated FUSE operations.
//!
//! With a single dispatch thread and a synchronous `read`, a slow network fetch
//! monopolises the dispatch loop: a concurrent `stat` on another file queues
//! behind it and the mount appears frozen. Etappe 1 hands the fetch to a blocking
//! task, so the dispatch thread returns at once and the `stat` is served while
//! the read is still waiting on the network.
//!
//! We make the mock's GET artificially slow (`WUSEL_MOCK_GET_DELAY_MS`) so one
//! read blocks for seconds, then assert a `stat` on a *different* file returns
//! long before that read finishes. On the pre-Etappe-1 code this test fails: the
//! stat is served only once the read completes.
//!
//! Needs `/dev/fuse`, so it runs only on Linux (the podman container).
#![cfg(target_os = "linux")]

mod common;

use std::time::{Duration, Instant};

#[test]
fn a_stat_is_served_while_a_slow_read_is_in_flight() {
    // Every GET now takes this long. The fixture files are tiny, so a read is a
    // single GET — one clearly-slow read callback, which is all the test needs.
    // Set before the mount starts so the mock picks it up (it reads the var per
    // request). PROPFIND is not delayed, so mounting itself stays fast.
    const GET_DELAY: Duration = Duration::from_secs(4);
    std::env::set_var("WUSEL_MOCK_GET_DELAY_MS", GET_DELAY.as_millis().to_string());

    let m = common::MountFixture::start("concurrentread");
    // The mock serves this from the fixture tree; confirm the setup before timing.
    assert!(
        m.fixture.join("Notes.txt").is_file(),
        "server fixture is set up"
    );
    // `Notes.txt` is online-only (never read yet), so reading it hits the network
    // and stalls on the delayed GET. `deep.txt` is only ever stat-ed — served
    // from local state, no GET — so its latency measures the dispatch thread, not
    // the link.
    let slow = m.mnt.join("Notes.txt");
    let other = m.mnt.join("Sub Folder/deep.txt");
    assert!(other.exists(), "fixture tree exists before the slow read");

    // Read the online-only file on another thread; it blocks on the delayed GET.
    let reader = std::thread::spawn(move || {
        let start = Instant::now();
        let data = std::fs::read(&slow).expect("read Notes.txt");
        (start.elapsed(), data)
    });

    // Let the read get in flight: its GET is now sleeping inside the mock, so the
    // dispatch thread is either free (fixed) or stuck on the fetch (old).
    std::thread::sleep(Duration::from_millis(500));

    let stat_start = Instant::now();
    let meta = std::fs::metadata(&other).expect("stat deep.txt while the read is in flight");
    let stat_elapsed = stat_start.elapsed();
    assert!(meta.is_file());

    let (read_elapsed, data) = reader.join().expect("reader thread");
    assert_eq!(
        data, b"hello",
        "the slow read still returns the right bytes"
    );

    // Sanity: the delay really was in effect (otherwise the test proves nothing).
    assert!(
        read_elapsed >= GET_DELAY - Duration::from_millis(500),
        "read took {read_elapsed:?}, expected to reflect the {GET_DELAY:?} GET delay"
    );
    // The point: the stat was NOT queued behind the multi-second read. A getattr
    // served off the dispatch thread returns in milliseconds; 1 s is a generous
    // ceiling that a queued stat (~{GET_DELAY} − 0.5 s) would blow past.
    assert!(
        stat_elapsed < Duration::from_secs(1),
        "stat took {stat_elapsed:?}; it queued behind the in-flight read (read was {read_elapsed:?})"
    );
}
