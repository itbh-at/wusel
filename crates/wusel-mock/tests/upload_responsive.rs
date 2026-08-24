// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Reading metadata must never wait on an in-flight upload. With asynchronous
//! write-back, `flush` returns once the change is durable locally and the upload
//! runs on behind it, holding the object's Publish flow for as long as the
//! transfer takes — minutes over a slow link. A `getattr` on that same file (an
//! `ls -l`, a file manager refreshing its view) must be answered from the
//! committed row at once, beside the upload, not queued behind it.

mod common;

use std::time::{Duration, Instant};
use wusel_core::state::ROOT_INODE;

#[test]
fn a_stat_is_not_blocked_by_an_in_flight_upload() {
    let base = std::env::temp_dir().join(format!("wusel-mock-upresp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);

    let body = b"hello world payload"; // 19 bytes
    let node = engine.create(ROOT_INODE, "big.txt").expect("create");
    engine.write(node.inode, 0, body).expect("write");

    // Delay only the flush's upload PUT (create/write have already happened), so
    // the object's Publish flow is held for 3 s on the mock while we probe it.
    std::env::set_var("WUSEL_MOCK_PUT_DELAY_MS", "3000");
    engine.flush(node.inode).expect("flush"); // returns at commit, upload runs on

    // The upload is now in flight (sleeping on the mock). A getattr on the very
    // file being uploaded must come straight back from the committed row.
    let started = Instant::now();
    let seen = engine.stat(node.inode).expect("stat answers");
    let waited = started.elapsed();

    assert!(
        waited < Duration::from_millis(1500),
        "getattr waited on the in-flight upload ({waited:?}); it must run beside it"
    );
    assert_eq!(
        seen.size,
        body.len() as u64,
        "getattr reports the committed size while the upload is still in flight"
    );

    engine.wait_for_uploads();
    std::env::remove_var("WUSEL_MOCK_PUT_DELAY_MS");
    std::fs::remove_dir_all(&base).ok();
}
