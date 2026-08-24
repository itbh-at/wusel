// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Pins are the user's intent, so they must outlive everything that is merely a
//! copy of the server: the state database, the blobs, the whole cache
//! directory. This is the property the move out of SQLite was for, tested where
//! it can actually be seen — a real engine against the mock server.

mod common;

use wusel_core::config::Account;

/// What `wusel cache clear` does to an account, minus the printing: the database
/// and every blob, and nothing else.
fn clear_the_cache(account: &Account) {
    let db = account.state_db_path();
    for suffix in ["", "-wal", "-shm"] {
        let mut p = db.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(p));
    }
    let _ = std::fs::remove_dir_all(account.cache_dir());
}

#[test]
fn clearing_the_cache_keeps_the_pins() {
    let base = std::env::temp_dir().join(format!("wusel-mock-pinsurvive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("Photos")).unwrap();
    std::fs::write(fixture.join("Photos/pic.jpg"), b"jpeg").unwrap();
    std::fs::write(fixture.join("Notes.txt"), b"hello").unwrap();
    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();
    let account = Account::new("default");

    {
        let mut engine = common::Engine::start(&addr);
        engine.pin("Photos").expect("pin the directory");
        assert_eq!(engine.pins().unwrap(), vec![("Photos".to_string(), true)]);
    }

    // Somebody needs disk space before a trip. This is the exact moment the old
    // arrangement lost the pins — and the exact moment they matter most.
    clear_the_cache(&account);
    assert!(
        !account.state_db_path().exists(),
        "the database really is gone"
    );

    let engine = common::Engine::start(&addr);
    assert_eq!(
        engine.pins().unwrap(),
        vec![("Photos".to_string(), true)],
        "the pin is intent, not cache: clearing the cache must not drop it"
    );

    std::fs::remove_dir_all(&base).ok();
}
