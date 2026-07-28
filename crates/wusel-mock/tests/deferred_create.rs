// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for deferred creation: `create` must not touch the server; the
//! file is materialised on the first flush. A file created and deleted before
//! any flush (an editor probe/temp file) never contacts the server at all.

mod common;

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::{StateDb, ROOT_INODE};
use wusel_core::webdav::WebDavClient;

#[test]
fn create_defers_upload_until_flush_and_temp_files_never_hit_the_server() {
    let base = std::env::temp_dir().join(format!("wusel-mock-defer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

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

    // 1. Deferred create: nothing on the server yet.
    let node = provider.create(ROOT_INODE, "new.txt").unwrap();
    assert!(
        !fixture.join("new.txt").exists(),
        "create must not upload anything"
    );
    // Readable before any flush — from the scratch — and still empty.
    assert!(provider.read(node.inode, 0, 16).unwrap().is_empty());

    // 2. Write, then flush → the file appears on the server with its content.
    provider.write(node.inode, 0, b"hello").unwrap();
    assert_eq!(provider.read(node.inode, 0, 16).unwrap(), b"hello");
    assert!(!fixture.join("new.txt").exists(), "still local until flush");
    provider.flush(node.inode).unwrap();
    assert_eq!(std::fs::read(fixture.join("new.txt")).unwrap(), b"hello");

    // 3. A file created and deleted before any flush never reaches the server —
    //    and delete must not error (there is nothing to DELETE remotely).
    let tmp = provider.create(ROOT_INODE, ".new.txt.swp").unwrap();
    provider.write(tmp.inode, 0, b"swapdata").unwrap();
    provider.remove(ROOT_INODE, ".new.txt.swp").unwrap();
    assert!(
        !fixture.join(".new.txt.swp").exists(),
        "a temp file created and deleted before flush must never materialise"
    );

    std::fs::remove_dir_all(&base).ok();
}
