// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end: a pending upload whose scratch buffer went missing must not be
//! retried forever.
//!
//! The on-disk record (`pending_uploads`, in SQLite) and the scratch buffer
//! it points at are two separate things, and only one of them being gone —
//! deleted out of band, by something outside this crate — is a state a resume
//! must survive without spinning. This is the counterpart to `resume.rs`,
//! same staging (`*.fail-once` plus a very high retry interval so the first
//! engine leaves the change owed, not landed), but the buffer is removed
//! before the second engine starts.
//!
//! Before the fix, `BufferSize`'s `ENOENT` was classified `Failure::Io` — a
//! transient failure — so the publish script left the record `pending` and
//! the uploader kept nudging it every cycle with nothing to send, restart
//! after restart. `wait_for_uploads` would time out; here it must not.

mod common;

#[test]
fn a_pending_upload_with_no_buffer_is_parked_not_retried_forever() {
    let base = std::env::temp_dir().join(format!("wusel-mock-resume-nobuf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    let backing = fixture.join("note.fail-once");
    std::fs::write(&backing, b"abcdef").unwrap();

    common::xdg_sandbox(&base);
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
        engine.wait_until_upload_attempted(node.inode);
        node.inode
        // The engine drops here, same as the crash/shutdown `resume.rs` picks
        // up from.
    };

    // Simulate the buffer having gone missing between shutdown and restart —
    // the pending record survives (it is in SQLite), the scratch file does
    // not.
    let buffer = wusel_core::config::Account::new("default")
        .cache_dir()
        .join("scratch")
        .join(inode.to_string());
    assert!(buffer.exists(), "the buffer exists before it is removed");
    std::fs::remove_file(&buffer).unwrap();

    let engine = common::Engine::start(&addr);
    // Must settle (park the record), not hang retrying a buffer that will
    // never come back — this is the assertion the fix is about.
    engine.wait_for_uploads();
    assert_eq!(
        std::fs::read(&backing).unwrap(),
        b"abcdef",
        "nothing was ever sent — there was no buffer to send"
    );
    assert_eq!(
        engine.upload_state(inode),
        Some(wusel_core::state::UploadState::Error),
        "parked as an error, visible to the user, instead of retried forever"
    );

    std::fs::remove_dir_all(&base).ok();
}
