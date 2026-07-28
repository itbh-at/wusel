// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Conflict handling — the opt-in text merge (`[sync] text_merge = true`): when
//! local and server edits are disjoint, a 3-way merge combines them and uploads
//! the result instead of making a conflicted copy.

mod common;

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::StateDb;
use wusel_core::webdav::WebDavClient;

#[test]
fn disjoint_edits_are_merged_when_enabled() {
    let base = std::env::temp_dir().join(format!("wusel-mock-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    let backing = fixture.join("doc.txt");
    std::fs::write(&backing, b"line1\nline2\nline3\n").unwrap();

    common::xdg_sandbox(&base);

    // Enable the opt-in text merge for the default account.
    let account = Account::new("default");
    std::fs::create_dir_all(account.config_path().parent().unwrap()).unwrap();
    std::fs::write(account.config_path(), "[sync]\ntext_merge = true\n").unwrap();

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let dav = WebDavClient::new(
        reqwest::Client::new(),
        &format!("http://{addr}"),
        "alice",
        "pw",
    );
    std::fs::create_dir_all(account.state_db_path().parent().unwrap()).unwrap();
    let state = StateDb::open(&account.state_db_path()).unwrap();
    let mut provider = Provider::new(dav, state, &account).unwrap();

    let node = provider.resolve("doc.txt").unwrap().expect("doc.txt");
    // Local edit: change line 1 (keeps the base cached for the merge).
    provider.write(node.inode, 0, b"LINE1").unwrap();

    // Server edit under us: change line 3.
    std::fs::write(&backing, b"line1\nline2\nLINE3\n").unwrap();

    provider.flush(node.inode).unwrap();

    // Both disjoint edits survive; no conflicted copy is made.
    assert_eq!(std::fs::read(&backing).unwrap(), b"LINE1\nline2\nLINE3\n");
    let has_copy = std::fs::read_dir(&fixture)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains("conflicted copy"));
    assert!(!has_copy, "a clean merge must not leave a conflicted copy");

    std::fs::remove_dir_all(&base).ok();
}
