// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for the ignore list: an ephemeral editor/OS file (matched by an
//! ignore pattern) is kept purely local — it never reaches the server — yet is
//! fully usable through the mount. And an ignored temp file renamed onto a real
//! name is "promoted": its content is uploaded under the new name (the atomic-
//! save pattern of office suites).

mod common;

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::{StateDb, ROOT_INODE};
use wusel_core::webdav::WebDavClient;

fn provider_for(addr: &str, fixture: &std::path::Path) -> Provider {
    let account = Account::new("default");
    let dav = WebDavClient::new(
        reqwest::Client::new(),
        &format!("http://{addr}"),
        "alice",
        "pw",
    );
    std::fs::create_dir_all(account.state_db_path().parent().unwrap()).unwrap();
    let state = StateDb::open(&account.state_db_path()).unwrap();
    let _ = fixture; // fixture is served by the mock; provider talks over HTTP
    Provider::new(dav, state, &account).unwrap()
}

#[test]
fn ignored_files_stay_local_and_promote_on_rename() {
    let base = std::env::temp_dir().join(format!("wusel-mock-ignore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let mut provider = provider_for(&addr, &fixture);

    // 1. A LibreOffice lock file: kept purely local through its whole lifecycle.
    let lock = provider.create(ROOT_INODE, ".~lock.notes.odt#").unwrap();
    provider.write(lock.inode, 0, b"lockdata").unwrap();
    provider.flush(lock.inode).unwrap();
    assert!(
        !fixture.join(".~lock.notes.odt#").exists(),
        "an ignored file must never be uploaded"
    );
    // Still fully usable locally: reads come from the buffer.
    assert_eq!(provider.read(lock.inode, 0, 8).unwrap(), b"lockdata");
    // Deleting it must not error (nothing to DELETE on the server).
    provider.remove(ROOT_INODE, ".~lock.notes.odt#").unwrap();
    assert!(!fixture.join(".~lock.notes.odt#").exists());

    // 2. Promotion: an ignored temp file renamed onto a real document uploads.
    let tmp = provider.create(ROOT_INODE, "scratch.tmp").unwrap();
    provider.write(tmp.inode, 0, b"the document").unwrap();
    assert!(
        !fixture.join("scratch.tmp").exists(),
        "the temp file itself is never uploaded"
    );
    provider
        .rename(ROOT_INODE, "scratch.tmp", ROOT_INODE, "notes.odt")
        .unwrap();
    assert_eq!(
        std::fs::read(fixture.join("notes.odt")).unwrap(),
        b"the document",
        "renaming an ignored temp onto a real name promotes (uploads) its content"
    );
    assert!(
        !fixture.join("scratch.tmp").exists(),
        "the temp name never materialises on the server"
    );

    std::fs::remove_dir_all(&base).ok();
}
