// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end: a file deleted on the server while our cache still lists it. A
//! read must not be a hard error storm — it returns `NotFound` (mapped to ESTALE
//! at the FUSE layer) and prunes the stale node, so the file disappears from
//! listings (what a file manager sitting in the directory expects).

mod common;

use wusel_fsm::{Failure, Outcome};

use std::time::Duration;

use wusel_core::config::Account;
use wusel_core::state::ROOT_INODE;

#[test]
fn a_server_side_delete_prunes_the_stale_node_on_read() {
    let base = std::env::temp_dir().join(format!("wusel-mock-del-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    let backing = fixture.join("doc.pdf");
    std::fs::write(&backing, vec![b'x'; 4096]).unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let account = Account::new("default");
    let mut engine = common::Engine::start(&addr);

    let node = engine
        .provider()
        .resolve("doc.pdf")
        .unwrap()
        .expect("doc.pdf");
    // A normal read works — and hydrates the file into the cache in the
    // background (opening a file caches it).
    assert_eq!(engine.read(node.inode, 0, 4).unwrap(), b"xxxx");

    // Wait for that hydration to finish, so the cache is in a known state. Then
    // simulate the server-side delete *and* drop the cached copy — a read of a
    // still-cached file is (correctly) served locally and would not notice the
    // deletion; the read-path prune is the online-only path, for an uncached
    // read (a cache miss / eviction). Server-side deletion of a *cached* file is
    // caught by background revalidation instead.
    let blob = account
        .blob_cache_dir()
        .join(node.file_id.expect("mock serves file ids").to_string());
    for _ in 0..100 {
        if blob.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::remove_file(&backing).unwrap();
    let _ = std::fs::remove_file(&blob);
    let _ = std::fs::remove_file(blob.with_extension("etag"));

    // The uncached read now goes live, returns NotFound (not a generic error)
    // and prunes the node.
    // A stale handle is what the caller should see: the object is gone, so
    // retrying into the same 404 helps nobody.
    match engine.read(node.inode, 0, 4096) {
        Err(Outcome::Failed(Failure::Stale)) => {}
        other => panic!("expected a stale handle, got {other:?}"),
    }
    // The stale entry is gone from the directory listing.
    let names: Vec<String> = engine
        .list_dir(ROOT_INODE)
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert!(
        !names.contains(&"doc.pdf".to_string()),
        "the deleted file must be pruned from the listing, got {names:?}"
    );

    std::fs::remove_dir_all(&base).ok();
}
