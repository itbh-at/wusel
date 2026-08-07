// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end: a promotion upload that the server rejects must NOT fail the
//! rename.
//!
//! Renaming a local-only file (a deferred create, or an ignored editor temp)
//! onto a real name "promotes" it — the buffer is uploaded under the new name.
//! That is the atomic-save pattern of office suites. If the upload fails, the
//! rename must still succeed: it is already committed locally, so returning an
//! error would tell the kernel "the rename did not happen" while our state says
//! it did, leaving the dentry cache permanently out of step. It is also the
//! wrong answer for the user, for whom EIO on the rename reads as "your
//! document could not be saved" although the content is safe in the scratch and
//! only the upload is outstanding.
//!
//! The failure is staged with the mock's `*.fail-once` marker (the first `PUT`
//! to such a name answers 500) — the same fault injection `flush_retry` uses,
//! here applied to the *destination* name so it is the promotion upload that
//! fails. `ignore_list` covers the succeeding half of the same path.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn a_failed_promotion_upload_does_not_fail_the_rename() {
    let base = std::env::temp_dir().join(format!("wusel-mock-promote-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    // An empty server root: everything in this test starts life local-only.
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);
    std::env::set_var("WUSEL_UPLOAD_RETRY_SECS", "1");

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let engine = common::Engine::start(&addr);

    // A deferred create (no server identity yet) with some content.
    let draft = engine.create(ROOT_INODE, "draft.txt").unwrap();
    engine.write(draft.inode, 0, b"hello").unwrap();

    // Renaming it to a non-ignored name promotes it — the buffer is uploaded.
    // The destination carries the `.fail-once` marker, so that upload is
    // answered with a 500; the *rename* must still stand.
    engine
        .rename(ROOT_INODE, "draft.txt", ROOT_INODE, "report.fail-once")
        .expect("the rename stands locally — the upload is asynchronous");
    let node = engine.stat(draft.inode).unwrap();
    assert_eq!(node.name, "report.fail-once", "the local rename committed");
    assert_eq!(node.path, "report.fail-once");

    // The promotion upload runs behind the rename and hits the injected 500 — a
    // transient failure — so it is retried automatically and lands under the new
    // name. The rename never failed and the content was never at risk.
    engine.wait_for_uploads();
    assert_eq!(
        std::fs::read(fixture.join("report.fail-once")).unwrap(),
        b"hello",
        "the promotion upload reaches the server under the new name"
    );
    assert_eq!(
        engine.upload_state(draft.inode),
        None,
        "and the pending record is cleared once it lands"
    );

    std::fs::remove_dir_all(&base).ok();
}
