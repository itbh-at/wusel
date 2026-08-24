// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A file leaving the cache is announced, like one arriving.
//!
//! Only the arrival used to be. So a blob dropped by the LRU budget left the
//! emblem claiming the file was still there — and the next open quietly went to
//! the network, which is exactly the "cached files sometimes feel as slow as
//! uncached ones" that started this.
//!
//! Eviction is the awkward half: it walks the blob directory by age and size
//! and knows only file ids, never paths. The id is announced and the name
//! resolved on the engine's side, where the state database is.

mod common;

use std::time::Duration;
use wusel_core::provider::Invalidation;

#[test]
fn a_blob_dropped_by_the_budget_is_announced() {
    let base = std::env::temp_dir().join(format!("wusel-mock-evict-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    // Two files, each comfortably over the budget set below, so caching the
    // second must throw the first out.
    std::fs::write(fixture.join("a.txt"), vec![b'a'; 4096]).unwrap();
    std::fs::write(fixture.join("b.txt"), vec![b'b'; 4096]).unwrap();
    common::xdg_sandbox(&base);
    // A budget only one of them fits in.
    let config_dir = wusel_core::config::Account::new("default").config_dir();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[cache]\nmax_size = \"5000\"\n",
    )
    .unwrap();

    let mock = common::Mock::serve(&fixture);
    let mut engine = common::Engine::start(&mock.addr);
    let invalidations = engine.take_invalidations().expect("the channel exists");

    let a = engine.resolve("a.txt").unwrap().unwrap();
    let b = engine.resolve("b.txt").unwrap().unwrap();

    // Read both whole, so both are cached — and the budget has to give.
    let _ = engine.read(a.inode, 0, 4096);
    let _ = engine.read(b.inode, 0, 4096);

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut seen = Vec::new();
    loop {
        match invalidations.recv_timeout(Duration::from_millis(500)) {
            Ok(Invalidation::Entry { path, .. }) if path == "a.txt" && !seen.is_empty() => break,
            Ok(inv) => seen.push(inv),
            Err(_) => assert!(
                std::time::Instant::now() < deadline,
                "an evicted file was never announced; only: {seen:?}"
            ),
        }
    }

    std::fs::remove_dir_all(&base).ok();
}
