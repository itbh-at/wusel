// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for chunked upload NG: a file larger than one chunk is uploaded
//! in parts (MKCOL + PUT chunks + MOVE assemble) and reassembled byte-for-byte
//! on the server side.

mod common;

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::{StateDb, ROOT_INODE};
use wusel_core::webdav::WebDavClient;

#[test]
fn large_file_uploads_in_chunks_and_reassembles() {
    let base = std::env::temp_dir().join(format!("wusel-mock-chunk-{}", std::process::id()));
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

    // ~9 MiB — larger than one 4 MiB chunk, so the upload is chunked.
    let data: Vec<u8> = (0..9 * 1024 * 1024).map(|i| (i % 251) as u8).collect();

    let node = provider.create(ROOT_INODE, "big.bin").expect("create");
    provider.write(node.inode, 0, &data).expect("write");
    provider.flush(node.inode).expect("flush");

    // The server reassembled the file byte-for-byte.
    assert_eq!(std::fs::read(fixture.join("big.bin")).unwrap(), data);

    std::fs::remove_dir_all(&base).ok();
}
