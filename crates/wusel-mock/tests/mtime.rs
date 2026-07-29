// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for mtime propagation: a mtime set via `setattr` is sent as
//! `X-OC-Mtime` on the upload, so the server-side file carries it (what
//! `cp -p` / `rsync -t` rely on).
//!
//! Both signs are covered. `X-OC-Mtime` is a *signed* unix timestamp, and
//! pre-epoch mtimes are ordinary in real archives (scanned documents, restored
//! backups, anything touched with a 1969 date). The client carries them as
//! `i64` end to end, so the mock must too — a server that quietly drops the
//! negative case would make that whole path untestable.

mod common;

use std::time::UNIX_EPOCH;

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::StateDb;
use wusel_core::webdav::WebDavClient;

/// A file's mtime as signed unix seconds. `SystemTime` has no signed accessor:
/// `duration_since(UNIX_EPOCH)` errors for pre-epoch times and hands the
/// magnitude back inside the error, so the sign has to be reconstructed.
fn mtime_secs(path: &std::path::Path) -> i64 {
    let modified = std::fs::metadata(path).unwrap().modified().unwrap();
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

#[test]
fn setattr_mtime_is_propagated_on_upload() {
    let base = std::env::temp_dir().join(format!("wusel-mock-mtime-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    let backing = fixture.join("note.txt");
    std::fs::write(&backing, b"hello").unwrap();
    let old_backing = fixture.join("scanned.txt");
    std::fs::write(&old_backing, b"hello").unwrap();

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

    assert_eq!(
        mtime_secs(&backing),
        target,
        "the server file must carry the set mtime"
    );

    // The same round-trip with a *pre-epoch* timestamp (1955-11-05T06:00:00Z).
    // The mock used to convert `X-OC-Mtime` through `u64::try_from` and discard
    // the error, so a negative value left the file's own mtime untouched.
    let pre_epoch = -445_824_000i64;
    let old = provider
        .resolve("scanned.txt")
        .unwrap()
        .expect("scanned.txt");
    provider.write(old.inode, 0, b"edited").unwrap();
    provider.set_mtime(old.inode, pre_epoch).unwrap();
    provider.flush(old.inode).unwrap();

    assert_eq!(
        mtime_secs(&old_backing),
        pre_epoch,
        "a pre-epoch mtime must survive the round-trip, not be silently dropped"
    );

    std::fs::remove_dir_all(&base).ok();
}
