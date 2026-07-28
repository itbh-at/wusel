// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end: a failed upload must NOT lose the buffered edit. The mock fails
//! the first `PUT` to a `*.fail-once` file (a stand-in for the 500 a real server
//! returned on an editor's swap file); the client must keep the scratch so the
//! next flush retries and the content still reaches the server.

mod common;

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::StateDb;
use wusel_core::webdav::WebDavClient;

#[test]
fn a_failed_flush_keeps_the_buffer_and_a_retry_uploads() {
    let base = std::env::temp_dir().join(format!("wusel-mock-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    // The server-side file whose FIRST upload the mock rejects with 500.
    let backing = fixture.join("note.fail-once");
    std::fs::write(&backing, b"abcdef").unwrap();

    common::xdg_sandbox(&base);

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

    let node = provider
        .resolve("note.fail-once")
        .unwrap()
        .expect("note.fail-once exists");

    provider.write(node.inode, 2, b"XY").unwrap();

    // First flush: the server rejects the upload (500). The flush errors — but the
    // buffered edit must be preserved, and the server file untouched.
    assert!(
        provider.flush(node.inode).is_err(),
        "the injected 500 must surface as a flush error"
    );
    assert_eq!(
        std::fs::read(&backing).unwrap(),
        b"abcdef",
        "a failed upload must not change the server file"
    );

    // Retry: the buffer is still there, so a second flush uploads successfully.
    provider.flush(node.inode).unwrap();
    assert_eq!(
        std::fs::read(&backing).unwrap(),
        b"abXYef",
        "the retry must upload the buffered edit — no data lost"
    );

    std::fs::remove_dir_all(&base).ok();
}
