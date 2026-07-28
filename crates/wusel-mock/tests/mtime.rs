// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for mtime propagation: a mtime set via `setattr` is sent as
//! `X-OC-Mtime` on the upload, so the server-side file carries it (what
//! `cp -p` / `rsync -t` rely on).

mod common;

use std::time::UNIX_EPOCH;

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::StateDb;
use wusel_core::webdav::WebDavClient;

#[test]
fn setattr_mtime_is_propagated_on_upload() {
    let base = std::env::temp_dir().join(format!("wusel-mock-mtime-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    let backing = fixture.join("note.txt");
    std::fs::write(&backing, b"hello").unwrap();

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

    let node = provider.resolve("note.txt").unwrap().expect("note.txt");

    // A specific past timestamp (2020-09-13T12:26:40Z).
    let target = 1_600_000_000i64;
    provider.write(node.inode, 0, b"edited").unwrap();
    provider.set_mtime(node.inode, target).unwrap();
    provider.flush(node.inode).unwrap();

    let got = std::fs::metadata(&backing)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(
        got as i64, target,
        "the server file must carry the set mtime"
    );

    std::fs::remove_dir_all(&base).ok();
}
