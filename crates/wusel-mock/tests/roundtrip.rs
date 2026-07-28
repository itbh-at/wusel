// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end: the real `wusel-core` WebDAV client against an in-process
//! `wusel-mock` server.
//!
//! This is the pure-Rust replacement for ad-hoc container probing — it proves
//! listing (root + subdir, names with spaces), file ids/ETags, and full + range
//! GET all work across the client/mock contract, with no Nextcloud in the loop.

mod common;

use wusel_core::webdav::WebDavClient;

#[tokio::test]
async fn client_lists_and_reads_through_the_mock() {
    let dir = std::env::temp_dir().join(format!("wusel-mock-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("Sub Folder")).unwrap();
    std::fs::write(dir.join("Notes.txt"), b"hello world").unwrap();
    std::fs::write(dir.join("Sub Folder/deep.txt"), b"nested content here").unwrap();

    let mock = common::Mock::serve(&dir);
    let addr = mock.addr.clone();

    let dav = WebDavClient::new(
        reqwest::Client::new(),
        &format!("http://{addr}"),
        "alice",
        "pw",
    );

    // Root listing: the file, the subdirectory (with its space), no self-entry.
    let root = dav.propfind_dir("").await.expect("propfind root");
    assert_eq!(
        root.len(),
        2,
        "root has Notes.txt and Sub Folder, not itself"
    );
    let file = root
        .iter()
        .find(|e| e.path == "Notes.txt")
        .expect("Notes.txt");
    assert!(!file.is_dir);
    assert_eq!(file.size, 11);
    assert!(file.file_id.is_some(), "mock supplies an oc:fileid");
    let sub = root
        .iter()
        .find(|e| e.path == "Sub Folder")
        .expect("Sub Folder");
    assert!(
        sub.is_dir,
        "collection detection across a percent-encoded name"
    );

    // Subdirectory listing returns paths relative to the user root.
    let kids = dav
        .propfind_dir("Sub Folder")
        .await
        .expect("propfind subdir");
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].path, "Sub Folder/deep.txt");

    // Full GET and a range GET (the hydration path).
    let whole = dav.get("Notes.txt", None).await.expect("get whole");
    assert_eq!(&whole[..], b"hello world");
    let part = dav.get("Notes.txt", Some((0, 5))).await.expect("get range");
    assert_eq!(&part[..], b"hello", "range GET must honour bytes=0-4");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn client_writes_through_the_mock() {
    let dir = std::env::temp_dir().join(format!("wusel-mock-write-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mock = common::Mock::serve(&dir);
    let addr = mock.addr.clone();

    let dav = WebDavClient::new(
        reqwest::Client::new(),
        &format!("http://{addr}"),
        "alice",
        "pw",
    );

    // PUT a new file, then read it back.
    let etag = dav.put("Note.txt", b"first".to_vec()).await.expect("put");
    assert!(etag.is_some(), "PUT returns an ETag");
    assert_eq!(&dav.get("Note.txt", None).await.unwrap()[..], b"first");

    // MKCOL + PUT into the new directory; PROPFIND sees it.
    dav.mkcol("Docs").await.expect("mkcol");
    dav.put("Docs/inner.txt", b"nested".to_vec())
        .await
        .expect("put nested");
    let docs = dav.propfind_dir("Docs").await.expect("propfind Docs");
    assert!(docs.iter().any(|e| e.path == "Docs/inner.txt"));

    // MOVE (rename), then the old path is gone and the new one has the content.
    dav.move_("Note.txt", "Renamed.txt", false)
        .await
        .expect("move");
    assert_eq!(&dav.get("Renamed.txt", None).await.unwrap()[..], b"first");
    assert!(
        dav.get("Note.txt", None).await.is_err(),
        "the old path is gone"
    );

    // DELETE removes it.
    dav.delete("Renamed.txt", false).await.expect("delete");
    assert!(
        dav.get("Renamed.txt", None).await.is_err(),
        "deleted file is gone"
    );

    std::fs::remove_dir_all(&dir).ok();
}
