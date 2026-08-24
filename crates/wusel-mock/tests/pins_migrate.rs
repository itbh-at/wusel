// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Taking the pins out of a database written before they had their own file.
//!
//! Its own binary because the harness allows one XDG sandbox per test process —
//! the environment mutation it does is only sound while the process is
//! effectively single-threaded.

mod common;

use wusel_core::config::Account;

#[test]
fn pins_in_an_older_database_are_taken_over_once() {
    let base = std::env::temp_dir().join(format!("wusel-mock-pinmigrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("Photos")).unwrap();
    std::fs::write(fixture.join("Photos/pic.jpg"), b"jpeg").unwrap();
    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();
    let account = Account::new("default");

    // An account as an older wusel left it: pins in the database, no pins file.
    {
        let mut engine = common::Engine::start(&addr);
        engine.pin("Photos").expect("pin the directory");
    }
    let pins_file = account.config_dir().join("pins.json");
    let db = rusqlite::Connection::open(account.state_db_path()).unwrap();
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS pins (path TEXT PRIMARY KEY, is_dir INTEGER NOT NULL);
         INSERT OR REPLACE INTO pins VALUES ('Photos', 1), ('Notes.txt', 0);",
    )
    .unwrap();
    drop(db);
    std::fs::remove_file(&pins_file).expect("remove the new-style file");

    // Starting picks them up …
    let engine = common::Engine::start(&addr);
    assert_eq!(
        engine.pins().unwrap(),
        vec![
            ("Notes.txt".to_string(), false),
            ("Photos".to_string(), true)
        ],
        "the old database's pins were taken over"
    );
    drop(engine);

    // … and a later start does not undo what the user has since done. The rows
    // are still in the database, so a migration that ran twice would resurrect
    // a pin they deliberately removed.
    let pins = wusel_core::pins::Pins::new(&account.config_dir());
    pins.remove("Notes.txt").unwrap();
    let engine = common::Engine::start(&addr);
    assert_eq!(
        engine.pins().unwrap(),
        vec![("Photos".to_string(), true)],
        "a second migration would have brought the removed pin back"
    );

    std::fs::remove_dir_all(&base).ok();
}
