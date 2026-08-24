// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Two independent reads run **in parallel**, not one after the other.
//!
//! This is the deterministic proof for Etappe 6 (concurrency), the reliable twin
//! of the real-Nextcloud timing step: the mount fixture runs with
//! `dispatch_threads = 4`, every GET is delayed a fixed amount, and two files are
//! read at once. If the engine serves them concurrently the pair finishes in
//! about one delay; if it serialised them it would take two. A fixed per-GET
//! delay (unlike network latency, which throttles throughput and makes parallel
//! transfers merely *share* the pipe) keeps the signal clean.
//!
//! Needs `/dev/fuse`, so it runs only on Linux (the podman container).
#![cfg(target_os = "linux")]

mod common;

use std::time::{Duration, Instant};

#[test]
fn two_reads_run_in_parallel() {
    // Each GET blocks this long in the mock. The two fixture files are online-only
    // (never read before now), so each read is a real, delayed GET.
    const DELAY: Duration = Duration::from_millis(700);
    std::env::set_var("WUSEL_MOCK_GET_DELAY_MS", DELAY.as_millis().to_string());

    let m = common::MountFixture::start("concurrentreads");
    // The mock serves these from the fixture tree; confirm the setup first.
    assert!(
        m.fixture.join("Notes.txt").is_file(),
        "server fixture is set up"
    );
    let a = m.mnt.join("Notes.txt");
    let b = m.mnt.join("Sub Folder/deep.txt");
    assert!(
        a.is_file() && b.is_file(),
        "both files are visible on the mount"
    );

    // Read both at once, on their own threads.
    let start = Instant::now();
    let ta = std::thread::spawn(move || std::fs::read(&a));
    let tb = std::thread::spawn(move || std::fs::read(&b));
    let ra = ta.join().expect("thread a").expect("read Notes.txt");
    let rb = tb.join().expect("thread b").expect("read deep.txt");
    let elapsed = start.elapsed();

    assert_eq!(ra, b"hello");
    assert_eq!(rb, b"nested");

    // Serial would be ~2 × DELAY; parallel is ~1 × DELAY plus overhead. A ceiling
    // of 1.5 × DELAY sits comfortably between the two — it proves the reads
    // overlapped without being a brittle benchmark figure.
    let ceiling = DELAY + DELAY / 2;
    assert!(
        elapsed < ceiling,
        "two {DELAY:?} reads took {elapsed:?} — they did not overlap (serial would be ~{:?})",
        DELAY * 2
    );
}
