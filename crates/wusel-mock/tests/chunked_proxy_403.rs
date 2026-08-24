// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A reverse proxy in front of Nextcloud can answer the chunked-assembly MOVE
//! with an error (a timeout as 403/5xx, or a WAF) while Nextcloud finishes
//! assembling the file — observed live on a real deployment. wusel must trust
//! the assembled file, not the MOVE's status: the upload landed, so it is a
//! success, not a parked "sync error" (and no "conflicted copy" on retry).
//!
//! The mock's `.proxy-403` marker assembles the file but answers the MOVE 403.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn a_proxy_403_on_the_assembly_move_counts_as_success_when_the_file_landed() {
    let base = std::env::temp_dir().join(format!("wusel-mock-p403-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);

    // >4 MiB so it is chunked; the name carries the mock's proxy-403 marker.
    let data: Vec<u8> = (0..6 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let node = engine
        .create(ROOT_INODE, "big.proxy-403.bin")
        .expect("create");
    engine.write(node.inode, 0, &data).expect("write");
    engine.flush(node.inode).expect("flush");
    engine.wait_for_uploads();

    // The file assembled on the server byte-for-byte, despite the MOVE's 403.
    assert_eq!(
        std::fs::read(fixture.join("big.proxy-403.bin")).unwrap(),
        data,
        "the file must be on the server even though the MOVE answered 403"
    );
    // wusel recognised the success: the pending record is cleared, not parked as
    // an error (which would show a red emblem on a file that is actually synced).
    assert!(
        engine.upload_state(node.inode).is_none(),
        "a landed upload must not stay parked: {:?}",
        engine.upload_state(node.inode)
    );
    // And nothing spawned a conflicted copy.
    let copies = std::fs::read_dir(&fixture)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("conflicted copy"))
        .count();
    assert_eq!(
        copies, 0,
        "a recognised success must not spawn a conflicted copy"
    );

    std::fs::remove_dir_all(&base).ok();
}
