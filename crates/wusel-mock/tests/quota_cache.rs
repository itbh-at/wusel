// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Behavioural test of [`QuotaCache`] against a real `wusel-mock` server — the
//! caching/refresh logic on top of the wire-level `WebDavClient::quota` test in
//! `webdav_client.rs`.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use wusel_core::model::Quota;
use wusel_core::runtime::QuotaCache;
use wusel_core::webdav::WebDavClient;

fn client_for(addr: &str) -> WebDavClient {
    WebDavClient::new(
        reqwest::Client::new(),
        &format!("http://{addr}"),
        "alice",
        "pw",
    )
}

#[test]
fn a_stale_cache_answers_at_once_and_refreshes_in_the_background() {
    let base = std::env::temp_dir().join(format!("wusel-mock-quota-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("Notes.txt"), vec![b'x'; 4096]).unwrap();

    let mock = common::Mock::serve(&fixture);
    let dav = client_for(&mock.addr);
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build a runtime"),
    );
    let cache = Arc::new(QuotaCache::new(
        dav,
        Arc::clone(&rt),
        Duration::from_millis(20),
    ));

    // Nothing has landed yet: the very first call must answer immediately with
    // the zero default, not block on the network — it only kicks a fetch off
    // in the background for next time.
    let before_any_fetch = Instant::now();
    let first = cache.snapshot();
    assert!(
        before_any_fetch.elapsed() < Duration::from_millis(200),
        "snapshot must never block on the network"
    );
    assert_eq!(first, Quota::default());

    // Poll until the background fetch has landed (bounded, so a real failure
    // fails the test instead of hanging it).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = cache.snapshot();
    while seen.available.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        seen = cache.snapshot();
    }
    assert_eq!(seen.used, 4096, "the mock reports the fixture's real size");
    assert_eq!(seen.available, Some(1_000_000_000));

    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn priming_fetches_without_anyone_asking_for_a_value() {
    // What the mount does at start-up: without it, the *first* `df` after a
    // mount is the call that triggers the fetch — and it cannot wait for it, so
    // it always reports the placeholder.
    let base = std::env::temp_dir().join(format!("wusel-mock-quota-prime-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("Notes.txt"), vec![b'x'; 4096]).unwrap();

    let mock = common::Mock::serve(&fixture);
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build a runtime"),
    );
    let cache = Arc::new(QuotaCache::new(
        client_for(&mock.addr),
        Arc::clone(&rt),
        Duration::from_secs(60),
    ));

    cache.prime();

    // The value has to arrive without a single `snapshot()` having asked for it
    // first — polling here reads the cache, but priming is what filled it.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = cache.snapshot();
    while seen.available.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        seen = cache.snapshot();
    }
    assert_eq!(
        seen.available,
        Some(1_000_000_000),
        "priming must have fetched the quota on its own"
    );

    std::fs::remove_dir_all(&base).ok();
}
