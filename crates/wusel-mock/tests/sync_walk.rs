// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for the background syncer (Part 1): a file deleted *deep* in the
//! tree is found by the ETag-guided walk on a (path-less) push trigger — without
//! re-listing everything — and reported as a kernel-invalidation event. Relies on
//! the mock propagating directory ETags up the tree, as Nextcloud does.

mod common;

use std::time::Duration;

use wusel_core::provider::Invalidation;

fn dir_names(engine: &common::Engine, inode: u64) -> Vec<String> {
    let mut v: Vec<String> = engine.list_dir(inode).into_iter().map(|n| n.name).collect();
    v.sort();
    v
}

#[test]
fn the_syncer_finds_a_deeply_nested_delete_via_etag_walk() {
    let base = std::env::temp_dir().join(format!("wusel-mock-syncwalk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    let deep = fixture.join("A").join("B");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("keep.txt"), b"k").unwrap();
    std::fs::write(deep.join("gone.txt"), b"g").unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let mut engine = common::Engine::start(&addr);

    // List root → A → B so the whole path is cached (the walk only descends into
    // already-listed directories).
    let b = engine.resolve("A/B").unwrap().expect("A/B");
    assert_eq!(dir_names(&engine, b.inode), vec!["gone.txt", "keep.txt"]);

    let invalidations = engine
        .provider()
        .take_invalidations()
        .expect("invalidation stream");

    // Delete deep in the tree, then fire the (path-less) push trigger.
    std::fs::remove_file(deep.join("gone.txt")).unwrap();
    engine.provider().sync_trigger().send(()).unwrap();

    // The syncer walks root→A→B by changed ETags and reconciles B.
    let mut pruned = false;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(50));
        if !dir_names(&engine, b.inode).contains(&"gone.txt".to_string()) {
            pruned = true;
            break;
        }
    }
    assert!(
        pruned,
        "the syncer must prune the deleted file from B's listing"
    );
    assert!(
        dir_names(&engine, b.inode).contains(&"keep.txt".to_string()),
        "the sibling that stayed must remain"
    );

    // And it must have reported the removal for kernel invalidation.
    let mut saw_event = false;
    while let Ok(inv) = invalidations.try_recv() {
        match inv {
            Invalidation::Entry { parent, name, .. } if parent == b.inode && name == "gone.txt" => {
                saw_event = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_event,
        "a removed entry must be reported for kernel invalidation"
    );

    std::fs::remove_dir_all(&base).ok();
}
