// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Unpinning a file that a pinned folder still covers reports that it stays
//! offline — the signal that drives the "kept offline by its folder" notice, so
//! the user learns why unpinning one file inside a pinned folder does nothing.
//!
//! Own test binary — `common::xdg_sandbox` mutates the process-global XDG
//! environment, so it must not share a binary with another pin test.

mod common;

#[test]
fn unpinning_a_file_inside_a_pinned_folder_reports_it_stays_offline() {
    let base = std::env::temp_dir().join(format!("wusel-mock-unpin-cov-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("Folder")).unwrap();
    std::fs::write(fixture.join("Folder/a.txt"), b"a").unwrap();
    std::fs::write(fixture.join("Folder/b.txt"), b"b").unwrap();
    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let mut engine = common::Engine::start(&mock.addr);

    engine.pin("Folder").expect("pin folder");
    assert!(
        engine.is_pinned("Folder/a.txt").unwrap(),
        "covered by the folder pin"
    );

    // Unpinning one file inside cannot lift it — the folder still covers it —
    // and unpin says so.
    let still = engine
        .unpin_reports_covered("Folder/a.txt")
        .expect("unpin a covered file");
    assert!(still, "the file stays offline while its folder is pinned");
    assert!(engine.is_pinned("Folder/a.txt").unwrap());

    // Unpinning the folder itself does lift it (and its children).
    let still = engine
        .unpin_reports_covered("Folder")
        .expect("unpin folder");
    assert!(!still, "the folder is no longer offline");
    assert!(
        !engine.is_pinned("Folder/a.txt").unwrap(),
        "children follow the folder"
    );

    std::fs::remove_dir_all(&base).ok();
}
