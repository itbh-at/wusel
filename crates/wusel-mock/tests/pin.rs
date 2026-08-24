// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for pinning: a `Provider` (real engine) against the `wusel-mock`
//! WebDAV server. Pinning the root crawls the whole tree and hydrates every file
//! into the cache with a `.pin` marker — no Nextcloud, no FUSE, pure Rust.

mod common;

use wusel_core::config::Account;

fn count_ext(dir: &std::path::Path, ext: Option<&str>) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == ext)
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn pin_root_hydrates_the_whole_tree() {
    let base = std::env::temp_dir().join(format!("wusel-mock-pin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("Sub Folder")).unwrap();
    std::fs::write(fixture.join("Notes.txt"), b"hello").unwrap();
    std::fs::write(fixture.join("Sub Folder/deep.txt"), b"nested").unwrap();
    std::fs::write(fixture.join("Sub Folder/more.txt"), b"and more").unwrap();

    // Point the account's XDG dirs at a throwaway location.
    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let account = Account::new("default");
    let mut engine = common::Engine::start(&addr);

    // Pin the root → the legacy "download everything".
    let hydrated = engine.pin("").expect("pin root");
    assert_eq!(hydrated, 3, "three files across root and the subfolder");

    let blobs = account.blob_cache_dir();
    assert_eq!(count_ext(&blobs, None), 3, "three cached blobs");
    assert_eq!(
        count_ext(&blobs, Some("pin")),
        3,
        "each blob has a .pin marker"
    );

    assert!(engine
        .pins()
        .unwrap()
        .iter()
        .any(|(p, is_dir)| p.is_empty() && *is_dir));

    std::fs::remove_dir_all(&base).ok();
}
