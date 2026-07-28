// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Behavioral test of the WebDAV client over real HTTP, against a mock server
//! (`wiremock`) — no Nextcloud needed. This covers the request + response path,
//! not just the parser (which has its own unit test).

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};
use wusel_core::webdav::WebDavClient;

/// A minimal Nextcloud-style multistatus for the user root of `alice`.
const MULTISTATUS: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/alice/</d:href>
    <d:propstat><d:prop>
      <d:resourcetype><d:collection/></d:resourcetype>
      <d:getetag>"root"</d:getetag>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Notes.txt</d:href>
    <d:propstat><d:prop>
      <d:getcontentlength>2048</d:getcontentlength>
      <d:getetag>"abc"</d:getetag>
      <oc:fileid>42</oc:fileid>
      <d:resourcetype/>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Sub%20Folder/</d:href>
    <d:propstat><d:prop>
      <d:resourcetype><d:collection/></d:resourcetype>
      <d:getetag>"def"</d:getetag>
      <oc:fileid>7</oc:fileid>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

#[tokio::test]
async fn propfind_dir_lists_children_over_http() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(207).set_body_raw(MULTISTATUS, "application/xml"))
        .mount(&server)
        .await;

    let dav = WebDavClient::new(reqwest::Client::new(), &server.uri(), "alice", "pw");
    let entries = dav.propfind_dir("").await.expect("propfind should succeed");

    assert_eq!(entries.len(), 2, "the directory itself must be excluded");
    let file = entries
        .iter()
        .find(|e| e.path == "Notes.txt")
        .expect("Notes.txt");
    assert!(!file.is_dir);
    assert_eq!(file.size, 2048);
    assert_eq!(file.file_id, Some(42));
    let dir = entries
        .iter()
        .find(|e| e.path == "Sub Folder")
        .expect("Sub Folder");
    assert!(dir.is_dir, "percent-decoding + collection detection");
}

#[tokio::test]
async fn write_verbs_reach_the_server() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).insert_header("ETag", "\"newetag\""))
        .mount(&server)
        .await;
    Mock::given(method("MKCOL"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("MOVE"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let dav = WebDavClient::new(reqwest::Client::new(), &server.uri(), "alice", "pw");

    let etag = dav.put("Notes.txt", b"hello".to_vec()).await.expect("put");
    assert_eq!(etag.as_deref(), Some("newetag"), "PUT returns the new ETag");
    dav.mkcol("New Folder").await.expect("mkcol");
    dav.delete("Old.txt", false).await.expect("delete");
    dav.move_("A.txt", "B.txt", false).await.expect("move");
}
