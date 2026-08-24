// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for background (stale-while-revalidate) directory revalidation: an
//! already-listed directory is served from cache immediately — a server-side
//! change is NOT reflected on that call — and a background PROPFIND then updates
//! the state so a later access sees it. The point is that the slow PROPFIND never
//! runs on the FUSE thread once a directory has been listed once.

mod common;

use std::time::Duration;

use wusel_core::state::ROOT_INODE;

fn names(engine: &common::Engine) -> Vec<String> {
    let mut v: Vec<String> = engine
        .list_dir(ROOT_INODE)
        .into_iter()
        .map(|n| n.name)
        .collect();
    v.sort();
    v
}

#[test]
fn a_listed_directory_is_served_stale_then_revalidated_in_the_background() {
    let base = std::env::temp_dir().join(format!("wusel-mock-bgreval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("a.txt"), b"a").unwrap();

    common::xdg_sandbox(&base);
    // TTL 0 ⇒ an already-listed directory is always due for revalidation, which
    // now happens in the background.
    std::env::set_var("WUSEL_REVALIDATE_SECS", "0");

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let engine = common::Engine::start(&addr);

    // Initial (synchronous) load lists the one file.
    assert_eq!(names(&engine), vec!["a.txt".to_string()]);

    // The server gains a file; the local listing is cached (and now stale).
    std::fs::write(fixture.join("b.txt"), b"b").unwrap();

    // The next access is served from the cache and schedules a refresh nobody
    // waits for. Whether that refresh has already landed by the time we look
    // again is genuinely a race — against an in-process mock a PROPFIND takes
    // microseconds — so the *content* here proves nothing either way and is not
    // asserted. That the caller is never made to wait for the server is proven
    // where it can be: `concurrent_read_e2e` in the FUSE crate, against an
    // injected delay.
    let served = names(&engine);
    assert!(
        served.contains(&"a.txt".to_string()),
        "the cached listing is served, refresh or no refresh"
    );

    // The background PROPFIND completes and a later access applies it.
    let mut saw_b = false;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(50));
        if names(&engine).contains(&"b.txt".to_string()) {
            saw_b = true;
            break;
        }
    }
    assert!(
        saw_b,
        "the refresh nobody asked for must eventually surface b.txt — that is \
         what makes serving a stale listing acceptable in the first place"
    );

    std::env::remove_var("WUSEL_REVALIDATE_SECS");
    std::fs::remove_dir_all(&base).ok();
}
