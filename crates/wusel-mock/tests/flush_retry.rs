// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end: a failed upload must NOT lose the buffered edit. The mock fails
//! the first `PUT` to a `*.fail-once` file (a stand-in for the 500 a real server
//! returned on an editor's swap file); the client must keep the scratch so the
//! next flush retries and the content still reaches the server.

mod common;

#[test]
fn a_failed_flush_keeps_the_buffer_and_a_retry_uploads() {
    let base = std::env::temp_dir().join(format!("wusel-mock-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    // The server-side file whose FIRST upload the mock rejects with 500.
    let backing = fixture.join("note.fail-once");
    std::fs::write(&backing, b"abcdef").unwrap();

    common::xdg_sandbox(&base);
    // Retry the transient failure quickly so the test does not wait on the
    // production cadence.
    std::env::set_var("WUSEL_UPLOAD_RETRY_SECS", "1");

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let mut engine = common::Engine::start(&addr);

    let node = engine
        .resolve("note.fail-once")
        .unwrap()
        .expect("note.fail-once exists");

    engine.write(node.inode, 2, b"XY").unwrap();

    // Flush succeeds locally and is answered; the upload runs behind it and hits
    // the injected 500 — a *transient* failure, so the change stays queued and
    // the uploader retries automatically. The second attempt (the mock fails
    // only once) lands it. No data lost, and no manual step.
    engine
        .flush(node.inode)
        .expect("flush commits locally and returns ok — the upload is async");
    engine.wait_for_uploads();
    assert_eq!(
        std::fs::read(&backing).unwrap(),
        b"abXYef",
        "the automatic retry uploads the buffered edit — no data lost"
    );
    assert_eq!(
        engine.upload_state(node.inode),
        None,
        "and the pending record is cleared once it lands"
    );

    std::fs::remove_dir_all(&base).ok();
}
