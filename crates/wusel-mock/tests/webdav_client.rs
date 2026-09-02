// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Behavioural test of the WebDAV client over real HTTP, against `wusel-mock` —
//! no Nextcloud needed. This covers the request + response path, not just the
//! parser (which has its own unit test in `webdav.rs`).
//!
//! These two live in one file, unlike the rest of this suite: they drive
//! [`WebDavClient`] directly and never touch an [`Account`], so they do not call
//! `common::xdg_sandbox` and are not bound by its one-test-per-binary rule.
//!
//! Because the mock serves a **real directory**, the fixtures are staged by
//! creating files, and the write verbs can be checked by their effect on disk —
//! a stronger assertion than "the client accepted the canned status code".

mod common;

use wusel_core::webdav::WebDavClient;

/// A client for `mock`, as user `alice` (what the shared harness serves).
fn client_for(addr: &str) -> WebDavClient {
    WebDavClient::new(
        reqwest::Client::new(),
        &format!("http://{addr}"),
        "alice",
        "pw",
    )
}

#[tokio::test]
async fn propfind_dir_lists_children_over_http() {
    let base = std::env::temp_dir().join(format!("wusel-mock-dav-list-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    // A space in the directory name on purpose: its href is percent-encoded by
    // the server, so the client's decoding is exercised, not assumed.
    std::fs::create_dir_all(fixture.join("Sub Folder")).unwrap();
    std::fs::write(fixture.join("Notes.txt"), vec![b'x'; 2048]).unwrap();

    let mock = common::Mock::serve(&fixture);
    let dav = client_for(&mock.addr);

    let entries = dav.propfind_dir("").await.expect("propfind should succeed");

    assert_eq!(entries.len(), 2, "the directory itself must be excluded");
    let file = entries
        .iter()
        .find(|e| e.path == "Notes.txt")
        .expect("Notes.txt");
    assert!(!file.is_dir);
    assert_eq!(file.size, 2048);
    assert!(file.file_id.is_some(), "the server reports a file id");
    let dir = entries
        .iter()
        .find(|e| e.path == "Sub Folder")
        .expect("Sub Folder");
    assert!(dir.is_dir, "percent-decoding + collection detection");

    std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn quota_reaches_the_server_over_http() {
    let base = std::env::temp_dir().join(format!("wusel-mock-dav-quota-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("Notes.txt"), vec![b'x'; 4096]).unwrap();

    let mock = common::Mock::serve(&fixture);
    let dav = client_for(&mock.addr);

    let quota = dav.quota().await.expect("quota should succeed");

    assert_eq!(quota.used, 4096, "the mock reports the fixture's real size");
    assert_eq!(quota.available, Some(1_000_000_000));

    std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn write_verbs_reach_the_server() {
    let base = std::env::temp_dir().join(format!("wusel-mock-dav-write-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("Old.txt"), b"obsolete").unwrap();
    std::fs::write(fixture.join("A.txt"), b"moved").unwrap();

    let mock = common::Mock::serve(&fixture);
    let dav = client_for(&mock.addr);

    let etag = dav.put("Notes.txt", b"hello".to_vec()).await.expect("put");
    assert!(etag.is_some(), "PUT returns the new ETag");
    assert_eq!(std::fs::read(fixture.join("Notes.txt")).unwrap(), b"hello");

    dav.mkcol("New Folder").await.expect("mkcol");
    assert!(fixture.join("New Folder").is_dir());

    dav.delete("Old.txt", false).await.expect("delete");
    assert!(!fixture.join("Old.txt").exists());

    dav.move_("A.txt", "B.txt", false).await.expect("move");
    assert!(!fixture.join("A.txt").exists());
    assert_eq!(std::fs::read(fixture.join("B.txt")).unwrap(), b"moved");

    std::fs::remove_dir_all(&base).ok();
}
