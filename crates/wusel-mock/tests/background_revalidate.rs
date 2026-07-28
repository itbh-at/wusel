// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for background (stale-while-revalidate) directory revalidation: an
//! already-listed directory is served from cache immediately — a server-side
//! change is NOT reflected on that call — and a background PROPFIND then updates
//! the state so a later access sees it. The point is that the slow PROPFIND never
//! runs on the FUSE thread once a directory has been listed once.

mod common;

use std::time::Duration;

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::{StateDb, ROOT_INODE};
use wusel_core::webdav::WebDavClient;

fn names(provider: &mut Provider) -> Vec<String> {
    let mut v: Vec<String> = provider
        .list_dir(ROOT_INODE)
        .unwrap()
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

    let account = Account::new("default");
    let dav = WebDavClient::new(
        reqwest::Client::new(),
        &format!("http://{addr}"),
        "alice",
        "pw",
    );
    std::fs::create_dir_all(account.state_db_path().parent().unwrap()).unwrap();
    let state = StateDb::open(&account.state_db_path()).unwrap();
    let mut provider = Provider::new(dav, state, &account).unwrap();

    // Initial (synchronous) load lists the one file.
    assert_eq!(names(&mut provider), vec!["a.txt".to_string()]);

    // The server gains a file; the local listing is cached (and now stale).
    std::fs::write(fixture.join("b.txt"), b"b").unwrap();

    // The very next access is served from the cache — it schedules a background
    // revalidation but must NOT block on it, so it still shows only a.txt.
    assert_eq!(
        names(&mut provider),
        vec!["a.txt".to_string()],
        "a stale listing is served immediately, without waiting for the PROPFIND"
    );

    // The background PROPFIND completes and a later access applies it.
    let mut saw_b = false;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(50));
        if names(&mut provider).contains(&"b.txt".to_string()) {
            saw_b = true;
            break;
        }
    }
    assert!(
        saw_b,
        "the background revalidation must eventually surface b.txt"
    );

    std::env::remove_var("WUSEL_REVALIDATE_SECS");
    std::fs::remove_dir_all(&base).ok();
}
