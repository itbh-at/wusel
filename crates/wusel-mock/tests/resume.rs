// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end: an upload owed at shutdown is resumed at the next start.
//!
//! With asynchronous write-back, `close()` returns once the change is durable
//! locally — the buffer on disk, the debt recorded in `pending_uploads`. If the
//! daemon stops (or crashes) before the transfer finishes, the change must not
//! be lost: the next start finds the record, re-opens the buffer from it, and
//! uploads. This is the property that makes "flush answered before the upload"
//! safe.
//!
//! The failure is staged deterministically: the destination carries the mock's
//! `*.fail-once` marker so the first `PUT` answers 500, and automatic retries
//! are suppressed (`WUSEL_UPLOAD_RETRY_SECS` very high) so the *first* engine
//! leaves the change owed rather than landing it itself. The *restart* is what
//! completes it — the mock fails only once, so the second attempt succeeds.

mod common;

#[test]
fn a_pending_upload_is_resumed_after_a_restart() {
    let base = std::env::temp_dir().join(format!("wusel-mock-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    let backing = fixture.join("note.fail-once");
    std::fs::write(&backing, b"abcdef").unwrap();

    common::xdg_sandbox(&base);
    // Suppress automatic retries, so the first engine leaves the upload owed
    // instead of landing it — the restart must be what completes it.
    std::env::set_var("WUSEL_UPLOAD_RETRY_SECS", "99999");

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let inode = {
        let mut engine = common::Engine::start(&addr);
        let node = engine
            .resolve("note.fail-once")
            .unwrap()
            .expect("note.fail-once exists");
        engine.write(node.inode, 2, b"XY").unwrap();
        engine.flush(node.inode).expect("flush commits locally");

        // The upload attempts, hits the 500, and — retries suppressed — is left
        // pending, with the server file untouched.
        engine.wait_until_upload_attempted(node.inode);
        assert_eq!(
            std::fs::read(&backing).unwrap(),
            b"abcdef",
            "the failed upload changed nothing on the server"
        );
        assert_eq!(
            engine.upload_state(node.inode),
            Some(wusel_core::state::UploadState::Pending),
            "the change is owed, not lost"
        );
        node.inode
        // The engine drops here: a shutdown with the change still owed.
    };

    // A fresh engine on the same state and scratch. Its start-up resume finds the
    // pending record, re-opens the buffer from it, and uploads — the second
    // attempt lands.
    let engine = common::Engine::start(&addr);
    engine.wait_for_uploads();
    assert_eq!(
        std::fs::read(&backing).unwrap(),
        b"abXYef",
        "the restart resumed the owed upload — no data lost across a restart"
    );
    assert_eq!(
        engine.upload_state(inode),
        None,
        "and the record is cleared once it lands"
    );

    std::fs::remove_dir_all(&base).ok();
}
