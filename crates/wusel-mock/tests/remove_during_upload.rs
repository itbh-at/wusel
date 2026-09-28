// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Deleting a freshly created file while its first upload is still in flight
//! must delete it for good. `rm` right after the file was written and closed:
//! the upload has already read the file and is sending it when the delete lands.
//!
//! The upload is held on the mock so the delete lands inside it every time.
//!
//! Own test binary: it sets the process-global mock PUT delay.

mod common;

use std::time::Duration;
use wusel_core::state::ROOT_INODE;

#[test]
fn deleting_a_new_file_mid_upload_leaves_nothing_behind() {
    let base = std::env::temp_dir().join(format!("wusel-mock-rm-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let engine = common::Engine::start(&mock.addr);

    // `echo q > n1`: created, written, closed — the upload starts.
    let node = engine.create(ROOT_INODE, "n1").expect("create");
    engine.write(node.inode, 0, b"q").expect("write");
    std::env::set_var("WUSEL_MOCK_PUT_DELAY_MS", "1000");
    engine.flush(node.inode).expect("flush");
    std::thread::sleep(Duration::from_millis(300));

    // `rm n1` while those bytes are still on their way.
    engine.remove(ROOT_INODE, "n1").expect("remove");

    engine.wait_for_uploads();
    std::env::remove_var("WUSEL_MOCK_PUT_DELAY_MS");

    let names: Vec<String> = std::fs::read_dir(&fixture)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n == "n1"),
        "the deleted file came back on the server: {names:?}"
    );
    assert!(
        engine.lookup(ROOT_INODE, "n1").is_none(),
        "the deleted file came back in the mount"
    );

    std::fs::remove_dir_all(&base).ok();
}
