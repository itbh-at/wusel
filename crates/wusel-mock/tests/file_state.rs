// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The per-file state an OS integration draws an emblem from.
//!
//! Every state is always visible — that is the contract the file-manager
//! integration relies on — so all four are checked here rather than the one
//! that happens to be easiest to produce.

mod common;

use wusel_core::provider::FileState;
use wusel_core::state::ROOT_INODE;

#[test]
fn every_state_is_reported() {
    let base = std::env::temp_dir().join(format!("wusel-mock-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("cold.txt"), b"never read").unwrap();
    std::fs::write(fixture.join("warm.txt"), b"read once").unwrap();
    std::fs::write(fixture.join("kept.txt"), b"pinned").unwrap();
    std::fs::write(fixture.join("edited.txt"), b"about to change").unwrap();

    common::xdg_sandbox(&base);
    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();
    let mut engine = common::Engine::start(&addr);

    let cold = engine.resolve("cold.txt").unwrap().expect("cold.txt");
    let warm = engine.resolve("warm.txt").unwrap().expect("warm.txt");
    let kept = engine.resolve("kept.txt").unwrap().expect("kept.txt");
    let edited = engine.resolve("edited.txt").unwrap().expect("edited.txt");

    // Never fetched: metadata only, which is the default for a VFS-first mount.
    assert_eq!(engine.state(cold.inode), Some(FileState::OnlineOnly));

    // Read once, so a whole copy is in the cache — evictable, but local.
    engine.read(warm.inode, 0, 9).unwrap();
    let mut cached = None;
    for _ in 0..100 {
        if engine.state(warm.inode) == Some(FileState::Cached) {
            cached = Some(FileState::Cached);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        cached,
        Some(FileState::Cached),
        "a read fills the cache in the background; the emblem follows it"
    );

    // Pinned: kept offline on purpose, and exempt from eviction.
    engine.pin("kept.txt").unwrap();
    assert_eq!(engine.state(kept.inode), Some(FileState::Pinned));

    // An unsaved edit outranks everything else — it is the most actionable
    // thing there is, and the only one that means data lives nowhere else.
    engine.write(edited.inode, 0, b"CHANGED").unwrap();
    assert_eq!(engine.state(edited.inode), Some(FileState::Modified));

    // A directory nobody pinned carries no content state at all.
    assert_eq!(engine.state(ROOT_INODE), None);

    std::fs::remove_dir_all(&base).ok();
}
