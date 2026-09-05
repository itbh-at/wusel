// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A Team/Group folder's root is marked — and only its root.
//!
//! The regression this guards: a group-folder root is always a *directory*, and
//! a directory carries no content emblem. The state read must still surface the
//! folder's kind, or the marker never reaches the file manager even though every
//! layer below computed it correctly. This drives the same
//! metadata → reconcile → state read path a real mount uses, against a mock that
//! answers `nc:mount-type` / `nc:is-mount-root` the way Nextcloud does.

mod common;

use wusel_core::provider::FileState;

#[test]
fn only_the_group_folder_root_is_marked() {
    let base = std::env::temp_dir().join(format!("wusel-mock-gf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("Team/Inside")).unwrap();
    std::fs::create_dir_all(fixture.join("Plain")).unwrap();
    std::fs::write(fixture.join("Team/doc.txt"), b"in the team folder").unwrap();

    // The mock presents "Team" as a group-folder mount before it starts serving.
    std::env::set_var("WUSEL_MOCK_GROUP_FOLDERS", "Team");

    common::xdg_sandbox(&base);
    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();
    let mut engine = common::Engine::start(&addr);

    let team = engine.resolve("Team").unwrap().expect("Team");
    let inside = engine.resolve("Team/Inside").unwrap().expect("Team/Inside");
    let doc = engine
        .resolve("Team/doc.txt")
        .unwrap()
        .expect("Team/doc.txt");
    let plain = engine.resolve("Plain").unwrap().expect("Plain");

    // The root of the mount is the one thing marked — the whole point.
    assert!(
        engine.group_root(team.inode),
        "the Team folder's root must be marked as a group folder"
    );
    // A directory has no content emblem; the kind must ride without one.
    assert_eq!(
        engine.state(team.inode),
        None,
        "a directory carries no content state, only its kind"
    );

    // Everything inside carries the same mount type on the server, so the
    // negative assertions are what prove the marking does not bleed downward.
    assert!(
        !engine.group_root(inside.inode),
        "a subdirectory of the Team folder must not be marked"
    );
    assert!(
        !engine.group_root(doc.inode),
        "a file inside the Team folder must not be marked"
    );
    assert!(
        !engine.group_root(plain.inode),
        "an ordinary folder must not be marked"
    );

    // A plain file still reports its ordinary content state — the kind read did
    // not disturb the state axis.
    assert_eq!(engine.state(doc.inode), Some(FileState::OnlineOnly));
}
